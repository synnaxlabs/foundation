//! The readers of a shard's indexes, and which of them to wake.

use std::mem;
use std::ops::Range;

use buffer::Buffer;
use delivery::{Position, Reader, Readers, Start};
use types::channel::Slot;
use types::frame::{Frame, Path};
use types::time::Stamp;

/// A reader on its shard: the slot of its index and its session there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Key {
    /// The slot of the reader's index.
    pub(crate) slot: Slot,
    /// The reader's session on the index.
    pub(crate) session: delivery::Key,
}

/// The readers of each index of a shard, by the index's place in the shard, and the
/// readers to wake. Each call is on the shard's thread.
#[derive(Debug, Default)]
pub(crate) struct Set {
    /// The readers of each carried index, by place.
    entries: Vec<Entry>,
    /// The place of each index whose readers may be
    /// [pending](delivery::Readers::pending), each once.
    listed: Vec<usize>,
    /// Each reader to wake, with a frame to take. A key can repeat.
    keys: Vec<Key>,
    /// The commits the buffer had ended at the last [`Set::woken`].
    commits: u64,
}

/// The readers of one carried index.
#[derive(Debug)]
struct Entry {
    slot: Slot,
    readers: Readers,
    /// Whether the set lists the index.
    listed: bool,
}

impl Set {
    /// Adds the readers of the index at `slot` at the next place, with `live` as the
    /// seq of its next live frame. The shard carries its indexes in the same order.
    pub(crate) fn carry(&mut self, slot: Slot, live: u64) {
        self.entries.push(Entry {
            slot,
            readers: Readers::new(live),
            listed: false,
        });
    }

    /// Opens an unnamed complete reader on the index at `place` at seq `live`, with a
    /// credit of `limit_bytes`.
    pub(crate) fn open_complete(
        &mut self,
        place: usize,
        live: u64,
        limit_bytes: u64,
    ) -> delivery::complete::Key {
        let start = Start::At(Position {
            live,
            backfill: None,
        });
        let readers = &mut self.entries[place].readers;
        readers.open(Reader::Unnamed, start, limit_bytes).key
    }

    /// Opens an unnamed latest reader on the index at `place` at mesh time `now`. It
    /// wakes at once when the index has a newest frame.
    pub(crate) fn open_latest(
        &mut self,
        place: usize,
        now: Stamp,
    ) -> delivery::latest::Key {
        let entry = &mut self.entries[place];
        let opened = entry.readers.open_latest(None, now);
        if opened.woken {
            wake(&mut self.keys, entry.slot, &[opened.key]);
        }
        opened.key
    }

    /// Raises the credit of the complete reader `session` on the index at `place`, as
    /// [`Readers::grant`] does.
    pub(crate) fn grant(
        &mut self,
        place: usize,
        session: delivery::complete::Key,
        limit_bytes: u64,
    ) {
        self.entries[place].readers.grant(session, limit_bytes);
    }

    /// Takes the next frame of the reader `session` on the index at `place`, or `None`
    /// when none waits.
    ///
    /// # Panics
    ///
    /// If the reader is not open.
    pub(crate) fn take(
        &mut self,
        place: usize,
        session: delivery::Key,
    ) -> Option<Frame> {
        self.entries[place].readers.take(session)
    }

    /// Closes the reader `session` on the index at `place` at mesh time `now`. Its
    /// waiting frames do not go out, and [`woken`](Self::woken) does not name it.
    ///
    /// # Panics
    ///
    /// If the reader is not open.
    pub(crate) fn close(&mut self, place: usize, session: delivery::Key, now: Stamp) {
        let entry = &mut self.entries[place];
        entry.readers.close(session, now);
        let key = Key {
            slot: entry.slot,
            session,
        };
        self.keys.retain(|&woken| woken != key);
    }

    /// Gives `frame`, stored in the buffer at `seq`, to the readers of the index at
    /// `place`. A live frame is the newest frame at once, and goes to complete readers
    /// after the commit that holds it. A backfill frame goes to no reader.
    pub(crate) fn store(&mut self, place: usize, frame: Frame, seq: Range<u64>) {
        if frame.path() == Path::Backfill {
            return;
        }
        let entry = &mut self.entries[place];
        entry.readers.queue(&frame, seq);
        wake(&mut self.keys, entry.slot, entry.readers.put(frame));
        if entry.readers.pending() && !mem::replace(&mut entry.listed, true) {
            self.listed.push(place);
        }
    }

    /// Gives `frame`, which found no room in the buffer, to the readers of the index at
    /// `place`: a live frame is the newest frame. Complete readers never get it.
    pub(crate) fn lose(&mut self, place: usize, frame: Frame) {
        if frame.path() == Path::Backfill {
            return;
        }
        let entry = &mut self.entries[place];
        wake(&mut self.keys, entry.slot, entry.readers.put(frame));
    }

    /// Replaces `keys` with the readers to wake since the last call, each once, in slot
    /// order and with the latest readers of an index first. Complete readers first get
    /// the live frames on disk in `buffer`. Reads no index when no commit ended since
    /// the last call, as only a commit moves `durable`.
    pub(crate) fn woken(&mut self, buffer: &Buffer, keys: &mut Vec<Key>) {
        let commits = buffer.commits();
        if mem::replace(&mut self.commits, commits) != commits {
            let (entries, woken) = (&mut self.entries, &mut self.keys);
            self.listed.retain(|&place| {
                let entry = &mut entries[place];
                let durable = buffer.durable(entry.slot, Path::Live).seq;
                wake(woken, entry.slot, entry.readers.release(durable));
                entry.listed = entry.readers.pending();
                entry.listed
            });
        }
        keys.clear();
        mem::swap(keys, &mut self.keys);
        keys.sort_unstable();
        keys.dedup();
    }

    /// The place of each index whose readers may be pending.
    #[cfg(test)]
    pub(crate) fn listed(&self) -> &[usize] {
        &self.listed
    }
}

/// Adds the `sessions` of the index at `slot` to `keys`.
fn wake<K: Copy + Into<delivery::Key>>(
    keys: &mut Vec<Key>,
    slot: Slot,
    sessions: &[K],
) {
    if sessions.is_empty() {
        return;
    }
    let woken = sessions.iter().map(|&session| Key {
        slot,
        session: session.into(),
    });
    keys.extend(woken);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use types::frame::key_set::{Group, Interner, KeySet};
    use types::frame::{self, Draft, Form};

    use super::*;
    use crate::common::{key, pool};

    /// Index frames of one index with no data channels.
    struct Frames {
        pool: block::Pool,
        set: Arc<KeySet>,
    }

    impl Frames {
        fn new() -> Self {
            let index = Group {
                index: key(Slot::new(0)),
                data: &[],
            };
            Self {
                pool: pool(4096),
                set: Interner::new().intern(&[index]),
            }
        }

        /// A frame on `path` with the samples `seq`.
        fn frame(&self, path: Path, seq: Range<u64>) -> Frame {
            let count = u32::try_from(seq.end - seq.start).expect("a short test frame");
            let len = usize::try_from(count).expect("a usize holds a u32") * 8;
            let mut draft = Draft::new(&self.pool, &self.set, Form::Raw, &[(0, len)])
                .expect("the pool has room");
            draft.set_count(0, count);
            draft.set_seq(0, seq.start);
            draft.freeze(path)
        }
    }

    fn now() -> Stamp {
        "2026-10-06T00:00:00Z".parse().expect("a valid stamp")
    }

    /// A set that carries `indexes` indexes, at slots from 0, each with no live frame.
    fn carried(indexes: u32) -> Set {
        let mut set = Set::default();
        for n in 0..indexes {
            set.carry(Slot::new(n), 0);
        }
        set
    }

    /// The readers to wake since the last call, with no commit read.
    fn woken(set: &mut Set) -> Vec<Key> {
        mem::take(&mut set.keys)
    }

    fn reader(place: u32, session: impl Into<delivery::Key>) -> Key {
        Key {
            slot: Slot::new(place),
            session: session.into(),
        }
    }

    fn range(frame: &Frame) -> Option<frame::Range> {
        frame.range(0)
    }

    mod store {
        use super::*;

        #[test]
        fn wakes_the_latest_readers_of_a_live_frame_at_once() {
            let frames = Frames::new();
            let mut set = carried(2);
            let latest = set.open_latest(1, now());
            assert_eq!(woken(&mut set), []);
            set.store(1, frames.frame(Path::Live, 0..2), 0..2);
            assert_eq!(woken(&mut set), [reader(1, latest)]);
            assert_eq!(set.listed(), []);
            let taken = set.take(1, latest.into()).expect("the newest frame");
            assert_eq!(range(&taken), Some(frame::Range { seq: 0, count: 2 }));
        }

        #[test]
        fn lists_an_index_once_while_its_live_frames_wait_for_a_commit() {
            let frames = Frames::new();
            let mut set = carried(2);
            let complete = set.open_complete(1, 0, u64::MAX);
            set.store(1, frames.frame(Path::Live, 0..1), 0..1);
            set.store(1, frames.frame(Path::Live, 1..3), 1..3);
            assert_eq!(set.listed(), [1]);
            assert_eq!(woken(&mut set), []);
            assert!(set.take(1, complete.into()).is_none());
        }

        #[test]
        fn gives_a_backfill_frame_to_no_reader() {
            let frames = Frames::new();
            let mut set = carried(1);
            let latest = set.open_latest(0, now());
            let _ = set.open_complete(0, 0, u64::MAX);
            set.store(0, frames.frame(Path::Backfill, 0..2), 0..2);
            assert_eq!(woken(&mut set), []);
            assert_eq!(set.listed(), []);
            assert!(set.take(0, latest.into()).is_none());
            set.store(0, frames.frame(Path::Live, 0..1), 0..1);
            assert_eq!(woken(&mut set), [reader(0, latest)]);
        }
    }

    mod lose {
        use super::*;

        #[test]
        fn wakes_the_latest_readers_of_a_live_frame_and_lists_nothing() {
            let frames = Frames::new();
            let mut set = carried(1);
            let latest = set.open_latest(0, now());
            let complete = set.open_complete(0, 0, u64::MAX);
            set.lose(0, frames.frame(Path::Live, 0..2));
            assert_eq!(woken(&mut set), [reader(0, latest)]);
            assert_eq!(set.listed(), []);
            assert!(set.take(0, complete.into()).is_none());
            let taken = set.take(0, latest.into()).expect("the newest frame");
            assert_eq!(range(&taken), Some(frame::Range { seq: 0, count: 2 }));
        }

        #[test]
        fn gives_a_backfill_frame_to_no_reader() {
            let frames = Frames::new();
            let mut set = carried(1);
            let latest = set.open_latest(0, now());
            set.lose(0, frames.frame(Path::Backfill, 0..2));
            assert_eq!(woken(&mut set), []);
            assert!(set.take(0, latest.into()).is_none());
        }
    }

    mod open_latest {
        use super::*;

        #[test]
        fn wakes_at_once_when_the_index_has_a_newest_frame() {
            let frames = Frames::new();
            let mut set = carried(1);
            set.store(0, frames.frame(Path::Live, 0..1), 0..1);
            let latest = set.open_latest(0, now());
            assert_eq!(woken(&mut set), [reader(0, latest)]);
            assert!(set.take(0, latest.into()).is_some());
        }
    }

    mod close {
        use super::*;

        #[test]
        fn drops_only_the_closed_reader_from_the_readers_to_wake() {
            let frames = Frames::new();
            let mut set = carried(2);
            let first = set.open_latest(0, now());
            let second = set.open_latest(1, now());
            assert_eq!(first, second, "each index numbers its own readers");
            set.store(0, frames.frame(Path::Live, 0..1), 0..1);
            set.store(1, frames.frame(Path::Live, 0..1), 0..1);
            set.close(0, first.into(), now());
            assert_eq!(woken(&mut set), [reader(1, second)]);
        }
    }
}

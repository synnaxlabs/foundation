//! The open readers of a shard: their keys, their charge, and what a take gives.

use std::fmt;
use std::mem;
use std::ops::Range;
use std::sync::Arc;

use buffer::Buffer;
pub use delivery::{Error, Next, Position, named};
use delivery::{Reader, Readers, Start};
use types::channel::Slot;
use types::frame::key_set::KeySet;
use types::frame::{Frame, Path};
use types::time::Stamp;

/// An open reader on its shard, in either mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    /// The slot of the reader's index.
    pub(crate) slot: Slot,
    /// The reader's session on the index.
    pub(crate) session: delivery::Key,
}

/// A named reader did not open: the node has no mesh time yet, and a named open can
/// close a named complete session, whose hold starts at mesh time. Nothing changed.
/// Open it again later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unsynced;

impl fmt::Display for Unsynced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the node has no mesh time yet: open the named reader again later"
        )
    }
}

impl std::error::Error for Unsynced {}

/// A reader that opened, and the session of its name that it took over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened<K> {
    /// The new reader.
    pub key: K,
    /// The reader's session that was open before, in either mode. It is closed: a
    /// take gives `Empty`.
    pub replaced: Option<Key>,
}

/// The key of a reader that takes every frame.
pub mod complete {
    pub use delivery::complete::Charge;
    use types::channel::Slot;

    /// An open complete reader on its shard. It converts into a
    /// [`reader::Key`](super::Key).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct Key {
        /// The slot of the reader's index.
        pub(crate) slot: Slot,
        /// The reader's session on the index.
        pub(crate) session: delivery::complete::Key,
    }

    impl From<Key> for super::Key {
        fn from(key: Key) -> Self {
            Self {
                slot: key.slot,
                session: key.session.into(),
            }
        }
    }
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
    /// Each reader to wake: one with a frame to take, or a complete reader that missed
    /// a frame with none waiting. A key can repeat.
    keys: Vec<Key>,
    /// The commits the buffer had ended at the last [`Set::woken`].
    commits: u64,
    /// The number of the first reader key of each index the set carries next: above
    /// each key of an index it shed, so that a key never names two readers.
    first: u64,
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
    /// Adds the readers of the index at `slot` at `place`, with `live` as the seq of
    /// its next live frame.
    ///
    /// # Panics
    ///
    /// If `place` is not the next place.
    pub(crate) fn carry(&mut self, place: usize, slot: Slot, live: u64) {
        assert_eq!(place, self.entries.len(), "{place} is not the next place");
        self.entries.push(Entry {
            slot,
            readers: Readers::after(live, self.first),
            listed: false,
        });
        // Room for every index, so that `applied` never grows the list.
        self.listed.reserve(self.entries.len());
    }

    /// Drops the readers of the index at `place`, and moves the readers of the last
    /// place there, as `Vec::swap_remove` moves an item.
    ///
    /// # Panics
    ///
    /// If a reader of the index is open.
    pub(crate) fn shed(&mut self, place: usize) {
        let last = self.entries.len() - 1;
        let entry = self.entries.swap_remove(place);
        self.first = self.first.max(entry.readers.end());
        self.listed.retain(|&listed| listed != place);
        if let Some(listed) = self.listed.iter_mut().find(|listed| **listed == last) {
            *listed = place;
        }
    }

    /// Opens an unnamed complete reader on the index at `place` at seq `live`, with a
    /// credit of `limit_bytes` that `charge` charges.
    pub(crate) fn open_complete(
        &mut self,
        place: usize,
        live: u64,
        limit_bytes: u64,
        charge: complete::Charge,
    ) -> delivery::complete::Key {
        let start = Start::At(Position {
            live,
            backfill: None,
        });
        let readers = &mut self.entries[place].readers;
        readers
            .open(Reader::Unnamed, start, limit_bytes, charge)
            .key
    }

    /// Opens `reader`, which is named, on the index at `place` at its held position,
    /// else at seq `live`, as [`open_complete`](Self::open_complete) does. Each hold
    /// that ended at or before mesh time `now` ends first.
    ///
    /// # Panics
    ///
    /// If the reader's hold is negative.
    pub(crate) fn open_named_complete(
        &mut self,
        place: usize,
        reader: Reader,
        live: u64,
        limit_bytes: u64,
        charge: complete::Charge,
        now: Stamp,
    ) -> delivery::complete::Opened {
        let start = Start::Resume {
            presented: None,
            otherwise: Position {
                live,
                backfill: None,
            },
        };
        let readers = &mut self.entries[place].readers;
        readers.advance(now);
        let opened = readers.open(reader, start, limit_bytes, charge);
        self.replaced(place, opened.replaced);
        self.drop_records(place);
        opened
    }

    /// Opens an unnamed latest reader on the index at `place`. It is not woken for the
    /// newest frame it can take at once.
    pub(crate) fn open_latest(&mut self, place: usize) -> delivery::latest::Key {
        self.entries[place].readers.open_latest().key
    }

    /// Opens a latest session for the named reader `reader` on the index at `place`,
    /// as [`open_latest`](Self::open_latest) does. A complete session that it takes
    /// over closes at mesh time `now`.
    pub(crate) fn open_named_latest(
        &mut self,
        place: usize,
        reader: named::Key,
        now: Stamp,
    ) -> delivery::latest::Opened {
        let readers = &mut self.entries[place].readers;
        let opened = readers.open_named_latest(reader, now);
        self.replaced(place, opened.replaced);
        self.drop_records(place);
        opened
    }

    /// Forgets the session `replaced` of the index at `place`, which a takeover
    /// closed.
    fn replaced(&mut self, place: usize, replaced: Option<delivery::Key>) {
        if let Some(delivery::Key::Latest(latest)) = replaced {
            self.forget(place, latest);
        }
    }

    /// Drops the position records of the index at `place`, until #274 appends them to
    /// the index log.
    fn drop_records(&mut self, place: usize) {
        self.entries[place].readers.records().for_each(drop);
    }

    /// Records that the complete reader `session` on the index at `place` has each
    /// sample below `position`, as [`Readers::ack`] does.
    pub(crate) fn ack(
        &mut self,
        place: usize,
        session: delivery::complete::Key,
        position: Position,
    ) -> Result<(), Error> {
        self.entries[place].readers.ack(session, position)
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

    /// Takes the next frame of the reader `session` on the index at `place`.
    ///
    /// # Panics
    ///
    /// If the index never gave `session`.
    pub(crate) fn take(&mut self, place: usize, session: delivery::Key) -> Next {
        self.entries[place].readers.take(session)
    }

    /// Closes the reader `session` on the index at `place` at mesh time `now`. Its
    /// waiting frames do not go out, and [`woken`](Self::woken) does not name it. A
    /// close of a closed reader changes nothing.
    ///
    /// # Panics
    ///
    /// If the index never gave `session`, or `now` is `None` and `session` is an open
    /// named complete session.
    pub(crate) fn close(
        &mut self,
        place: usize,
        session: delivery::Key,
        now: Option<Stamp>,
    ) {
        self.entries[place].readers.close(session, now);
        self.drop_records(place);
        if let delivery::Key::Latest(latest) = session {
            self.forget(place, latest);
        }
    }

    /// Drops the closed latest reader `session` of the index at `place` from the
    /// readers to wake. Only a latest reader waits there between calls of
    /// [`woken`](Self::woken).
    fn forget(&mut self, place: usize, session: delivery::latest::Key) {
        let key = Key {
            slot: self.entries[place].slot,
            session: session.into(),
        };
        self.keys.retain(|&woken| woken != key);
    }

    /// Gives `frame`, of key set `set`, stored in the buffer at `seq`, to the readers
    /// of the index at `place`. A live frame is the newest frame at once, and goes to
    /// complete readers after the commit that holds it. A backfill frame goes to no
    /// reader.
    pub(crate) fn applied(
        &mut self,
        place: usize,
        frame: Frame,
        set: &Arc<KeySet>,
        seq: Range<u64>,
    ) {
        if frame.path() == Path::Backfill {
            return;
        }
        let entry = &mut self.entries[place];
        entry.readers.queue(&frame, set, seq);
        wake(&mut self.keys, entry.slot, entry.readers.put(frame));
        if entry.readers.pending() && !mem::replace(&mut entry.listed, true) {
            self.listed.push(place);
        }
    }

    /// Makes `frame`, a live frame that found no room in the buffer, the newest frame
    /// of the index at `place`. Complete readers never get it.
    ///
    /// # Panics
    ///
    /// If `frame` is a backfill frame, which waits for room instead.
    pub(crate) fn lost(&mut self, place: usize, frame: Frame) {
        assert_eq!(
            frame.path(),
            Path::Live,
            "invariant: only a live frame is lost"
        );
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
        keys.append(&mut self.keys);
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
    use delivery::complete;
    use types::frame::key_set::{Group, Interner};
    use types::frame::{self, Draft, Form};

    use super::*;
    use crate::common::{create_pool, key};

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
                pool: create_pool(4096),
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

    /// A set that carries `indexes` indexes, at slots from 0, each with no live frame.
    fn carried(indexes: u32) -> Set {
        let mut set = Set::default();
        for (place, n) in (0..indexes).enumerate() {
            set.carry(place, Slot::new(n), 0);
        }
        set
    }

    /// The readers to wake since the last call, with no commit read.
    fn woken(set: &mut Set) -> Vec<Key> {
        mem::take(&mut set.keys)
    }

    /// The complete readers of the index at `place` that get frames once its live
    /// frames before `durable` are on disk.
    fn released(set: &mut Set, place: usize, durable: u64) -> Vec<complete::Key> {
        set.entries[place].readers.release(durable).to_vec()
    }

    /// The range of each frame that the reader `session` of the index at `place` takes
    /// before [`Next::Empty`].
    ///
    /// # Panics
    ///
    /// If the reader gets [`Next::Behind`].
    #[track_caller]
    fn taken(
        set: &mut Set,
        place: usize,
        session: impl Into<delivery::Key>,
    ) -> Vec<frame::Range> {
        let session = session.into();
        let mut ranges = Vec::new();
        loop {
            match set.take(place, session) {
                Next::Frame(frame) => {
                    ranges.push(frame.range(0).expect("the index is present"));
                }
                Next::Empty => return ranges,
                Next::Behind => panic!("behind after {ranges:?}"),
            }
        }
    }

    fn reader(place: u32, session: impl Into<delivery::Key>) -> Key {
        Key {
            slot: Slot::new(place),
            session: session.into(),
        }
    }

    fn range(seq: u64, count: u32) -> frame::Range {
        frame::Range { seq, count }
    }

    mod applied {
        use super::*;

        #[test]
        fn wakes_the_latest_readers_of_a_live_frame_at_once() {
            let frames = Frames::new();
            let mut set = carried(2);
            let latest = set.open_latest(1);
            assert_eq!(woken(&mut set), []);
            set.applied(1, frames.frame(Path::Live, 0..2), &frames.set, 0..2);
            assert_eq!(woken(&mut set), [reader(1, latest)]);
            assert_eq!(set.listed(), []);
            assert_eq!(taken(&mut set, 1, latest), [range(0, 2)]);
        }

        #[test]
        fn queues_live_frames_for_complete_readers_and_lists_the_index_once() {
            let frames = Frames::new();
            let mut set = carried(2);
            let complete = set.open_complete(1, 0, u64::MAX, complete::Charge::Whole);
            set.applied(1, frames.frame(Path::Live, 0..1), &frames.set, 0..1);
            set.applied(1, frames.frame(Path::Live, 1..3), &frames.set, 1..3);
            assert_eq!(set.listed(), [1]);
            assert_eq!(woken(&mut set), []);
            assert_eq!(released(&mut set, 1, 3), [complete]);
            assert_eq!(taken(&mut set, 1, complete), [range(0, 1), range(1, 2)]);
        }

        #[test]
        fn gives_a_backfill_frame_to_no_reader() {
            let frames = Frames::new();
            let mut set = carried(1);
            let latest = set.open_latest(0);
            let _ = set.open_complete(0, 0, u64::MAX, complete::Charge::Whole);
            set.applied(0, frames.frame(Path::Backfill, 0..2), &frames.set, 0..2);
            assert_eq!(woken(&mut set), []);
            assert_eq!(set.listed(), []);
            assert_eq!(released(&mut set, 0, 2), []);
            assert_eq!(taken(&mut set, 0, latest), []);
            set.applied(0, frames.frame(Path::Live, 0..1), &frames.set, 0..1);
            assert_eq!(woken(&mut set), [reader(0, latest)]);
        }
    }

    mod lost {
        use super::*;

        #[test]
        fn gives_a_live_frame_to_the_latest_readers_only() {
            let frames = Frames::new();
            let mut set = carried(1);
            let latest = set.open_latest(0);
            let complete = set.open_complete(0, 0, u64::MAX, complete::Charge::Whole);
            set.lost(0, frames.frame(Path::Live, 0..2));
            assert_eq!(woken(&mut set), [reader(0, latest)]);
            assert_eq!(set.listed(), []);
            assert_eq!(taken(&mut set, 0, latest), [range(0, 2)]);
            set.applied(0, frames.frame(Path::Live, 2..3), &frames.set, 2..3);
            assert_eq!(released(&mut set, 0, 3), [complete]);
            assert_eq!(taken(&mut set, 0, complete), [range(2, 1)]);
        }

        #[test]
        #[should_panic(expected = "invariant: only a live frame is lost")]
        fn panics_on_a_backfill_frame() {
            let frames = Frames::new();
            let mut set = carried(1);
            set.lost(0, frames.frame(Path::Backfill, 0..2));
        }
    }

    mod carry {
        use super::*;

        #[test]
        #[should_panic(expected = "2 is not the next place")]
        fn panics_when_the_place_is_not_the_next() {
            let mut set = carried(1);
            set.carry(2, Slot::new(1), 0);
        }
    }

    mod open_latest {
        use super::*;

        #[test]
        fn does_not_wake_for_the_newest_frame_it_can_take_at_once() {
            let frames = Frames::new();
            let mut set = carried(1);
            set.applied(0, frames.frame(Path::Live, 0..1), &frames.set, 0..1);
            let _woken = woken(&mut set);
            let latest = set.open_latest(0);
            assert_eq!(woken(&mut set), []);
            assert_eq!(taken(&mut set, 0, latest), [range(0, 1)]);
        }
    }

    mod close {
        use super::*;

        #[test]
        fn drops_only_the_closed_reader_from_the_readers_to_wake() {
            let frames = Frames::new();
            let mut set = carried(2);
            let first = set.open_latest(0);
            let second = set.open_latest(1);
            assert_eq!(first, second, "each index numbers its own readers");
            set.applied(0, frames.frame(Path::Live, 0..1), &frames.set, 0..1);
            set.applied(1, frames.frame(Path::Live, 0..1), &frames.set, 0..1);
            set.close(0, first.into(), None);
            assert_eq!(woken(&mut set), [reader(1, second)]);
        }
    }

    mod named {
        use types::time::Span;

        use super::*;

        fn key() -> delivery::named::Key {
            delivery::named::Key {
                subject: "s".parse().expect("a valid name"),
                name: "r".parse().expect("a valid name"),
            }
        }

        fn open(set: &mut Set) -> complete::Key {
            let reader = Reader::Named {
                reader: key(),
                hold: Span::from_nanos(10),
            };
            let now = Stamp::from_nanos(0);
            set.open_named_complete(
                0,
                reader,
                0,
                u64::MAX,
                complete::Charge::Whole,
                now,
            )
            .key
        }

        /// The position records that wait on the index at slot 0. No call shows them,
        /// and each would stay for the life of the shard.
        fn records(set: &mut Set) -> usize {
            set.entries[0].readers.records().count()
        }

        #[test]
        fn drops_the_position_records_at_each_open_takeover_and_close() {
            let mut set = carried(1);
            let _first = open(&mut set);
            assert_eq!(records(&mut set), 0);
            let second = open(&mut set);
            assert_eq!(records(&mut set), 0);
            set.close(0, second.into(), Some(Stamp::from_nanos(1)));
            assert_eq!(records(&mut set), 0);
            let _third = open(&mut set);
            let _latest = set.open_named_latest(0, key(), Stamp::from_nanos(2));
            assert_eq!(records(&mut set), 0);
        }

        #[test]
        fn says_why_a_named_reader_did_not_open() {
            assert_eq!(
                Unsynced.to_string(),
                "the node has no mesh time yet: open the named reader again later"
            );
        }
    }
}

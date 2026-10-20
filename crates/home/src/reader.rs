//! The open readers of a shard: their keys, their charge, what a take gives, and the
//! named records of each index in the buffer.

use std::fmt;
use std::mem;
use std::ops::Range;
use std::sync::Arc;

use buffer::{Buffer, Stored};
pub use delivery::{Error, Position, named};
use delivery::{Reader, Readers, Record, Start};
use types::channel::Slot;
use types::frame::key_set::KeySet;
use types::frame::{Frame, Path};
use types::hash;
use types::time::{Span, Stamp};

use crate::named::{Full, Parts};
use crate::state;

pub mod disk;

/// Pool bytes that the entries of one read from disk take at most.
const BUDGET: usize = 256 * 1024;

/// What a reader gets from [`Shard::take`](crate::Shard::take).
#[derive(Debug)]
pub enum Next {
    /// The reader's next frame.
    Frame(Frame),
    /// No frame waits, the next frame waits for a grant or a commit, or the reader is
    /// closed.
    Empty,
    /// The reader's next frames are on disk. Run [`disk::Read::read`] with no borrow
    /// of the shard, give the result to [`Shard::fill`](crate::Shard::fill), then
    /// take again.
    Disk(disk::Read),
}

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

/// Why a named complete reader did not open. The open opened nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unopened {
    /// As [`Unsynced`].
    Unsynced,
    /// The open adds a record, and no part of the named records of the index has room
    /// for it: at least 448 records of the longest subjects and names. Close a named
    /// reader of the index whose hold is zero, wait for a hold to end, or open the
    /// reader unnamed. A reopen or a takeover never gives it. The room of a closed
    /// reader's record is free when the entry that drops it appends, at the close or
    /// a later tick.
    Full,
    /// The open adds a record, opens a held one, or takes over an open session with a
    /// new hold, and the buffer had no block or no room for the entry of the part that
    /// holds its record or of a part before it, so a restart would not resume the
    /// reader. Open it again later.
    Unrecorded,
}

impl fmt::Display for Unopened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsynced => Unsynced.fmt(f),
            Self::Full => f.write_str(
                "no part of the named records of the index has room for the reader",
            ),
            Self::Unrecorded => write!(
                f,
                "the home had no room to record the position of the named reader: \
                 open it again later"
            ),
        }
    }
}

impl std::error::Error for Unopened {}

/// A reader that opened, and the session of its name that it took over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened<K> {
    /// The new reader.
    pub key: K,
    /// The first live seq of the index that the session can get: its open position.
    /// A latest session opens at the first seq of the newest frame of the index, or at
    /// the live tail when there is none.
    pub live: u64,
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

/// The readers of each index of a shard, by the index's place in the shard, the
/// readers to wake, and the named records of each index in the buffer. Each call is on
/// the shard's thread.
#[derive(Debug, Default)]
pub(crate) struct Set {
    /// The readers of each carried index, by place.
    entries: Vec<Entry>,
    /// The place of each index whose readers may be
    /// [pending](delivery::Readers::pending), each once.
    listed: Vec<usize>,
    /// The place of each index with named state ([`Entry::named`]), each once: the
    /// indexes that a tick visits.
    ticked: Vec<usize>,
    /// Each index whose records from the open of the shard wait to restore: until its
    /// carry with mesh time, its first [`advance`](Set::advance) after the carry, or
    /// its shed.
    waiting: hash::Set<Slot>,
    /// The named records in the buffer of each index that the set does not carry and
    /// that holds one: from the open of the shard, or from a shed whose entries found
    /// no block or no room, or that had no mesh time. Its carry places each record in
    /// the part that holds it.
    parts: hash::Map<Slot, Parts>,
    /// Each reader to wake: one with a frame to take, or a complete reader that missed
    /// a frame with none waiting. A key can repeat.
    keys: Vec<Key>,
    /// The commits the buffer had ended at the last [`Set::woken`].
    commits: u64,
    /// The number of the first reader key of each index that the set shed and does
    /// not carry now: above each key that the index had, so that a key never names two
    /// readers. An index with no number starts at 0.
    first: hash::Map<Slot, u64>,
}

/// The readers of one carried index.
#[derive(Debug)]
struct Entry {
    slot: Slot,
    readers: Readers,
    /// Whether `Set::listed` holds the index.
    listed: bool,
    /// Whether `Set::ticked` holds the index.
    ticked: bool,
    /// The named records of the index in the buffer.
    parts: Parts,
    /// Whether `parts` hold records that the readers do not restore, from a shed, until
    /// a record appends the parts.
    stale: bool,
}

impl Entry {
    /// Whether the index has named state: its records changed, or are in the buffer,
    /// where the end of a hold or the restore at mesh time changes them.
    fn named(&self) -> bool {
        self.parts.held() || self.readers.changed()
    }
}

impl Set {
    /// A set that carries no index, with the parts of each index in the buffer, by
    /// slot. The first carry of each index restores the records of its parts.
    pub(crate) fn new(newest: hash::Map<Slot, Parts>) -> Self {
        let parts: hash::Map<_, _> = newest
            .into_iter()
            .filter(|(_, parts)| parts.held())
            .collect();
        Self {
            waiting: parts.keys().copied().collect(),
            parts,
            ..Self::default()
        }
    }

    /// Adds the readers of the index at `slot` at `place`, with `live` as the seq of
    /// its next live frame, and the holds of the records that [`new`](Self::new) got
    /// for it, at mesh time `now`. With no mesh time, the records wait for the first
    /// [`advance`](Self::advance).
    ///
    /// # Panics
    ///
    /// If `place` is not the next place.
    pub(crate) fn carry(
        &mut self,
        place: usize,
        slot: Slot,
        live: u64,
        now: Option<Stamp>,
    ) {
        assert_eq!(place, self.entries.len(), "{place} is not the next place");
        let parts = self.parts.remove(&slot).unwrap_or_default();
        self.entries.push(Entry {
            slot,
            readers: Readers::after(live, self.first.remove(&slot).unwrap_or(0)),
            listed: false,
            ticked: false,
            stale: parts.held() && !self.waiting.contains(&slot),
            parts,
        });
        if let Some(now) = now {
            self.restore(place, now);
        }
        self.note(place);
        // Room for every index, so that `applied` never grows the list.
        self.listed.reserve(self.entries.len());
    }

    /// Drops the readers of the index at `place`, and moves the readers of the last
    /// place there, as `Vec::swap_remove` moves an item. With `target`, where an entry
    /// at mesh time goes, it first appends, in part order, an entry with no record for
    /// each part that holds one. The set keeps the parts that still hold a record for
    /// the next carry, which drops them at its first record at mesh time.
    ///
    /// # Panics
    ///
    /// If a reader of the index is open.
    pub(crate) fn shed(&mut self, place: usize, target: Option<state::Target<'_>>) {
        let last = self.entries.len() - 1;
        let mut entry = self.entries.swap_remove(place);
        if let Some(target) = target {
            entry.parts.append(target, &[]);
        }
        if entry.parts.held() {
            self.parts.insert(entry.slot, entry.parts);
        }
        self.first.insert(entry.slot, entry.readers.end());
        self.waiting.remove(&entry.slot);
        for places in [&mut self.listed, &mut self.ticked] {
            places.retain(|&listed| listed != place);
            if let Some(listed) = places.iter_mut().find(|listed| **listed == last) {
                *listed = place;
            }
        }
    }

    /// Adds the holds of the records of the parts of the index at `place` when they
    /// wait for mesh time, as at mesh time `now`.
    fn restore(&mut self, place: usize, now: Stamp) {
        let entry = &mut self.entries[place];
        if self.waiting.remove(&entry.slot) {
            entry.readers.restore(entry.parts.records(), now);
        }
    }

    /// Adds the index at `place` to [`ticked`](Self::ticked) when it has named state.
    fn note(&mut self, place: usize) {
        let entry = &mut self.entries[place];
        if entry.named() && !mem::replace(&mut entry.ticked, true) {
            self.ticked.push(place);
        }
    }

    /// The place of each index with named state: its records changed, wait for mesh
    /// time, or are in the buffer, where the end of a hold changes them. Each index
    /// that a tick visits.
    pub(crate) fn ticked(&self) -> &[usize] {
        &self.ticked
    }

    /// Drops each index with no named state from [`ticked`](Self::ticked).
    pub(crate) fn prune(&mut self) {
        let entries = &mut self.entries;
        self.ticked.retain(|&place| {
            let entry = &mut entries[place];
            entry.ticked = entry.named();
            entry.ticked
        });
    }

    /// Whether the records of the index at `place` changed or wait for mesh time, its
    /// parts hold records of a shed, or a named complete reader of it is open, whose
    /// acks move its record: the index needs a tick soon. Takes time linear in the
    /// complete readers of the index.
    pub(crate) fn busy(&self, place: usize) -> bool {
        let entry = &self.entries[place];
        entry.readers.changed()
            || entry.stale
            || self.waiting.contains(&entry.slot)
            || entry.readers.moving()
    }

    /// The mesh time at which the next hold of the index at `place` ends, or `None`
    /// when no named reader of it holds. Takes time linear in its held readers.
    pub(crate) fn deadline(&self, place: usize) -> Option<Stamp> {
        self.entries[place].readers.deadline()
    }

    /// Appends the records of the named readers of the index at `place` in `target`,
    /// as [`Parts::append`] does, when they changed since the last append that
    /// appended each part, or the parts hold records of a shed. Call it after each
    /// change of the named readers of the index at mesh time.
    pub(crate) fn record(&mut self, place: usize, target: state::Target<'_>) {
        let entry = &mut self.entries[place];
        if !entry.readers.changed() && !entry.stale {
            return;
        }
        let records: Vec<_> = entry.readers.records().collect();
        if entry.parts.append(target, &records) {
            entry.readers.recorded();
            entry.stale = false;
        }
    }

    /// The record of each named reader of the index at `place`.
    fn records(&self, place: usize) -> Vec<Record> {
        self.entries[place].readers.records().collect()
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
            .open(Reader::Unnamed, start, limit_bytes, charge, None)
            .key
    }

    /// Restores the records that wait on the index at `place`, and ends each hold that
    /// ended at or before mesh time `now`. Call it before each named open.
    pub(crate) fn advance(&mut self, place: usize, now: Stamp) {
        self.restore(place, now);
        self.entries[place].readers.advance(now);
        self.note(place);
    }

    /// Opens a complete session for the named reader `reader` with `hold` on the index
    /// at `place`, at its held position, else at the live tail, the seq `first` of the
    /// head of `target`, with a credit of `limit_bytes` that `charge` charges. An open
    /// that adds a record, opens a held one, or takes over an open session with a new
    /// hold first appends its parts in `target`. Its reader takes no frame until each
    /// entry appended before the open is durable: a restart before the commit of its
    /// record would lose the frames that it got and did not ack.
    ///
    /// # Errors
    ///
    /// [`Unopened::Full`] when no part has room for its new record, and
    /// [`Unopened::Unrecorded`] when the part of its record, or a part before it, found
    /// no block or no room. It then opens nothing.
    ///
    /// # Panics
    ///
    /// If the reader's hold is negative.
    pub(crate) fn open_named_complete(
        &mut self,
        place: usize,
        reader: named::Key,
        hold: Span,
        limit_bytes: u64,
        charge: complete::Charge,
        target: state::Target<'_>,
    ) -> Result<delivery::complete::Opened, Unopened> {
        let live = target.head.first;
        if let Some(records) = self.opening(place, &reader, hold, live) {
            let parts = &mut self.entries[place].parts;
            let recorded = parts.open(target, &records, &reader);
            if !recorded.map_err(|Full| Unopened::Full)? {
                return Err(Unopened::Unrecorded);
            }
        }
        let until = target.buffer.durable_at();
        let reader = Reader::Named { reader, hold };
        Ok(self.resume(place, reader, live, limit_bytes, charge, until))
    }

    /// The records of the index at `place` after a named complete open of `reader`
    /// with `hold`, at its held position, else at seq `live`: a first open, a reopen
    /// in its hold, or a takeover with a new hold. `None` when its session is open with
    /// its hold, and the records stay.
    fn opening(
        &self,
        place: usize,
        reader: &named::Key,
        hold: Span,
        live: u64,
    ) -> Option<Vec<Record>> {
        let mut records = self.records(place);
        let open = |live| Record {
            reader: reader.clone(),
            position: Position {
                live,
                backfill: None,
            },
            hold,
            closed: None,
        };
        match records.iter_mut().find(|record| record.reader == *reader) {
            Some(record) if record.closed.is_none() && record.hold == hold => {
                return None;
            }
            Some(record) => *record = open(record.position.live),
            None => records.push(open(live)),
        }
        Some(records)
    }

    /// Opens `reader`, which is named, on the index at `place` at its held position,
    /// else at seq `live`, as [`open_complete`](Self::open_complete) does. With
    /// `until`, a count of [`Buffer::commits`] that has not ended, it takes no frame
    /// until then.
    ///
    /// # Panics
    ///
    /// If the reader's hold is negative.
    fn resume(
        &mut self,
        place: usize,
        reader: Reader,
        live: u64,
        limit_bytes: u64,
        charge: complete::Charge,
        until: Option<u64>,
    ) -> delivery::complete::Opened {
        let start = Start::Resume {
            presented: None,
            otherwise: Position {
                live,
                backfill: None,
            },
        };
        let readers = &mut self.entries[place].readers;
        let opened = readers.open(reader, start, limit_bytes, charge, until);
        self.replaced(place, opened.replaced);
        self.note(place);
        self.list(place);
        opened
    }

    /// Opens an unnamed latest reader on the index at `place`. It is not woken for the
    /// newest frame it can take at once.
    pub(crate) fn open_latest(&mut self, place: usize) -> delivery::latest::Key {
        self.entries[place].readers.open_latest()
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
        opened
    }

    /// The first seq of the newest live frame of the index at `place`, which a latest
    /// session gets at its open, or `None` before the first.
    pub(crate) fn newest(&self, place: usize) -> Option<u64> {
        self.entries[place].readers.newest().map(|frame| {
            let (_, range) = frame
                .ranges()
                .next()
                .expect("invariant: an index frame holds its one group");
            range.seq
        })
    }

    /// Forgets the session `replaced` of the index at `place`, which a takeover
    /// closed.
    fn replaced(&mut self, place: usize, replaced: Option<delivery::Key>) {
        if let Some(session) = replaced {
            self.forget(place, session);
        }
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

    /// Takes the next frame of the reader `session` on the index at `place`. A read
    /// from disk reads `buffer`.
    ///
    /// # Panics
    ///
    /// If the index never gave `session`.
    pub(crate) fn take(
        &mut self,
        place: usize,
        session: delivery::Key,
        buffer: &Buffer,
    ) -> Next {
        let entry = &mut self.entries[place];
        let from = match entry.readers.take(session) {
            delivery::Next::Frame(frame) => return Next::Frame(frame),
            delivery::Next::Empty => return Next::Empty,
            delivery::Next::Disk { from } => from,
        };
        let delivery::Key::Complete(session) = session else {
            unreachable!("invariant: a latest session never reads from disk");
        };
        let key = complete::Key {
            slot: entry.slot,
            session,
        };
        Next::Disk(disk::Read {
            key,
            from,
            budget: BUDGET,
            reader: buffer.reader(),
        })
    }

    /// Gives the frames of `entries` to its session on the index at `place`, whose
    /// live frames on disk end at seq `durable`: the frame of key set `set` that
    /// `decode` makes of each entry, or of none. Drops `entries` when its session
    /// closed after the read started.
    ///
    /// # Errors
    ///
    /// The error of `decode` for the first frame of `entries`, which then gives
    /// nothing. An error after a frame stops the fill there.
    pub(crate) fn fill(
        &mut self,
        place: usize,
        entries: disk::Entries,
        durable: u64,
        set: &Arc<KeySet>,
        mut decode: impl FnMut(&Stored) -> Result<Option<Frame>, block::Error>,
    ) -> Result<(), block::Error> {
        let disk::Entries { key, read } = entries;
        let entry = &mut self.entries[place];
        if !entry.readers.on_disk(key.session) {
            return Ok(());
        }
        let mut given = false;
        for stored in &read.entries {
            let frame = match decode(stored) {
                Ok(Some(frame)) => frame,
                Ok(None) => continue,
                Err(error) if !given => return Err(error),
                Err(_) => return Ok(()),
            };
            given = true;
            let seq = stored.seqs();
            if !entry.readers.fill(key.session, frame, set, seq) {
                return Ok(());
            }
        }
        entry.readers.reached(key.session, read.next, durable);
        Ok(())
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
        self.forget(place, session);
    }

    /// Drops the closed reader `session` of the index at `place` from the readers to
    /// wake, where only a latest reader waits between calls of
    /// [`woken`](Self::woken).
    fn forget(&mut self, place: usize, session: delivery::Key) {
        if let delivery::Key::Latest(_) = session {
            let key = Key {
                slot: self.entries[place].slot,
                session,
            };
            self.keys.retain(|&woken| woken != key);
        }
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
        self.list(place);
    }

    /// Lists the index at `place` when its readers are pending and it is not listed.
    fn list(&mut self, place: usize) {
        let entry = &mut self.entries[place];
        if entry.readers.pending() && !mem::replace(&mut entry.listed, true) {
            self.listed.push(place);
        }
    }

    /// Makes `frame`, a live frame that found no room in the buffer, the newest frame
    /// of the index at `place`. When the pool had no block for it (`None`), the newest
    /// frame stays. Only latest readers get it.
    ///
    /// # Panics
    ///
    /// If `frame` is a backfill frame, which waits for room instead.
    pub(crate) fn lost(&mut self, place: usize, frame: Option<Frame>) {
        let entry = &mut self.entries[place];
        if let Some(frame) = frame {
            assert_eq!(
                frame.path(),
                Path::Live,
                "invariant: only a live frame is lost"
            );
            wake(&mut self.keys, entry.slot, entry.readers.put(frame));
        }
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
                let released = entry.readers.release(durable, commits);
                wake(woken, entry.slot, released);
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

    /// Whether the set keeps named parts for the index at `slot`, which it does not
    /// carry.
    #[cfg(test)]
    pub(crate) fn keeps(&self, slot: Slot) -> bool {
        self.parts.contains_key(&slot)
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
    use std::num::NonZeroU32;

    use delivery::complete;
    use types::frame::key_set::{Group, Interner};
    use types::frame::{self, Draft, Form};
    use types::log::Mark;

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
            set.carry(place, Slot::new(n), 0, None);
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
        set.entries[place].readers.release(durable, 0).to_vec()
    }

    /// The range of each frame that the reader `session` of the index at `place` takes
    /// before [`delivery::Next::Empty`].
    ///
    /// # Panics
    ///
    /// If the reader gets another [`delivery::Next`].
    #[track_caller]
    fn taken(
        set: &mut Set,
        place: usize,
        session: impl Into<delivery::Key>,
    ) -> Vec<frame::Range> {
        let session = session.into();
        let mut ranges = Vec::new();
        loop {
            match set.entries[place].readers.take(session) {
                delivery::Next::Frame(frame) => {
                    ranges.push(frame.range(0).expect("the index is present"));
                }
                delivery::Next::Empty => return ranges,
                disk @ delivery::Next::Disk { .. } => {
                    panic!("{disk:?} after {ranges:?}")
                }
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
            assert_eq!(set.listed(), [0_usize; 0]);
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
            let _: complete::Key =
                set.open_complete(0, 0, u64::MAX, complete::Charge::Whole);
            set.applied(0, frames.frame(Path::Backfill, 0..2), &frames.set, 0..2);
            assert_eq!(woken(&mut set), []);
            assert_eq!(set.listed(), [0_usize; 0]);
            assert_eq!(released(&mut set, 0, 2), []);
            assert_eq!(taken(&mut set, 0, latest), []);
            set.applied(0, frames.frame(Path::Live, 0..1), &frames.set, 0..1);
            assert_eq!(woken(&mut set), [reader(0, latest)]);
        }
    }

    mod lost {
        use super::*;

        #[test]
        fn gives_a_live_frame_only_to_the_latest_readers() {
            let frames = Frames::new();
            let mut set = carried(1);
            let latest = set.open_latest(0);
            let complete = set.open_complete(0, 0, u64::MAX, complete::Charge::Whole);
            set.lost(0, Some(frames.frame(Path::Live, 0..2)));
            assert_eq!(woken(&mut set), [reader(0, latest)]);
            assert_eq!(set.listed(), [0_usize; 0]);
            assert_eq!(taken(&mut set, 0, latest), [range(0, 2)]);
            assert_eq!(taken(&mut set, 0, complete), []);
            set.applied(0, frames.frame(Path::Live, 2..3), &frames.set, 2..3);
            assert_eq!(released(&mut set, 0, 3), [complete]);
            assert_eq!(taken(&mut set, 0, complete), [range(2, 1)]);
        }

        #[test]
        fn gives_a_complete_reader_the_frames_on_each_side_of_the_lost() {
            let frames = Frames::new();
            let mut set = carried(1);
            let complete = set.open_complete(0, 0, u64::MAX, complete::Charge::Whole);
            set.applied(0, frames.frame(Path::Live, 0..1), &frames.set, 0..1);
            set.lost(0, Some(frames.frame(Path::Live, 1..3)));
            set.applied(0, frames.frame(Path::Live, 3..4), &frames.set, 3..4);
            assert_eq!(woken(&mut set), []);
            assert_eq!(released(&mut set, 0, 4), [complete]);
            assert_eq!(taken(&mut set, 0, complete), [range(0, 1), range(3, 1)]);
        }

        #[test]
        #[should_panic(expected = "invariant: only a live frame is lost")]
        fn panics_on_a_backfill_frame() {
            let frames = Frames::new();
            let mut set = carried(1);
            set.lost(0, Some(frames.frame(Path::Backfill, 0..2)));
        }
    }

    mod carry {
        use super::*;

        #[test]
        #[should_panic(expected = "2 is not the next place")]
        fn panics_when_the_place_is_not_the_next() {
            let mut set = carried(1);
            set.carry(2, Slot::new(1), 0, None);
        }

        /// The set keeps the number of a shed index only until it carries it again.
        /// No caller sees a kept number: a slot is carried again only after a `shed`,
        /// which writes over it.
        #[test]
        fn drops_the_number_of_a_shed_index_that_it_carries_again() {
            let mut set = carried(2);
            set.shed(0, None);
            assert_eq!(set.first.len(), 1);
            set.carry(1, Slot::new(0), 0, None);
            assert!(set.first.is_empty());
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

    mod disk {
        use super::*;
        use crate::reader::disk::{self as read, Entries};

        /// A set whose complete reader at slot 0 reads from disk from seq 0, after the
        /// live frames `0..durable` were released with no reader open, and the read
        /// of its next entries, `0..durable`, one sample each.
        fn reading(durable: u64) -> (Set, Frames, Entries) {
            let (mut set, frames) = (carried(1), Frames::new());
            for seq in 0..durable {
                set.applied(
                    0,
                    frames.frame(Path::Live, seq..seq + 1),
                    &frames.set,
                    seq..seq + 1,
                );
            }
            released(&mut set, 0, durable);
            let session = set.open_complete(0, 0, u64::MAX, complete::Charge::Whole);
            let key = crate::reader::complete::Key {
                slot: Slot::new(0),
                session,
            };
            let delivery::Next::Disk { .. } =
                set.entries[0].readers.take(session.into())
            else {
                panic!("the reader reads from disk");
            };
            let bytes = frames.pool.alloc(8).expect("room").freeze();
            let entries = (0..durable)
                .map(|first| Stored {
                    first,
                    samples: Some(buffer::Samples {
                        len: NonZeroU32::MIN,
                        last: None,
                    }),
                    stored_at: Stamp::from_nanos(0),
                    tag: 0,
                    bytes: bytes.clone(),
                })
                .collect();
            let read = buffer::Read {
                gap: None,
                entries,
                next: Mark::at(durable),
            };
            (set, frames, Entries { key, read })
        }

        /// Fills `entries` into `set`, with a decode that fails at the entry at
        /// `failed`.
        fn fill(
            set: &mut Set,
            frames: &Frames,
            entries: Entries,
            durable: u64,
            failed: Option<u64>,
        ) -> Result<(), block::Error> {
            set.fill(0, entries, durable, &frames.set, |stored| {
                if Some(stored.first) == failed {
                    return Err(block::Error::Refused { requested: 8 });
                }
                let seq = stored.first..stored.first + 1;
                Ok(Some(frames.frame(Path::Live, seq)))
            })
        }

        #[test]
        fn gives_each_frame_and_goes_live_at_the_durable_end() {
            let (mut set, frames, read) = reading(3);
            let session = read.key.session;
            assert_eq!(fill(&mut set, &frames, read, 3, None), Ok(()));
            assert_eq!(
                taken(&mut set, 0, session),
                [range(0, 1), range(1, 1), range(2, 1)]
            );
            assert_eq!(set.listed(), [0_usize; 0], "no live frame waits");
        }

        #[test]
        fn gives_the_error_of_the_first_frame_and_nothing() {
            let (mut set, frames, read) = reading(3);
            let session = read.key.session;
            assert_eq!(
                fill(&mut set, &frames, read, 3, Some(0)),
                Err(block::Error::Refused { requested: 8 })
            );
            assert!(
                matches!(
                    set.entries[0].readers.take(session.into()),
                    delivery::Next::Disk {
                        from: Mark { seq: 0, given: 0 }
                    }
                ),
                "the reader reads again"
            );
        }

        #[test]
        fn stops_at_an_error_after_a_frame() {
            let (mut set, frames, read) = reading(3);
            let session = read.key.session;
            assert_eq!(fill(&mut set, &frames, read, 3, Some(1)), Ok(()));
            let readers = &mut set.entries[0].readers;
            let first = readers.take(session.into());
            assert!(matches!(first, delivery::Next::Frame(_)), "{first:?}");
            let next = readers.take(session.into());
            assert!(
                matches!(
                    next,
                    delivery::Next::Disk {
                        from: Mark { seq: 1, given: 0 }
                    }
                ),
                "{next:?}"
            );
        }

        #[test]
        fn drops_the_read_of_a_closed_reader_with_no_decode() {
            let (mut set, frames, read) = reading(3);
            set.close(0, read.key.session.into(), None);
            assert_eq!(fill(&mut set, &frames, read, 3, Some(0)), Ok(()));
        }

        #[test]
        fn says_why_a_read_failed() {
            let files = env::files::Error::Full {
                path: "ring".into(),
            };
            assert_eq!(
                read::Error::Files(files).to_string(),
                "a read from disk failed: no room on the disk for ring"
            );
            assert_eq!(
                read::Error::Pool(block::Error::Refused { requested: 8 }).to_string(),
                "the pool has no block for a read: the system refused memory for a \
                 block of 8 bytes"
            );
        }
    }

    /// These tests call the private `resume`, `opening`, and `records`, and read
    /// `Readers::changed` of an entry: the crate calls of `Set` append to a buffer,
    /// and the `shard` tests of `records` check what the buffer holds. These pin the
    /// records in memory, which a buffer shows only after an append.
    mod named {
        use super::*;

        fn named(name: &str) -> delivery::named::Key {
            delivery::named::Key {
                subject: "s".parse().expect("a valid name"),
                name: name.parse().expect("a valid name"),
            }
        }

        fn open(set: &mut Set, name: &str) -> complete::Key {
            let reader = Reader::Named {
                reader: named(name),
                hold: Span::from_nanos(10),
            };
            let charge = complete::Charge::Whole;
            set.resume(0, reader, 0, u64::MAX, charge, None).key
        }

        /// Whether the records of the index at `place` changed since they were last
        /// recorded.
        fn changed(set: &Set, place: usize) -> bool {
            set.entries[place].readers.changed()
        }

        /// Marks the records of the index at `place` as in parts that hold them, as an
        /// append of each part does, with no buffer.
        fn recorded(set: &mut Set, place: usize) {
            let records = set.records(place);
            let entry = &mut set.entries[place];
            entry.parts = crate::named::create_parts(&records);
            entry.readers.recorded();
        }

        fn record(name: &str, closed: Option<i64>) -> Record {
            Record {
                reader: named(name),
                position: Position {
                    live: 3,
                    backfill: None,
                },
                hold: Span::from_nanos(10),
                closed: closed.map(Stamp::from_nanos),
            }
        }

        /// A set that restores `records` at the first carry of the index at each slot,
        /// and carries `indexes` indexes, at slots from 0, with no mesh time.
        fn restoring(records: &[(u32, Vec<Record>)], indexes: u32) -> Set {
            let newest = records.iter().map(|(n, records)| {
                (Slot::new(*n), crate::named::create_parts(records))
            });
            let mut set = Set::new(newest.collect());
            for (place, n) in (0..indexes).enumerate() {
                set.carry(place, Slot::new(n), 0, None);
            }
            set
        }

        fn opened(name: &str, live: u64) -> Record {
            Record {
                position: Position {
                    live,
                    backfill: None,
                },
                ..record(name, None)
            }
        }

        #[test]
        fn change_until_recorded() {
            let mut set = carried(1);
            assert!(!changed(&set, 0));
            assert!(set.ticked().is_empty());
            let a = open(&mut set, "a");
            assert!(changed(&set, 0));
            assert_eq!(set.records(0).len(), 1);
            assert_eq!(set.ticked(), [0]);
            set.prune();
            assert_eq!(set.ticked(), [0], "the records changed");
            recorded(&mut set, 0);
            assert!(!changed(&set, 0));
            set.prune();
            assert_eq!(set.ticked(), [0], "the buffer holds a record");
            set.close(0, a.into(), Some(Stamp::from_nanos(1)));
            assert!(changed(&set, 0), "the close changed them");
            assert_eq!(set.records(0)[0].closed, Some(Stamp::from_nanos(1)));
        }

        #[test]
        fn leave_the_tick_once_an_empty_entry_is_recorded() {
            let mut set = carried(1);
            let a = open(&mut set, "a");
            recorded(&mut set, 0);
            set.close(0, a.into(), Some(Stamp::from_nanos(1)));
            recorded(&mut set, 0);
            set.advance(0, Stamp::from_nanos(11));
            assert!(changed(&set, 0));
            assert_eq!(set.records(0), []);
            recorded(&mut set, 0);
            set.prune();
            assert!(set.ticked().is_empty());
        }

        #[test]
        fn list_only_the_indexes_with_named_state() {
            let mut set = restoring(&[(3, vec![record("a", None)])], 3);
            let _b = open(&mut set, "b");
            recorded(&mut set, 0);
            let latest = set.open_latest(1);
            set.close(1, latest.into(), None);
            let _c = open(&mut set, "c");
            let mut ticked = set.ticked().to_vec();
            ticked.sort_unstable();
            assert_eq!(ticked, [0]);
            set.carry(3, Slot::new(3), 0, None);
            assert_eq!(set.ticked(), [0, 3]);
        }

        #[test]
        fn keep_the_place_of_each_listed_index_at_a_shed() {
            let records = [
                (3, vec![record("a", Some(1))]),
                (4, vec![record("b", Some(1))]),
            ];
            let set = &mut restoring(&records, 5);
            assert_eq!(set.ticked(), [3, 4]);
            set.shed(3, None);
            assert_eq!(set.ticked(), [3], "the index at 4 moved to 3");
            set.advance(3, Stamp::from_nanos(2));
            assert_eq!(set.records(3), [record("b", Some(1))]);
        }

        #[test]
        fn are_busy_while_records_change_wait_or_move_with_acks() {
            let mut set = restoring(&[(1, vec![record("b", Some(1))])], 1);
            assert!(!set.busy(0));
            let a = open(&mut set, "a");
            assert!(set.busy(0), "the records changed");
            recorded(&mut set, 0);
            assert!(set.busy(0), "an ack can move the record");
            set.close(0, a.into(), Some(Stamp::from_nanos(1)));
            recorded(&mut set, 0);
            assert!(!set.busy(0), "only a hold");
            set.carry(1, Slot::new(1), 0, None);
            assert!(set.busy(1), "the records wait");
            assert!(!set.busy(0), "each index is busy alone");
        }

        #[test]
        fn end_at_the_earliest_hold_of_the_index() {
            let mut set = carried(2);
            assert_eq!(set.deadline(0), None);
            let a = open(&mut set, "a");
            let b = open(&mut set, "b");
            set.close(0, a.into(), Some(Stamp::from_nanos(5)));
            set.close(0, b.into(), Some(Stamp::from_nanos(3)));
            assert_eq!(set.deadline(0), Some(Stamp::from_nanos(13)));
            assert_eq!(set.deadline(1), None, "each index ends alone");
            set.advance(0, Stamp::from_nanos(13));
            assert_eq!(set.deadline(0), Some(Stamp::from_nanos(15)));
        }

        #[test]
        fn wait_for_mesh_time_at_a_carry_with_none() {
            let mut set = restoring(&[(0, vec![record("a", None)])], 1);
            assert_eq!(set.ticked(), [0], "the records wait");
            assert!(!changed(&set, 0));
            set.advance(0, Stamp::from_nanos(4));
            assert!(changed(&set, 0), "the open session closed at 4");
            assert_eq!(set.records(0), [record("a", Some(4))]);
            recorded(&mut set, 0);
            set.advance(0, Stamp::from_nanos(5));
            assert!(!changed(&set, 0), "a second advance restores nothing");
        }

        #[test]
        fn restore_at_a_carry_with_mesh_time() {
            let records = vec![record("a", None), record("b", Some(1))];
            let newest = [(Slot::new(0), crate::named::create_parts(&records))];
            let mut set = Set::new(newest.into_iter().collect());
            set.carry(0, Slot::new(0), 0, Some(Stamp::from_nanos(4)));
            assert!(changed(&set, 0), "the open session closed at 4");
            let mut restored = set.records(0);
            restored.sort_by(|a, b| a.reader.name.cmp(&b.reader.name));
            assert_eq!(restored, [record("a", Some(4)), record("b", Some(1))]);
        }

        #[test]
        fn restore_no_empty_entry() {
            let set = restoring(&[(0, Vec::new())], 1);
            assert!(set.ticked().is_empty(), "the buffer holds no record");
        }

        #[test]
        fn add_the_record_of_a_first_open() {
            let mut set = carried(1);
            let _a = open(&mut set, "a");
            let _b = open(&mut set, "b");
            let hold = Span::from_nanos(10);
            let mut added = set.opening(0, &named("c"), hold, 3).expect("changed");
            added.sort_by(|a, b| a.reader.name.cmp(&b.reader.name));
            assert_eq!(added, [opened("a", 0), opened("b", 0), record("c", None)]);
        }

        #[test]
        fn add_nothing_at_a_takeover_of_an_open_session_with_its_hold() {
            let mut set = carried(1);
            let _a = open(&mut set, "a");
            let hold = Span::from_nanos(10);
            assert_eq!(set.opening(0, &named("a"), hold, 3), None);
        }

        #[test]
        fn change_the_hold_at_a_takeover_of_an_open_session() {
            let mut set = carried(1);
            let _a = open(&mut set, "a");
            let hold = Span::from_nanos(20);
            let records = set.opening(0, &named("a"), hold, 3);
            let taken = Record {
                hold,
                ..opened("a", 0)
            };
            assert_eq!(records, Some(vec![taken]));
        }

        #[test]
        fn open_a_held_record_at_its_position_with_the_new_hold() {
            let mut set = carried(1);
            let a = open(&mut set, "a");
            set.ack(
                0,
                a,
                Position {
                    live: 2,
                    backfill: None,
                },
            )
            .expect("forward");
            set.close(0, a.into(), Some(Stamp::from_nanos(1)));
            let hold = Span::from_nanos(20);
            let records = set.opening(0, &named("a"), hold, 3);
            let reopened = Record {
                hold,
                ..opened("a", 2)
            };
            assert_eq!(records, Some(vec![reopened]));
        }

        #[test]
        fn open_a_held_record_with_its_hold() {
            let mut set = carried(1);
            let a = open(&mut set, "a");
            set.close(0, a.into(), Some(Stamp::from_nanos(1)));
            let hold = Span::from_nanos(10);
            let records = set.opening(0, &named("a"), hold, 3);
            assert_eq!(records, Some(vec![opened("a", 0)]));
        }

        #[test]
        fn says_why_a_named_reader_did_not_open() {
            assert_eq!(
                Unsynced.to_string(),
                "the node has no mesh time yet: open the named reader again later"
            );
            assert_eq!(
                Unopened::Unsynced.to_string(),
                "the node has no mesh time yet: open the named reader again later"
            );
            assert_eq!(
                Unopened::Full.to_string(),
                "no part of the named records of the index has room for the reader"
            );
        }
    }
}

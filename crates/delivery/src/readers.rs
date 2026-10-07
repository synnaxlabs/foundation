//! The readers of one index at its home.

pub mod complete;
pub mod latest;

use std::collections::{BTreeMap, VecDeque};
use std::iter;
use std::ops::Range;

use types::frame::Frame;
use types::name::Name;
use types::time::{Span, Stamp};

use crate::{Error, Position, Reader, Record, Start};

/// A session on one index, in either mode. Latest sessions order first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    /// A latest session.
    Latest(latest::Key),
    /// A complete session.
    Complete(complete::Key),
}

impl From<complete::Key> for Key {
    fn from(key: complete::Key) -> Self {
        Self::Complete(key)
    }
}

impl From<latest::Key> for Key {
    fn from(key: latest::Key) -> Self {
        Self::Latest(key)
    }
}

/// The readers of one index at its home, in both modes. For complete readers: their
/// positions, the data they hold, the credit each session has, the live frames on
/// their way to disk and the frames that wait for each session, and the records that
/// let a new home continue. For latest readers: the index's newest live frame and the
/// frame that waits for each session. Sans-I/O: the home passes mesh time in, appends
/// [`Readers::records`] to the index log after each input, and calls
/// [`Readers::advance`] at [`Readers::deadline`].
#[derive(Debug)]
pub struct Readers {
    /// The complete sessions, sorted by key.
    complete: Vec<Session>,
    /// The flow of each complete session, at its index in `complete`. Apart so that a
    /// `Session` stays 64 bytes, which a search by key indexes with a shift, not a
    /// multiply.
    flows: Vec<Flow>,
    /// The live frames not yet on disk, with their seq, oldest first. Empty when no
    /// complete session is open.
    queue: VecDeque<(Frame, Range<u64>)>,
    /// The end of the last live frame queued, or the live seq at start.
    queued: u64,
    /// The end of the newest live frame with samples that was released, or dropped
    /// with no complete session open. Memory holds no frame below it.
    released: u64,
    /// Named readers that still hold after their session closed.
    closed: Vec<Closed>,
    /// The latest sessions, sorted by key.
    latest: Vec<latest::Session>,
    /// The index's newest live frame.
    newest: Option<Frame>,
    /// The last [`Readers::release`]'s result, kept so that it does not allocate.
    woken_complete: Vec<complete::Key>,
    /// The last [`Readers::put`]'s result, kept so that it does not allocate.
    woken_latest: Vec<latest::Key>,
    next_complete: u64,
    next_latest: u64,
    records: Vec<Record>,
}

#[derive(Debug)]
struct Session {
    key: complete::Key,
    reader: Reader,
    position: Position,
    /// The position moved since the last record.
    changed: bool,
}

/// The frames a new complete session can wait on before its list grows: as many as
/// a first push would take room for.
const WAITING: usize = 4;

/// What a complete session got of the live path, and its credit for more.
#[derive(Debug)]
struct Flow {
    credit: Credit,
    /// The session missed a frame, so it gets no later frame.
    behind: bool,
    /// The frames released to the session and not taken, oldest first.
    waiting: VecDeque<Frame>,
}

#[derive(Debug)]
struct Credit {
    /// Bytes sent since the session opened.
    spent_bytes: u64,
    /// The highest grant.
    limit_bytes: u64,
}

#[derive(Debug)]
struct Closed {
    name: Name,
    hold: Span,
    position: Position,
    at: Stamp,
}

impl Readers {
    /// No readers, on an index whose next live sample takes seq `live`.
    #[must_use]
    pub fn new(live: u64) -> Self {
        Self {
            complete: Vec::new(),
            flows: Vec::new(),
            queue: VecDeque::new(),
            queued: live,
            released: live,
            closed: Vec::new(),
            latest: Vec::new(),
            newest: None,
            woken_complete: Vec::new(),
            woken_latest: Vec::new(),
            next_complete: 0,
            next_latest: 0,
            records: Vec::new(),
        }
    }

    /// The readers that `records` describe, in log order, on an index whose next live
    /// sample takes seq `live`. A session that was open at the crash counts as closed
    /// at `now`.
    ///
    /// # Panics
    ///
    /// If a record's hold is negative.
    pub fn restore(
        records: impl IntoIterator<Item = Record>,
        now: Stamp,
        live: u64,
    ) -> Self {
        let mut last = BTreeMap::new();
        for record in records {
            check(record.hold);
            last.insert(record.reader, (record.position, record.hold, record.closed));
        }
        let closed = last
            .into_iter()
            .map(|(name, (position, hold, closed))| Closed {
                name,
                hold,
                position,
                at: closed.unwrap_or(now),
            })
            .collect();
        let mut readers = Self {
            closed,
            ..Self::new(live)
        };
        readers.advance(now);
        readers
    }

    /// Starts a complete session with credit for `limit_bytes` since it opens, its
    /// first grant. A named reader's open session in either mode is taken over. A
    /// session that starts below a live frame that memory no longer holds gets no live
    /// frame.
    ///
    /// # Panics
    ///
    /// If the reader's hold is negative.
    pub fn open(
        &mut self,
        reader: Reader,
        start: Start,
        limit_bytes: u64,
    ) -> complete::Opened {
        let (stored, replaced) = match &reader {
            Reader::Unnamed => (None, None),
            Reader::Named { name, hold } => {
                check(*hold);
                self.take_over(name)
            }
        };
        let position = match start {
            Start::At(position) => position,
            Start::Resume {
                presented,
                otherwise,
            } => resume(presented, stored, otherwise),
        };
        let key = complete::Key(self.next_complete);
        self.next_complete += 1;
        let session = Session {
            key,
            reader,
            position,
            changed: false,
        };
        self.records.extend(session.record());
        self.complete.push(session);
        self.flows.push(Flow {
            credit: Credit::new(limit_bytes),
            behind: position.live < self.released,
            waiting: VecDeque::with_capacity(WAITING),
        });
        self.woken_complete.clear();
        self.woken_complete.reserve(self.complete.len());
        complete::Opened {
            key,
            position,
            replaced,
        }
    }

    /// Records that the session has every sample below `position`. An ack to a closed
    /// session changes nothing: an ack can arrive after its session closes.
    ///
    /// # Errors
    ///
    /// [`Error::Ack`] when the session is open and `position` drops a path, adds one,
    /// or moves back on one.
    ///
    /// # Panics
    ///
    /// If this `Readers` never gave `key`.
    pub fn ack(&mut self, key: complete::Key, position: Position) -> Result<(), Error> {
        let Some(i) = self.find(key) else {
            return Ok(());
        };
        let session = &mut self.complete[i];
        let from = session.position;
        let forward = position.live >= from.live
            && match (from.backfill, position.backfill) {
                (Some(from), Some(to)) => to >= from,
                (None, None) => true,
                (Some(_), None) | (None, Some(_)) => false,
            };
        if !forward {
            return Err(Error::Ack { from, to: position });
        }
        session.changed |= position != from;
        session.position = position;
        Ok(())
    }

    /// Raises the session's credit to `limit_bytes` since it opened. A limit that is
    /// not higher than the current one changes nothing, and so does a grant to a
    /// closed session: a grant can arrive after its session closes.
    ///
    /// # Panics
    ///
    /// If this `Readers` never gave `key`.
    pub fn grant(&mut self, key: complete::Key, limit_bytes: u64) {
        if let Some(i) = self.find(key) {
            self.flows[i].credit.grant(limit_bytes);
        }
    }

    /// Queues `frame`, stored on the live path with the samples `seq`, for the
    /// complete sessions. [`Readers::release`] gives it to them once it is on disk.
    /// Keeps nothing when no complete session is open. A frame with no samples
    /// reaches no session.
    ///
    /// # Panics
    ///
    /// If `seq` ends before it starts, or starts below the end of an earlier live
    /// frame or below the `live` given to [`Readers::new`] or [`Readers::restore`].
    pub fn queue(&mut self, frame: &Frame, seq: Range<u64>) {
        assert!(
            seq.start <= seq.end,
            "live frame at seq {}..{} ends before it starts",
            seq.start,
            seq.end
        );
        assert!(
            self.queued <= seq.start,
            "live frame at seq {}..{} queued after seq {}",
            seq.start,
            seq.end,
            self.queued
        );
        self.queued = seq.end;
        if seq.is_empty() {
            return;
        }
        if self.complete.is_empty() {
            self.released = seq.end;
        } else {
            self.queue.push_back((frame.clone(), seq));
        }
    }

    /// Gives each queued frame that ends at or below `durable`, the first live seq not
    /// on disk, to each complete session, in seq order. A session gets each frame that
    /// ends past its position while it has credit for it, and no frame after the first
    /// it has no credit for. Returns the complete sessions that had no waiting frame and
    /// now have one, each once: wake them.
    #[must_use]
    pub fn release(&mut self, durable: u64) -> &[complete::Key] {
        self.woken_complete.clear();
        while let Some((frame, seq)) =
            self.queue.pop_front_if(|(_, seq)| seq.end <= durable)
        {
            let charge = frame.charge();
            let mut last: Option<&mut VecDeque<Frame>> = None;
            for (session, flow) in iter::zip(&self.complete, &mut self.flows) {
                if flow.behind || seq.end <= session.position.live {
                    continue;
                }
                if !flow.credit.spend(charge) {
                    flow.behind = true;
                    continue;
                }
                if flow.waiting.is_empty() {
                    self.woken_complete.push(session.key);
                }
                if let Some(waiting) = last.replace(&mut flow.waiting) {
                    waiting.push_back(frame.clone());
                }
            }
            if let Some(waiting) = last {
                waiting.push_back(frame);
            }
            self.released = seq.end;
        }
        &self.woken_complete
    }

    /// Whether a queued live frame waits to be on disk. While one does, call
    /// [`Readers::release`] after each commit.
    #[must_use]
    pub fn pending(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Takes the session's next waiting frame, or `None` when it has none or is
    /// closed. A latest session has at most one; a complete session has the frames
    /// that [`Readers::release`] gave it, in seq order.
    ///
    /// # Panics
    ///
    /// If this `Readers` never gave `key`.
    pub fn take(&mut self, key: Key) -> Option<Frame> {
        match key {
            Key::Complete(key) => {
                let i = self.find(key)?;
                self.flows[i].waiting.pop_front()
            }
            Key::Latest(key) => self.take_latest(key),
        }
    }

    /// Ends the session at `now`, in either mode. A waiting frame does not go out. The
    /// last complete session drops the queued frames. A close of a closed session
    /// changes nothing: a close can arrive after a takeover.
    ///
    /// # Panics
    ///
    /// If this `Readers` never gave `key`.
    pub fn close(&mut self, key: Key, now: Stamp) {
        match key {
            Key::Complete(key) => {
                if let Some(i) = self.find(key) {
                    self.close_complete(i, now);
                }
            }
            Key::Latest(key) => self.close_latest(key),
        }
    }

    fn close_complete(&mut self, i: usize, now: Stamp) {
        let session = self.remove(i);
        if self.complete.is_empty()
            && let Some((_, seq)) = self.queue.back()
        {
            self.released = seq.end;
            self.queue.clear();
        }
        if let Reader::Named { name, hold } = session.reader {
            let closed = Closed {
                name,
                hold,
                position: session.position,
                at: now,
            };
            self.records.push(closed.record());
            if now < closed.end() {
                self.closed.push(closed);
            }
        }
    }

    /// Forgets each named reader whose hold ended at or before `now`.
    pub fn advance(&mut self, now: Stamp) {
        self.closed.retain(|closed| now < closed.end());
    }

    /// When the next hold ends, or `None` when no closed reader holds.
    #[must_use]
    pub fn deadline(&self) -> Option<Stamp> {
        self.closed.iter().map(Closed::end).min()
    }

    /// The lowest position that any reader holds, path by path, or `None` when no
    /// reader holds. `buffer` may trim below it. Takes time linear in the readers.
    #[must_use]
    pub fn floor(&self) -> Option<Position> {
        let open = self.complete.iter().map(|session| session.position);
        let closed = self.closed.iter().map(|closed| closed.position);
        open.chain(closed).reduce(|a, b| Position {
            live: a.live.min(b.live),
            backfill: match (a.backfill, b.backfill) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, None) => a,
                (None, b) => b,
            },
        })
    }

    /// Queues a record for each named reader whose position changed since its last
    /// record. The home calls it on its position interval.
    pub fn flush(&mut self) {
        for session in self.complete.iter_mut().filter(|s| s.changed) {
            session.changed = false;
            self.records.extend(session.record());
        }
    }

    /// Takes the queued records, oldest first.
    pub fn records(&mut self) -> impl Iterator<Item = Record> {
        self.records.drain(..)
    }

    /// Removes the named reader: its open session in either mode, and its hold.
    /// Returns its position, open or closed, and the session it had open.
    fn take_over(&mut self, name: &Name) -> (Option<Position>, Option<Key>) {
        if let Some(i) = self.named(name) {
            let session = self.remove(i);
            return (Some(session.position), Some(session.key.into()));
        }
        let replaced = self.remove_latest(name).map(Key::from);
        let closed = self.closed.iter().position(|closed| closed.name == *name);
        (closed.map(|i| self.closed.remove(i).position), replaced)
    }

    /// Closes the named reader's open session in either mode at `now`. Returns it.
    fn close_named(&mut self, name: &Name, now: Stamp) -> Option<Key> {
        let Some(i) = self.named(name) else {
            return self.remove_latest(name).map(Key::from);
        };
        let key = self.complete[i].key;
        self.close_complete(i, now);
        Some(key.into())
    }

    /// The named reader's open complete session.
    fn named(&self, name: &Name) -> Option<usize> {
        self.complete.iter().position(|s| s.name() == Some(name))
    }

    /// The open complete session `key`, or `None` when it closed.
    ///
    /// # Panics
    ///
    /// If this `Readers` never gave `key`.
    fn find(&self, key: complete::Key) -> Option<usize> {
        match self.complete.binary_search_by_key(&key, |s| s.key) {
            Ok(i) => Some(i),
            Err(_) if key.0 < self.next_complete => None,
            Err(_) => never_open(key.into()),
        }
    }

    fn remove(&mut self, i: usize) -> Session {
        self.flows.remove(i);
        self.complete.remove(i)
    }
}

/// Panics because this `Readers` never gave `key`.
#[cold]
#[inline(never)]
fn never_open(key: Key) -> ! {
    match key {
        Key::Latest(key) => panic!("latest session {key} was never open"),
        Key::Complete(key) => panic!("complete session {key} was never open"),
    }
}

impl Credit {
    /// A credit of `limit_bytes` with nothing spent.
    fn new(limit_bytes: u64) -> Self {
        Self {
            spent_bytes: 0,
            limit_bytes,
        }
    }

    /// Raises the limit to `limit_bytes`. A lower limit changes nothing.
    fn grant(&mut self, limit_bytes: u64) {
        self.limit_bytes = self.limit_bytes.max(limit_bytes);
    }

    /// Spends `bytes`, the charge of one frame, and returns `true` when less than the
    /// limit is spent: the frame may pass the limit. Otherwise returns `false` and
    /// spends nothing.
    fn spend(&mut self, bytes: u64) -> bool {
        if self.spent_bytes >= self.limit_bytes {
            return false;
        }
        self.spent_bytes += bytes;
        true
    }
}

impl Session {
    fn name(&self) -> Option<&Name> {
        match &self.reader {
            Reader::Named { name, .. } => Some(name),
            Reader::Unnamed => None,
        }
    }

    fn record(&self) -> Option<Record> {
        match &self.reader {
            Reader::Named { name, hold } => Some(Record {
                reader: name.clone(),
                position: self.position,
                hold: *hold,
                closed: None,
            }),
            Reader::Unnamed => None,
        }
    }
}

impl Closed {
    /// A hold that would end past the last stamp ends at it.
    fn end(&self) -> Stamp {
        self.at
            .checked_add(self.hold)
            .unwrap_or(Stamp::from_nanos(i64::MAX))
    }

    fn record(&self) -> Record {
        Record {
            reader: self.name.clone(),
            position: self.position,
            hold: self.hold,
            closed: Some(self.at),
        }
    }
}

/// `config` rejects a negative hold at plan, so one here is a broken invariant.
fn check(hold: Span) {
    assert!(hold >= Span::ZERO, "reader hold is negative: {hold}");
}

/// On each path: `presented`, else `stored`, else `otherwise`. The reader follows
/// backfill only when `otherwise` does.
fn resume(
    presented: Option<Position>,
    stored: Option<Position>,
    otherwise: Position,
) -> Position {
    let backfill = |position: Option<Position>| position.and_then(|p| p.backfill);
    Position {
        live: presented.or(stored).map_or(otherwise.live, |p| p.live),
        backfill: otherwise
            .backfill
            .map(|b| backfill(presented).or(backfill(stored)).unwrap_or(b)),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::Arc;

    use types::channel;
    use types::frame::key_set::{Group, Interner, KeySet};
    use types::frame::{Draft, Form, Path};

    use super::*;

    /// The charge of each frame from [`Frames`]: a 64-byte block and its header.
    const CHARGE: u64 = 128;

    /// Frames of one index with no data channels. Frame `n` holds `n` in its index
    /// series.
    pub(super) struct Frames {
        pool: block::Pool,
        set: Arc<KeySet>,
    }

    impl Frames {
        /// Room for `count` frames at once.
        pub(super) fn new(count: usize) -> Self {
            let config = block::Config {
                budget: count * 128,
            };
            let memory = block::Heap::new(config.reservation());
            let index = Group {
                index: channel::Key::from_u128(1),
                data: &[],
            };
            Self {
                pool: block::Pool::new(config, memory),
                set: Interner::new().intern(&[index]),
            }
        }

        pub(super) fn make(&self, n: u64) -> Result<Frame, types::frame::Error> {
            let series = [(0, 8)];
            let mut draft = Draft::new(&self.pool, &self.set, Form::Raw, &series)?;
            let bytes = draft.series_mut(0).expect("the index is present");
            bytes.copy_from_slice(&n.to_le_bytes());
            Ok(draft.freeze(Path::Live))
        }

        pub(super) fn frame(&self, n: u64) -> Frame {
            self.make(n).expect("the pool has room")
        }

        /// The pool has room for one more frame.
        fn spare(&self) -> bool {
            self.make(0).is_ok()
        }
    }

    pub(super) fn number(frame: &Frame) -> u64 {
        let bytes = frame.series(0).expect("the index is present");
        u64::from_le_bytes(bytes.try_into().expect("8 bytes"))
    }

    fn at(nanos: i64) -> Stamp {
        Stamp::from_nanos(nanos)
    }

    fn live(seq: u64) -> Position {
        Position {
            live: seq,
            backfill: None,
        }
    }

    fn both(live: u64, backfill: u64) -> Position {
        Position {
            live,
            backfill: Some(backfill),
        }
    }

    fn named(name: &str, hold: i64) -> Reader {
        Reader::Named {
            name: name.parse().expect("valid name"),
            hold: Span::from_nanos(hold),
        }
    }

    fn resume(otherwise: Position) -> Start {
        Start::Resume {
            presented: None,
            otherwise,
        }
    }

    fn record(
        name: &str,
        position: Position,
        hold: i64,
        closed: Option<i64>,
    ) -> Record {
        Record {
            reader: name.parse().expect("valid name"),
            position,
            hold: Span::from_nanos(hold),
            closed: closed.map(at),
        }
    }

    fn drained(readers: &mut Readers) -> Vec<Record> {
        readers.records().collect()
    }

    /// Opens `name` at `position`, closes it at `closed`, and drops the records.
    fn left(readers: &mut Readers, name: &str, position: Position, closed: i64) {
        let key = readers.open(named(name, 10), Start::At(position), 0).key;
        readers.close(key.into(), at(closed));
        drained(readers);
    }

    /// Makes each call on the closed session `key`, and asserts that it changes
    /// nothing.
    pub(super) fn dropped(readers: &mut Readers, key: Key) {
        let before = format!("{readers:?}");
        if let Key::Complete(key) = key {
            readers.grant(key, u64::MAX);
            assert_eq!(readers.ack(key, live(0)), Ok(()));
        }
        assert!(readers.take(key).is_none());
        readers.close(key, at(i64::MAX));
        assert_eq!(format!("{readers:?}"), before);
    }

    /// Checks [`dropped`] on each complete key given that `open` does not hold.
    fn dropped_closed(readers: &mut Readers, open: impl Fn(&complete::Key) -> bool) {
        for key in (0..readers.next_complete).map(complete::Key) {
            if !open(&key) {
                dropped(readers, key.into());
            }
        }
    }

    mod open {
        use super::*;

        #[test]
        fn starts_at_the_given_position() {
            let mut readers = Readers::new(0);
            let opened = readers.open(Reader::Unnamed, Start::At(live(5)), 0);
            assert_eq!(opened.position, live(5));
            assert_eq!(opened.replaced, None);
        }

        #[test]
        fn gives_each_session_its_own_key() {
            let mut readers = Readers::new(0);
            let first = readers.open(Reader::Unnamed, Start::At(live(0)), 0).key;
            readers.close(first.into(), at(0));
            let second = readers.open(Reader::Unnamed, Start::At(live(0)), 0).key;
            assert_ne!(first, second);
        }

        #[test]
        fn resume_takes_the_presented_position() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(7), 1);
            let start = Start::Resume {
                presented: Some(live(9)),
                otherwise: live(0),
            };
            assert_eq!(readers.open(named("a", 10), start, 0).position, live(9));
        }

        #[test]
        fn resume_falls_back_to_the_position_at_this_home() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(7), 1);
            let opened = readers.open(named("a", 10), resume(live(0)), 0);
            assert_eq!(opened.position, live(7));
        }

        #[test]
        fn resume_falls_back_to_otherwise() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(7), 1);
            let opened = readers.open(named("b", 10), resume(both(2, 1)), 0);
            assert_eq!(opened.position, both(2, 1));
        }

        #[test]
        fn resume_falls_back_per_path() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(7), 1);
            let opened = readers.open(named("a", 10), resume(both(0, 2)), 0);
            assert_eq!(opened.position, both(7, 2));
        }

        #[test]
        fn resume_prefers_presented_backfill_to_the_position_at_this_home() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", both(7, 3), 1);
            let start = Start::Resume {
                presented: Some(both(9, 5)),
                otherwise: both(0, 0),
            };
            assert_eq!(readers.open(named("a", 10), start, 0).position, both(9, 5));
        }

        #[test]
        fn resume_takes_backfill_from_this_home_when_the_presented_has_none() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", both(7, 3), 1);
            let start = Start::Resume {
                presented: Some(live(9)),
                otherwise: both(0, 0),
            };
            assert_eq!(readers.open(named("a", 10), start, 0).position, both(9, 3));
        }

        #[test]
        fn resume_follows_backfill_only_when_otherwise_does() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", both(7, 3), 1);
            let opened = readers.open(named("a", 10), resume(live(0)), 0);
            assert_eq!(opened.position, live(7));
        }

        #[test]
        fn at_ignores_the_position_at_this_home() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(7), 1);
            let opened = readers.open(named("a", 10), Start::At(live(2)), 0);
            assert_eq!(opened.position, live(2));
        }
    }

    mod takeover {
        use super::*;

        #[test]
        fn continues_from_the_old_session() {
            let mut readers = Readers::new(0);
            let old = readers.open(named("a", 10), resume(live(0)), 0).key;
            readers.ack(old, live(5)).expect("forward");
            let opened = readers.open(named("a", 10), resume(live(0)), 0);
            assert_eq!(opened.position, live(5));
            assert_eq!(opened.replaced, Some(old.into()));
        }

        #[test]
        fn takes_the_presented_position() {
            let mut readers = Readers::new(0);
            let old = readers.open(named("a", 10), resume(live(0)), 0).key;
            readers.ack(old, live(5)).expect("forward");
            let start = Start::Resume {
                presented: Some(live(8)),
                otherwise: live(0),
            };
            assert_eq!(readers.open(named("a", 10), start, 0).position, live(8));
        }

        #[test]
        fn closes_the_old_session() {
            let mut readers = Readers::new(0);
            let old = readers.open(named("a", 10), resume(live(0)), 0).key;
            let new = readers.open(named("a", 10), resume(live(0)), 0).key;
            readers.ack(new, live(5)).expect("forward");
            assert_eq!(readers.ack(old, live(1)), Ok(()));
            assert_eq!(readers.floor(), Some(live(5)));
        }

        #[test]
        fn never_happens_to_unnamed_readers() {
            let mut readers = Readers::new(0);
            readers.open(Reader::Unnamed, Start::At(live(0)), 0);
            let opened = readers.open(Reader::Unnamed, Start::At(live(0)), 0);
            assert_eq!(opened.replaced, None);
        }
    }

    mod ack {
        use super::*;

        fn opened(position: Position) -> (Readers, complete::Key) {
            let mut readers = Readers::new(0);
            let key = readers.open(named("a", 10), Start::At(position), 0).key;
            drained(&mut readers);
            (readers, key)
        }

        #[test]
        fn moves_the_position_forward() {
            let (mut readers, key) = opened(both(0, 0));
            assert_eq!(readers.ack(key, both(5, 2)), Ok(()));
            readers.flush();
            assert_eq!(drained(&mut readers), [record("a", both(5, 2), 10, None)]);
        }

        #[test]
        fn moves_one_path_alone() {
            let (mut readers, key) = opened(both(0, 2));
            assert_eq!(readers.ack(key, both(5, 2)), Ok(()));
        }

        #[test]
        fn rejects_a_move_back() {
            let (mut readers, key) = opened(live(0));
            readers.ack(key, live(5)).expect("forward");
            let error = Error::Ack {
                from: live(5),
                to: live(4),
            };
            assert_eq!(readers.ack(key, live(4)), Err(error));
            readers.flush();
            assert_eq!(drained(&mut readers), [record("a", live(5), 10, None)]);
        }

        #[test]
        fn rejects_a_move_back_on_backfill() {
            let (mut readers, key) = opened(both(5, 5));
            let error = Error::Ack {
                from: both(5, 5),
                to: both(6, 4),
            };
            assert_eq!(readers.ack(key, both(6, 4)), Err(error));
        }

        #[test]
        fn rejects_a_dropped_path() {
            let (mut readers, key) = opened(both(0, 0));
            let error = Error::Ack {
                from: both(0, 0),
                to: live(3),
            };
            assert_eq!(readers.ack(key, live(3)), Err(error));
        }

        #[test]
        fn rejects_an_added_path() {
            let (mut readers, key) = opened(live(0));
            let error = Error::Ack {
                from: live(0),
                to: both(3, 1),
            };
            assert_eq!(readers.ack(key, both(3, 1)), Err(error));
        }

        #[test]
        fn of_the_same_position_changes_nothing() {
            let (mut readers, key) = opened(live(4));
            assert_eq!(readers.ack(key, live(4)), Ok(()));
            readers.flush();
            assert_eq!(drained(&mut readers), []);
        }

        #[test]
        fn to_a_closed_session_changes_nothing() {
            let (mut readers, key) = opened(live(4));
            readers.close(key.into(), at(1));
            drained(&mut readers);
            assert_eq!(readers.ack(key, live(9)), Ok(()));
            assert_eq!(readers.ack(key, live(2)), Ok(()));
            readers.flush();
            assert_eq!(drained(&mut readers), []);
            assert_eq!(readers.floor(), Some(live(4)));
        }

        #[test]
        #[should_panic(expected = "complete session 1 was never open")]
        fn panics_on_a_session_never_open() {
            let (mut readers, _) = opened(live(0));
            readers
                .ack(complete::Key(1), live(1))
                .expect("panics before");
        }
    }

    mod late {
        use super::*;

        #[test]
        fn after_a_close_keeps_the_hold_of_the_first_close() {
            let mut readers = Readers::new(0);
            let key = readers.open(named("a", 10), Start::At(live(2)), 0).key;
            readers.close(key.into(), at(1));
            drained(&mut readers);
            dropped(&mut readers, key.into());
            assert_eq!(drained(&mut readers), []);
            assert_eq!(readers.floor(), Some(live(2)));
            assert_eq!(readers.deadline(), Some(at(11)));
        }

        #[test]
        fn after_a_takeover_by_a_complete_session_leaves_it_open() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let old = readers.open(named("a", 10), resume(live(0)), CHARGE).key;
            let new = readers.open(named("a", 10), resume(live(0)), CHARGE).key;
            readers.queue(&frames.frame(1), 0..1);
            drained(&mut readers);
            dropped(&mut readers, old.into());
            assert_eq!(drained(&mut readers), []);
            assert_eq!(readers.release(1), [new]);
            assert_eq!(readers.take(new.into()).as_ref().map(number), Some(1));
            assert_eq!(readers.ack(new, live(1)), Ok(()));
        }

        #[test]
        fn after_a_takeover_by_a_latest_session_leaves_it_open() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let old = readers.open(named("a", 10), resume(live(0)), CHARGE).key;
            let name = "a".parse().expect("valid name");
            let new = readers.open_latest(Some(name), at(1)).key;
            assert_eq!(readers.put(frames.frame(1)), [new]);
            drained(&mut readers);
            dropped(&mut readers, old.into());
            assert_eq!(drained(&mut readers), []);
            assert_eq!(readers.take(new.into()).as_ref().map(number), Some(1));
        }

        #[test]
        #[should_panic(expected = "complete session 1 was never open")]
        fn take_panics_on_a_session_never_open() {
            let mut readers = Readers::new(0);
            let _ = readers.open(Reader::Unnamed, Start::At(live(0)), 0);
            readers.take(complete::Key(1).into());
        }

        #[test]
        #[should_panic(expected = "complete session 1 was never open")]
        fn close_panics_on_a_session_never_open() {
            let mut readers = Readers::new(0);
            let _ = readers.open(Reader::Unnamed, Start::At(live(0)), 0);
            readers.close(complete::Key(1).into(), at(0));
        }
    }

    mod hold {
        use super::*;

        #[test]
        fn lasts_while_the_session_is_open() {
            let mut readers = Readers::new(0);
            readers.open(Reader::Unnamed, Start::At(live(3)), 0);
            readers.advance(at(i64::MAX));
            assert_eq!(readers.floor(), Some(live(3)));
            assert_eq!(readers.deadline(), None);
        }

        #[test]
        fn of_an_unnamed_reader_ends_at_the_close() {
            let mut readers = Readers::new(0);
            let key = readers.open(Reader::Unnamed, Start::At(live(3)), 0).key;
            readers.close(key.into(), at(1));
            assert_eq!(readers.floor(), None);
        }

        #[test]
        fn of_a_named_reader_ends_after_the_close() {
            let mut readers = Readers::new(0);
            let key = readers.open(named("a", 10), Start::At(live(0)), 0).key;
            readers.ack(key, live(4)).expect("forward");
            readers.close(key.into(), at(5));
            assert_eq!(readers.deadline(), Some(at(15)));
            readers.advance(at(14));
            assert_eq!(readers.floor(), Some(live(4)));
            readers.advance(at(15));
            assert_eq!(readers.floor(), None);
            assert_eq!(readers.deadline(), None);
        }

        #[test]
        fn of_zero_ends_at_the_close() {
            let mut readers = Readers::new(0);
            let key = readers.open(named("a", 0), Start::At(live(0)), 0).key;
            readers.close(key.into(), at(5));
            assert_eq!(readers.floor(), None);
        }

        #[test]
        #[should_panic(expected = "reader hold is negative: -3ns")]
        fn panics_when_negative() {
            Readers::new(0).open(named("a", -3), Start::At(live(0)), 0);
        }

        #[test]
        fn ends_first_for_the_reader_that_closed_first() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(1), 15);
            left(&mut readers, "b", live(1), 5);
            assert_eq!(readers.deadline(), Some(at(15)));
        }

        #[test]
        fn ends_at_the_last_stamp() {
            let mut readers = Readers::new(0);
            let key = readers.open(named("a", 10), Start::At(live(0)), 0).key;
            readers.close(key.into(), at(i64::MAX - 1));
            assert_eq!(readers.deadline(), Some(at(i64::MAX)));
        }

        #[test]
        fn is_cancelled_by_a_reopen() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(4), 5);
            let opened = readers.open(named("a", 10), resume(live(0)), 0);
            assert_eq!(opened.position, live(4));
            assert_eq!(readers.deadline(), None);
        }

        #[test]
        fn that_ended_leaves_no_position_to_resume() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(4), 5);
            readers.advance(at(15));
            let opened = readers.open(named("a", 10), resume(live(9)), 0);
            assert_eq!(opened.position, live(9));
        }
    }

    mod floor {
        use super::*;

        #[test]
        fn is_the_lowest_position_on_each_path() {
            let mut readers = Readers::new(0);
            readers.open(Reader::Unnamed, Start::At(both(5, 9)), 0);
            readers.open(Reader::Unnamed, Start::At(both(7, 2)), 0);
            readers.open(Reader::Unnamed, Start::At(live(3)), 0);
            assert_eq!(readers.floor(), Some(both(3, 2)));
        }

        #[test]
        fn has_no_backfill_when_no_reader_records() {
            let mut readers = Readers::new(0);
            readers.open(Reader::Unnamed, Start::At(live(5)), 0);
            readers.open(Reader::Unnamed, Start::At(live(7)), 0);
            assert_eq!(readers.floor(), Some(live(5)));
        }

        #[test]
        fn is_none_without_readers() {
            assert_eq!(Readers::new(0).floor(), None);
        }

        #[test]
        fn goes_down_when_a_session_opens_below_it() {
            let mut readers = Readers::new(0);
            readers.open(Reader::Unnamed, Start::At(live(5)), 0);
            readers.open(Reader::Unnamed, Start::At(live(2)), 0);
            assert_eq!(readers.floor(), Some(live(2)));
        }
    }

    mod records {
        use super::*;

        #[test]
        fn are_written_at_once_when_a_named_reader_opens() {
            let mut readers = Readers::new(0);
            readers.open(named("a", 10), Start::At(live(3)), 0);
            assert_eq!(drained(&mut readers), [record("a", live(3), 10, None)]);
        }

        #[test]
        fn are_written_at_once_when_a_named_reader_closes() {
            let mut readers = Readers::new(0);
            let key = readers.open(named("a", 10), Start::At(live(3)), 0).key;
            readers.ack(key, live(5)).expect("forward");
            readers.close(key.into(), at(7));
            assert_eq!(
                drained(&mut readers),
                [
                    record("a", live(3), 10, None),
                    record("a", live(5), 10, Some(7))
                ]
            );
        }

        #[test]
        fn are_written_at_once_when_a_reader_closes_where_it_opened() {
            let mut readers = Readers::new(0);
            let key = readers.open(named("a", 10), Start::At(live(3)), 0).key;
            drained(&mut readers);
            readers.close(key.into(), at(7));
            assert_eq!(drained(&mut readers), [record("a", live(3), 10, Some(7))]);
        }

        #[test]
        fn are_written_at_once_on_a_takeover() {
            let mut readers = Readers::new(0);
            let old = readers.open(named("a", 10), Start::At(live(3)), 0).key;
            readers.ack(old, live(5)).expect("forward");
            drained(&mut readers);
            readers.open(named("a", 20), resume(live(0)), 0);
            assert_eq!(drained(&mut readers), [record("a", live(5), 20, None)]);
        }

        #[test]
        fn are_written_on_flush_only_for_changed_positions() {
            let mut readers = Readers::new(0);
            let a = readers.open(named("a", 10), Start::At(live(0)), 0).key;
            readers.open(named("b", 10), Start::At(live(0)), 0);
            drained(&mut readers);
            readers.ack(a, live(2)).expect("forward");
            readers.flush();
            assert_eq!(drained(&mut readers), [record("a", live(2), 10, None)]);
            readers.flush();
            assert_eq!(drained(&mut readers), []);
        }

        #[test]
        fn are_never_written_for_unnamed_readers() {
            let mut readers = Readers::new(0);
            let key = readers.open(Reader::Unnamed, Start::At(live(0)), 0).key;
            readers.ack(key, live(2)).expect("forward");
            readers.flush();
            readers.close(key.into(), at(1));
            assert_eq!(drained(&mut readers), []);
        }
    }

    mod restore {
        use super::*;

        #[test]
        fn continues_from_the_last_record() {
            let records = [
                record("a", live(3), 10, None),
                record("a", live(5), 10, None),
            ];
            let mut readers = Readers::restore(records, at(0), 0);
            assert_eq!(readers.floor(), Some(live(5)));
            let opened = readers.open(named("a", 10), resume(live(0)), 0);
            assert_eq!(opened.position, live(5));
        }

        #[test]
        fn counts_an_open_session_as_closed_at_the_restore() {
            let readers = Readers::restore([record("a", live(3), 10, None)], at(20), 0);
            assert_eq!(readers.deadline(), Some(at(30)));
        }

        #[test]
        fn keeps_a_closed_reader_until_its_hold_ends() {
            let readers =
                Readers::restore([record("a", live(3), 10, Some(5))], at(8), 0);
            assert_eq!(readers.deadline(), Some(at(15)));
            assert_eq!(readers.floor(), Some(live(3)));
        }

        #[test]
        #[should_panic(expected = "reader hold is negative: -5ns")]
        fn panics_on_a_negative_hold() {
            Readers::restore([record("a", live(3), -5, Some(1))], at(0), 0);
        }

        #[test]
        fn keeps_the_last_record_of_each_reader() {
            let records = [
                record("b", live(1), 10, Some(15)),
                record("a", live(3), 10, None),
                record("b", live(4), 10, None),
            ];
            let readers = Readers::restore(records, at(20), 0);
            assert_eq!(readers.floor(), Some(live(3)));
            assert_eq!(readers.deadline(), Some(at(30)));
        }

        #[test]
        fn forgets_a_reader_whose_hold_ended() {
            let readers =
                Readers::restore([record("a", live(3), 10, Some(5))], at(15), 0);
            assert_eq!(readers.floor(), None);
        }

        #[test]
        fn writes_no_records() {
            let mut readers =
                Readers::restore([record("a", live(3), 10, None)], at(0), 0);
            assert_eq!(drained(&mut readers), []);
        }
    }

    #[test]
    fn a_test_frame_charges_one_block() {
        assert_eq!(Frames::new(1).frame(1).charge(), CHARGE);
    }

    mod credit {
        use super::*;

        #[test]
        fn is_none_at_a_limit_of_0() {
            assert!(!Credit::new(0).spend(1));
        }

        #[test]
        fn is_spent_up_to_the_limit() {
            let mut credit = Credit::new(10);
            assert!(credit.spend(4));
            assert!(credit.spend(6));
            assert!(!credit.spend(1));
        }

        #[test]
        fn is_not_lowered_by_a_grant() {
            let mut credit = Credit::new(10);
            assert!(credit.spend(6));
            credit.grant(5);
            assert!(credit.spend(1));
        }

        #[test]
        fn lets_one_frame_pass_the_limit() {
            let mut credit = Credit::new(10);
            assert!(credit.spend(25));
            assert!(!credit.spend(1));
        }

        #[test]
        fn counts_a_frame_past_the_limit_against_the_next_grant() {
            let mut credit = Credit::new(10);
            assert!(credit.spend(25));
            credit.grant(20);
            assert!(!credit.spend(1));
            credit.grant(26);
            assert!(credit.spend(1));
        }

        #[test]
        fn is_not_spent_by_a_refused_frame() {
            let mut credit = Credit::new(10);
            assert!(credit.spend(10));
            assert!(!credit.spend(5));
            credit.grant(12);
            assert!(credit.spend(1));
        }

        #[test]
        #[should_panic(expected = "complete session 1 was never open")]
        fn grant_panics_on_a_session_never_open() {
            let mut readers = Readers::new(0);
            let _ = readers.open(Reader::Unnamed, Start::At(live(0)), 0);
            readers.grant(complete::Key(1), 10);
        }

        #[test]
        #[should_panic(expected = "complete session 2 was never open")]
        fn grant_panics_on_a_key_past_the_next() {
            let mut readers = Readers::new(0);
            let _ = readers.open(Reader::Unnamed, Start::At(live(0)), 0);
            readers.grant(complete::Key(2), 10);
        }
    }

    mod release {
        use super::*;

        /// Opens a complete session at live seq `seq` with credit for `frames` frames.
        fn opened(readers: &mut Readers, seq: u64, frames: u64) -> complete::Key {
            readers
                .open(Reader::Unnamed, Start::At(live(seq)), frames * CHARGE)
                .key
        }

        fn released(readers: &mut Readers, durable: u64) -> Vec<complete::Key> {
            readers.release(durable).to_vec()
        }

        fn taken(readers: &mut Readers, key: impl Into<Key>) -> Vec<u64> {
            let key = key.into();
            iter::from_fn(|| readers.take(key))
                .map(|frame| number(&frame))
                .collect()
        }

        #[test]
        fn pends_only_while_a_frame_for_a_complete_session_waits_for_the_disk() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            readers.queue(&frames.frame(1), 0..2);
            assert!(!readers.pending());
            let key = opened(&mut readers, 2, 10);
            readers.queue(&frames.frame(2), 2..4);
            assert!(readers.pending());
            assert_eq!(released(&mut readers, 4), [key]);
            assert!(!readers.pending());
            readers.queue(&frames.frame(3), 4..4);
            assert!(!readers.pending());
            readers.queue(&frames.frame(4), 4..6);
            readers.close(key.into(), at(0));
            assert!(!readers.pending());
        }

        #[test]
        fn gives_only_frames_on_disk() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), 0..2);
            readers.queue(&frames.frame(2), 2..5);
            assert_eq!(released(&mut readers, 4), [key]);
            assert_eq!(taken(&mut readers, key), [1]);
            assert_eq!(released(&mut readers, 5), [key]);
            assert_eq!(taken(&mut readers, key), [2]);
        }

        #[test]
        fn gives_frames_in_seq_order_across_a_gap() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), 0..2);
            readers.queue(&frames.frame(2), 4..5);
            readers.queue(&frames.frame(3), 5..8);
            assert_eq!(released(&mut readers, 8), [key]);
            assert_eq!(taken(&mut readers, key), [1, 2, 3]);
        }

        #[test]
        fn gives_only_frames_that_end_past_the_position() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 3, 10);
            readers.queue(&frames.frame(1), 0..3);
            readers.queue(&frames.frame(2), 3..4);
            readers.queue(&frames.frame(3), 4..6);
            assert_eq!(released(&mut readers, 6), [key]);
            assert_eq!(taken(&mut readers, key), [2, 3]);
        }

        #[test]
        fn gives_no_frame_at_or_below_the_acked_position() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 1);
            readers.ack(key, live(4)).expect("forward");
            readers.queue(&frames.frame(1), 0..4);
            readers.queue(&frames.frame(2), 4..6);
            assert_eq!(released(&mut readers, 6), [key]);
            assert_eq!(taken(&mut readers, key), [2]);
        }

        #[test]
        fn gives_no_frame_with_no_samples() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), 0..2);
            readers.queue(&frames.frame(2), 2..2);
            readers.queue(&frames.frame(3), 2..3);
            assert_eq!(released(&mut readers, 3), [key]);
            assert_eq!(taken(&mut readers, key), [1, 3]);
        }

        #[test]
        fn marks_no_session_behind_for_a_frame_with_no_samples_and_none_open() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            readers.queue(&frames.frame(1), 1..1);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(2), 1..2);
            readers.queue(&frames.frame(3), 2..4);
            assert_eq!(released(&mut readers, 4), [key]);
            assert_eq!(taken(&mut readers, key), [2, 3]);
        }

        #[test]
        fn gives_nothing_to_a_session_below_a_dropped_frame() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            readers.queue(&frames.frame(1), 0..4);
            let behind = opened(&mut readers, 2, 10);
            let current = opened(&mut readers, 4, 10);
            readers.queue(&frames.frame(2), 4..6);
            assert_eq!(released(&mut readers, 6), [current]);
            assert_eq!(taken(&mut readers, behind), []);
            assert_eq!(taken(&mut readers, current), [2]);
        }

        #[test]
        fn gives_nothing_to_a_session_below_a_released_frame() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let first = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), 0..4);
            assert_eq!(released(&mut readers, 4), [first]);
            let behind = opened(&mut readers, 3, 10);
            readers.queue(&frames.frame(2), 4..6);
            assert_eq!(released(&mut readers, 6), []);
            assert_eq!(taken(&mut readers, behind), []);
            assert_eq!(taken(&mut readers, first), [1, 2]);
        }

        #[test]
        fn gives_nothing_below_the_live_seq_of_a_new_index() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(10);
            let key = opened(&mut readers, 5, 10);
            readers.queue(&frames.frame(1), 10..12);
            assert_eq!(released(&mut readers, 12), []);
            assert_eq!(taken(&mut readers, key), []);
        }

        #[test]
        fn gives_nothing_below_the_live_seq_of_a_restored_index() {
            let frames = Frames::new(1);
            let mut readers = Readers::restore([], at(0), 10);
            let key = opened(&mut readers, 5, 10);
            readers.queue(&frames.frame(1), 10..12);
            assert_eq!(released(&mut readers, 12), []);
            assert_eq!(taken(&mut readers, key), []);
        }

        #[test]
        fn lets_one_frame_pass_the_limit() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = readers
                .open(Reader::Unnamed, Start::At(live(0)), CHARGE + 1)
                .key;
            for n in 0..3 {
                readers.queue(&frames.frame(n + 1), n..n + 1);
            }
            assert_eq!(released(&mut readers, 3), [key]);
            assert_eq!(taken(&mut readers, key), [1, 2]);
        }

        #[test]
        fn gives_frames_up_to_the_limit_of_the_open_with_no_grant() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = readers
                .open(Reader::Unnamed, Start::At(live(0)), 2 * CHARGE)
                .key;
            for n in 0..3 {
                readers.queue(&frames.frame(n + 1), n..n + 1);
            }
            assert_eq!(released(&mut readers, 3), [key]);
            assert_eq!(taken(&mut readers, key), [1, 2]);
        }

        #[test]
        fn raises_the_limit_of_the_open_with_a_grant() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let key = readers
                .open(Reader::Unnamed, Start::At(live(0)), CHARGE)
                .key;
            readers.grant(key, 3 * CHARGE);
            for n in 0..4 {
                readers.queue(&frames.frame(n + 1), n..n + 1);
            }
            assert_eq!(released(&mut readers, 4), [key]);
            assert_eq!(taken(&mut readers, key), [1, 2, 3]);
        }

        #[test]
        fn keeps_the_limit_of_the_open_after_a_lower_grant() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let key = readers
                .open(Reader::Unnamed, Start::At(live(0)), 3 * CHARGE)
                .key;
            readers.grant(key, CHARGE);
            for n in 0..4 {
                readers.queue(&frames.frame(n + 1), n..n + 1);
            }
            assert_eq!(released(&mut readers, 4), [key]);
            assert_eq!(taken(&mut readers, key), [1, 2, 3]);
        }

        #[test]
        fn gives_no_frame_after_one_without_credit() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 1);
            readers.queue(&frames.frame(1), 0..1);
            readers.queue(&frames.frame(2), 1..2);
            assert_eq!(released(&mut readers, 2), [key]);
            readers.grant(key, 10 * CHARGE);
            readers.queue(&frames.frame(3), 2..3);
            assert_eq!(released(&mut readers, 3), []);
            assert_eq!(taken(&mut readers, key), [1]);
        }

        #[test]
        fn spends_the_credit_of_each_session() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let short = opened(&mut readers, 0, 1);
            let long = opened(&mut readers, 0, 3);
            for n in 0..3 {
                readers.queue(&frames.frame(n + 1), n..n + 1);
            }
            assert_eq!(released(&mut readers, 3), [short, long]);
            assert_eq!(taken(&mut readers, short), [1]);
            assert_eq!(taken(&mut readers, long), [1, 2, 3]);
        }

        #[test]
        fn gives_no_credit_to_a_takeover() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            readers.open(named("a", 10), Start::At(live(0)), 10 * CHARGE);
            let new = readers.open(named("a", 10), resume(live(0)), 0).key;
            readers.queue(&frames.frame(1), 0..1);
            assert_eq!(released(&mut readers, 1), []);
            assert_eq!(taken(&mut readers, new), []);
        }

        #[test]
        fn counts_the_credit_of_a_takeover_from_zero() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let old = readers
                .open(named("a", 10), Start::At(live(0)), 2 * CHARGE)
                .key;
            readers.queue(&frames.frame(1), 0..1);
            readers.queue(&frames.frame(2), 1..2);
            assert_eq!(released(&mut readers, 2), [old]);
            assert_eq!(taken(&mut readers, old), [1, 2]);
            readers.ack(old, live(2)).expect("forward");
            let new = readers.open(named("a", 10), resume(live(0)), CHARGE).key;
            readers.queue(&frames.frame(3), 2..3);
            assert_eq!(released(&mut readers, 3), [new]);
            assert_eq!(taken(&mut readers, new), [3]);
        }

        #[test]
        fn drops_a_grant_to_a_closed_session() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let closed = opened(&mut readers, 0, 0);
            let open = opened(&mut readers, 0, 1);
            readers.close(closed.into(), at(0));
            readers.grant(closed, 10 * CHARGE);
            readers.queue(&frames.frame(1), 0..1);
            readers.queue(&frames.frame(2), 1..2);
            assert_eq!(released(&mut readers, 2), [open]);
            assert_eq!(taken(&mut readers, open), [1]);
        }

        #[test]
        fn drops_a_grant_to_a_session_a_takeover_closed() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let old = readers.open(named("a", 10), Start::At(live(0)), 0).key;
            let new = readers.open(named("a", 10), resume(live(0)), 0).key;
            readers.grant(old, 10 * CHARGE);
            readers.queue(&frames.frame(1), 0..1);
            assert_eq!(released(&mut readers, 1), []);
            assert_eq!(taken(&mut readers, new), []);
        }

        #[test]
        fn drops_a_grant_to_a_session_a_latest_takeover_closed() {
            let mut readers = Readers::new(0);
            let old = readers.open(named("a", 10), Start::At(live(0)), 0).key;
            let name = "a".parse().expect("a valid name");
            let latest = readers.open_latest(Some(name), at(1));
            assert_eq!(latest.replaced, Some(old.into()));
            readers.grant(old, 10 * CHARGE);
            assert_eq!(taken(&mut readers, latest.key), []);
        }

        #[test]
        fn wakes_a_session_once_while_frames_wait() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), 0..1);
            readers.queue(&frames.frame(2), 1..2);
            assert_eq!(released(&mut readers, 2), [key]);
            readers.queue(&frames.frame(3), 2..3);
            assert_eq!(released(&mut readers, 3), []);
            assert_eq!(taken(&mut readers, key), [1, 2, 3]);
            readers.queue(&frames.frame(4), 3..4);
            assert_eq!(released(&mut readers, 4), [key]);
        }

        #[test]
        fn wakes_nothing_without_a_frame_on_disk() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), 0..2);
            assert_eq!(released(&mut readers, 1), []);
        }

        #[test]
        fn holds_a_frame_until_each_session_takes_it() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let a = opened(&mut readers, 0, 10);
            let b = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), 0..1);
            assert_eq!(released(&mut readers, 1), [a, b]);
            assert_eq!(taken(&mut readers, a), [1]);
            assert!(!frames.spare());
            assert_eq!(taken(&mut readers, b), [1]);
            assert!(frames.spare());
        }

        #[test]
        fn drops_a_frame_no_session_got() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            opened(&mut readers, 0, 0);
            readers.queue(&frames.frame(1), 0..1);
            assert!(!frames.spare());
            assert_eq!(released(&mut readers, 1), []);
            assert!(frames.spare());
        }

        #[test]
        fn keeps_nothing_with_no_complete_session() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let latest = readers.open_latest(None, at(0)).key;
            readers.queue(&frames.frame(1), 0..1);
            assert!(frames.spare());
            assert_eq!(
                readers.take(latest.into()).map(|frame| number(&frame)),
                None
            );
        }

        #[test]
        fn drops_the_waiting_frames_of_a_closed_session() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), 0..1);
            assert_eq!(released(&mut readers, 1), [key]);
            readers.close(key.into(), at(0));
            assert!(frames.spare());
        }

        #[test]
        fn drops_the_queued_frames_when_the_last_complete_session_closes() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), 0..1);
            readers.close(key.into(), at(0));
            assert!(frames.spare());
            let later = opened(&mut readers, 0, 10);
            assert_eq!(released(&mut readers, 1), []);
            assert_eq!(taken(&mut readers, later), []);
        }

        #[test]
        fn drops_the_queued_frames_when_a_latest_session_takes_over_the_last() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let key = readers.open(named("a", 10), Start::At(live(0)), 0).key;
            readers.queue(&frames.frame(1), 0..1);
            let latest = readers.open_latest(Some("a".parse().expect("name")), at(0));
            assert_eq!(latest.replaced, Some(key.into()));
            assert!(frames.spare());
        }

        #[test]
        fn keeps_the_queued_frames_through_a_complete_takeover() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            readers.open(named("a", 10), Start::At(live(0)), 0);
            readers.queue(&frames.frame(1), 0..1);
            let new = readers
                .open(named("a", 10), resume(live(0)), 10 * CHARGE)
                .key;
            assert_eq!(released(&mut readers, 1), [new]);
            assert_eq!(taken(&mut readers, new), [1]);
        }

        #[test]
        fn take_gives_nothing_before_a_release() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), 0..1);
            assert_eq!(readers.take(key.into()).map(|frame| number(&frame)), None);
        }

        #[test]
        #[should_panic(expected = "live frame at seq 3..5 queued after seq 4")]
        fn queue_panics_on_a_seq_below_the_last_frame() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), 0..4);
            readers.queue(&frames.frame(2), 3..5);
        }

        #[test]
        #[should_panic(expected = "live frame at seq 5..6 queued after seq 10")]
        fn queue_panics_on_a_seq_below_the_live_seq() {
            let frames = Frames::new(1);
            Readers::new(10).queue(&frames.frame(1), 5..6);
        }

        #[test]
        #[should_panic(expected = "live frame at seq 3..3 queued after seq 4")]
        fn queue_panics_on_a_frame_with_no_samples_below_the_last_frame() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            readers.queue(&frames.frame(1), 0..4);
            readers.queue(&frames.frame(2), 3..3);
        }

        #[test]
        #[should_panic(expected = "live frame at seq 2..3 queued after seq 5")]
        fn queue_panics_on_a_seq_below_a_frame_with_no_samples() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            readers.queue(&frames.frame(1), 5..5);
            readers.queue(&frames.frame(2), 2..3);
        }

        #[test]
        #[should_panic(expected = "live frame at seq 5..3 ends before it starts")]
        fn queue_panics_on_a_seq_that_ends_before_it_starts() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), Range { start: 5, end: 3 });
        }

        #[test]
        fn take_gives_a_closed_session_nothing() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), 0..1);
            assert_eq!(released(&mut readers, 1), [key]);
            readers.close(key.into(), at(0));
            assert_eq!(taken(&mut readers, key), []);
        }
    }

    mod properties {
        use std::collections::BTreeSet;

        use proptest::prelude::*;

        use super::*;

        const NAMES: [&str; 3] = ["a", "b", "c"];

        #[derive(Clone, Copy, Debug)]
        enum Input {
            Open {
                name: Option<usize>,
                hold: i64,
                start: Position,
                resumed: bool,
                presented: Option<Position>,
            },
            Ack {
                session: usize,
                live: i64,
                backfill: i64,
                flipped: bool,
            },
            Close(usize),
            Flush,
            Advance,
        }

        /// The rules stated a second way: open sessions by key, and named readers that
        /// still hold after their close.
        #[derive(Default)]
        struct Model {
            open: BTreeMap<complete::Key, (Option<usize>, Position, i64)>,
            closed: BTreeMap<usize, (Position, i64, i64)>,
        }

        impl Model {
            fn forget(&mut self, now: i64) {
                self.closed.retain(|_, (_, hold, at)| *at + *hold > now);
            }

            fn open(
                &mut self,
                key: complete::Key,
                name: Option<usize>,
                hold: i64,
                start: Start,
            ) -> (Position, Option<Key>) {
                let session = self
                    .open
                    .iter()
                    .find(|(_, (n, ..))| name.is_some() && *n == name)
                    .map(|(key, (_, position, _))| (*key, *position));
                let mut stored = session.map(|(_, position)| position);
                if let Some((replaced, _)) = session {
                    self.open.remove(&replaced);
                }
                if let Some(name) = name {
                    stored = stored.or(self.closed.remove(&name).map(|(p, ..)| p));
                }
                let position = match start {
                    Start::At(position) => position,
                    Start::Resume {
                        presented,
                        otherwise,
                    } => {
                        let known = [presented, stored];
                        let first = known.iter().flatten().next();
                        Position {
                            live: first.map_or(otherwise.live, |p| p.live),
                            backfill: otherwise.backfill.map(|b| {
                                known
                                    .iter()
                                    .flatten()
                                    .find_map(|p| p.backfill)
                                    .unwrap_or(b)
                            }),
                        }
                    }
                };
                self.open.insert(key, (name, position, hold));
                (position, session.map(|(key, _)| Key::Complete(key)))
            }

            fn ack(&mut self, key: complete::Key, to: Position) -> Result<(), Error> {
                let (_, from, _) = self.open.get_mut(&key).expect("open in the model");
                // `None` sorts below every `Some`, so equal shapes compare by value.
                let forward = to.live >= from.live
                    && to.backfill.is_some() == from.backfill.is_some()
                    && to.backfill >= from.backfill;
                if !forward {
                    return Err(Error::Ack { from: *from, to });
                }
                *from = to;
                Ok(())
            }

            fn close(&mut self, key: complete::Key, now: i64) {
                let (name, position, hold) = self.open.remove(&key).expect("open");
                if let Some(name) = name {
                    self.closed.insert(name, (position, hold, now));
                }
                self.forget(now);
            }

            fn floor(&self) -> Option<Position> {
                let open = self.open.values().map(|(_, p, _)| *p);
                let held: Vec<Position> =
                    open.chain(self.closed.values().map(|(p, ..)| *p)).collect();
                Some(Position {
                    live: held.iter().map(|p| p.live).min()?,
                    backfill: held.iter().filter_map(|p| p.backfill).min(),
                })
            }

            fn deadline(&self) -> Option<Stamp> {
                let ends = self.closed.values().map(|(_, hold, at)| *at + *hold);
                ends.min().map(at)
            }
        }

        fn position() -> impl Strategy<Value = Position> {
            (0..40_u64, proptest::option::of(0..40_u64))
                .prop_map(|(live, backfill)| Position { live, backfill })
        }

        fn input() -> impl Strategy<Value = Input> {
            let presented = proptest::option::of(position());
            prop_oneof![
                (
                    proptest::option::of(0..NAMES.len()),
                    0..30_i64,
                    position(),
                    any::<bool>(),
                    presented
                )
                    .prop_map(
                        |(name, hold, start, resumed, presented)| {
                            Input::Open {
                                name,
                                hold,
                                start,
                                resumed,
                                presented,
                            }
                        }
                    ),
                (
                    any::<usize>(),
                    -2..8_i64,
                    -2..8_i64,
                    proptest::bool::weighted(0.1)
                )
                    .prop_map(|(session, live, backfill, flipped)| {
                        Input::Ack {
                            session,
                            live,
                            backfill,
                            flipped,
                        }
                    }),
                any::<usize>().prop_map(Input::Close),
                Just(Input::Flush),
                Just(Input::Advance),
            ]
        }

        /// Moves each path of `from` by a step, and adds or drops backfill when
        /// `flipped`.
        fn moved(from: Position, live: i64, backfill: i64, flipped: bool) -> Position {
            let backfill = match (from.backfill, flipped) {
                (Some(seq), false) => Some(seq.saturating_add_signed(backfill)),
                (Some(_), true) | (None, false) => None,
                (None, true) => Some(0),
            };
            Position {
                live: from.live.saturating_add_signed(live),
                backfill,
            }
        }

        /// Reports whether `a` holds at least what `b` holds: on each path, `None`
        /// holds nothing.
        fn holds_more(a: Option<Position>, b: Option<Position>) -> bool {
            let path = |x: Option<u64>, y: Option<u64>| match (x, y) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(x), Some(y)) => x <= y,
            };
            match (a, b) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(a), Some(b)) => a.live <= b.live && path(a.backfill, b.backfill),
            }
        }

        /// Applies one input to the readers and the model, and checks that they agree.
        fn apply(readers: &mut Readers, model: &mut Model, input: Input, now: i64) {
            match input {
                Input::Open {
                    name,
                    hold,
                    start,
                    resumed,
                    presented,
                } => {
                    let reader =
                        name.map_or(Reader::Unnamed, |n| named(NAMES[n], hold));
                    let start = if resumed {
                        Start::Resume {
                            presented,
                            otherwise: start,
                        }
                    } else {
                        Start::At(start)
                    };
                    let opened = readers.open(reader, start, 0);
                    let expected = model.open(opened.key, name, hold, start);
                    assert_eq!((opened.position, opened.replaced), expected);
                }
                Input::Ack {
                    session,
                    live,
                    backfill,
                    flipped,
                } if !model.open.is_empty() => {
                    let (key, (_, from, _)) = model
                        .open
                        .iter()
                        .nth(session % model.open.len())
                        .expect("in range");
                    let (key, to) = (*key, moved(*from, live, backfill, flipped));
                    assert_eq!(readers.ack(key, to), model.ack(key, to));
                }
                Input::Close(session) if !model.open.is_empty() => {
                    let key = *model
                        .open
                        .keys()
                        .nth(session % model.open.len())
                        .expect("in range");
                    readers.close(key.into(), at(now));
                    model.close(key, now);
                }
                Input::Flush => readers.flush(),
                Input::Ack { .. } | Input::Close(_) | Input::Advance => {}
            }
            assert_eq!(readers.floor(), model.floor());
            assert_eq!(readers.deadline(), model.deadline());
            assert!(readers.deadline().is_none_or(|end| at(now) < end));
        }

        fn check(steps: Vec<(i64, Input)>, flushed: bool) {
            let mut readers = Readers::new(0);
            let mut model = Model::default();
            let mut records = Vec::new();
            let mut now = 0;
            for (step, input) in steps {
                now += step;
                readers.advance(at(now));
                model.forget(now);
                apply(&mut readers, &mut model, input, now);
                records.extend(readers.records());
                dropped_closed(&mut readers, |key| model.open.contains_key(key));
            }
            if flushed {
                readers.flush();
                records.extend(readers.records());
            }
            let mut restored = Readers::restore(records, at(now), 0);
            for key in model.open.keys() {
                readers.close((*key).into(), at(now));
            }
            for later in [0, 1, 5, 10, 20, 40] {
                restored.advance(at(now + later));
                readers.advance(at(now + later));
                assert_eq!(restored.deadline(), readers.deadline());
                if flushed {
                    assert_eq!(restored.floor(), readers.floor());
                } else {
                    assert!(holds_more(restored.floor(), readers.floor()));
                }
            }
        }

        #[derive(Clone, Copy, Debug)]
        enum Spending {
            Grant(u64),
            Spend(u64),
        }

        /// Checks each spend against the credit rules stated a second way: the limit is
        /// the largest grant, the open's `first` among them, and the spent bytes are
        /// the sum of the frames.
        fn check_credit(first: u64, steps: Vec<Spending>) {
            let mut credit = Credit::new(first);
            let mut grants = vec![first];
            let mut frames = Vec::new();
            for step in steps {
                match step {
                    Spending::Grant(limit) => {
                        credit.grant(limit);
                        grants.push(limit);
                    }
                    Spending::Spend(bytes) => {
                        let limit = grants.iter().copied().max().unwrap_or(0);
                        let spent: u64 = frames.iter().sum();
                        let accepted = credit.spend(bytes);
                        assert_eq!(accepted, spent < limit);
                        if accepted {
                            frames.push(bytes);
                        }
                    }
                }
            }
        }

        #[derive(Clone, Copy, Debug)]
        enum Live {
            Open(u64),
            Close(usize),
            Ack(usize, u64),
            Grant(usize, u64),
            Queue { gap: u64, len: u64 },
            Release(u64),
            Take(usize),
        }

        /// A complete session of the live path stated a second way: the frames it got,
        /// by number, and how many it took.
        #[derive(Default)]
        struct Got {
            position: u64,
            behind: bool,
            limit: u64,
            spent: u64,
            frames: Vec<u64>,
            taken: usize,
        }

        /// The live path stated a second way: open sessions by key, and each queued
        /// frame.
        #[derive(Default)]
        struct Flows {
            open: BTreeMap<complete::Key, Got>,
            queued: Vec<Queued>,
        }

        /// A queued frame of the model: its number, its seq, and whether memory holds
        /// it.
        struct Queued {
            n: u64,
            seq: Range<u64>,
            held: bool,
        }

        /// Whether `seq` holds a sample at or past `position`.
        fn holds(seq: &Range<u64>, position: u64) -> bool {
            position.max(seq.start) < seq.end
        }

        impl Flows {
            /// The `i`th open session, wrapping, or `None` when none is open.
            fn pick(&self, i: usize) -> Option<complete::Key> {
                self.open.keys().nth(i % self.open.len().max(1)).copied()
            }

            fn got(&mut self, key: complete::Key) -> &mut Got {
                self.open.get_mut(&key).expect("the session is open")
            }

            fn queue(&mut self, n: u64, seq: Range<u64>) {
                let held = !self.open.is_empty();
                self.queued.push(Queued { n, seq, held });
            }

            /// Whether memory holds a frame with samples that no release gave yet.
            fn pending(&self) -> bool {
                let mut queued = self.queued.iter();
                queued.any(|queued| queued.held && !queued.seq.is_empty())
            }

            fn close(&mut self, key: complete::Key) {
                self.open.remove(&key);
                if self.open.is_empty() {
                    for queued in &mut self.queued {
                        queued.held = false;
                    }
                }
            }

            /// Whether memory no longer holds a frame with a sample at or past `start`.
            fn behind(&self, start: u64) -> bool {
                let mut queued = self.queued.iter();
                queued.any(|queued| !queued.held && holds(&queued.seq, start))
            }

            /// The end of the newest frame with samples that memory no longer holds, or
            /// 0. Sessions open near it, where `behind` changes.
            fn gone(&self) -> u64 {
                let gone = self.queued.iter().filter(|queued| !queued.held);
                let ends = gone.filter(|queued| !queued.seq.is_empty());
                ends.map(|queued| queued.seq.end).max().unwrap_or(0)
            }

            /// Returns the sessions woken, sorted.
            fn release(&mut self, durable: u64) -> Vec<complete::Key> {
                let idle: BTreeSet<complete::Key> = self
                    .open
                    .iter()
                    .filter(|(_, got)| got.taken == got.frames.len())
                    .map(|(&key, _)| key)
                    .collect();
                let held = self.queued.iter_mut().filter(|queued| queued.held);
                for queued in held.filter(|queued| queued.seq.end <= durable) {
                    queued.held = false;
                    for got in self.open.values_mut() {
                        if got.behind || !holds(&queued.seq, got.position) {
                            continue;
                        }
                        if got.spent >= got.limit {
                            got.behind = true;
                            continue;
                        }
                        got.spent += CHARGE;
                        got.frames.push(queued.n);
                    }
                }
                self.open
                    .iter()
                    .filter(|(key, got)| {
                        idle.contains(key) && got.taken < got.frames.len()
                    })
                    .map(|(&key, _)| key)
                    .collect()
            }

            fn take(&mut self, key: complete::Key) -> Option<u64> {
                let got = self.got(key);
                let n = got.frames.get(got.taken).copied();
                got.taken += usize::from(n.is_some());
                n
            }
        }

        fn live_input() -> impl Strategy<Value = Live> {
            prop_oneof![
                (0..8_u64).prop_map(Live::Open),
                any::<usize>().prop_map(Live::Close),
                (any::<usize>(), 0..4_u64).prop_map(|(i, ahead)| Live::Ack(i, ahead)),
                (any::<usize>(), 0..6 * CHARGE).prop_map(|(i, b)| Live::Grant(i, b)),
                (0..3_u64, 0..4_u64).prop_map(|(gap, len)| Live::Queue { gap, len }),
                (0..4_u64).prop_map(Live::Release),
                any::<usize>().prop_map(Live::Take),
            ]
        }

        /// Checks the live path against a model of the rules: a session gets each
        /// released frame with a sample at or past its position, in seq order, while it
        /// has credit, and none after the first it has no credit for. A session that
        /// starts at or below a sample no longer in memory gets none, and nothing is
        /// kept with no session open. A release wakes each session that had no frame waiting and now
        /// has one.
        ///
        /// The `n`th open takes `limits[n]`, or 0 past the end of `limits`.
        fn check_live(steps: Vec<Live>, limits: &[u64]) {
            let frames = Frames::new(steps.len());
            let mut readers = Readers::new(0);
            let mut model = Flows::default();
            let mut limits = limits.iter().copied();
            let mut end = 0;
            let mut made = 0;
            for step in steps {
                match step {
                    Live::Open(back) => {
                        let start = (model.gone() + 4).saturating_sub(back);
                        let limit = limits.next().unwrap_or(0);
                        let key = readers.open(
                            Reader::Unnamed,
                            Start::At(live(start)),
                            limit,
                        );
                        let got = Got {
                            position: start,
                            behind: model.behind(start),
                            limit,
                            ..Got::default()
                        };
                        model.open.insert(key.key, got);
                    }
                    Live::Close(i) => {
                        let Some(key) = model.pick(i) else { continue };
                        readers.close(key.into(), at(0));
                        model.close(key);
                    }
                    Live::Ack(i, ahead) => {
                        let Some(key) = model.pick(i) else { continue };
                        let got = model.got(key);
                        got.position += ahead;
                        readers.ack(key, live(got.position)).expect("forward");
                    }
                    Live::Grant(i, limit) => {
                        let Some(key) = model.pick(i) else { continue };
                        readers.grant(key, limit);
                        let got = model.got(key);
                        got.limit = got.limit.max(limit);
                    }
                    Live::Queue { gap, len } => {
                        made += 1;
                        let seq = end + gap..end + gap + len;
                        end = seq.end;
                        readers.queue(&frames.frame(made), seq.clone());
                        model.queue(made, seq);
                    }
                    Live::Release(back) => {
                        let durable = end.saturating_sub(back);
                        let mut woken = readers.release(durable).to_vec();
                        woken.sort_unstable();
                        assert_eq!(woken, model.release(durable));
                    }
                    Live::Take(i) => {
                        let Some(key) = model.pick(i) else { continue };
                        let taken =
                            readers.take(key.into()).map(|frame| number(&frame));
                        assert_eq!(taken, model.take(key));
                    }
                }
                assert_eq!(readers.pending(), model.pending());
                dropped_closed(&mut readers, |key| model.open.contains_key(key));
            }
        }

        proptest! {
            #[test]
            fn follow_the_credit_rules(
                steps in proptest::collection::vec(
                    prop_oneof![
                        (0..200_u64).prop_map(Spending::Grant),
                        (0..40_u64).prop_map(Spending::Spend),
                    ],
                    0..120,
                ),
            ) {
                check_credit(0, steps);
            }

            #[test]
            fn follow_the_credit_rules_from_the_limit_of_the_open(
                first in 0..200_u64,
                steps in proptest::collection::vec(
                    prop_oneof![
                        (0..200_u64).prop_map(Spending::Grant),
                        (0..40_u64).prop_map(Spending::Spend),
                    ],
                    0..120,
                ),
            ) {
                check_credit(first, steps);
            }

            #[test]
            fn follow_the_live_rules(
                steps in proptest::collection::vec(live_input(), 0..80),
            ) {
                check_live(steps, &[]);
            }

            #[test]
            fn follow_the_live_rules_from_the_limit_of_the_open(
                steps in proptest::collection::vec(live_input(), 0..80),
                limits in proptest::collection::vec(0..6 * CHARGE, 0..80),
            ) {
                check_live(steps, &limits);
            }

            #[test]
            fn follow_the_delivery_rules(
                steps in proptest::collection::vec((0..10_i64, input()), 0..60),
                flushed in any::<bool>(),
            ) {
                check(steps, flushed);
            }
        }
    }
}

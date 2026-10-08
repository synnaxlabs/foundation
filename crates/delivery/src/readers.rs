//! The readers of one index at its home.

pub mod complete;
pub mod latest;

use std::collections::{BTreeMap, VecDeque};
use std::iter;
use std::ops::Range;
use std::sync::Arc;

use types::frame::Frame;
use types::frame::key_set::KeySet;
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

/// What a session gets from [`Readers::take`].
#[derive(Debug)]
pub enum Next {
    /// The session's next frame.
    Frame(Frame),
    /// No frame waits, the next frame waits for a grant, or the session is closed.
    Empty,
    /// The complete session missed a live frame, so it gets no later frame. It comes
    /// after the frames before the miss, on this and each later call. A latest session
    /// never gives it.
    Behind,
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
    /// The live frames not yet on disk, with their seq, oldest first, after the first
    /// `held`. Empty when no complete session is open.
    queue: VecDeque<(Frame, Range<u64>)>,
    /// The released frames at the front of `queue`, from the first that a session
    /// waits for a grant on. Zero when no session waits.
    held: usize,
    /// The complete sessions that wait for a grant.
    owing: usize,
    /// The key set of each run of queued frames of one key set, oldest first. The
    /// front may be the set of frames already released. Each frame in `queue` has its
    /// set here, in the same order.
    sets: VecDeque<Arc<KeySet>>,
    /// The end of the last live frame queued, or the live seq at start.
    queued: u64,
    /// The end of the newest live frame with samples that was released, or dropped
    /// with no complete session open. A session that opens below it is behind.
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
    cost: complete::Cost,
    /// The session missed a frame, so it gets no later frame.
    behind: bool,
    /// The frames released to the session and not taken, oldest first.
    waiting: VecDeque<Frame>,
    /// The next frame that waits for a grant, after the frames of `waiting`.
    owed: Option<Owed>,
}

/// A frame held in [`Readers::queue`] that a session waits for a grant on.
#[derive(Clone, Copy, Debug)]
struct Owed {
    /// Its index in `queue`.
    frame: usize,
    /// The index in `sets` of its key set.
    set: usize,
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
            held: 0,
            owing: 0,
            sets: VecDeque::new(),
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
    /// first grant, that `charge` charges for each frame. A named reader's open session
    /// in either mode is taken over. A session that starts below a live frame that
    /// memory no longer holds gets no live frame: [`Readers::take`] gives
    /// [`Next::Behind`] at once, and no [`Readers::release`] names it.
    ///
    /// # Panics
    ///
    /// If the reader's hold is negative.
    pub fn open(
        &mut self,
        reader: Reader,
        start: Start,
        limit_bytes: u64,
        charge: complete::Charge,
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
            cost: complete::Cost::new(charge),
            behind: position.live < self.released,
            waiting: VecDeque::with_capacity(WAITING),
            owed: None,
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

    /// Raises the session's credit to `limit_bytes` since it opened. A frame that
    /// waits for a grant can be taken while the bytes the session spent are below its
    /// credit, and no call names the session for it: take after the grant. A limit
    /// that is not higher than the current one changes nothing, and so does a grant to
    /// a closed session: a grant can arrive after its session closes.
    ///
    /// # Panics
    ///
    /// If this `Readers` never gave `key`.
    pub fn grant(&mut self, key: complete::Key, limit_bytes: u64) {
        if let Some(i) = self.find(key) {
            self.flows[i].credit.grant(limit_bytes);
        }
    }

    /// Queues `frame`, of key set `set`, stored on the live path with the samples
    /// `seq`, for the complete sessions. [`Readers::release`] gives it to them once it
    /// is on disk. Keeps nothing when no complete session is open. A frame with no
    /// samples reaches no session.
    ///
    /// # Panics
    ///
    /// If `set` is not the frame's key set, `seq` ends before it starts, or `seq`
    /// starts below the end of an earlier live frame or below the `live` given to
    /// [`Readers::new`] or [`Readers::restore`].
    pub fn queue(&mut self, frame: &Frame, set: &Arc<KeySet>, seq: Range<u64>) {
        assert!(
            set.key() == frame.key_set(),
            "the frame is of key set {} and `set` is key set {}",
            frame.key_set().get(),
            set.key().get()
        );
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
            if self.sets.back().is_none_or(|last| last.key() != set.key()) {
                self.sets.push_back(Arc::clone(set));
            }
            self.queue.push_back((frame.clone(), seq));
        }
    }

    /// Gives each queued frame that ends at or below `durable`, the first live seq not
    /// on disk, to each complete session whose position it ends past, in seq order. A
    /// frame that finds the session's credit spent waits for a grant, and so does each
    /// later frame. A frame that still waits for a grant when a later call releases a
    /// frame is a miss: it drops, and the session gets no later frame. Returns the
    /// complete sessions that had no frame to take and now have one, or that missed a
    /// frame now and have none to take, each once: wake them.
    #[must_use]
    pub fn release(&mut self, durable: u64) -> &[complete::Key] {
        self.woken_complete.clear();
        if self.owing != 0 {
            let releases = self.queue.get(self.held);
            if releases.is_none_or(|(_, seq)| seq.end > durable) {
                return &self.woken_complete;
            }
            self.miss_owed();
        }
        while let Some((frame, seq)) =
            self.queue.pop_front_if(|(_, seq)| seq.end <= durable)
        {
            let stale = |set: &mut Arc<KeySet>| set.key() != frame.key_set();
            while self.sets.pop_front_if(stale).is_some() {}
            let at = Owed { frame: 0, set: 0 };
            let (last, owing) = give(
                &self.complete,
                &mut self.flows,
                &mut self.woken_complete,
                (&frame, &self.sets[0], &seq),
                at,
            );
            self.released = seq.end;
            if owing == 0 {
                if let Some(waiting) = last {
                    waiting.push_back(frame);
                }
                continue;
            }
            if let Some(waiting) = last {
                waiting.push_back(frame.clone());
            }
            self.owing = owing;
            self.queue.push_front((frame, seq));
            self.hold(durable);
            break;
        }
        &self.woken_complete
    }

    /// Gives the frames after the front of `queue` that end at or below `durable`, and
    /// keeps them all, from the front, for the sessions that wait for a grant.
    fn hold(&mut self, durable: u64) {
        let mut at = Owed { frame: 1, set: 0 };
        while let Some((frame, seq)) = self.queue.get(at.frame)
            && seq.end <= durable
        {
            while self.sets[at.set].key() != frame.key_set() {
                at.set += 1;
            }
            let (last, owing) = give(
                &self.complete,
                &mut self.flows,
                &mut self.woken_complete,
                (frame, &self.sets[at.set], seq),
                at,
            );
            if let Some(waiting) = last {
                waiting.push_back(frame.clone());
            }
            self.owing += owing;
            self.released = seq.end;
            at.frame += 1;
        }
        self.held = at.frame;
    }

    /// Makes each session that waits for a grant miss its frame, adds those with no
    /// frame to take to the sessions to wake, and drops the held frames.
    fn miss_owed(&mut self) {
        for (session, flow) in iter::zip(&self.complete, &mut self.flows) {
            if flow.owed.take().is_none() {
                continue;
            }
            flow.behind = true;
            // A session with frames to take sees the miss after it takes them.
            if flow.waiting.is_empty() {
                self.woken_complete.push(session.key);
            }
        }
        self.owing = 0;
        self.drop_held();
    }

    fn drop_held(&mut self) {
        self.queue.drain(..self.held);
        self.held = 0;
    }

    /// Whether a queued live frame waits to be on disk. While one does, call
    /// [`Readers::release`] after each commit.
    #[must_use]
    pub fn pending(&self) -> bool {
        self.queue.len() > self.held
    }

    /// Takes the session's next frame. A latest session has at most one; a complete
    /// session has the frames that [`Readers::release`] gave it, in seq order, a
    /// frame that waits for a grant only while the bytes the session spent are below
    /// its credit, then [`Next::Behind`] once it missed a live frame. A session can
    /// miss one at its open, which no `release` names.
    ///
    /// # Panics
    ///
    /// If this `Readers` never gave `key`.
    #[must_use]
    pub fn take(&mut self, key: Key) -> Next {
        match key {
            Key::Complete(key) => {
                let Some(i) = self.find(key) else {
                    return Next::Empty;
                };
                let flow = &mut self.flows[i];
                match flow.waiting.pop_front() {
                    Some(frame) => Next::Frame(frame),
                    None => self.take_owed(i),
                }
            }
            Key::Latest(key) => self.take_latest(key).map_or(Next::Empty, Next::Frame),
        }
    }

    /// Takes the next frame of the complete session at `i`, which has no frame
    /// waiting: the frame that waits for a grant, if its credit covers it.
    // Out of line, so that `take` stays small: inline, it made each take slower.
    #[inline(never)]
    fn take_owed(&mut self, i: usize) -> Next {
        let flow = &mut self.flows[i];
        let Some(owed) = &mut flow.owed else {
            return if flow.behind {
                Next::Behind
            } else {
                Next::Empty
            };
        };
        let (frame, _) = &self.queue[owed.frame];
        while self.sets[owed.set].key() != frame.key_set() {
            owed.set += 1;
        }
        let set = &self.sets[owed.set];
        if !flow
            .credit
            .spend(flow.cost.charge(frame, set, frame.charge()))
        {
            return Next::Empty;
        }
        let frame = frame.clone();
        owed.frame += 1;
        if owed.frame == self.held {
            flow.owed = None;
            self.paid();
        }
        Next::Frame(frame)
    }

    /// Counts that a session no longer waits for a grant. The last drops the held
    /// frames.
    fn paid(&mut self) {
        self.owing -= 1;
        if self.owing == 0 {
            self.drop_held();
        }
    }

    /// Ends a session that holds nothing after it closes: an unnamed one in either
    /// mode, or a named latest one. A waiting frame does not go out. The last complete
    /// session drops the queued frames. A close of a closed session changes nothing: a
    /// close can arrive after a takeover.
    ///
    /// # Panics
    ///
    /// If this `Readers` never gave `key`, or the session is open and is a named
    /// complete session.
    pub fn close(&mut self, key: Key) {
        match key {
            Key::Complete(key) => {
                if let Some(i) = self.find(key) {
                    let session = self.end(i);
                    assert!(
                        session.name().is_none(),
                        "complete session {key} is named: `close_named` ends it"
                    );
                }
            }
            Key::Latest(key) => self.close_latest(key),
        }
    }

    /// Ends a named complete session at `now`. The reader holds its position until
    /// `hold` after `now`. Else as [`Readers::close`].
    ///
    /// # Panics
    ///
    /// If this `Readers` never gave `key`, or the session is open and is unnamed.
    pub fn close_named(&mut self, key: complete::Key, now: Stamp) {
        if let Some(i) = self.find(key) {
            self.end_named(i, now);
        }
    }

    /// Ends the named complete session at `i` at `now`. Its reader holds from `now`.
    fn end_named(&mut self, i: usize, now: Stamp) {
        let session = self.end(i);
        let Reader::Named { name, hold } = session.reader else {
            panic!(
                "complete session {} is unnamed: `close` ends it",
                session.key
            );
        };
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

    /// Removes the complete session at `i`. The last one drops the queued frames.
    fn end(&mut self, i: usize) -> Session {
        let session = self.remove(i);
        if self.complete.is_empty() {
            if let Some((_, seq)) = self.queue.back() {
                self.released = seq.end;
            }
            self.queue.clear();
            self.sets.clear();
        }
        session
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
    fn replace(&mut self, name: &Name, now: Stamp) -> Option<Key> {
        let Some(i) = self.named(name) else {
            return self.remove_latest(name).map(Key::from);
        };
        let key = self.complete[i].key;
        self.end_named(i, now);
        Some(key.into())
    }

    /// The named reader's open complete session.
    fn named(&self, name: &Name) -> Option<usize> {
        self.complete.iter().position(|s| s.name() == Some(name))
    }

    /// The open complete session `key`, or `None` when it closed. Panics on a key
    /// never given.
    fn find(&self, key: complete::Key) -> Option<usize> {
        if key.0 >= self.next_complete {
            never_open(key.into());
        }
        self.complete.binary_search_by_key(&key, |s| s.key).ok()
    }

    fn remove(&mut self, i: usize) -> Session {
        if self.flows.remove(i).owed.is_some() {
            self.paid();
        }
        self.complete.remove(i)
    }
}

/// Gives `frame`, of key set `set`, released with the samples `seq`, to each of
/// `flows` that gets it and has credit for it, and adds each that had no frame to take
/// to `woken`. A flow that has no credit for it waits for a grant from `at`. Returns
/// the waiting frames of the last flow that gets it, without the frame, so that the
/// caller can move it there, and the count of flows that now wait for a grant.
fn give<'a>(
    complete: &[Session],
    flows: &'a mut [Flow],
    woken: &mut Vec<complete::Key>,
    (frame, set, seq): (&Frame, &KeySet, &Range<u64>),
    at: Owed,
) -> (Option<&'a mut VecDeque<Frame>>, usize) {
    let whole = frame.charge();
    let mut last: Option<&mut VecDeque<Frame>> = None;
    let mut owing = 0;
    for (session, flow) in iter::zip(complete, flows) {
        if flow.behind || flow.owed.is_some() || seq.end <= session.position.live {
            continue;
        }
        if !flow.credit.spend(flow.cost.charge(frame, set, whole)) {
            flow.owed = Some(at);
            owing += 1;
            continue;
        }
        if flow.waiting.is_empty() {
            woken.push(session.key);
        }
        if let Some(waiting) = last.replace(&mut flow.waiting) {
            waiting.push_back(frame.clone());
        }
    }
    (last, owing)
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
    use types::channel;
    use types::frame::key_set::{Group, Interner, KeySet};
    use types::frame::{Draft, Form, Path};

    use super::*;
    use crate::complete::Charge;

    /// The charge of each frame from [`Frames`]: a 64-byte block and its header.
    const CHARGE: u64 = 128;

    /// Frames of one index with no data channels. Frame `n` holds `n` in its index
    /// series.
    pub(super) struct Frames {
        pool: block::Pool,
        pub(super) set: Arc<KeySet>,
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

    /// Each frame that the session `key` takes now, then the [`Next`] after them.
    fn drain(readers: &mut Readers, key: impl Into<Key>) -> (Vec<Frame>, Next) {
        let key = key.into();
        let mut frames = Vec::new();
        loop {
            match readers.take(key) {
                Next::Frame(frame) => frames.push(frame),
                end @ (Next::Empty | Next::Behind) => return (frames, end),
            }
        }
    }

    /// The number of each frame that the session `key` takes before
    /// [`Next::Empty`].
    ///
    /// # Panics
    ///
    /// If the session gets [`Next::Behind`].
    #[track_caller]
    pub(super) fn taken(readers: &mut Readers, key: impl Into<Key>) -> Vec<u64> {
        let (frames, end) = drain(readers, key);
        let numbers = frames.iter().map(number).collect();
        assert!(matches!(end, Next::Empty), "behind after {numbers:?}");
        numbers
    }

    /// The number of each frame that the session `key` takes before
    /// [`Next::Behind`].
    ///
    /// # Panics
    ///
    /// If the session gets [`Next::Empty`].
    #[track_caller]
    pub(super) fn missed(readers: &mut Readers, key: impl Into<Key>) -> Vec<u64> {
        let (frames, end) = drain(readers, key);
        let numbers = frames.iter().map(number).collect();
        assert!(matches!(end, Next::Behind), "not behind after {numbers:?}");
        numbers
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
        let key = readers
            .open(named(name, 10), Start::At(position), 0, Charge::Whole)
            .key;
        readers.close_named(key, at(closed));
        drained(readers);
    }

    /// Makes each call on the closed session `key`, and asserts that they do not
    /// change the floor, the deadline, or whether a frame is pending. The caller
    /// checks its records, open sessions, key counters, and queued end after it: only
    /// an open or a panic of `queue` shows the last two.
    pub(super) fn dropped(readers: &mut Readers, key: Key) {
        let before = (readers.floor(), readers.deadline(), readers.pending());
        if let Key::Complete(key) = key {
            readers.grant(key, u64::MAX);
            assert_eq!(readers.ack(key, live(0)), Ok(()));
        }
        assert!(matches!(readers.take(key), Next::Empty));
        readers.close(key);
        if let Key::Complete(key) = key {
            readers.close_named(key, at(i64::MAX));
        }
        assert_eq!(
            (readers.floor(), readers.deadline(), readers.pending()),
            before
        );
    }

    mod open {
        use super::*;

        #[test]
        fn starts_at_the_given_position() {
            let mut readers = Readers::new(0);
            let opened =
                readers.open(Reader::Unnamed, Start::At(live(5)), 0, Charge::Whole);
            assert_eq!(opened.position, live(5));
            assert_eq!(opened.replaced, None);
        }

        #[test]
        fn gives_each_session_its_own_key() {
            let mut readers = Readers::new(0);
            let first = readers
                .open(Reader::Unnamed, Start::At(live(0)), 0, Charge::Whole)
                .key;
            readers.close(first.into());
            let second = readers
                .open(Reader::Unnamed, Start::At(live(0)), 0, Charge::Whole)
                .key;
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
            assert_eq!(
                readers
                    .open(named("a", 10), start, 0, Charge::Whole)
                    .position,
                live(9)
            );
        }

        #[test]
        fn resume_falls_back_to_the_position_at_this_home() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(7), 1);
            let opened =
                readers.open(named("a", 10), resume(live(0)), 0, Charge::Whole);
            assert_eq!(opened.position, live(7));
        }

        #[test]
        fn resume_falls_back_to_otherwise() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(7), 1);
            let opened =
                readers.open(named("b", 10), resume(both(2, 1)), 0, Charge::Whole);
            assert_eq!(opened.position, both(2, 1));
        }

        #[test]
        fn resume_falls_back_per_path() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(7), 1);
            let opened =
                readers.open(named("a", 10), resume(both(0, 2)), 0, Charge::Whole);
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
            assert_eq!(
                readers
                    .open(named("a", 10), start, 0, Charge::Whole)
                    .position,
                both(9, 5)
            );
        }

        #[test]
        fn resume_takes_backfill_from_this_home_when_the_presented_has_none() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", both(7, 3), 1);
            let start = Start::Resume {
                presented: Some(live(9)),
                otherwise: both(0, 0),
            };
            assert_eq!(
                readers
                    .open(named("a", 10), start, 0, Charge::Whole)
                    .position,
                both(9, 3)
            );
        }

        #[test]
        fn resume_follows_backfill_only_when_otherwise_does() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", both(7, 3), 1);
            let opened =
                readers.open(named("a", 10), resume(live(0)), 0, Charge::Whole);
            assert_eq!(opened.position, live(7));
        }

        #[test]
        fn at_ignores_the_position_at_this_home() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(7), 1);
            let opened =
                readers.open(named("a", 10), Start::At(live(2)), 0, Charge::Whole);
            assert_eq!(opened.position, live(2));
        }
    }

    mod takeover {
        use super::*;

        #[test]
        fn continues_from_the_old_session() {
            let mut readers = Readers::new(0);
            let old = readers
                .open(named("a", 10), resume(live(0)), 0, Charge::Whole)
                .key;
            readers.ack(old, live(5)).expect("forward");
            let opened =
                readers.open(named("a", 10), resume(live(0)), 0, Charge::Whole);
            assert_eq!(opened.position, live(5));
            assert_eq!(opened.replaced, Some(old.into()));
        }

        #[test]
        fn takes_the_presented_position() {
            let mut readers = Readers::new(0);
            let old = readers
                .open(named("a", 10), resume(live(0)), 0, Charge::Whole)
                .key;
            readers.ack(old, live(5)).expect("forward");
            let start = Start::Resume {
                presented: Some(live(8)),
                otherwise: live(0),
            };
            assert_eq!(
                readers
                    .open(named("a", 10), start, 0, Charge::Whole)
                    .position,
                live(8)
            );
        }

        #[test]
        fn closes_the_old_session() {
            let mut readers = Readers::new(0);
            let old = readers
                .open(named("a", 10), resume(live(0)), 0, Charge::Whole)
                .key;
            let new = readers
                .open(named("a", 10), resume(live(0)), 0, Charge::Whole)
                .key;
            readers.ack(new, live(5)).expect("forward");
            assert_eq!(readers.ack(old, live(1)), Ok(()));
            assert_eq!(readers.floor(), Some(live(5)));
        }

        #[test]
        fn never_happens_to_unnamed_readers() {
            let mut readers = Readers::new(0);
            readers.open(Reader::Unnamed, Start::At(live(0)), 0, Charge::Whole);
            let opened =
                readers.open(Reader::Unnamed, Start::At(live(0)), 0, Charge::Whole);
            assert_eq!(opened.replaced, None);
        }
    }

    mod ack {
        use super::*;

        fn opened(position: Position) -> (Readers, complete::Key) {
            let mut readers = Readers::new(0);
            let key = readers
                .open(named("a", 10), Start::At(position), 0, Charge::Whole)
                .key;
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
            readers.close_named(key, at(1));
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
            let key = readers
                .open(named("a", 10), Start::At(live(2)), 0, Charge::Whole)
                .key;
            readers.close_named(key, at(1));
            drained(&mut readers);
            dropped(&mut readers, key.into());
            assert_eq!(drained(&mut readers), []);
            assert_eq!(readers.floor(), Some(live(2)));
            assert_eq!(readers.deadline(), Some(at(11)));
        }

        #[test]
        fn after_a_close_leaves_an_open_reader_with_nothing_to_flush() {
            let mut readers = Readers::new(0);
            let old = readers
                .open(named("a", 10), Start::At(live(2)), 0, Charge::Whole)
                .key;
            readers.close_named(old, at(1));
            readers.open(named("b", 10), Start::At(live(3)), 0, Charge::Whole);
            drained(&mut readers);
            dropped(&mut readers, old.into());
            readers.flush();
            assert_eq!(drained(&mut readers), []);
        }

        #[test]
        fn after_a_takeover_by_a_complete_session_leaves_it_open() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let old = readers
                .open(named("a", 10), resume(live(0)), CHARGE, Charge::Whole)
                .key;
            let new = readers
                .open(named("a", 10), resume(live(0)), CHARGE, Charge::Whole)
                .key;
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            drained(&mut readers);
            dropped(&mut readers, old.into());
            assert_eq!(drained(&mut readers), []);
            assert_eq!(readers.release(1), [new]);
            assert_eq!(taken(&mut readers, new), [1]);
            assert_eq!(readers.ack(new, live(1)), Ok(()));
        }

        #[test]
        fn after_a_takeover_by_a_latest_session_leaves_it_open() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let old = readers
                .open(named("a", 10), resume(live(0)), CHARGE, Charge::Whole)
                .key;
            let name = "a".parse().expect("valid name");
            let new = readers.open_named_latest(name, at(1)).key;
            assert_eq!(readers.put(frames.frame(1)), [new]);
            drained(&mut readers);
            dropped(&mut readers, old.into());
            assert_eq!(drained(&mut readers), []);
            assert_eq!(taken(&mut readers, new), [1]);
        }

        /// Queues frame 1 of `frames` at seq 0..2 to a named and an unnamed session,
        /// closes the named one at 1, and makes each late call on it.
        fn closed_with_a_queue(frames: &Frames) -> Readers {
            let mut readers = Readers::new(0);
            let old = readers
                .open(named("a", 10), Start::At(live(0)), 0, Charge::Whole)
                .key;
            readers.open(Reader::Unnamed, Start::At(live(0)), 0, Charge::Whole);
            readers.queue(&frames.frame(1), &frames.set, 0..2);
            readers.close_named(old, at(1));
            dropped(&mut readers, old.into());
            readers
        }

        #[test]
        fn after_a_close_leaves_the_key_counters() {
            let mut readers = closed_with_a_queue(&Frames::new(1));
            let next = readers
                .open(Reader::Unnamed, Start::At(live(0)), 0, Charge::Whole)
                .key;
            assert_eq!(next, complete::Key(2));
            assert_eq!(readers.open_latest().key, latest::Key(0));
        }

        #[test]
        #[should_panic(expected = "live frame at seq 1..2 queued after seq 2")]
        fn after_a_close_keeps_the_end_of_the_queued_frames() {
            let frames = Frames::new(2);
            let mut readers = closed_with_a_queue(&frames);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
        }

        #[test]
        #[should_panic(expected = "complete session 1 was never open")]
        fn take_panics_on_a_session_never_open() {
            let mut readers = Readers::new(0);
            let _ = readers.open(Reader::Unnamed, Start::At(live(0)), 0, Charge::Whole);
            drop(readers.take(complete::Key(1).into()));
        }

        #[test]
        #[should_panic(expected = "complete session 1 was never open")]
        fn close_panics_on_a_session_never_open() {
            let mut readers = Readers::new(0);
            let _ = readers.open(Reader::Unnamed, Start::At(live(0)), 0, Charge::Whole);
            readers.close(complete::Key(1).into());
        }

        #[test]
        #[should_panic(expected = "complete session 1 was never open")]
        fn close_named_panics_on_a_session_never_open() {
            let mut readers = Readers::new(0);
            let _ = readers.open(named("a", 10), Start::At(live(0)), 0, Charge::Whole);
            readers.close_named(complete::Key(1), at(0));
        }
    }

    mod close {
        use super::*;

        #[test]
        #[should_panic(expected = "complete session 0 is named: `close_named` ends it")]
        fn panics_on_an_open_named_complete_session() {
            let mut readers = Readers::new(0);
            let key = readers
                .open(named("a", 10), Start::At(live(0)), 0, Charge::Whole)
                .key;
            readers.close(key.into());
        }

        #[test]
        #[should_panic(expected = "complete session 0 is unnamed: `close` ends it")]
        fn named_panics_on_an_open_unnamed_session() {
            let mut readers = Readers::new(0);
            let key = readers
                .open(Reader::Unnamed, Start::At(live(0)), 0, Charge::Whole)
                .key;
            readers.close_named(key, at(0));
        }
    }

    mod hold {
        use super::*;

        #[test]
        fn lasts_while_the_session_is_open() {
            let mut readers = Readers::new(0);
            readers.open(Reader::Unnamed, Start::At(live(3)), 0, Charge::Whole);
            readers.advance(at(i64::MAX));
            assert_eq!(readers.floor(), Some(live(3)));
            assert_eq!(readers.deadline(), None);
        }

        #[test]
        fn of_an_unnamed_reader_ends_at_the_close() {
            let mut readers = Readers::new(0);
            let key = readers
                .open(Reader::Unnamed, Start::At(live(3)), 0, Charge::Whole)
                .key;
            readers.close(key.into());
            assert_eq!(readers.floor(), None);
        }

        #[test]
        fn of_a_named_reader_ends_after_the_close() {
            let mut readers = Readers::new(0);
            let key = readers
                .open(named("a", 10), Start::At(live(0)), 0, Charge::Whole)
                .key;
            readers.ack(key, live(4)).expect("forward");
            readers.close_named(key, at(5));
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
            let key = readers
                .open(named("a", 0), Start::At(live(0)), 0, Charge::Whole)
                .key;
            readers.close_named(key, at(5));
            assert_eq!(readers.floor(), None);
        }

        #[test]
        #[should_panic(expected = "reader hold is negative: -3ns")]
        fn panics_when_negative() {
            Readers::new(0).open(named("a", -3), Start::At(live(0)), 0, Charge::Whole);
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
            let key = readers
                .open(named("a", 10), Start::At(live(0)), 0, Charge::Whole)
                .key;
            readers.close_named(key, at(i64::MAX - 1));
            assert_eq!(readers.deadline(), Some(at(i64::MAX)));
        }

        #[test]
        fn is_cancelled_by_a_reopen() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(4), 5);
            let opened =
                readers.open(named("a", 10), resume(live(0)), 0, Charge::Whole);
            assert_eq!(opened.position, live(4));
            assert_eq!(readers.deadline(), None);
        }

        #[test]
        fn that_ended_leaves_no_position_to_resume() {
            let mut readers = Readers::new(0);
            left(&mut readers, "a", live(4), 5);
            readers.advance(at(15));
            let opened =
                readers.open(named("a", 10), resume(live(9)), 0, Charge::Whole);
            assert_eq!(opened.position, live(9));
        }
    }

    mod floor {
        use super::*;

        #[test]
        fn is_the_lowest_position_on_each_path() {
            let mut readers = Readers::new(0);
            readers.open(Reader::Unnamed, Start::At(both(5, 9)), 0, Charge::Whole);
            readers.open(Reader::Unnamed, Start::At(both(7, 2)), 0, Charge::Whole);
            readers.open(Reader::Unnamed, Start::At(live(3)), 0, Charge::Whole);
            assert_eq!(readers.floor(), Some(both(3, 2)));
        }

        #[test]
        fn has_no_backfill_when_no_reader_records() {
            let mut readers = Readers::new(0);
            readers.open(Reader::Unnamed, Start::At(live(5)), 0, Charge::Whole);
            readers.open(Reader::Unnamed, Start::At(live(7)), 0, Charge::Whole);
            assert_eq!(readers.floor(), Some(live(5)));
        }

        #[test]
        fn is_none_without_readers() {
            assert_eq!(Readers::new(0).floor(), None);
        }

        #[test]
        fn goes_down_when_a_session_opens_below_it() {
            let mut readers = Readers::new(0);
            readers.open(Reader::Unnamed, Start::At(live(5)), 0, Charge::Whole);
            readers.open(Reader::Unnamed, Start::At(live(2)), 0, Charge::Whole);
            assert_eq!(readers.floor(), Some(live(2)));
        }
    }

    mod records {
        use super::*;

        #[test]
        fn are_written_at_once_when_a_named_reader_opens() {
            let mut readers = Readers::new(0);
            readers.open(named("a", 10), Start::At(live(3)), 0, Charge::Whole);
            assert_eq!(drained(&mut readers), [record("a", live(3), 10, None)]);
        }

        #[test]
        fn are_written_at_once_when_a_named_reader_closes() {
            let mut readers = Readers::new(0);
            let key = readers
                .open(named("a", 10), Start::At(live(3)), 0, Charge::Whole)
                .key;
            readers.ack(key, live(5)).expect("forward");
            readers.close_named(key, at(7));
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
            let key = readers
                .open(named("a", 10), Start::At(live(3)), 0, Charge::Whole)
                .key;
            drained(&mut readers);
            readers.close_named(key, at(7));
            assert_eq!(drained(&mut readers), [record("a", live(3), 10, Some(7))]);
        }

        #[test]
        fn are_written_at_once_on_a_takeover() {
            let mut readers = Readers::new(0);
            let old = readers
                .open(named("a", 10), Start::At(live(3)), 0, Charge::Whole)
                .key;
            readers.ack(old, live(5)).expect("forward");
            drained(&mut readers);
            readers.open(named("a", 20), resume(live(0)), 0, Charge::Whole);
            assert_eq!(drained(&mut readers), [record("a", live(5), 20, None)]);
        }

        #[test]
        fn are_written_on_flush_only_for_changed_positions() {
            let mut readers = Readers::new(0);
            let a = readers
                .open(named("a", 10), Start::At(live(0)), 0, Charge::Whole)
                .key;
            readers.open(named("b", 10), Start::At(live(0)), 0, Charge::Whole);
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
            let key = readers
                .open(Reader::Unnamed, Start::At(live(0)), 0, Charge::Whole)
                .key;
            readers.ack(key, live(2)).expect("forward");
            readers.flush();
            readers.close(key.into());
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
            let opened =
                readers.open(named("a", 10), resume(live(0)), 0, Charge::Whole);
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
            let _ = readers.open(Reader::Unnamed, Start::At(live(0)), 0, Charge::Whole);
            readers.grant(complete::Key(1), 10);
        }

        #[test]
        #[should_panic(expected = "complete session 2 was never open")]
        fn grant_panics_on_a_key_past_the_next() {
            let mut readers = Readers::new(0);
            let _ = readers.open(Reader::Unnamed, Start::At(live(0)), 0, Charge::Whole);
            readers.grant(complete::Key(2), 10);
        }
    }

    mod release {
        use super::*;

        /// Opens a complete session at live seq `seq` with credit for `frames` frames.
        fn opened(readers: &mut Readers, seq: u64, frames: u64) -> complete::Key {
            readers
                .open(
                    Reader::Unnamed,
                    Start::At(live(seq)),
                    frames * CHARGE,
                    Charge::Whole,
                )
                .key
        }

        fn released(readers: &mut Readers, durable: u64) -> Vec<complete::Key> {
            readers.release(durable).to_vec()
        }

        #[test]
        fn pends_only_while_a_frame_for_a_complete_session_waits_for_the_disk() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            readers.queue(&frames.frame(1), &frames.set, 0..2);
            assert!(!readers.pending());
            let key = opened(&mut readers, 2, 10);
            readers.queue(&frames.frame(2), &frames.set, 2..4);
            assert!(readers.pending());
            assert_eq!(released(&mut readers, 4), [key]);
            assert!(!readers.pending());
            readers.queue(&frames.frame(3), &frames.set, 4..4);
            assert!(!readers.pending());
            readers.queue(&frames.frame(4), &frames.set, 4..6);
            readers.close(key.into());
            assert!(!readers.pending());
        }

        #[test]
        fn gives_only_frames_on_disk() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), &frames.set, 0..2);
            readers.queue(&frames.frame(2), &frames.set, 2..5);
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
            readers.queue(&frames.frame(1), &frames.set, 0..2);
            readers.queue(&frames.frame(2), &frames.set, 4..5);
            readers.queue(&frames.frame(3), &frames.set, 5..8);
            assert_eq!(released(&mut readers, 8), [key]);
            assert_eq!(taken(&mut readers, key), [1, 2, 3]);
        }

        #[test]
        fn gives_only_frames_that_end_past_the_position() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 3, 10);
            readers.queue(&frames.frame(1), &frames.set, 0..3);
            readers.queue(&frames.frame(2), &frames.set, 3..4);
            readers.queue(&frames.frame(3), &frames.set, 4..6);
            assert_eq!(released(&mut readers, 6), [key]);
            assert_eq!(taken(&mut readers, key), [2, 3]);
        }

        #[test]
        fn gives_no_frame_at_or_below_the_acked_position() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 1);
            readers.ack(key, live(4)).expect("forward");
            readers.queue(&frames.frame(1), &frames.set, 0..4);
            readers.queue(&frames.frame(2), &frames.set, 4..6);
            assert_eq!(released(&mut readers, 6), [key]);
            assert_eq!(taken(&mut readers, key), [2]);
        }

        #[test]
        fn gives_no_frame_with_no_samples() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), &frames.set, 0..2);
            readers.queue(&frames.frame(2), &frames.set, 2..2);
            readers.queue(&frames.frame(3), &frames.set, 2..3);
            assert_eq!(released(&mut readers, 3), [key]);
            assert_eq!(taken(&mut readers, key), [1, 3]);
        }

        #[test]
        fn marks_no_session_behind_for_a_frame_with_no_samples_and_none_open() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            readers.queue(&frames.frame(1), &frames.set, 1..1);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            readers.queue(&frames.frame(3), &frames.set, 2..4);
            assert_eq!(released(&mut readers, 4), [key]);
            assert_eq!(taken(&mut readers, key), [2, 3]);
        }

        #[test]
        fn gives_nothing_to_a_session_below_a_dropped_frame() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            readers.queue(&frames.frame(1), &frames.set, 0..4);
            let below = opened(&mut readers, 2, 10);
            let current = opened(&mut readers, 4, 10);
            readers.queue(&frames.frame(2), &frames.set, 4..6);
            assert_eq!(released(&mut readers, 6), [current]);
            assert_eq!(missed(&mut readers, below), []);
            assert_eq!(taken(&mut readers, current), [2]);
        }

        #[test]
        fn gives_nothing_to_a_session_below_a_released_frame() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let first = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), &frames.set, 0..4);
            assert_eq!(released(&mut readers, 4), [first]);
            let behind = opened(&mut readers, 3, 10);
            readers.queue(&frames.frame(2), &frames.set, 4..6);
            assert_eq!(released(&mut readers, 6), []);
            assert_eq!(missed(&mut readers, behind), []);
            assert_eq!(taken(&mut readers, first), [1, 2]);
        }

        #[test]
        fn gives_nothing_to_a_session_below_a_frame_that_waits_for_credit() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let first = opened(&mut readers, 0, 1);
            readers.queue(&frames.frame(1), &frames.set, 0..2);
            readers.queue(&frames.frame(2), &frames.set, 2..4);
            readers.queue(&frames.frame(3), &frames.set, 4..6);
            assert_eq!(released(&mut readers, 6), [first]);
            let behind = opened(&mut readers, 5, 10);
            assert_eq!(missed(&mut readers, behind), []);
            assert_eq!(taken(&mut readers, first), [1]);
        }

        #[test]
        fn gives_nothing_below_the_live_seq_of_a_new_index() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(10);
            let key = opened(&mut readers, 5, 10);
            readers.queue(&frames.frame(1), &frames.set, 10..12);
            assert_eq!(released(&mut readers, 12), []);
            assert_eq!(missed(&mut readers, key), []);
        }

        #[test]
        fn gives_nothing_below_the_live_seq_of_a_restored_index() {
            let frames = Frames::new(1);
            let mut readers = Readers::restore([], at(0), 10);
            let key = opened(&mut readers, 5, 10);
            readers.queue(&frames.frame(1), &frames.set, 10..12);
            assert_eq!(released(&mut readers, 12), []);
            assert_eq!(missed(&mut readers, key), []);
        }

        #[test]
        fn lets_one_frame_pass_the_limit() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = readers
                .open(
                    Reader::Unnamed,
                    Start::At(live(0)),
                    CHARGE + 1,
                    Charge::Whole,
                )
                .key;
            for n in 0..3 {
                readers.queue(&frames.frame(n + 1), &frames.set, n..n + 1);
            }
            assert_eq!(released(&mut readers, 3), [key]);
            assert_eq!(taken(&mut readers, key), [1, 2]);
        }

        #[test]
        fn gives_frames_up_to_the_limit_of_the_open_with_no_grant() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = readers
                .open(
                    Reader::Unnamed,
                    Start::At(live(0)),
                    2 * CHARGE,
                    Charge::Whole,
                )
                .key;
            for n in 0..3 {
                readers.queue(&frames.frame(n + 1), &frames.set, n..n + 1);
            }
            assert_eq!(released(&mut readers, 3), [key]);
            assert_eq!(taken(&mut readers, key), [1, 2]);
        }

        #[test]
        fn raises_the_limit_of_the_open_with_a_grant() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let key = readers
                .open(Reader::Unnamed, Start::At(live(0)), CHARGE, Charge::Whole)
                .key;
            readers.grant(key, 3 * CHARGE);
            for n in 0..4 {
                readers.queue(&frames.frame(n + 1), &frames.set, n..n + 1);
            }
            assert_eq!(released(&mut readers, 4), [key]);
            assert_eq!(taken(&mut readers, key), [1, 2, 3]);
        }

        #[test]
        fn keeps_the_limit_of_the_open_after_a_lower_grant() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let key = readers
                .open(
                    Reader::Unnamed,
                    Start::At(live(0)),
                    3 * CHARGE,
                    Charge::Whole,
                )
                .key;
            readers.grant(key, CHARGE);
            for n in 0..4 {
                readers.queue(&frames.frame(n + 1), &frames.set, n..n + 1);
            }
            assert_eq!(released(&mut readers, 4), [key]);
            assert_eq!(taken(&mut readers, key), [1, 2, 3]);
        }

        #[test]
        fn misses_a_frame_that_waits_for_credit_at_the_next_release() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 1);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert_eq!(released(&mut readers, 2), [key]);
            readers.queue(&frames.frame(3), &frames.set, 2..3);
            assert_eq!(released(&mut readers, 3), []);
            readers.grant(key, 10 * CHARGE);
            assert_eq!(missed(&mut readers, key), [1]);
        }

        #[test]
        fn gives_a_frame_that_waits_for_credit_at_a_grant() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 1);
            for n in 0..3 {
                readers.queue(&frames.frame(n + 1), &frames.set, n..n + 1);
            }
            assert_eq!(released(&mut readers, 3), [key]);
            assert_eq!(taken(&mut readers, key), [1]);
            readers.grant(key, 2 * CHARGE);
            assert_eq!(taken(&mut readers, key), [2]);
            readers.grant(key, 3 * CHARGE);
            assert_eq!(taken(&mut readers, key), [3]);
        }

        #[test]
        fn misses_no_frame_taken_after_a_grant_before_the_next_release() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 1);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert_eq!(released(&mut readers, 2), [key]);
            readers.grant(key, 10 * CHARGE);
            assert_eq!(taken(&mut readers, key), [1, 2]);
            readers.queue(&frames.frame(3), &frames.set, 2..3);
            assert_eq!(released(&mut readers, 3), [key]);
            assert_eq!(taken(&mut readers, key), [3]);
        }

        #[test]
        fn misses_a_frame_that_a_grant_covers_and_the_session_did_not_take() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 1);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert_eq!(released(&mut readers, 2), [key]);
            readers.grant(key, 10 * CHARGE);
            readers.queue(&frames.frame(3), &frames.set, 2..3);
            assert_eq!(released(&mut readers, 3), []);
            assert_eq!(missed(&mut readers, key), [1]);
        }

        #[test]
        fn misses_no_frame_at_a_release_that_gives_none() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 1);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            readers.queue(&frames.frame(3), &frames.set, 2..4);
            assert_eq!(released(&mut readers, 2), [key]);
            assert_eq!(released(&mut readers, 3), []);
            readers.grant(key, 10 * CHARGE);
            assert_eq!(taken(&mut readers, key), [1, 2]);
            assert_eq!(released(&mut readers, 4), [key]);
            assert_eq!(taken(&mut readers, key), [3]);
        }

        #[test]
        fn pends_not_for_a_frame_that_waits_for_credit() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 1);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert_eq!(released(&mut readers, 2), [key]);
            assert!(!readers.pending());
            assert_eq!(taken(&mut readers, key), [1]);
            readers.grant(key, 2 * CHARGE);
            assert_eq!(taken(&mut readers, key), [2]);
        }

        #[test]
        fn drops_the_frames_that_wait_for_credit_once_no_session_waits() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let paid = opened(&mut readers, 0, 1);
            let closed = opened(&mut readers, 0, 1);
            for n in 0..3 {
                readers.queue(&frames.frame(n + 1), &frames.set, n..n + 1);
            }
            assert_eq!(released(&mut readers, 3), [paid, closed]);
            readers.grant(paid, 3 * CHARGE);
            assert_eq!(taken(&mut readers, paid), [1, 2, 3]);
            assert!(!frames.spare(), "a session waits for frames 2 and 3");
            readers.close(closed.into());
            let room: Vec<_> = (0..3).map(|n| frames.make(n)).collect();
            assert!(room.iter().all(Result::is_ok), "the pool holds no frame");
        }

        #[test]
        fn gives_the_frames_that_wait_for_credit_to_each_session_from_its_own_first() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let early = opened(&mut readers, 0, 1);
            let late = opened(&mut readers, 0, 2);
            for n in 0..4 {
                readers.queue(&frames.frame(n + 1), &frames.set, n..n + 1);
            }
            assert_eq!(released(&mut readers, 4), [early, late]);
            assert_eq!(taken(&mut readers, early), [1]);
            assert_eq!(taken(&mut readers, late), [1, 2]);
            readers.grant(early, 4 * CHARGE);
            readers.grant(late, 4 * CHARGE);
            assert_eq!(taken(&mut readers, late), [3, 4]);
            assert_eq!(taken(&mut readers, early), [2, 3, 4]);
        }

        #[test]
        fn gives_a_prompt_session_each_frame_of_a_release_of_many_windows() {
            const WINDOW: u64 = 8 * CHARGE;
            let frames = Frames::new(1);
            let frame = frames.frame(1);
            for windows in [2, 10, 100] {
                let mut readers = Readers::new(0);
                let prompt = opened(&mut readers, 0, WINDOW / CHARGE);
                let idle = opened(&mut readers, 0, WINDOW / CHARGE);
                let count = windows * WINDOW / CHARGE;
                for n in 0..count {
                    readers.queue(&frame, &frames.set, n..n + 1);
                }
                assert_eq!(released(&mut readers, count), [prompt, idle]);
                // As a local reader grants: past the frames it took, at its next take.
                let mut taken_bytes = 0;
                let mut got = 0;
                while let Next::Frame(frame) = readers.take(prompt.into()) {
                    got += 1;
                    taken_bytes += frame.charge();
                    readers.grant(prompt, taken_bytes + WINDOW);
                }
                assert_eq!(got, count, "{windows} windows");
                readers.queue(&frame, &frames.set, count..count + 1);
                assert_eq!(released(&mut readers, count + 1), [prompt]);
                assert_eq!(taken(&mut readers, prompt), [1]);
                assert_eq!(missed(&mut readers, idle).len(), 8);
            }
        }

        #[test]
        fn spends_the_credit_of_each_session() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let short = opened(&mut readers, 0, 1);
            let long = opened(&mut readers, 0, 3);
            for n in 0..3 {
                readers.queue(&frames.frame(n + 1), &frames.set, n..n + 1);
            }
            assert_eq!(released(&mut readers, 3), [short, long]);
            assert_eq!(taken(&mut readers, short), [1]);
            assert_eq!(taken(&mut readers, long), [1, 2, 3]);
        }

        #[test]
        fn gives_no_credit_to_a_takeover() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            readers.open(
                named("a", 10),
                Start::At(live(0)),
                10 * CHARGE,
                Charge::Whole,
            );
            let new = readers
                .open(named("a", 10), resume(live(0)), 0, Charge::Whole)
                .key;
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            assert_eq!(released(&mut readers, 1), []);
            assert_eq!(taken(&mut readers, new), []);
            readers.grant(new, CHARGE);
            assert_eq!(taken(&mut readers, new), [1]);
        }

        #[test]
        fn counts_the_credit_of_a_takeover_from_zero() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let old = readers
                .open(
                    named("a", 10),
                    Start::At(live(0)),
                    2 * CHARGE,
                    Charge::Whole,
                )
                .key;
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert_eq!(released(&mut readers, 2), [old]);
            assert_eq!(taken(&mut readers, old), [1, 2]);
            readers.ack(old, live(2)).expect("forward");
            let new = readers
                .open(named("a", 10), resume(live(0)), CHARGE, Charge::Whole)
                .key;
            readers.queue(&frames.frame(3), &frames.set, 2..3);
            assert_eq!(released(&mut readers, 3), [new]);
            assert_eq!(taken(&mut readers, new), [3]);
        }

        #[test]
        fn drops_a_grant_to_a_closed_session() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let closed = opened(&mut readers, 0, 0);
            let open = opened(&mut readers, 0, 1);
            readers.close(closed.into());
            readers.grant(closed, 10 * CHARGE);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert_eq!(released(&mut readers, 2), [open]);
            assert_eq!(taken(&mut readers, open), [1]);
        }

        #[test]
        fn drops_a_grant_to_a_session_a_takeover_closed() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let old = readers
                .open(named("a", 10), Start::At(live(0)), 0, Charge::Whole)
                .key;
            let new = readers
                .open(named("a", 10), resume(live(0)), 0, Charge::Whole)
                .key;
            readers.grant(old, 10 * CHARGE);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            assert_eq!(released(&mut readers, 1), []);
            assert_eq!(taken(&mut readers, new), []);
        }

        #[test]
        fn drops_a_grant_to_a_session_a_latest_takeover_closed() {
            let mut readers = Readers::new(0);
            let old = readers
                .open(named("a", 10), Start::At(live(0)), 0, Charge::Whole)
                .key;
            let name = "a".parse().expect("a valid name");
            let latest = readers.open_named_latest(name, at(1));
            assert_eq!(latest.replaced, Some(old.into()));
            readers.grant(old, 10 * CHARGE);
            assert_eq!(taken(&mut readers, latest.key), []);
        }

        #[test]
        fn wakes_a_session_once_while_frames_wait() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert_eq!(released(&mut readers, 2), [key]);
            readers.queue(&frames.frame(3), &frames.set, 2..3);
            assert_eq!(released(&mut readers, 3), []);
            assert_eq!(taken(&mut readers, key), [1, 2, 3]);
            readers.queue(&frames.frame(4), &frames.set, 3..4);
            assert_eq!(released(&mut readers, 4), [key]);
        }

        #[test]
        fn wakes_a_session_once_when_it_misses_a_frame_with_none_waiting() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 1);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            assert_eq!(released(&mut readers, 1), [key]);
            assert_eq!(taken(&mut readers, key), [1]);
            assert_eq!(taken(&mut readers, key), []);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert_eq!(released(&mut readers, 2), []);
            assert_eq!(taken(&mut readers, key), []);
            readers.queue(&frames.frame(3), &frames.set, 2..3);
            assert_eq!(released(&mut readers, 3), [key]);
            assert_eq!(missed(&mut readers, key), []);
            readers.queue(&frames.frame(4), &frames.set, 3..4);
            assert_eq!(released(&mut readers, 4), []);
            assert_eq!(missed(&mut readers, key), []);
        }

        #[test]
        fn wakes_a_session_once_for_a_frame_and_a_miss_in_one_release() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 1);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert_eq!(released(&mut readers, 2), [key]);
            assert_eq!(taken(&mut readers, key), [1]);
            readers.queue(&frames.frame(3), &frames.set, 2..3);
            readers.queue(&frames.frame(4), &frames.set, 3..4);
            assert_eq!(released(&mut readers, 4), [key]);
            assert_eq!(missed(&mut readers, key), []);
        }

        #[test]
        fn wakes_no_session_with_frames_waiting_when_it_misses_one() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 1);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            assert_eq!(released(&mut readers, 1), [key]);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert_eq!(released(&mut readers, 2), []);
            readers.queue(&frames.frame(3), &frames.set, 2..3);
            assert_eq!(released(&mut readers, 3), []);
            assert_eq!(missed(&mut readers, key), [1]);
        }

        #[test]
        fn gives_no_closed_session_as_behind() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 0);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            assert_eq!(released(&mut readers, 1), []);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert_eq!(released(&mut readers, 2), [key]);
            assert_eq!(missed(&mut readers, key), []);
            assert_eq!(missed(&mut readers, key), []);
            readers.close(key.into());
            assert_eq!(taken(&mut readers, key), []);
        }

        #[test]
        fn wakes_nothing_without_a_frame_on_disk() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), &frames.set, 0..2);
            assert_eq!(released(&mut readers, 1), []);
        }

        #[test]
        fn holds_a_frame_until_each_session_takes_it() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let a = opened(&mut readers, 0, 10);
            let b = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
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
            opened(&mut readers, 1, 0);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            assert!(!frames.spare());
            assert_eq!(released(&mut readers, 1), []);
            assert!(frames.spare());
        }

        #[test]
        fn drops_a_frame_that_waits_for_credit_at_the_next_release() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 0);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            assert_eq!(released(&mut readers, 1), []);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert!(!frames.spare());
            assert_eq!(released(&mut readers, 2), [key]);
            assert!(frames.spare());
        }

        #[test]
        fn keeps_nothing_with_no_complete_session() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let latest = readers.open_latest().key;
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            assert!(frames.spare());
            assert_eq!(taken(&mut readers, latest), []);
        }

        #[test]
        fn drops_the_waiting_frames_of_a_closed_session() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            assert_eq!(released(&mut readers, 1), [key]);
            readers.close(key.into());
            assert!(frames.spare());
        }

        #[test]
        fn drops_the_queued_frames_when_the_last_complete_session_closes() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            readers.close(key.into());
            assert!(frames.spare());
            let later = opened(&mut readers, 0, 10);
            assert_eq!(released(&mut readers, 1), []);
            assert_eq!(missed(&mut readers, later), []);
        }

        #[test]
        fn drops_the_queued_frames_when_the_last_named_session_closes() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let key = readers
                .open(named("a", 10), Start::At(live(0)), 0, Charge::Whole)
                .key;
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            readers.close_named(key, at(0));
            assert!(frames.spare());
            assert!(!readers.pending());
        }

        #[test]
        fn gives_a_named_reader_that_resumes_no_frame_after_one_its_close_dropped() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let key = readers
                .open(named("a", 10), Start::At(live(0)), 0, Charge::Whole)
                .key;
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            readers.close_named(key, at(0));
            let later = readers
                .open(named("a", 10), resume(live(1)), 10 * CHARGE, Charge::Whole)
                .key;
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert_eq!(released(&mut readers, 2), []);
            assert_eq!(missed(&mut readers, later), []);
        }

        #[test]
        fn gives_a_named_reader_that_resumes_the_frames_past_its_position() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let key = readers
                .open(
                    named("a", 10),
                    Start::At(live(0)),
                    10 * CHARGE,
                    Charge::Whole,
                )
                .key;
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            assert_eq!(released(&mut readers, 1), [key]);
            assert_eq!(taken(&mut readers, key), [1]);
            assert_eq!(readers.ack(key, live(1)), Ok(()));
            readers.close_named(key, at(0));
            let later = readers
                .open(named("a", 10), resume(live(0)), 10 * CHARGE, Charge::Whole)
                .key;
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            assert_eq!(released(&mut readers, 2), [later]);
            assert_eq!(taken(&mut readers, later), [2]);
        }

        #[test]
        fn drops_the_queued_frames_when_a_latest_session_takes_over_the_last() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let key = readers
                .open(named("a", 10), Start::At(live(0)), 0, Charge::Whole)
                .key;
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            let latest = readers.open_named_latest("a".parse().expect("name"), at(0));
            assert_eq!(latest.replaced, Some(key.into()));
            assert!(frames.spare());
        }

        #[test]
        fn keeps_the_queued_frames_through_a_complete_takeover() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            readers.open(named("a", 10), Start::At(live(0)), 0, Charge::Whole);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            let new = readers
                .open(named("a", 10), resume(live(0)), 10 * CHARGE, Charge::Whole)
                .key;
            assert_eq!(released(&mut readers, 1), [new]);
            assert_eq!(taken(&mut readers, new), [1]);
        }

        #[test]
        fn take_gives_nothing_before_a_release() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            assert!(matches!(readers.take(key.into()), Next::Empty));
        }

        #[test]
        #[should_panic(expected = "live frame at seq 3..5 queued after seq 4")]
        fn queue_panics_on_a_seq_below_the_last_frame() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), &frames.set, 0..4);
            readers.queue(&frames.frame(2), &frames.set, 3..5);
        }

        #[test]
        #[should_panic(expected = "live frame at seq 5..6 queued after seq 10")]
        fn queue_panics_on_a_seq_below_the_live_seq() {
            let frames = Frames::new(1);
            Readers::new(10).queue(&frames.frame(1), &frames.set, 5..6);
        }

        #[test]
        #[should_panic(expected = "live frame at seq 3..3 queued after seq 4")]
        fn queue_panics_on_a_frame_with_no_samples_below_the_last_frame() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            readers.queue(&frames.frame(1), &frames.set, 0..4);
            readers.queue(&frames.frame(2), &frames.set, 3..3);
        }

        #[test]
        #[should_panic(expected = "live frame at seq 2..3 queued after seq 5")]
        fn queue_panics_on_a_seq_below_a_frame_with_no_samples() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            readers.queue(&frames.frame(1), &frames.set, 5..5);
            readers.queue(&frames.frame(2), &frames.set, 2..3);
        }

        #[test]
        #[should_panic(expected = "live frame at seq 5..3 ends before it starts")]
        fn queue_panics_on_a_seq_that_ends_before_it_starts() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), &frames.set, Range { start: 5, end: 3 });
        }

        #[test]
        fn take_gives_a_closed_session_nothing() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let key = opened(&mut readers, 0, 10);
            readers.queue(&frames.frame(1), &frames.set, 0..1);
            assert_eq!(released(&mut readers, 1), [key]);
            readers.close(key.into());
            assert_eq!(taken(&mut readers, key), []);
        }
    }

    mod charge {
        use proptest::prelude::*;
        use types::sample::{Scalar, Type};

        use super::*;

        const F64: Type = Type::Scalar(Scalar::F64);
        const U8: Type = Type::Scalar(Scalar::U8);

        /// Frames of one index and nine `F64` channels, entries 0 to 9, from two key
        /// sets: `wide` holds all ten, `narrow` the index and the last two.
        struct Sets {
            pool: block::Pool,
            wide: Arc<KeySet>,
            narrow: Arc<KeySet>,
        }

        impl Sets {
            fn new() -> Self {
                let config = block::Config { budget: 1 << 20 };
                let memory = block::Heap::new(config.reservation());
                let keys: Vec<_> = (2..=10)
                    .map(|n| (channel::Key::from_u128(n), F64))
                    .collect();
                let mut interner = Interner::new();
                let index = channel::Key::from_u128(1);
                let wide = interner.intern(&[Group { index, data: &keys }]);
                let narrow = interner.intern(&[Group {
                    index,
                    data: &keys[7..],
                }]);
                Self {
                    pool: block::Pool::new(config, memory),
                    wide,
                    narrow,
                }
            }

            /// A frame of `set` whose series have the lengths `lens`, by entry.
            fn frame(&self, set: &KeySet, lens: &[(usize, usize)]) -> Frame {
                Draft::new(&self.pool, set, Form::Raw, lens)
                    .expect("the pool holds the frame")
                    .freeze(Path::Live)
            }

            /// A frame of `wide` with 8 bytes in each series.
            fn full(&self) -> Frame {
                let lens: Vec<_> = (0..10).map(|entry| (entry, 8)).collect();
                self.frame(&self.wide, &lens)
            }

            fn slot(&self, entry: usize) -> channel::Slot {
                self.wide.entries()[entry].slot
            }
        }

        /// Asserts that `frames` cost a session that `charge` charges `spent` bytes: a
        /// frame after them passes a credit of `spent + 1`, and waits for credit at a
        /// credit of `spent`.
        fn spends(charge: &Charge, frames: &[(&Frame, &Arc<KeySet>)], spent: u64) {
            for (limit, passes) in [(spent, false), (spent + 1, true)] {
                let mut readers = Readers::new(0);
                let key = readers
                    .open(Reader::Unnamed, Start::At(live(0)), limit, charge.clone())
                    .key;
                let after = frames[0];
                let mut seq = 0..0;
                for (frame, set) in frames.iter().chain([&after]) {
                    seq = seq.end..seq.end + 1;
                    readers.queue(frame, set, seq.clone());
                }
                assert_eq!(readers.release(seq.end), [key]);
                let (taken, end) = drain(&mut readers, key);
                let count = frames.len() + usize::from(passes);
                assert_eq!(taken.len(), count, "credit {limit}");
                assert!(matches!(end, Next::Empty), "credit {limit}");
            }
        }

        #[test]
        fn pays_a_frame_that_waits_for_credit_at_the_charge_of_its_places() {
            let sets = Sets::new();
            let mut readers = Readers::new(0);
            let places = Charge::Places([sets.slot(3)].into());
            let key = readers
                .open(Reader::Unnamed, Start::At(live(0)), CHARGE, places)
                .key;
            for n in 0..3 {
                readers.queue(&sets.full(), &sets.wide, n..n + 1);
            }
            assert_eq!(readers.release(3), [key]);
            assert_eq!(drain(&mut readers, key).0.len(), 1);
            readers.grant(key, 3 * CHARGE);
            let (taken, end) = drain(&mut readers, key);
            assert_eq!(taken.len(), 2, "each frame that waits costs one series");
            assert!(matches!(end, Next::Empty));
        }

        #[test]
        fn gives_the_frames_that_wait_for_credit_across_a_change_of_key_set() {
            let sets = Sets::new();
            let mut readers = Readers::new(0);
            let mut open = |limit, charge| {
                readers
                    .open(Reader::Unnamed, Start::At(live(0)), limit, charge)
                    .key
            };
            let slow = open(1, Charge::Whole);
            // `view` gets the held frames at the release, and `owed` at its take.
            let view = open(u64::MAX, Charge::Places([sets.slot(9)].into()));
            let owed = open(1, Charge::Places([sets.slot(9)].into()));
            let narrow = sets.frame(&sets.narrow, &[(0, 8), (1, 8), (2, 8)]);
            readers.queue(&sets.full(), &sets.wide, 0..1);
            readers.queue(&sets.full(), &sets.wide, 1..2);
            readers.queue(&narrow, &sets.narrow, 2..3);
            assert_eq!(readers.release(3), [slow, view, owed]);
            let key_sets = |taken: Vec<Frame>| -> Vec<_> {
                taken.iter().map(Frame::key_set).collect()
            };
            let (wide, narrow) = (sets.wide.key(), sets.narrow.key());
            let (taken, end) = drain(&mut readers, view);
            assert_eq!(key_sets(taken), [wide, wide, narrow]);
            assert!(matches!(end, Next::Empty));
            for key in [slow, owed] {
                assert_eq!(drain(&mut readers, key).0.len(), 1);
                readers.grant(key, u64::MAX);
                let (taken, end) = drain(&mut readers, key);
                assert_eq!(key_sets(taken), [wide, narrow]);
                assert!(matches!(end, Next::Empty));
            }
        }

        #[test]
        fn gives_a_one_channel_view_of_ten_a_frame_for_each_charge_of_its_series() {
            let sets = Sets::new();
            let mut readers = Readers::new(0);
            let places = Charge::Places([sets.slot(3)].into());
            let key = readers
                .open(Reader::Unnamed, Start::At(live(0)), 10 * CHARGE, places)
                .key;
            for n in 0..11 {
                readers.queue(&sets.full(), &sets.wide, n..n + 1);
            }
            assert_eq!(readers.release(11), [key]);
            let (taken, end) = drain(&mut readers, key);
            assert_eq!(taken.len(), 10);
            assert!(matches!(end, Next::Empty), "the last waits for credit");
        }

        #[test]
        fn charges_a_whole_session_the_home_frame() {
            let sets = Sets::new();
            let frame = sets.full();
            assert!(frame.charge() > CHARGE);
            spends(&Charge::Whole, &[(&frame, &sets.wide)], frame.charge());
        }

        #[test]
        fn charges_the_first_listing_of_a_repeated_slot_once() {
            let sets = Sets::new();
            let frame = sets.full();
            let places = Charge::Places([sets.slot(3), sets.slot(3)].into());
            spends(&places, &[(&frame, &sets.wide)], CHARGE);
        }

        #[test]
        fn finds_the_places_again_in_a_frame_of_another_key_set() {
            let sets = Sets::new();
            let places = Charge::Places([sets.slot(9)].into());
            let first = sets.frame(&sets.wide, &[(0, 400), (9, 8)]);
            // In `narrow`, the slot of entry 9 of `wide` is entry 2.
            let second = sets.frame(&sets.narrow, &[(0, 400), (1, 8), (2, 200)]);
            let built = built(&sets.pool, &[200]).charge();
            assert!(built > CHARGE);
            let frames = [(&first, &sets.wide), (&second, &sets.narrow)];
            spends(&places, &frames, CHARGE + built);
        }

        #[test]
        #[should_panic(expected = "the frame is of key set 0 and `set` is key set 1")]
        fn refuses_a_frame_with_another_key_set() {
            let sets = Sets::new();
            Readers::new(0).queue(&sets.full(), &sets.narrow, 0..1);
        }

        /// The frame that a remote reader builds: one series for each of `lens`, in
        /// order, in a key set of its own.
        fn built(pool: &block::Pool, lens: &[usize]) -> Frame {
            let keys: Vec<_> = (0..lens.len().max(1))
                .map(|n| {
                    (
                        channel::Key::from_u128(100 + u128::try_from(n).expect("few")),
                        U8,
                    )
                })
                .collect();
            let set = Interner::new().intern(&[Group {
                index: keys[0].0,
                data: &keys[1..],
            }]);
            let series: Vec<_> = lens.iter().copied().enumerate().collect();
            Draft::new(pool, &set, Form::Raw, &series)
                .expect("the pool holds the frame")
                .freeze(Path::Live)
        }

        proptest! {
            #[test]
            fn spends_the_charge_of_the_frame_a_remote_reader_builds(
                lens in proptest::collection::vec(
                    proptest::option::of(0..40_usize), 9),
                index in 0..40_usize,
                places in proptest::collection::vec(0..12_usize, 0..8),
            ) {
                let sets = Sets::new();
                let mut present = vec![(0, index)];
                present.extend(lens.iter().enumerate().filter_map(|(n, len)| {
                    len.map(|len| (n + 1, len))
                }));
                let frame = sets.frame(&sets.wide, &present);
                // Places 10 and 11 are slots that the key set lacks.
                let slots: Vec<_> = places
                    .iter()
                    .map(|&place| {
                        let lacking = 1000 + u32::try_from(place).expect("few");
                        if place < 10 {
                            sets.slot(place)
                        } else {
                            channel::Slot::new(lacking)
                        }
                    })
                    .collect();
                let mut seen = Vec::new();
                let mut read = Vec::new();
                for &place in &places {
                    if place < 10 && !seen.contains(&place) {
                        seen.push(place);
                        if let Some(len) = frame.series(place) {
                            read.push(len.len());
                        }
                    }
                }
                let built = built(&sets.pool, &read);
                // The parts too, as two errors in them could cancel in the charge.
                let mut places = types::frame::Places::new(slots.clone().into());
                let placed = places.lay(&frame, &sets.wide);
                prop_assert_eq!(placed.len(), read.len());
                let end = placed.last().map_or(0, |placed| placed.end);
                prop_assert_eq!(end, built.body().len());
                let charge = Charge::Places(slots.into());
                spends(&charge, &[(&frame, &sets.wide)], built.charge());
            }
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
            /// Each key ever opened.
            given: BTreeSet<complete::Key>,
        }

        impl Model {
            fn forget(&mut self, now: i64) {
                self.closed.retain(|_, (_, hold, at)| *at + *hold > now);
            }

            /// Opens the next key, and returns it with the start position and the
            /// session it replaces.
            fn open(
                &mut self,
                name: Option<usize>,
                hold: i64,
                start: Start,
            ) -> (complete::Key, Position, Option<Key>) {
                let key = complete::Key(self.given.last().map_or(0, |key| key.0 + 1));
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
                self.given.insert(key);
                (key, position, session.map(|(key, _)| Key::Complete(key)))
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

        /// Checks [`dropped`] on each key that `model` gave that is no longer open.
        fn dropped_closed(readers: &mut Readers, model: &Model) {
            for key in model
                .given
                .iter()
                .filter(|key| !model.open.contains_key(key))
            {
                dropped(readers, (*key).into());
            }
        }

        /// Closes the open session `key` at `now` with the call for its kind.
        fn close(readers: &mut Readers, model: &Model, key: complete::Key, now: i64) {
            match model.open[&key].0 {
                Some(_) => readers.close_named(key, at(now)),
                None => readers.close(key.into()),
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
                    let opened = readers.open(reader, start, 0, Charge::Whole);
                    let expected = model.open(name, hold, start);
                    assert_eq!(
                        (opened.key, opened.position, opened.replaced),
                        expected
                    );
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
                    close(readers, model, key, now);
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
                dropped_closed(&mut readers, &model);
                records.extend(readers.records());
            }
            if flushed {
                readers.flush();
                records.extend(readers.records());
            }
            let mut restored = Readers::restore(records, at(now), 0);
            for key in model.open.keys() {
                close(&mut readers, &model, *key, now);
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
            /// Opens a session at the count below 4 past the end of the newest gone
            /// frame, unnamed, or as a named reader that resumes, with that position
            /// as the fallback.
            Open(u64, Option<usize>),
            Close(usize),
            Ack(usize, u64),
            Grant(usize, u64),
            Queue {
                gap: u64,
                len: u64,
            },
            Release(u64),
            Take(usize),
        }

        /// A complete session of the live path stated a second way: the frames it got,
        /// by number, and how many it took.
        #[derive(Default)]
        struct Got {
            behind: bool,
            /// The session missed a frame that waited for credit, in the last release.
            missed: bool,
            limit: u64,
            spent: u64,
            frames: Vec<u64>,
            /// The frames that wait for credit, oldest first.
            owed: Vec<u64>,
            taken: usize,
        }

        impl Got {
            /// Gives the session its next frame that waits for credit, if it has
            /// credit.
            fn pay(&mut self) {
                if !self.owed.is_empty() && self.spent < self.limit {
                    self.spent += CHARGE;
                    self.frames.push(self.owed.remove(0));
                }
            }
        }

        /// The live path stated a second way: the readers and their positions, what
        /// each open session got, by key, and each queued frame.
        #[derive(Default)]
        struct Flows {
            readers: Model,
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

            /// The live position of the open session `key`.
            fn position(&self, key: complete::Key) -> u64 {
                self.readers.open[&key].1.live
            }

            fn close(&mut self, key: complete::Key, now: i64) {
                self.readers.close(key, now);
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
                let releases = self.queued.iter().any(|queued| {
                    queued.held && !queued.seq.is_empty() && queued.seq.end <= durable
                });
                for got in self.open.values_mut() {
                    got.missed = releases && !got.owed.is_empty();
                    if got.missed {
                        got.owed.clear();
                        got.behind = true;
                    }
                }
                let held = self.queued.iter_mut().filter(|queued| queued.held);
                for queued in held.filter(|queued| queued.seq.end <= durable) {
                    queued.held = false;
                    for (key, got) in &mut self.open {
                        let position = self.readers.open[key].1.live;
                        if got.behind || !holds(&queued.seq, position) {
                            continue;
                        }
                        if got.owed.is_empty() && got.spent < got.limit {
                            got.spent += CHARGE;
                            got.frames.push(queued.n);
                        } else {
                            got.owed.push(queued.n);
                        }
                    }
                }
                self.open
                    .iter()
                    .filter(|(key, got)| {
                        idle.contains(key)
                            && (got.taken < got.frames.len() || got.missed)
                    })
                    .map(|(&key, _)| key)
                    .collect()
            }

            /// What a take gives `key`.
            fn take(&mut self, key: complete::Key) -> Told {
                let got = self.got(key);
                if got.taken == got.frames.len() {
                    got.pay();
                }
                let n = got.frames.get(got.taken).copied();
                got.taken += usize::from(n.is_some());
                match n {
                    Some(n) => Told::Frame(n),
                    None if got.behind => Told::Behind,
                    None => Told::Empty,
                }
            }
        }

        fn live_input() -> impl Strategy<Value = Live> {
            prop_oneof![
                (0..8_u64, proptest::option::of(0..2_usize))
                    .prop_map(|(back, name)| Live::Open(back, name)),
                any::<usize>().prop_map(Live::Close),
                (any::<usize>(), 0..4_u64).prop_map(|(i, ahead)| Live::Ack(i, ahead)),
                (any::<usize>(), 0..6 * CHARGE).prop_map(|(i, b)| Live::Grant(i, b)),
                (0..3_u64, 0..4_u64).prop_map(|(gap, len)| Live::Queue { gap, len }),
                (0..4_u64).prop_map(Live::Release),
                any::<usize>().prop_map(Live::Take),
            ]
        }

        /// Opens a session at `start` in both, unnamed or as the named reader `name`
        /// that resumes, and checks where it starts.
        fn open_live(
            readers: &mut Readers,
            model: &mut Flows,
            start: u64,
            name: Option<usize>,
            limit: u64,
        ) {
            let (reader, from) = match name {
                Some(name) => (named(NAMES[name], 10), resume(live(start))),
                None => (Reader::Unnamed, Start::At(live(start))),
            };
            let opened = readers.open(reader, from, limit, Charge::Whole);
            let expected = model.readers.open(name, 10, from);
            assert_eq!((opened.key, opened.position, opened.replaced), expected);
            if let Some(Key::Complete(replaced)) = opened.replaced {
                model.open.remove(&replaced);
            }
            let got = Got {
                behind: model.behind(opened.position.live),
                limit,
                ..Got::default()
            };
            model.open.insert(opened.key, got);
        }

        /// Checks the live path against a model of the rules: a session gets each
        /// released frame with a sample at or past its position, in seq order, while it
        /// has credit. A frame with no credit, and each after it, waits for a grant,
        /// and one that still waits at the next release that gives a frame is a miss. A
        /// session that starts at or below a sample no longer in memory gets none, and
        /// nothing is kept with no session open. A named reader that resumes starts
        /// where its last session stopped. A release wakes each session that had no
        /// frame waiting and now has one or missed one.
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
                    Live::Open(back, name) => {
                        let start = (model.gone() + 4).saturating_sub(back);
                        let limit = limits.next().unwrap_or(0);
                        open_live(&mut readers, &mut model, start, name, limit);
                    }
                    Live::Close(i) => {
                        let Some(key) = model.pick(i) else { continue };
                        close(&mut readers, &model.readers, key, 0);
                        model.close(key, 0);
                    }
                    Live::Ack(i, ahead) => {
                        let Some(key) = model.pick(i) else { continue };
                        let to = live(model.position(key) + ahead);
                        assert_eq!(readers.ack(key, to), model.readers.ack(key, to));
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
                        readers.queue(&frames.frame(made), &frames.set, seq.clone());
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
                        let taken = readers.take(key.into());
                        assert_eq!(told(taken), model.take(key), "session {key:?}");
                    }
                }
                dropped_closed(&mut readers, &model.readers);
                assert_eq!(readers.pending(), model.pending());
            }
            let keys: Vec<_> = model.open.keys().copied().collect();
            for key in keys {
                loop {
                    let taken = told(readers.take(key.into()));
                    assert_eq!(taken, model.take(key), "session {key:?}");
                    if !matches!(taken, Told::Frame(_)) {
                        break;
                    }
                }
            }
        }

        /// A [`Next`] as the model states it, with a frame by its number.
        #[derive(Debug, PartialEq)]
        enum Told {
            Frame(u64),
            Empty,
            Behind,
        }

        fn told(next: Next) -> Told {
            match next {
                Next::Frame(frame) => Told::Frame(number(&frame)),
                Next::Empty => Told::Empty,
                Next::Behind => Told::Behind,
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

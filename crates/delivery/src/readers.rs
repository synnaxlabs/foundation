//! The complete readers of one index at its home.

use std::collections::BTreeMap;
use std::fmt;

use types::name::Name;
use types::time::{Span, Stamp};

use crate::{Error, Position, Reader, Record, Start};

/// One session on one index. Keys are unique within one [`Readers`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(u64);

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A session that [`Readers::open`] started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened {
    /// The session.
    pub key: Key,
    /// Where the session starts.
    pub position: Position,
    /// The session of the same named reader that this one took over. It is closed.
    pub replaced: Option<Key>,
}

/// The complete readers of one index at its home: their positions, the data they hold,
/// the credit each session has, and the records that let a new home continue. Sans-I/O:
/// the home passes mesh time in, appends [`Readers::records`] to the index log after
/// each input, and calls [`Readers::advance`] at [`Readers::deadline`].
#[derive(Debug, Default)]
pub struct Readers {
    /// Sorted by key.
    sessions: Vec<Session>,
    /// The credit of each session, at its index in `sessions`. Apart so that a
    /// `Session` stays 64 bytes, which `find` indexes with a shift, not a multiply.
    credits: Vec<Credit>,
    /// Named readers that still hold after their session closed.
    closed: Vec<Closed>,
    next: u64,
    records: Vec<Record>,
}

#[derive(Debug)]
struct Session {
    key: Key,
    reader: Reader,
    position: Position,
    /// The position moved since the last record.
    changed: bool,
}

#[derive(Debug, Default)]
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
    /// No readers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The readers that `records` describe, in log order. A session that was open at
    /// the crash counts as closed at `now`.
    ///
    /// # Panics
    ///
    /// If a record's hold is negative.
    pub fn restore(records: impl IntoIterator<Item = Record>, now: Stamp) -> Self {
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
            ..Self::default()
        };
        readers.advance(now);
        readers
    }

    /// Starts a session. A named reader with an open session is taken over.
    ///
    /// # Panics
    ///
    /// If the reader's hold is negative.
    pub fn open(&mut self, reader: Reader, start: Start) -> Opened {
        let (stored, replaced) = match &reader {
            Reader::Unnamed => (None, None),
            Reader::Named { name, hold } => {
                check(*hold);
                self.take(name)
            }
        };
        let position = match start {
            Start::At(position) => position,
            Start::Resume {
                presented,
                otherwise,
            } => resume(presented, stored, otherwise),
        };
        let key = Key(self.next);
        self.next += 1;
        let session = Session {
            key,
            reader,
            position,
            changed: false,
        };
        self.records.extend(session.record());
        self.sessions.push(session);
        self.credits.push(Credit::default());
        Opened {
            key,
            position,
            replaced,
        }
    }

    /// Records that the session has every sample below `position`.
    ///
    /// # Errors
    ///
    /// [`Error::Ack`] when `position` drops a path, adds one, or moves back on one.
    ///
    /// # Panics
    ///
    /// If the session is not open.
    pub fn ack(&mut self, key: Key, position: Position) -> Result<(), Error> {
        let i = self.find(key);
        let session = &mut self.sessions[i];
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
    /// not higher than the current one changes nothing.
    ///
    /// # Panics
    ///
    /// If the session is not open.
    pub fn grant(&mut self, key: Key, limit_bytes: u64) {
        let i = self.find(key);
        let credit = &mut self.credits[i];
        credit.limit_bytes = credit.limit_bytes.max(limit_bytes);
    }

    /// Spends credit on one frame whose charge is `bytes`: the bytes its pool block
    /// pins. Returns `true` and spends when the session has spent less than its limit;
    /// the frame may take it past the limit. Otherwise returns `false` and spends
    /// nothing. After a refusal, send the session no later frame until it has the
    /// refused one.
    ///
    /// # Panics
    ///
    /// If the session is not open.
    #[must_use]
    pub fn spend(&mut self, key: Key, bytes: u64) -> bool {
        let i = self.find(key);
        let credit = &mut self.credits[i];
        if credit.spent_bytes >= credit.limit_bytes {
            return false;
        }
        credit.spent_bytes += bytes;
        true
    }

    /// Ends the session at `now`.
    ///
    /// # Panics
    ///
    /// If the session is not open.
    pub fn close(&mut self, key: Key, now: Stamp) {
        let session = self.remove(self.find(key));
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
        let open = self.sessions.iter().map(|session| session.position);
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
        for session in self.sessions.iter_mut().filter(|s| s.changed) {
            session.changed = false;
            self.records.extend(session.record());
        }
    }

    /// Takes the queued records, oldest first.
    pub fn records(&mut self) -> impl Iterator<Item = Record> {
        self.records.drain(..)
    }

    /// Removes the named reader, open or closed. Returns its position and the session
    /// it had open.
    fn take(&mut self, name: &Name) -> (Option<Position>, Option<Key>) {
        let open = self.sessions.iter().position(|s| s.name() == Some(name));
        if let Some(i) = open {
            let session = self.remove(i);
            return (Some(session.position), Some(session.key));
        }
        let closed = self.closed.iter().position(|closed| closed.name == *name);
        (closed.map(|i| self.closed.remove(i).position), None)
    }

    fn find(&self, key: Key) -> usize {
        self.sessions
            .binary_search_by_key(&key, |session| session.key)
            .unwrap_or_else(|_| panic!("session {key} is not open"))
    }

    fn remove(&mut self, i: usize) -> Session {
        self.credits.remove(i);
        self.sessions.remove(i)
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
mod tests {
    use super::*;

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
        let key = readers.open(named(name, 10), Start::At(position)).key;
        readers.close(key, at(closed));
        drained(readers);
    }

    mod open {
        use super::*;

        #[test]
        fn starts_at_the_given_position() {
            let mut readers = Readers::new();
            let opened = readers.open(Reader::Unnamed, Start::At(live(5)));
            assert_eq!(opened.position, live(5));
            assert_eq!(opened.replaced, None);
        }

        #[test]
        fn gives_each_session_its_own_key() {
            let mut readers = Readers::new();
            let first = readers.open(Reader::Unnamed, Start::At(live(0))).key;
            readers.close(first, at(0));
            let second = readers.open(Reader::Unnamed, Start::At(live(0))).key;
            assert_ne!(first, second);
        }

        #[test]
        fn resume_takes_the_presented_position() {
            let mut readers = Readers::new();
            left(&mut readers, "a", live(7), 1);
            let start = Start::Resume {
                presented: Some(live(9)),
                otherwise: live(0),
            };
            assert_eq!(readers.open(named("a", 10), start).position, live(9));
        }

        #[test]
        fn resume_falls_back_to_the_position_at_this_home() {
            let mut readers = Readers::new();
            left(&mut readers, "a", live(7), 1);
            let opened = readers.open(named("a", 10), resume(live(0)));
            assert_eq!(opened.position, live(7));
        }

        #[test]
        fn resume_falls_back_to_otherwise() {
            let mut readers = Readers::new();
            left(&mut readers, "a", live(7), 1);
            let opened = readers.open(named("b", 10), resume(both(2, 1)));
            assert_eq!(opened.position, both(2, 1));
        }

        #[test]
        fn resume_falls_back_per_path() {
            let mut readers = Readers::new();
            left(&mut readers, "a", live(7), 1);
            let opened = readers.open(named("a", 10), resume(both(0, 2)));
            assert_eq!(opened.position, both(7, 2));
        }

        #[test]
        fn resume_prefers_presented_backfill_to_the_position_at_this_home() {
            let mut readers = Readers::new();
            left(&mut readers, "a", both(7, 3), 1);
            let start = Start::Resume {
                presented: Some(both(9, 5)),
                otherwise: both(0, 0),
            };
            assert_eq!(readers.open(named("a", 10), start).position, both(9, 5));
        }

        #[test]
        fn resume_takes_backfill_from_this_home_when_the_presented_has_none() {
            let mut readers = Readers::new();
            left(&mut readers, "a", both(7, 3), 1);
            let start = Start::Resume {
                presented: Some(live(9)),
                otherwise: both(0, 0),
            };
            assert_eq!(readers.open(named("a", 10), start).position, both(9, 3));
        }

        #[test]
        fn resume_follows_backfill_only_when_otherwise_does() {
            let mut readers = Readers::new();
            left(&mut readers, "a", both(7, 3), 1);
            let opened = readers.open(named("a", 10), resume(live(0)));
            assert_eq!(opened.position, live(7));
        }

        #[test]
        fn at_ignores_the_position_at_this_home() {
            let mut readers = Readers::new();
            left(&mut readers, "a", live(7), 1);
            let opened = readers.open(named("a", 10), Start::At(live(2)));
            assert_eq!(opened.position, live(2));
        }
    }

    mod takeover {
        use super::*;

        #[test]
        fn continues_from_the_old_session() {
            let mut readers = Readers::new();
            let old = readers.open(named("a", 10), resume(live(0))).key;
            readers.ack(old, live(5)).expect("forward");
            let opened = readers.open(named("a", 10), resume(live(0)));
            assert_eq!(opened.position, live(5));
            assert_eq!(opened.replaced, Some(old));
        }

        #[test]
        fn takes_the_presented_position() {
            let mut readers = Readers::new();
            let old = readers.open(named("a", 10), resume(live(0))).key;
            readers.ack(old, live(5)).expect("forward");
            let start = Start::Resume {
                presented: Some(live(8)),
                otherwise: live(0),
            };
            assert_eq!(readers.open(named("a", 10), start).position, live(8));
        }

        #[test]
        #[should_panic(expected = "session 0 is not open")]
        fn closes_the_old_session() {
            let mut readers = Readers::new();
            let old = readers.open(named("a", 10), resume(live(0))).key;
            readers.open(named("a", 10), resume(live(0)));
            readers.ack(old, live(1)).expect("panics before");
        }

        #[test]
        fn never_happens_to_unnamed_readers() {
            let mut readers = Readers::new();
            readers.open(Reader::Unnamed, Start::At(live(0)));
            let opened = readers.open(Reader::Unnamed, Start::At(live(0)));
            assert_eq!(opened.replaced, None);
        }
    }

    mod ack {
        use super::*;

        fn opened(position: Position) -> (Readers, Key) {
            let mut readers = Readers::new();
            let key = readers.open(named("a", 10), Start::At(position)).key;
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
        #[should_panic(expected = "session 0 is not open")]
        fn panics_on_a_closed_session() {
            let (mut readers, key) = opened(live(0));
            readers.close(key, at(1));
            readers.ack(key, live(1)).expect("panics before");
        }
    }

    mod hold {
        use super::*;

        #[test]
        fn lasts_while_the_session_is_open() {
            let mut readers = Readers::new();
            readers.open(Reader::Unnamed, Start::At(live(3)));
            readers.advance(at(i64::MAX));
            assert_eq!(readers.floor(), Some(live(3)));
            assert_eq!(readers.deadline(), None);
        }

        #[test]
        fn of_an_unnamed_reader_ends_at_the_close() {
            let mut readers = Readers::new();
            let key = readers.open(Reader::Unnamed, Start::At(live(3))).key;
            readers.close(key, at(1));
            assert_eq!(readers.floor(), None);
        }

        #[test]
        fn of_a_named_reader_ends_after_the_close() {
            let mut readers = Readers::new();
            let key = readers.open(named("a", 10), Start::At(live(0))).key;
            readers.ack(key, live(4)).expect("forward");
            readers.close(key, at(5));
            assert_eq!(readers.deadline(), Some(at(15)));
            readers.advance(at(14));
            assert_eq!(readers.floor(), Some(live(4)));
            readers.advance(at(15));
            assert_eq!(readers.floor(), None);
            assert_eq!(readers.deadline(), None);
        }

        #[test]
        fn of_zero_ends_at_the_close() {
            let mut readers = Readers::new();
            let key = readers.open(named("a", 0), Start::At(live(0))).key;
            readers.close(key, at(5));
            assert_eq!(readers.floor(), None);
        }

        #[test]
        #[should_panic(expected = "reader hold is negative: -3ns")]
        fn panics_when_negative() {
            Readers::new().open(named("a", -3), Start::At(live(0)));
        }

        #[test]
        fn ends_first_for_the_reader_that_closed_first() {
            let mut readers = Readers::new();
            left(&mut readers, "a", live(1), 15);
            left(&mut readers, "b", live(1), 5);
            assert_eq!(readers.deadline(), Some(at(15)));
        }

        #[test]
        fn ends_at_the_last_stamp() {
            let mut readers = Readers::new();
            let key = readers.open(named("a", 10), Start::At(live(0))).key;
            readers.close(key, at(i64::MAX - 1));
            assert_eq!(readers.deadline(), Some(at(i64::MAX)));
        }

        #[test]
        fn is_cancelled_by_a_reopen() {
            let mut readers = Readers::new();
            left(&mut readers, "a", live(4), 5);
            let opened = readers.open(named("a", 10), resume(live(0)));
            assert_eq!(opened.position, live(4));
            assert_eq!(readers.deadline(), None);
        }

        #[test]
        fn that_ended_leaves_no_position_to_resume() {
            let mut readers = Readers::new();
            left(&mut readers, "a", live(4), 5);
            readers.advance(at(15));
            let opened = readers.open(named("a", 10), resume(live(9)));
            assert_eq!(opened.position, live(9));
        }
    }

    mod floor {
        use super::*;

        #[test]
        fn is_the_lowest_position_on_each_path() {
            let mut readers = Readers::new();
            readers.open(Reader::Unnamed, Start::At(both(5, 9)));
            readers.open(Reader::Unnamed, Start::At(both(7, 2)));
            readers.open(Reader::Unnamed, Start::At(live(3)));
            assert_eq!(readers.floor(), Some(both(3, 2)));
        }

        #[test]
        fn has_no_backfill_when_no_reader_records() {
            let mut readers = Readers::new();
            readers.open(Reader::Unnamed, Start::At(live(5)));
            readers.open(Reader::Unnamed, Start::At(live(7)));
            assert_eq!(readers.floor(), Some(live(5)));
        }

        #[test]
        fn is_none_without_readers() {
            assert_eq!(Readers::new().floor(), None);
        }

        #[test]
        fn goes_down_when_a_session_opens_below_it() {
            let mut readers = Readers::new();
            readers.open(Reader::Unnamed, Start::At(live(5)));
            readers.open(Reader::Unnamed, Start::At(live(2)));
            assert_eq!(readers.floor(), Some(live(2)));
        }
    }

    mod records {
        use super::*;

        #[test]
        fn are_written_at_once_when_a_named_reader_opens() {
            let mut readers = Readers::new();
            readers.open(named("a", 10), Start::At(live(3)));
            assert_eq!(drained(&mut readers), [record("a", live(3), 10, None)]);
        }

        #[test]
        fn are_written_at_once_when_a_named_reader_closes() {
            let mut readers = Readers::new();
            let key = readers.open(named("a", 10), Start::At(live(3))).key;
            readers.ack(key, live(5)).expect("forward");
            readers.close(key, at(7));
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
            let mut readers = Readers::new();
            let key = readers.open(named("a", 10), Start::At(live(3))).key;
            drained(&mut readers);
            readers.close(key, at(7));
            assert_eq!(drained(&mut readers), [record("a", live(3), 10, Some(7))]);
        }

        #[test]
        fn are_written_at_once_on_a_takeover() {
            let mut readers = Readers::new();
            let old = readers.open(named("a", 10), Start::At(live(3))).key;
            readers.ack(old, live(5)).expect("forward");
            drained(&mut readers);
            readers.open(named("a", 20), resume(live(0)));
            assert_eq!(drained(&mut readers), [record("a", live(5), 20, None)]);
        }

        #[test]
        fn are_written_on_flush_only_for_changed_positions() {
            let mut readers = Readers::new();
            let a = readers.open(named("a", 10), Start::At(live(0))).key;
            readers.open(named("b", 10), Start::At(live(0)));
            drained(&mut readers);
            readers.ack(a, live(2)).expect("forward");
            readers.flush();
            assert_eq!(drained(&mut readers), [record("a", live(2), 10, None)]);
            readers.flush();
            assert_eq!(drained(&mut readers), []);
        }

        #[test]
        fn are_never_written_for_unnamed_readers() {
            let mut readers = Readers::new();
            let key = readers.open(Reader::Unnamed, Start::At(live(0))).key;
            readers.ack(key, live(2)).expect("forward");
            readers.flush();
            readers.close(key, at(1));
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
            let mut readers = Readers::restore(records, at(0));
            assert_eq!(readers.floor(), Some(live(5)));
            let opened = readers.open(named("a", 10), resume(live(0)));
            assert_eq!(opened.position, live(5));
        }

        #[test]
        fn counts_an_open_session_as_closed_at_the_restore() {
            let readers = Readers::restore([record("a", live(3), 10, None)], at(20));
            assert_eq!(readers.deadline(), Some(at(30)));
        }

        #[test]
        fn keeps_a_closed_reader_until_its_hold_ends() {
            let readers = Readers::restore([record("a", live(3), 10, Some(5))], at(8));
            assert_eq!(readers.deadline(), Some(at(15)));
            assert_eq!(readers.floor(), Some(live(3)));
        }

        #[test]
        #[should_panic(expected = "reader hold is negative: -5ns")]
        fn panics_on_a_negative_hold() {
            Readers::restore([record("a", live(3), -5, Some(1))], at(0));
        }

        #[test]
        fn keeps_the_last_record_of_each_reader() {
            let records = [
                record("b", live(1), 10, Some(15)),
                record("a", live(3), 10, None),
                record("b", live(4), 10, None),
            ];
            let readers = Readers::restore(records, at(20));
            assert_eq!(readers.floor(), Some(live(3)));
            assert_eq!(readers.deadline(), Some(at(30)));
        }

        #[test]
        fn forgets_a_reader_whose_hold_ended() {
            let readers = Readers::restore([record("a", live(3), 10, Some(5))], at(15));
            assert_eq!(readers.floor(), None);
        }

        #[test]
        fn writes_no_records() {
            let mut readers = Readers::restore([record("a", live(3), 10, None)], at(0));
            assert_eq!(drained(&mut readers), []);
        }
    }

    mod credit {
        use super::*;

        fn opened() -> (Readers, Key) {
            let mut readers = Readers::new();
            let key = readers.open(Reader::Unnamed, Start::At(live(0))).key;
            (readers, key)
        }

        #[test]
        fn is_none_for_a_new_session() {
            let (mut readers, key) = opened();
            assert!(!readers.spend(key, 1));
        }

        #[test]
        fn is_spent_up_to_the_limit() {
            let (mut readers, key) = opened();
            readers.grant(key, 10);
            assert!(readers.spend(key, 4));
            assert!(readers.spend(key, 6));
            assert!(!readers.spend(key, 1));
        }

        #[test]
        fn is_not_lowered_by_a_grant() {
            let (mut readers, key) = opened();
            readers.grant(key, 10);
            assert!(readers.spend(key, 6));
            readers.grant(key, 5);
            assert!(readers.spend(key, 1));
        }

        #[test]
        fn lets_one_frame_pass_the_limit() {
            let (mut readers, key) = opened();
            readers.grant(key, 10);
            assert!(readers.spend(key, 25));
            assert!(!readers.spend(key, 1));
        }

        #[test]
        fn counts_a_frame_past_the_limit_against_the_next_grant() {
            let (mut readers, key) = opened();
            readers.grant(key, 10);
            assert!(readers.spend(key, 25));
            readers.grant(key, 20);
            assert!(!readers.spend(key, 1));
            readers.grant(key, 26);
            assert!(readers.spend(key, 1));
        }

        #[test]
        fn is_kept_per_session() {
            let mut readers = Readers::new();
            let a = readers.open(Reader::Unnamed, Start::At(live(0))).key;
            let b = readers.open(Reader::Unnamed, Start::At(live(0))).key;
            readers.grant(b, 10);
            assert!(!readers.spend(a, 1));
            assert!(readers.spend(b, 10));
            readers.grant(a, 5);
            assert!(!readers.spend(b, 1));
            assert!(readers.spend(a, 1));
        }

        #[test]
        fn is_not_spent_by_a_blocked_frame() {
            let (mut readers, key) = opened();
            readers.grant(key, 10);
            assert!(readers.spend(key, 10));
            assert!(!readers.spend(key, 5));
            readers.grant(key, 12);
            assert!(readers.spend(key, 1));
        }

        #[test]
        fn is_none_after_a_takeover() {
            let mut readers = Readers::new();
            let old = readers.open(named("a", 10), Start::At(live(0))).key;
            readers.grant(old, 100);
            let new = readers.open(named("a", 10), resume(live(0))).key;
            assert!(!readers.spend(new, 1));
        }

        #[test]
        #[should_panic(expected = "session 0 is not open")]
        fn grant_panics_on_a_closed_session() {
            let (mut readers, key) = opened();
            readers.close(key, at(0));
            readers.grant(key, 10);
        }

        #[test]
        #[should_panic(expected = "session 0 is not open")]
        fn spend_panics_on_a_closed_session() {
            let (mut readers, key) = opened();
            readers.close(key, at(0));
            assert!(!readers.spend(key, 1));
        }
    }

    mod properties {
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
            open: BTreeMap<Key, (Option<usize>, Position, i64)>,
            closed: BTreeMap<usize, (Position, i64, i64)>,
        }

        impl Model {
            fn forget(&mut self, now: i64) {
                self.closed.retain(|_, (_, hold, at)| *at + *hold > now);
            }

            fn open(
                &mut self,
                key: Key,
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
                (position, session.map(|(key, _)| key))
            }

            fn ack(&mut self, key: Key, to: Position) -> Result<(), Error> {
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

            fn close(&mut self, key: Key, now: i64) {
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
                    let opened = readers.open(reader, start);
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
                    readers.close(key, at(now));
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
            let mut readers = Readers::new();
            let mut model = Model::default();
            let mut records = Vec::new();
            let mut now = 0;
            for (step, input) in steps {
                now += step;
                readers.advance(at(now));
                model.forget(now);
                apply(&mut readers, &mut model, input, now);
                records.extend(readers.records());
            }
            if flushed {
                readers.flush();
                records.extend(readers.records());
            }
            let mut restored = Readers::restore(records, at(now));
            for key in model.open.keys() {
                readers.close(*key, at(now));
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
        enum Flow {
            Grant(usize, u64),
            Spend(usize, u64),
        }

        const SESSIONS: usize = 3;

        /// Checks each spend against the credit rules stated a second way: a session's
        /// limit is its largest grant, and its spent bytes are the sum of its frames.
        fn check_credit(flows: Vec<Flow>) {
            let mut readers = Readers::new();
            let keys: Vec<Key> = (0..SESSIONS)
                .map(|_| readers.open(Reader::Unnamed, Start::At(live(0))).key)
                .collect();
            let mut grants = vec![Vec::new(); SESSIONS];
            let mut frames = vec![Vec::new(); SESSIONS];
            for flow in flows {
                match flow {
                    Flow::Grant(i, limit) => {
                        readers.grant(keys[i], limit);
                        grants[i].push(limit);
                    }
                    Flow::Spend(i, bytes) => {
                        let limit = grants[i].iter().copied().max().unwrap_or(0);
                        let spent: u64 = frames[i].iter().sum();
                        let accepted = readers.spend(keys[i], bytes);
                        assert_eq!(accepted, spent < limit);
                        if accepted {
                            frames[i].push(bytes);
                        }
                    }
                }
            }
        }

        proptest! {
            #[test]
            fn follow_the_credit_rules(
                flows in proptest::collection::vec(
                    prop_oneof![
                        (0..SESSIONS, 0..200_u64).prop_map(|(i, b)| Flow::Grant(i, b)),
                        (0..SESSIONS, 0..40_u64).prop_map(|(i, b)| Flow::Spend(i, b)),
                    ],
                    0..120,
                ),
            ) {
                check_credit(flows);
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

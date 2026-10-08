//! The latest sessions of [`Readers`].

use std::{fmt, mem};

use types::frame::Frame;
use types::name::Name;
use types::time::Stamp;

use super::{Readers, never_open};

/// A latest session on one index. Keys are unique within one [`Readers`]. Use a key
/// only with the `Readers` that gave it: another one, such as a restored one, takes
/// the key as its own when it gave the same number, and panics when it did not. A
/// latest key does not compile where only a complete session fits:
///
/// ```compile_fail,E0308
/// let mut readers = delivery::Readers::new(0);
/// let key = readers.open_latest().key;
/// readers.grant(key, 10);
/// ```
///
/// ```compile_fail,E0308
/// let mut readers = delivery::Readers::new(0);
/// let key = readers.open_latest().key;
/// let position = delivery::Position { live: 1, backfill: None };
/// readers.ack(key, position).unwrap();
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub(super) u64);

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A latest session that [`Readers::open_latest`] or [`Readers::open_named_latest`]
/// started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct Opened {
    /// The session.
    pub key: Key,
    /// The session of the same named reader that this one took over, in either mode.
    /// It is closed.
    pub replaced: Option<super::Key>,
}

#[derive(Debug)]
pub(super) struct Session {
    pub(super) key: Key,
    pub(super) name: Option<Name>,
    /// The newest frame waits for the session. Only the newest can wait: each put
    /// makes it wait for every session.
    waiting: bool,
}

impl Readers {
    /// Starts an unnamed latest session, which gets the index's newest live frame, if
    /// any, at once. A latest session holds nothing and writes no record.
    pub fn open_latest(&mut self) -> Opened {
        self.push_latest(None)
    }

    /// Starts a latest session for `name`, as [`Readers::open_latest`] does. The
    /// name's open session in either mode is taken over: a complete one closes at
    /// `now`, as after [`Readers::close_named`].
    pub fn open_named_latest(&mut self, name: Name, now: Stamp) -> Opened {
        let replaced = self.replace(&name, now);
        Opened {
            replaced,
            ..self.push_latest(Some(name))
        }
    }

    fn push_latest(&mut self, name: Option<Name>) -> Opened {
        let key = Key(self.next_latest);
        self.next_latest += 1;
        self.latest.push(Session {
            key,
            name,
            waiting: self.newest.is_some(),
        });
        self.woken_latest.clear();
        self.woken_latest.reserve(self.latest.len());
        Opened {
            key,
            replaced: None,
        }
    }

    /// Makes `frame`, which landed on the live path, the newest frame and the waiting
    /// frame of every latest session, and drops the frame it replaces. Returns the
    /// latest sessions that had no waiting frame, in key order: wake them.
    #[must_use]
    pub fn put(&mut self, frame: Frame) -> &[Key] {
        self.newest = Some(frame);
        self.woken_latest.clear();
        for session in &mut self.latest {
            if !mem::replace(&mut session.waiting, true) {
                self.woken_latest.push(session.key);
            }
        }
        &self.woken_latest
    }

    /// Takes the latest session's waiting frame, or `None` when it has none or is
    /// closed.
    pub(super) fn take_latest(&mut self, key: Key) -> Option<Frame> {
        let i = self.find_latest(key)?;
        if mem::replace(&mut self.latest[i].waiting, false) {
            self.newest.clone()
        } else {
            None
        }
    }

    /// Ends the latest session, if it is open. A waiting frame does not go out.
    pub(super) fn close_latest(&mut self, key: Key) {
        if let Some(i) = self.find_latest(key) {
            self.latest.remove(i);
        }
    }

    /// Removes the named reader's latest session. Returns it.
    pub(super) fn remove_latest(&mut self, name: &Name) -> Option<Key> {
        let i = self
            .latest
            .iter()
            .position(|s| s.name.as_ref() == Some(name))?;
        Some(self.latest.remove(i).key)
    }

    /// The open latest session `key`, or `None` when it closed. Panics on a key never
    /// given.
    fn find_latest(&self, key: Key) -> Option<usize> {
        if key.0 >= self.next_latest {
            never_open(key.into());
        }
        self.latest.binary_search_by_key(&key, |s| s.key).ok()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;
    use types::time::Span;

    use super::*;
    use crate::complete::Charge;
    use crate::readers::tests::{Frames, behind, dropped, frame, number};
    use crate::{Position, Reader, Record, Start, complete};

    fn at(nanos: i64) -> Stamp {
        Stamp::from_nanos(nanos)
    }

    fn name(name: &str) -> Name {
        name.parse().expect("valid name")
    }

    fn live(seq: u64) -> Position {
        Position {
            live: seq,
            backfill: None,
        }
    }

    fn complete(
        readers: &mut Readers,
        name: &str,
        position: Position,
    ) -> complete::Key {
        let reader = Reader::Named {
            name: self::name(name),
            hold: Span::from_nanos(10),
        };
        readers
            .open(reader, Start::At(position), 0, Charge::Whole)
            .key
    }

    fn unnamed(readers: &mut Readers) -> Key {
        readers.open_latest().key
    }

    fn taken(readers: &mut Readers, key: Key) -> Option<u64> {
        frame(readers.take(key.into())).as_ref().map(number)
    }

    fn put(readers: &mut Readers, frame: Frame) -> Vec<Key> {
        readers.put(frame).to_vec()
    }

    mod open_latest {
        use super::*;

        #[test]
        fn gets_the_newest_frame_at_once() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            assert_eq!(put(&mut readers, frames.frame(1)), []);
            assert_eq!(put(&mut readers, frames.frame(2)), []);
            let latest = readers.open_latest();
            assert_eq!(taken(&mut readers, latest.key), Some(2));
            assert_eq!(taken(&mut readers, latest.key), None);
        }

        #[test]
        fn gets_nothing_before_the_first_frame() {
            let mut readers = Readers::new(0);
            let latest = readers.open_latest();
            assert_eq!(taken(&mut readers, latest.key), None);
        }

        #[test]
        fn counts_keys_apart_from_complete_sessions() {
            let mut readers = Readers::new(0);
            let a = complete(&mut readers, "a", live(0));
            let b = unnamed(&mut readers);
            readers.close(b.into());
            let c = unnamed(&mut readers);
            assert_eq!((a, b, c), (complete::Key(0), Key(0), Key(1)));
            assert_ne!(crate::Key::from(a), crate::Key::from(b));
        }

        #[test]
        fn takes_over_a_latest_session() {
            let mut readers = Readers::new(0);
            let old = readers.open_named_latest(name("a"), at(0)).key;
            let new = readers.open_named_latest(name("a"), at(1));
            assert_eq!(new.replaced, Some(old.into()));
            readers.close(new.key.into());
            assert_eq!(readers.open_named_latest(name("a"), at(3)).replaced, None);
        }

        #[test]
        fn closes_the_latest_session_it_takes_over() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let old = readers.open_named_latest(name("a"), at(0)).key;
            assert_eq!(put(&mut readers, frames.frame(1)), [old]);
            let new = readers.open_named_latest(name("a"), at(1));
            assert_eq!(new.replaced, Some(old.into()));
            dropped(&mut readers, old.into());
            assert_eq!(taken(&mut readers, new.key), Some(1));
        }

        #[test]
        fn closes_the_complete_session_it_takes_over() {
            let mut readers = Readers::new(0);
            let old = complete(&mut readers, "a", live(5));
            assert_eq!(readers.records().count(), 1, "the open record");
            let new = readers.open_named_latest(name("a"), at(3));
            assert_eq!(new.replaced, Some(old.into()));
            let closed = Record {
                reader: name("a"),
                position: live(5),
                hold: Span::from_nanos(10),
                closed: Some(at(3)),
            };
            assert_eq!(readers.records().collect::<Vec<_>>(), [closed]);
            assert_eq!(readers.floor(), Some(live(5)));
            assert_eq!(readers.deadline(), Some(at(13)));
        }

        #[test]
        fn leaves_a_hold_to_the_next_complete_session() {
            let mut readers = Readers::new(0);
            let old = complete(&mut readers, "a", live(5));
            readers.close_named(old, at(1));
            let latest = readers.open_named_latest(name("a"), at(2)).key;
            assert_eq!(readers.floor(), Some(live(5)));
            let reader = Reader::Named {
                name: name("a"),
                hold: Span::from_nanos(10),
            };
            let resume = Start::Resume {
                presented: None,
                otherwise: live(0),
            };
            let opened = readers.open(reader, resume, 0, Charge::Whole);
            assert_eq!(opened.replaced, Some(latest.into()));
            assert_eq!(opened.position, live(5));
        }

        #[test]
        fn holds_nothing_and_writes_no_record() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let key = readers.open_named_latest(name("a"), at(0)).key;
            assert_eq!(put(&mut readers, frames.frame(1)), [key]);
            readers.close(key.into());
            readers.flush();
            assert_eq!(readers.records().count(), 0);
            assert_eq!(readers.floor(), None);
            assert_eq!(readers.deadline(), None);
        }
    }

    mod open {
        use super::*;

        #[test]
        fn closes_the_latest_session_it_takes_over() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let old = readers.open_named_latest(name("a"), at(0)).key;
            assert_eq!(put(&mut readers, frames.frame(1)), [old]);
            let new = complete(&mut readers, "a", live(0));
            assert_eq!(new, complete::Key(0));
            dropped(&mut readers, old.into());
            let open = Record {
                reader: name("a"),
                position: live(0),
                hold: Span::from_nanos(10),
                closed: None,
            };
            assert_eq!(readers.records().collect::<Vec<_>>(), [open]);
            assert_eq!(readers.ack(new, live(1)), Ok(()));
        }

        #[test]
        fn leaves_the_complete_session_that_took_over_as_it_was() {
            let frames = Frames::new(3);
            let mut readers = Readers::new(0);
            let old = readers.open_named_latest(name("a"), at(0)).key;
            let first = frames.frame(1);
            let reader = Reader::Named {
                name: name("a"),
                hold: Span::from_nanos(10),
            };
            let new = readers
                .open(reader, Start::At(live(0)), first.charge(), Charge::Whole)
                .key;
            assert_eq!(readers.records().count(), 1);
            readers.queue(&first, &frames.set, 0..1);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            dropped(&mut readers, old.into());
            readers.flush();
            assert_eq!(readers.records().count(), 0);
            assert_eq!(readers.release(2), [new]);
            dropped(&mut readers, old.into());
            assert_eq!(
                frame(readers.take(new.into())).as_ref().map(number),
                Some(1)
            );
            assert!(behind(&mut readers, new));
        }

        #[test]
        fn leaves_credit_holds_and_keys_after_a_late_call() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let gone = complete(&mut readers, "b", live(0));
            readers.close_named(gone, at(0));
            let old = readers.open_named_latest(name("a"), at(0)).key;
            let first = frames.frame(1);
            let reader = Reader::Named {
                name: name("a"),
                hold: Span::from_nanos(10),
            };
            let limit = 2 * first.charge();
            let new = readers
                .open(reader, Start::At(live(0)), limit, Charge::Whole)
                .key;
            readers.queue(&first, &frames.set, 0..1);
            assert_eq!(readers.release(1), [new]);
            readers.records().for_each(drop);
            assert_eq!(readers.ack(new, live(1)), Ok(()));
            dropped(&mut readers, old.into());
            readers.flush();
            let acked = Record {
                reader: name("a"),
                position: live(1),
                hold: Span::from_nanos(10),
                closed: None,
            };
            assert_eq!(readers.records().collect::<Vec<_>>(), [acked]);
            readers.queue(&frames.frame(2), &frames.set, 1..2);
            readers.queue(&frames.frame(3), &frames.set, 2..3);
            assert_eq!(readers.release(3), []);
            for n in [1, 2] {
                assert_eq!(
                    frame(readers.take(new.into())).as_ref().map(number),
                    Some(n)
                );
            }
            assert!(behind(&mut readers, new));
            dropped(&mut readers, old.into());
            assert_eq!(readers.open_latest().key, Key(1));
            let next = complete(&mut readers, "c", live(3));
            assert_eq!(next, complete::Key(2));
            assert!(!behind(&mut readers, next));
            let missed = readers
                .open(Reader::Unnamed, Start::At(live(2)), 0, Charge::Whole)
                .key;
            assert!(behind(&mut readers, missed));
            let reader = Reader::Named {
                name: name("b"),
                hold: Span::from_nanos(10),
            };
            let resume = Start::Resume {
                presented: None,
                otherwise: live(9),
            };
            assert_eq!(
                readers.open(reader, resume, 0, Charge::Whole).position,
                live(0)
            );
        }
    }

    mod put {
        use super::*;

        #[test]
        fn makes_the_frame_wait_for_each_session() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let (a, b) = (unnamed(&mut readers), unnamed(&mut readers));
            assert_eq!(put(&mut readers, frames.frame(1)), [a, b]);
            assert_eq!(taken(&mut readers, a), Some(1));
            assert_eq!(taken(&mut readers, b), Some(1));
        }

        #[test]
        fn replaces_the_waiting_frame() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let key = unnamed(&mut readers);
            assert_eq!(put(&mut readers, frames.frame(1)), [key]);
            assert_eq!(put(&mut readers, frames.frame(2)), []);
            assert_eq!(taken(&mut readers, key), Some(2));
            assert_eq!(taken(&mut readers, key), None);
        }

        #[test]
        fn wakes_only_sessions_with_no_waiting_frame() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let (a, b, c) = (
                unnamed(&mut readers),
                unnamed(&mut readers),
                unnamed(&mut readers),
            );
            assert_eq!(put(&mut readers, frames.frame(1)), [a, b, c]);
            assert_eq!(taken(&mut readers, c), Some(1));
            assert_eq!(taken(&mut readers, a), Some(1));
            assert_eq!(put(&mut readers, frames.frame(2)), [a, c]);
        }

        #[test]
        fn wakes_no_closed_session() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let (a, b) = (unnamed(&mut readers), unnamed(&mut readers));
            readers.close(a.into());
            assert_eq!(put(&mut readers, frames.frame(1)), [b]);
        }

        #[test]
        fn wakes_no_complete_session() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            complete(&mut readers, "a", live(0));
            let key = unnamed(&mut readers);
            assert_eq!(put(&mut readers, frames.frame(1)), [key]);
        }

        #[test]
        fn releases_the_frame_it_replaces() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let key = unnamed(&mut readers);
            assert_eq!(put(&mut readers, frames.frame(1)), [key]);
            let second = frames.frame(2);
            assert!(matches!(
                frames.make(3),
                Err(types::frame::Error::Pool(block::Error::Exhausted { .. }))
            ));
            assert_eq!(put(&mut readers, second), []);
            assert_eq!(frames.make(3).as_ref().map(number), Ok(3));
            assert_eq!(taken(&mut readers, key), Some(2));
        }

        #[test]
        fn keeps_a_taken_frame_alive() {
            let frames = Frames::new(2);
            let mut readers = Readers::new(0);
            let key = unnamed(&mut readers);
            assert_eq!(put(&mut readers, frames.frame(1)), [key]);
            let sending = frame(readers.take(key.into())).expect("frame 1 waits");
            assert_eq!(put(&mut readers, frames.frame(2)), [key]);
            assert!(matches!(
                frames.make(3),
                Err(types::frame::Error::Pool(block::Error::Exhausted { .. }))
            ));
            assert_eq!(number(&sending), 1);
        }
    }

    mod take {
        use super::*;

        #[test]
        fn gives_a_closed_session_nothing() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let key = unnamed(&mut readers);
            assert_eq!(put(&mut readers, frames.frame(1)), [key]);
            readers.close(key.into());
            assert_eq!(taken(&mut readers, key), None);
        }

        #[test]
        #[should_panic(expected = "latest session 0 was never open")]
        fn panics_on_a_key_only_a_complete_session_had() {
            let mut readers = Readers::new(0);
            complete(&mut readers, "a", live(0));
            drop(readers.take(Key(0).into()));
        }

        #[test]
        #[should_panic(expected = "latest session 1 was never open")]
        fn panics_on_the_next_key() {
            let mut readers = Readers::new(0);
            unnamed(&mut readers);
            drop(readers.take(Key(1).into()));
        }

        #[test]
        #[should_panic(expected = "latest session 2 was never open")]
        fn panics_on_a_key_past_the_next() {
            let mut readers = Readers::new(0);
            unnamed(&mut readers);
            drop(readers.take(Key(2).into()));
        }
    }

    mod ack {
        use super::*;

        #[test]
        #[should_panic(expected = "complete session 0 was never open")]
        fn panics_on_a_key_only_a_latest_session_had() {
            let mut readers = Readers::new(0);
            unnamed(&mut readers);
            readers
                .ack(complete::Key(0), live(1))
                .expect("panics before");
        }
    }

    mod grant {
        use super::*;

        #[test]
        #[should_panic(expected = "complete session 0 was never open")]
        fn panics_on_a_key_only_a_latest_session_had() {
            let mut readers = Readers::new(0);
            unnamed(&mut readers);
            readers.grant(complete::Key(0), 10);
        }
    }

    mod close {
        use super::*;

        #[test]
        fn gives_a_new_session_only_the_newest_frame() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let old = unnamed(&mut readers);
            assert_eq!(put(&mut readers, frames.frame(1)), [old]);
            assert_eq!(put(&mut readers, frames.frame(2)), []);
            readers.close(old.into());
            let new = unnamed(&mut readers);
            assert_eq!(taken(&mut readers, new), Some(2));
            assert_eq!(taken(&mut readers, new), None);
        }

        #[test]
        fn of_a_closed_session_changes_nothing() {
            let frames = Frames::new(1);
            let mut readers = Readers::new(0);
            let key = unnamed(&mut readers);
            let other = unnamed(&mut readers);
            readers.close(key.into());
            dropped(&mut readers, key.into());
            assert_eq!(put(&mut readers, frames.frame(1)), [other]);
        }

        #[test]
        #[should_panic(expected = "latest session 0 was never open")]
        fn panics_on_a_key_only_a_complete_session_had() {
            let mut readers = Readers::new(0);
            complete(&mut readers, "a", live(0));
            readers.close(Key(0).into());
        }

        #[test]
        #[should_panic(expected = "latest session 2 was never open")]
        fn panics_on_a_key_past_the_next() {
            let mut readers = Readers::new(0);
            unnamed(&mut readers);
            readers.close(Key(2).into());
        }
    }

    mod rules {
        use super::*;

        #[derive(Clone, Copy, Debug)]
        enum Input {
            Open,
            Complete,
            Put,
            Take(usize),
            Close(usize),
        }

        fn input() -> impl Strategy<Value = Input> {
            prop_oneof![
                Just(Input::Open),
                Just(Input::Complete),
                Just(Input::Put),
                any::<usize>().prop_map(Input::Take),
                any::<usize>().prop_map(Input::Close),
            ]
        }

        /// The rules stated a second way: each latest session's mailbox holds a frame
        /// number or nothing, and complete sessions get no frame from a put.
        #[derive(Default)]
        struct Model {
            newest: Option<u64>,
            mailboxes: BTreeMap<Key, Option<u64>>,
            /// Each latest key ever opened.
            latest: BTreeSet<Key>,
            complete: BTreeSet<complete::Key>,
        }

        impl Model {
            fn nth(&self, i: usize) -> Option<Key> {
                let open = self.mailboxes.len();
                (open > 0)
                    .then(|| *self.mailboxes.keys().nth(i % open).expect("in range"))
            }
        }

        fn check(inputs: Vec<Input>) {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let mut model = Model::default();
            let mut n = 0;
            for input in inputs {
                match input {
                    Input::Open => {
                        let latest = readers.open_latest();
                        assert!(
                            model.latest.insert(latest.key),
                            "{} is new",
                            latest.key
                        );
                        model.mailboxes.insert(latest.key, model.newest);
                    }
                    Input::Complete => {
                        let key = readers
                            .open(Reader::Unnamed, Start::At(live(0)), 0, Charge::Whole)
                            .key;
                        assert!(model.complete.insert(key), "{key} is new");
                    }
                    Input::Put => {
                        n += 1;
                        let empty: Vec<Key> = model
                            .mailboxes
                            .iter()
                            .filter(|(_, mailbox)| mailbox.is_none())
                            .map(|(key, _)| *key)
                            .collect();
                        assert_eq!(put(&mut readers, frames.frame(n)), empty);
                        model.newest = Some(n);
                        model.mailboxes.values_mut().for_each(|m| *m = Some(n));
                    }
                    Input::Take(i) => {
                        if let Some(key) = model.nth(i) {
                            let expected = model.mailboxes.insert(key, None).flatten();
                            assert_eq!(taken(&mut readers, key), expected);
                        }
                    }
                    Input::Close(i) => {
                        if let Some(key) = model.nth(i) {
                            readers.close(key.into());
                            model.mailboxes.remove(&key);
                        }
                    }
                }
                for key in &model.latest {
                    if !model.mailboxes.contains_key(key) {
                        dropped(&mut readers, (*key).into());
                    }
                }
            }
            for (&key, mailbox) in &model.mailboxes {
                assert_eq!(taken(&mut readers, key), *mailbox);
            }
            let open: Vec<Key> = model.mailboxes.keys().copied().collect();
            assert_eq!(put(&mut readers, frames.frame(n + 1)), open);
        }

        proptest! {
            #[test]
            fn follow_the_latest_rules(
                inputs in proptest::collection::vec(input(), 0..60),
            ) {
                check(inputs);
            }
        }
    }
}

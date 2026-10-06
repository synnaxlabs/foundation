//! The latest sessions of [`Readers`].

use std::mem;

use types::frame::Frame;
use types::name::Name;
use types::time::Stamp;

use super::{Key, Readers};

/// A latest session that [`Readers::open_latest`] started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct Latest {
    /// The session.
    pub key: Key,
    /// The session of the same named reader that this one took over. It is closed.
    pub replaced: Option<Key>,
    /// The index's newest live frame waits for the session: wake it.
    pub woken: bool,
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
    /// Starts a latest session, which gets the index's newest live frame, if any, at
    /// once. A named reader's open session in either mode is taken over: a complete one
    /// closes at `now`, as after [`Readers::close`]. A latest session holds nothing and
    /// writes no record.
    pub fn open_latest(&mut self, name: Option<Name>, now: Stamp) -> Latest {
        let replaced = name.as_ref().and_then(|name| self.close_named(name, now));
        let key = self.key();
        let woken = self.newest.is_some();
        self.latest.push(Session {
            key,
            name,
            waiting: woken,
        });
        self.woken.clear();
        self.woken.reserve(self.latest.len());
        Latest {
            key,
            replaced,
            woken,
        }
    }

    /// Makes `frame`, which landed on the live path, the newest frame and the waiting
    /// frame of every latest session, and drops the frame it replaces. Returns the
    /// latest sessions that had no waiting frame, in key order: wake them.
    #[must_use]
    pub fn put(&mut self, frame: Frame) -> &[Key] {
        self.newest = Some(frame);
        self.woken.clear();
        for session in &mut self.latest {
            if !mem::replace(&mut session.waiting, true) {
                self.woken.push(session.key);
            }
        }
        &self.woken
    }

    /// Takes the latest session's waiting frame, or `None` when it has none.
    ///
    /// # Panics
    ///
    /// If the latest session is not open.
    pub(super) fn take_latest(&mut self, key: Key) -> Option<Frame> {
        let i = self
            .latest
            .binary_search_by_key(&key, |session| session.key)
            .unwrap_or_else(|_| panic!("session {key} is not open"));
        if mem::replace(&mut self.latest[i].waiting, false) {
            self.newest.clone()
        } else {
            None
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
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;
    use types::time::Span;

    use super::*;
    use crate::readers::tests::{Frames, number};
    use crate::{Position, Reader, Record, Start};

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

    fn complete(readers: &mut Readers, name: &str, position: Position) -> Key {
        let reader = Reader::Named {
            name: self::name(name),
            hold: Span::from_nanos(10),
        };
        readers.open(reader, Start::At(position), 0).key
    }

    fn unnamed(readers: &mut Readers) -> Key {
        readers.open_latest(None, at(0)).key
    }

    fn taken(readers: &mut Readers, key: Key) -> Option<u64> {
        readers.take(key).as_ref().map(number)
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
            let latest = readers.open_latest(None, at(0));
            assert!(latest.woken);
            assert_eq!(taken(&mut readers, latest.key), Some(2));
            assert_eq!(taken(&mut readers, latest.key), None);
        }

        #[test]
        fn gets_nothing_before_the_first_frame() {
            let mut readers = Readers::new(0);
            let latest = readers.open_latest(None, at(0));
            assert!(!latest.woken);
            assert_eq!(taken(&mut readers, latest.key), None);
        }

        #[test]
        fn shares_the_key_space_with_complete_sessions() {
            let mut readers = Readers::new(0);
            let a = complete(&mut readers, "a", live(0));
            let b = unnamed(&mut readers);
            readers.close(b, at(0));
            let c = unnamed(&mut readers);
            assert!(a != b && b != c && a != c);
        }

        #[test]
        fn takes_over_a_latest_session() {
            let mut readers = Readers::new(0);
            let old = readers.open_latest(Some(name("a")), at(0)).key;
            let new = readers.open_latest(Some(name("a")), at(1));
            assert_eq!(new.replaced, Some(old));
            readers.close(new.key, at(2));
            assert_eq!(readers.open_latest(Some(name("a")), at(3)).replaced, None);
        }

        #[test]
        #[should_panic(expected = "session 0 is not open")]
        fn closes_the_latest_session_it_takes_over() {
            let mut readers = Readers::new(0);
            let old = readers.open_latest(Some(name("a")), at(0)).key;
            let new = readers.open_latest(Some(name("a")), at(1));
            assert_eq!(new.replaced, Some(old));
            readers.take(old);
        }

        #[test]
        fn closes_the_complete_session_it_takes_over() {
            let mut readers = Readers::new(0);
            let old = complete(&mut readers, "a", live(5));
            assert_eq!(readers.records().count(), 1, "the open record");
            let new = readers.open_latest(Some(name("a")), at(3));
            assert_eq!(new.replaced, Some(old));
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
            readers.close(old, at(1));
            let latest = readers.open_latest(Some(name("a")), at(2)).key;
            assert_eq!(readers.floor(), Some(live(5)));
            let reader = Reader::Named {
                name: name("a"),
                hold: Span::from_nanos(10),
            };
            let resume = Start::Resume {
                presented: None,
                otherwise: live(0),
            };
            let opened = readers.open(reader, resume, 0);
            assert_eq!(opened.replaced, Some(latest));
            assert_eq!(opened.position, live(5));
        }

        #[test]
        fn holds_nothing_and_writes_no_record() {
            let frames = Frames::new(4);
            let mut readers = Readers::new(0);
            let key = readers.open_latest(Some(name("a")), at(0)).key;
            assert_eq!(put(&mut readers, frames.frame(1)), [key]);
            readers.close(key, at(1));
            readers.flush();
            assert_eq!(readers.records().count(), 0);
            assert_eq!(readers.floor(), None);
            assert_eq!(readers.deadline(), None);
        }
    }

    mod open {
        use super::*;

        #[test]
        #[should_panic(expected = "session 0 is not open")]
        fn closes_the_latest_session_it_takes_over() {
            let mut readers = Readers::new(0);
            let old = readers.open_latest(Some(name("a")), at(0)).key;
            assert_eq!(complete(&mut readers, "a", live(0)), Key(1));
            readers.take(old);
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
            readers.close(a, at(0));
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
            let sending = readers.take(key).expect("frame 1 waits");
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
        #[should_panic(expected = "session 0 is not open")]
        fn panics_on_a_closed_session() {
            let mut readers = Readers::new(0);
            let key = unnamed(&mut readers);
            readers.close(key, at(0));
            readers.take(key);
        }
    }

    mod ack {
        use super::*;

        #[test]
        #[should_panic(expected = "complete session 0 is not open")]
        fn panics_on_a_latest_session() {
            let mut readers = Readers::new(0);
            let key = unnamed(&mut readers);
            readers.ack(key, live(1)).expect("panics before");
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
            readers.close(old, at(0));
            let new = unnamed(&mut readers);
            assert_eq!(taken(&mut readers, new), Some(2));
            assert_eq!(taken(&mut readers, new), None);
        }

        #[test]
        #[should_panic(expected = "session 0 is not open")]
        fn panics_on_a_closed_session() {
            let mut readers = Readers::new(0);
            let key = unnamed(&mut readers);
            readers.close(key, at(0));
            readers.close(key, at(1));
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
            complete: BTreeSet<Key>,
        }

        impl Model {
            fn nth(&self, i: usize) -> Option<Key> {
                let open = self.mailboxes.len();
                (open > 0)
                    .then(|| *self.mailboxes.keys().nth(i % open).expect("in range"))
            }

            fn fresh(&self, key: Key) -> bool {
                !self.mailboxes.contains_key(&key) && !self.complete.contains(&key)
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
                        let latest = readers.open_latest(None, at(0));
                        assert!(model.fresh(latest.key), "{} is new", latest.key);
                        assert_eq!(latest.woken, model.newest.is_some());
                        model.mailboxes.insert(latest.key, model.newest);
                    }
                    Input::Complete => {
                        let key =
                            readers.open(Reader::Unnamed, Start::At(live(0)), 0).key;
                        assert!(model.fresh(key), "{key} is new");
                        model.complete.insert(key);
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
                            readers.close(key, at(0));
                            model.mailboxes.remove(&key);
                        }
                    }
                }
            }
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

//! The latest readers of one index at its home.

use std::mem;

use types::frame::Frame;

use crate::Key;

/// The latest readers of one index at its home: the index's newest live frame, and for
/// each session at most one frame that waits to go out. A newer frame replaces the
/// waiting one. Sans-I/O: the home puts each frame before the disk sync, and takes a
/// session's frame when its connection has room.
#[derive(Debug, Default)]
pub struct Latest {
    newest: Option<Frame>,
    /// Sorted by key.
    sessions: Vec<Session>,
    next: u64,
    /// The last [`Latest::put`]'s result, kept so that a put does not allocate.
    woken: Vec<Key>,
}

#[derive(Debug)]
struct Session {
    key: Key,
    /// The newest frame waits for the session. Only the newest can wait: each put
    /// makes it wait for every session.
    waiting: bool,
}

impl Latest {
    /// No sessions and no frame.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a session, and gives it the index's newest live frame, if any, at once.
    /// Treat the new session as woken.
    pub fn open(&mut self) -> Key {
        let key = Key(self.next);
        self.next += 1;
        self.sessions.push(Session {
            key,
            waiting: self.newest.is_some(),
        });
        key
    }

    /// Makes `frame`, which landed on the live path, the newest frame and the waiting
    /// frame of every session, and drops the frame it replaces. Returns the sessions
    /// that had no waiting frame, in key order: wake them.
    #[must_use]
    pub fn put(&mut self, frame: Frame) -> &[Key] {
        self.newest = Some(frame);
        self.woken.clear();
        for session in &mut self.sessions {
            if !mem::replace(&mut session.waiting, true) {
                self.woken.push(session.key);
            }
        }
        &self.woken
    }

    /// Takes the session's waiting frame, or `None` when it has none.
    ///
    /// # Panics
    ///
    /// If the session is not open.
    pub fn take(&mut self, key: Key) -> Option<Frame> {
        let i = self.find(key);
        if mem::replace(&mut self.sessions[i].waiting, false) {
            self.newest.clone()
        } else {
            None
        }
    }

    /// Ends the session. Its waiting frame does not go out.
    ///
    /// # Panics
    ///
    /// If the session is not open.
    pub fn close(&mut self, key: Key) {
        let i = self.find(key);
        self.sessions.remove(i);
    }

    fn find(&self, key: Key) -> usize {
        self.sessions
            .binary_search_by_key(&key, |session| session.key)
            .unwrap_or_else(|_| panic!("session {key} is not open"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use proptest::prelude::*;
    use types::channel::Slot;
    use types::frame::key_set::{Group, Interner, KeySet};
    use types::frame::{Draft, Form, Path};

    use super::*;

    /// Frames of one index with no data channels. Frame `n` holds `n` in its index
    /// series.
    struct Frames {
        pool: block::Pool,
        set: Arc<KeySet>,
    }

    impl Frames {
        /// Room for `count` frames at once. Each takes a 64-byte block and the block's
        /// 64-byte header.
        fn new(count: usize) -> Self {
            let config = block::Config {
                budget: count * 128,
            };
            let memory = block::Heap::new(config.reservation());
            let index = Group {
                index: Slot::new(1),
                data: &[],
            };
            Self {
                pool: block::Pool::new(config, memory),
                set: Interner::new().intern(&[index]),
            }
        }

        fn make(&self, n: u64) -> Result<Frame, types::frame::Error> {
            let series = [(0, 8)];
            let mut draft =
                Draft::new(&self.pool, &self.set, Path::Live, Form::Raw, &series)?;
            let bytes = draft.series(0).expect("the index is present");
            bytes.copy_from_slice(&n.to_le_bytes());
            Ok(draft.freeze())
        }

        fn frame(&self, n: u64) -> Frame {
            self.make(n).expect("the pool has room")
        }
    }

    fn number(frame: &Frame) -> u64 {
        let bytes = frame.series(0).expect("the index is present");
        u64::from_le_bytes(bytes.try_into().expect("8 bytes"))
    }

    fn taken(latest: &mut Latest, key: Key) -> Option<u64> {
        latest.take(key).as_ref().map(number)
    }

    fn put(latest: &mut Latest, frame: Frame) -> Vec<Key> {
        latest.put(frame).to_vec()
    }

    mod open {
        use super::*;

        #[test]
        fn gets_the_newest_frame_at_once() {
            let frames = Frames::new(4);
            let mut latest = Latest::new();
            assert_eq!(put(&mut latest, frames.frame(1)), []);
            assert_eq!(put(&mut latest, frames.frame(2)), []);
            let key = latest.open();
            assert_eq!(taken(&mut latest, key), Some(2));
            assert_eq!(taken(&mut latest, key), None);
        }

        #[test]
        fn gets_nothing_before_the_first_frame() {
            let mut latest = Latest::new();
            let key = latest.open();
            assert_eq!(taken(&mut latest, key), None);
        }

        #[test]
        fn gives_each_session_a_new_key() {
            let mut latest = Latest::new();
            let a = latest.open();
            latest.close(a);
            let b = latest.open();
            assert_ne!(a, b);
        }
    }

    mod put {
        use super::*;

        #[test]
        fn makes_the_frame_wait_for_each_session() {
            let frames = Frames::new(4);
            let mut latest = Latest::new();
            let (a, b) = (latest.open(), latest.open());
            assert_eq!(put(&mut latest, frames.frame(1)), [a, b]);
            assert_eq!(taken(&mut latest, a), Some(1));
            assert_eq!(taken(&mut latest, b), Some(1));
        }

        #[test]
        fn replaces_the_waiting_frame() {
            let frames = Frames::new(4);
            let mut latest = Latest::new();
            let key = latest.open();
            assert_eq!(put(&mut latest, frames.frame(1)), [key]);
            assert_eq!(put(&mut latest, frames.frame(2)), []);
            assert_eq!(taken(&mut latest, key), Some(2));
            assert_eq!(taken(&mut latest, key), None);
        }

        #[test]
        fn wakes_only_sessions_with_no_waiting_frame() {
            let frames = Frames::new(4);
            let mut latest = Latest::new();
            let (a, b, c) = (latest.open(), latest.open(), latest.open());
            assert_eq!(put(&mut latest, frames.frame(1)), [a, b, c]);
            assert_eq!(taken(&mut latest, c), Some(1));
            assert_eq!(taken(&mut latest, a), Some(1));
            assert_eq!(put(&mut latest, frames.frame(2)), [a, c]);
        }

        #[test]
        fn wakes_no_closed_session() {
            let frames = Frames::new(4);
            let mut latest = Latest::new();
            let (a, b) = (latest.open(), latest.open());
            latest.close(a);
            assert_eq!(put(&mut latest, frames.frame(1)), [b]);
        }

        #[test]
        fn releases_the_frame_it_replaces() {
            let frames = Frames::new(2);
            let mut latest = Latest::new();
            let key = latest.open();
            assert_eq!(put(&mut latest, frames.frame(1)), [key]);
            let second = frames.frame(2);
            assert!(matches!(
                frames.make(3),
                Err(types::frame::Error::Pool(block::Error::Exhausted { .. }))
            ));
            assert_eq!(put(&mut latest, second), []);
            assert_eq!(frames.make(3).as_ref().map(number), Ok(3));
            assert_eq!(taken(&mut latest, key), Some(2));
        }

        #[test]
        fn keeps_a_taken_frame_alive() {
            let frames = Frames::new(2);
            let mut latest = Latest::new();
            let key = latest.open();
            assert_eq!(put(&mut latest, frames.frame(1)), [key]);
            let sending = latest.take(key).expect("frame 1 waits");
            assert_eq!(put(&mut latest, frames.frame(2)), [key]);
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
            let mut latest = Latest::new();
            let key = latest.open();
            latest.close(key);
            latest.take(key);
        }
    }

    mod close {
        use super::*;

        #[test]
        fn gives_a_new_session_only_the_newest_frame() {
            let frames = Frames::new(4);
            let mut latest = Latest::new();
            let old = latest.open();
            assert_eq!(put(&mut latest, frames.frame(1)), [old]);
            assert_eq!(put(&mut latest, frames.frame(2)), []);
            latest.close(old);
            let new = latest.open();
            assert_eq!(taken(&mut latest, new), Some(2));
            assert_eq!(taken(&mut latest, new), None);
        }

        #[test]
        #[should_panic(expected = "session 0 is not open")]
        fn panics_on_a_closed_session() {
            let mut latest = Latest::new();
            let key = latest.open();
            latest.close(key);
            latest.close(key);
        }
    }

    mod rules {
        use super::*;

        #[derive(Clone, Copy, Debug)]
        enum Input {
            Open,
            Put,
            Take(usize),
            Close(usize),
        }

        fn input() -> impl Strategy<Value = Input> {
            prop_oneof![
                Just(Input::Open),
                Just(Input::Put),
                any::<usize>().prop_map(Input::Take),
                any::<usize>().prop_map(Input::Close),
            ]
        }

        /// The rules stated a second way: each session's mailbox holds a frame number
        /// or nothing.
        #[derive(Default)]
        struct Model {
            newest: Option<u64>,
            mailboxes: BTreeMap<Key, Option<u64>>,
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
            let mut latest = Latest::new();
            let mut model = Model::default();
            let mut n = 0;
            for input in inputs {
                match input {
                    Input::Open => {
                        let key = latest.open();
                        assert!(!model.mailboxes.contains_key(&key), "{key} is new");
                        model.mailboxes.insert(key, model.newest);
                    }
                    Input::Put => {
                        n += 1;
                        let empty: Vec<Key> = model
                            .mailboxes
                            .iter()
                            .filter(|(_, mailbox)| mailbox.is_none())
                            .map(|(key, _)| *key)
                            .collect();
                        assert_eq!(put(&mut latest, frames.frame(n)), empty);
                        model.newest = Some(n);
                        model.mailboxes.values_mut().for_each(|m| *m = Some(n));
                    }
                    Input::Take(i) => {
                        if let Some(key) = model.nth(i) {
                            let expected = model.mailboxes.insert(key, None).flatten();
                            assert_eq!(taken(&mut latest, key), expected);
                        }
                    }
                    Input::Close(i) => {
                        if let Some(key) = model.nth(i) {
                            latest.close(key);
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

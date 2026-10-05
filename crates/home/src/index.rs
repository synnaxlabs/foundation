//! One index of a shard: who may write it, the order of its samples, and its newest
//! frame.

use std::fmt;

use control::{Gate, Handoff, Lease};
use delivery::Readers;
use types::frame::{Frame, Path};
use types::time::{Interval, Monotonic};

use crate::order::{self, Accepted, Order, Tail};

/// One index of a shard. It reads no clock: each input takes the time.
#[derive(Debug)]
pub(crate) struct Index {
    gate: Gate,
    order: Order,
    readers: Readers,
    /// The change of holder that the log does not hold yet.
    handoff: Option<Handoff>,
}

impl Index {
    /// An index whose paths stand at `live` and `backfill`, with an empty gate.
    pub(crate) fn new(limits: order::Config, live: Tail, backfill: Tail) -> Self {
        Self {
            gate: Gate::new(),
            order: Order::new(limits, live, backfill),
            readers: Readers::new(),
            handoff: None,
        }
    }

    /// Opens a writer at `now`.
    pub(crate) fn open(
        &mut self,
        writer: control::Writer,
        lease: Option<Lease>,
        now: Monotonic,
    ) -> control::Key {
        let key = self.gate.open(writer, lease, now);
        self.note();
        key
    }

    /// Closes the writer `key` at `now`.
    ///
    /// # Panics
    ///
    /// If `key` is not open.
    pub(crate) fn close(&mut self, key: control::Key, now: Monotonic) {
        self.gate.close(key, now);
        self.note();
    }

    /// Checks a frame from `key` whose index series on `path` is `stamps`, at
    /// monotonic time `now` and mesh time `mesh`, and returns the seq its samples take.
    /// Only the gate changes: the holder's control lease renews, and a lease that ran
    /// out hands control on.
    ///
    /// # Errors
    ///
    /// [`Refusal::Control`] when `key` does not hold control, else
    /// [`Refusal::Order`] when a stamp breaks a rule.
    ///
    /// # Panics
    ///
    /// If `key` is not open.
    pub(crate) fn check(
        &mut self,
        key: control::Key,
        path: Path,
        stamps: &[[u8; 8]],
        now: Monotonic,
        mesh: Interval,
    ) -> Result<Accepted, Refusal> {
        let gate = self.gate.write(key, now);
        self.note();
        gate.map_err(Refusal::Control)?;
        self.order.check(path, stamps, mesh).map_err(Refusal::Order)
    }

    /// Spends the seq of `accepted`. A live `frame` becomes the newest frame, and the
    /// latest sessions to wake are returned. `frame` is `None` when the pool had no
    /// room for it.
    ///
    /// # Panics
    ///
    /// If either path moved after the check.
    pub(crate) fn advance(
        &mut self,
        accepted: Accepted,
        frame: Option<Frame>,
    ) -> &[delivery::Key] {
        let path = accepted.path;
        self.order.advance(accepted);
        match (path, frame) {
            (Path::Live, Some(frame)) => self.readers.put(frame),
            (Path::Live, None) | (Path::Backfill, _) => &[],
        }
    }

    /// The change of holder to record before the index's next frame, if any.
    pub(crate) fn handoff(&self) -> Option<&Handoff> {
        self.handoff.as_ref()
    }

    /// Drops the change of holder that [`Index::handoff`] gave. Call it once the log
    /// holds that change.
    pub(crate) fn clear_handoff(&mut self) {
        self.handoff = None;
    }

    /// Keeps the gate's change of holder, if any. A newer change replaces one that
    /// was not recorded: no frame was stored under it.
    fn note(&mut self) {
        if let Some(handoff) = self.gate.handoff() {
            self.handoff = Some(handoff);
        }
    }
}

/// Why an index refused a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The gate refused the write.
    Control(control::Error),
    /// A stamp broke a rule.
    Order(order::Error),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Control(error) => error.fmt(f),
            Self::Order(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Refusal {}

#[cfg(test)]
mod tests {
    use std::ops::Range;
    use std::sync::Arc;

    use control::Writer;
    use types::authority::Authority;
    use types::channel::Slot;
    use types::frame::key_set::{Group, Interner, KeySet};
    use types::frame::{Draft, Form};
    use types::time::{Span, Stamp};

    use super::*;

    /// Index frames of one index with no data channels.
    struct Frames {
        pool: block::Pool,
        set: Arc<KeySet>,
    }

    impl Frames {
        fn new() -> Self {
            let config = block::Config { budget: 4096 };
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

        fn frame(&self, path: Path, stamps: &[[u8; 8]]) -> Frame {
            let series = [(0, stamps.len() * 8)];
            let mut draft = Draft::new(&self.pool, &self.set, Form::Raw, &series)
                .expect("the pool has room");
            let bytes = draft.series(0).expect("the index is present");
            bytes.copy_from_slice(stamps.as_flattened());
            draft.freeze(path)
        }
    }

    fn writer(subject: &str, authority: u8) -> Writer {
        Writer {
            subject: subject.parse().expect("a valid name"),
            authority: Authority(authority),
        }
    }

    /// Second `seconds` of 2026-10-05.
    fn s(seconds: i64) -> Stamp {
        let day: Stamp = "2026-10-05T00:00:00Z".parse().expect("a valid stamp");
        day + Span::from_nanos(seconds * Span::SECOND.nanos())
    }

    fn stamps(seconds: &[i64]) -> Vec<[u8; 8]> {
        seconds
            .iter()
            .map(|&n| s(n).nanos().to_le_bytes())
            .collect()
    }

    /// Mesh time in the tests: the latest stamp accepted is `s(61)`.
    fn mesh() -> Interval {
        Interval {
            earliest: s(59),
            latest: s(60),
        }
    }

    fn index() -> Index {
        let limits = order::Config {
            earliest: "2000-01-01T00:00:00Z".parse().expect("a valid stamp"),
            ahead: Span::SECOND,
        };
        Index::new(limits, Tail::default(), Tail::default())
    }

    fn at(nanos: u64) -> Monotonic {
        Monotonic(nanos)
    }

    fn lease(nanos: i64) -> Lease {
        Lease::new(Span::from_nanos(nanos)).expect("a lease longer than zero")
    }

    /// Checks a live frame of `seconds` from `key` at `now`, and spends its seq.
    fn write(
        index: &mut Index,
        key: control::Key,
        seconds: &[i64],
        now: Monotonic,
    ) -> Result<Range<u64>, Refusal> {
        let accepted = index.check(key, Path::Live, &stamps(seconds), now, mesh())?;
        let seq = accepted.seq.clone();
        let _ = index.advance(accepted, None);
        Ok(seq)
    }

    fn handed_to(index: &Index) -> Option<Writer> {
        index.handoff().map(|handoff| handoff.to.clone())?
    }

    mod check {
        use super::*;

        #[test]
        fn refuses_a_writer_without_control_and_spends_nothing() {
            let mut index = index();
            let holder = index.open(writer("a", 10), None, at(0));
            let waiter = index.open(writer("b", 5), None, at(0));
            let refusal = write(&mut index, waiter, &[1, 2], at(1))
                .expect_err("b does not hold control");
            assert_eq!(refusal, Refusal::Control(control::Error::Waiting));
            assert_eq!(
                refusal.to_string(),
                "not in control: another writer holds the gate"
            );
            assert_eq!(write(&mut index, holder, &[1, 2], at(2)), Ok(0..2));
        }

        #[test]
        fn refuses_a_waiter_for_control_before_its_stamps() {
            let mut index = index();
            let holder = index.open(writer("a", 10), None, at(0));
            let waiter = index.open(writer("b", 5), None, at(0));
            assert_eq!(write(&mut index, holder, &[5], at(1)), Ok(0..1));
            assert_eq!(
                write(&mut index, waiter, &[4], at(2)),
                Err(Refusal::Control(control::Error::Waiting))
            );
        }

        #[test]
        fn refuses_a_backwards_stamp_with_the_order_error() {
            let mut index = index();
            let key = index.open(writer("a", 10), None, at(0));
            assert_eq!(write(&mut index, key, &[5, 6], at(1)), Ok(0..2));
            let refusal =
                write(&mut index, key, &[4], at(2)).expect_err("a backwards stamp");
            assert_eq!(
                refusal,
                Refusal::Order(order::Error::Backwards {
                    path: Path::Live,
                    before: s(6),
                    stamp: s(4),
                })
            );
            assert_eq!(
                refusal.to_string(),
                "live stamp 2026-10-05T00:00:04.000000000Z is not after \
                 2026-10-05T00:00:06.000000000Z: write late data on the backfill path"
            );
            assert_eq!(write(&mut index, key, &[7], at(3)), Ok(2..3));
        }

        #[test]
        fn spends_no_seq_until_advance() {
            let mut index = index();
            let key = index.open(writer("a", 10), None, at(0));
            let series = stamps(&[1, 2]);
            let first = index.check(key, Path::Live, &series, at(1), mesh());
            let again = index.check(key, Path::Live, &series, at(2), mesh());
            assert_eq!(first, again);
            assert_eq!(first.map(|accepted| accepted.seq), Ok(0..2));
        }

        #[test]
        fn hands_control_on_when_the_holder_lease_ran_out() {
            let mut index = index();
            let _ = index.open(writer("a", 10), Some(lease(10)), at(0));
            let waiter = index.open(writer("b", 5), None, at(0));
            index.clear_handoff();
            assert_eq!(write(&mut index, waiter, &[1], at(20)), Ok(0..1));
            assert_eq!(handed_to(&index), Some(writer("b", 5)));
        }

        #[test]
        fn keeps_a_handoff_from_a_refused_write() {
            let mut index = index();
            let holder = index.open(writer("a", 10), Some(lease(10)), at(0));
            let _ = index.open(writer("b", 5), None, at(0));
            index.clear_handoff();
            let refusal = write(&mut index, holder, &[1], at(20));
            assert_eq!(refusal, Err(Refusal::Control(control::Error::Expired)));
            assert_eq!(handed_to(&index), Some(writer("b", 5)));
        }
    }

    mod advance {
        use super::*;

        #[test]
        fn makes_a_live_frame_the_newest_and_wakes_latest_sessions() {
            let frames = Frames::new();
            let mut index = index();
            let key = index.open(writer("a", 10), None, at(0));
            let session = index.readers.open_latest(None, s(0)).key;
            let series = stamps(&[1, 2]);
            let accepted = index
                .check(key, Path::Live, &series, at(1), mesh())
                .expect("a holder's frame in order");
            let frame = frames.frame(Path::Live, &series);
            assert_eq!(index.advance(accepted, Some(frame.clone())), &[session]);
            let taken = index.readers.take(session).expect("the newest frame");
            assert_eq!(taken.series(0), frame.series(0));
        }

        #[test]
        fn keeps_a_backfill_frame_from_latest_sessions() {
            let frames = Frames::new();
            let mut index = index();
            let key = index.open(writer("a", 10), None, at(0));
            let session = index.readers.open_latest(None, s(0)).key;
            let series = stamps(&[1, 2]);
            let accepted = index
                .check(key, Path::Backfill, &series, at(1), mesh())
                .expect("a holder's frame in order");
            assert_eq!(
                index.advance(accepted, Some(frames.frame(Path::Backfill, &series))),
                &[]
            );
            assert!(index.readers.take(session).is_none());
            assert_eq!(write(&mut index, key, &[3], at(2)), Ok(0..1));
        }

        #[test]
        fn spends_the_seq_of_a_frame_the_pool_had_no_room_for() {
            let mut index = index();
            let key = index.open(writer("a", 10), None, at(0));
            let session = index.readers.open_latest(None, s(0)).key;
            assert_eq!(write(&mut index, key, &[1, 2], at(1)), Ok(0..2));
            assert!(index.readers.take(session).is_none());
            assert_eq!(write(&mut index, key, &[3], at(2)), Ok(2..3));
        }

        #[test]
        #[should_panic(expected = "invariant: the order moved after the check")]
        fn panics_when_the_path_moved_after_the_check() {
            let mut index = index();
            let key = index.open(writer("a", 10), None, at(0));
            let first = index.check(key, Path::Live, &stamps(&[1]), at(1), mesh());
            let first = first.expect("a holder's frame in order");
            assert_eq!(write(&mut index, key, &[1], at(2)), Ok(0..1));
            let _ = index.advance(first, None);
        }
    }

    mod handoff {
        use proptest::prelude::*;

        use super::*;

        #[test]
        fn is_kept_until_recorded() {
            let mut index = index();
            let first = index.open(writer("a", 5), None, at(0));
            assert_eq!(handed_to(&index), Some(writer("a", 5)));
            index.clear_handoff();
            assert_eq!(index.handoff(), None);
            let _ = index.open(writer("b", 1), None, at(1));
            assert_eq!(index.handoff(), None);
            let _ = index.open(writer("c", 9), None, at(2));
            assert_eq!(handed_to(&index), Some(writer("c", 9)));
            index.close(first, at(3));
            assert_eq!(handed_to(&index), Some(writer("c", 9)));
        }

        #[test]
        fn names_an_empty_gate_after_the_last_close() {
            let mut index = index();
            let key = index.open(writer("a", 5), None, at(0));
            index.clear_handoff();
            index.close(key, at(1));
            assert_eq!(index.handoff(), Some(&Handoff { to: None }));
        }

        #[derive(Clone, Debug)]
        enum Input {
            Open { authority: u8, lease: Option<i64> },
            Close(usize),
            Write(usize),
            Record,
        }

        fn input() -> impl Strategy<Value = Input> {
            prop_oneof![
                (0..4_u8, proptest::option::of(1..20_i64))
                    .prop_map(|(authority, lease)| Input::Open { authority, lease }),
                any::<usize>().prop_map(Input::Close),
                any::<usize>().prop_map(Input::Write),
                Just(Input::Record),
            ]
        }

        proptest! {
            /// The last recorded holder, or the change that waits to be recorded,
            /// names the gate's holder after every input.
            #[test]
            fn and_the_log_name_the_holder(
                inputs in proptest::collection::vec(input(), 0..60),
            ) {
                let mut index = index();
                let mut open = Vec::new();
                let mut logged = None;
                let mut stamp = 0;
                for (step, input) in inputs.into_iter().enumerate() {
                    let now = at(step as u64 * 5);
                    match input {
                        Input::Open { authority, lease } => {
                            let subject = format!("w{step}");
                            let writer = writer(&subject, authority);
                            let lease = lease.map(super::lease);
                            open.push(index.open(writer, lease, now));
                        }
                        Input::Close(n) if !open.is_empty() => {
                            let key = open.swap_remove(n % open.len());
                            index.close(key, now);
                        }
                        Input::Write(n) if !open.is_empty() => {
                            stamp += 1;
                            let key = open[n % open.len()];
                            let written = write(&mut index, key, &[stamp], now);
                            if let Err(refusal) = written {
                                prop_assert!(
                                    matches!(refusal, Refusal::Control(_)),
                                    "stamps in order were refused: {refusal}",
                                );
                            }
                        }
                        Input::Record => {
                            if let Some(handoff) = index.handoff() {
                                logged = handoff.to.clone();
                            }
                            index.clear_handoff();
                        }
                        Input::Close(_) | Input::Write(_) => {}
                    }
                    let named = index.handoff().map_or(&logged, |h| &h.to);
                    prop_assert_eq!(named.as_ref(), index.gate.holder());
                }
            }
        }
    }
}

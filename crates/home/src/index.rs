//! One index of a shard: who may write it, the order of its samples, and its newest
//! frame.

use std::fmt;
use std::ops::Range;

use control::{Gate, Permit};
use delivery::Readers;
use types::frame::{self, Draft, Frame, Path};
use types::time::{Interval, Monotonic};

use crate::order::{self, Order, Tail};

/// One index of a shard. It reads no clock: each input takes the time.
#[derive(Debug)]
pub(crate) struct Index {
    /// Who may write the index. The shard opens and closes writers on it, and records
    /// each [`Gate::handoff`] before the index's next frame.
    pub(crate) gate: Gate,
    order: Order,
    readers: Readers,
}

/// A frame that passed [`Index::check`]. [`Index::advance`] spends it.
#[derive(Debug)]
pub(crate) struct Accepted {
    order: order::Accepted,
    permit: Permit,
    frame: Option<Frame>,
}

impl Accepted {
    /// The seq the frame's samples take.
    pub(crate) fn seq(&self) -> Range<u64> {
        self.order.seq.clone()
    }

    /// Sets the seq of `group` in `draft` and freezes it on the accepted path. The
    /// frame becomes the newest frame at [`Index::advance`].
    ///
    /// # Panics
    ///
    /// If `group` is absent from `draft`, or a frame was frozen already.
    pub(crate) fn freeze(&mut self, mut draft: Draft, group: u32) -> &Frame {
        assert!(self.frame.is_none(), "invariant: a frame is frozen once");
        let seq = &self.order.seq;
        let count = u32::try_from(seq.end - seq.start)
            .expect("invariant: a group holds fewer than 2^32 samples");
        draft.set_range(
            group,
            frame::Range {
                seq: seq.start,
                count,
            },
        );
        self.frame.insert(draft.freeze(self.order.path))
    }
}

impl Index {
    /// An index whose paths stand at `live` and `backfill`, with an empty gate.
    pub(crate) fn new(limits: order::Config, live: Tail, backfill: Tail) -> Self {
        Self {
            gate: Gate::new(),
            order: Order::new(limits, live, backfill),
            readers: Readers::new(),
        }
    }

    /// Checks a frame from `key` whose index series on `path` is `stamps`, at
    /// monotonic time `now` and mesh time `mesh`. Only the gate changes: a lease that
    /// ran out by `now` hands control on.
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
        let permit = self.gate.check(key, now).map_err(Refusal::Control)?;
        let order = self
            .order
            .check(path, stamps, mesh)
            .map_err(Refusal::Order)?;
        Ok(Accepted {
            order,
            permit,
            frame: None,
        })
    }

    /// Spends the seq of `accepted` and renews the writer's control lease. A live
    /// frame becomes the newest frame, and the latest sessions to wake are returned.
    /// With no frozen frame, as when the pool had no room, the seq is a gap.
    ///
    /// # Panics
    ///
    /// If either path moved, or the holder changed, after the check.
    pub(crate) fn advance(&mut self, accepted: Accepted) -> &[delivery::Key] {
        let Accepted {
            order,
            permit,
            frame,
        } = accepted;
        let path = order.path;
        self.gate.renew(permit);
        self.order.advance(order);
        match (path, frame) {
            (Path::Live, Some(frame)) => self.readers.put(frame),
            (Path::Live, None) | (Path::Backfill, _) => &[],
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
    use std::sync::Arc;

    use control::{Handoff, Lease, Writer};
    use types::authority::Authority;
    use types::channel::Slot;
    use types::frame::Form;
    use types::frame::key_set::{Group, Interner, KeySet};
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

        fn draft(&self, stamps: &[[u8; 8]]) -> Draft {
            let series = [(0, stamps.len() * 8)];
            let mut draft = Draft::new(&self.pool, &self.set, Form::Raw, &series)
                .expect("the pool has room");
            let bytes = draft.series(0).expect("the index is present");
            bytes.copy_from_slice(stamps.as_flattened());
            draft
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
        let seq = accepted.seq();
        let _ = index.advance(accepted);
        Ok(seq)
    }

    fn handed_to(index: &Index) -> Option<Writer> {
        index.gate.handoff().and_then(|handoff| handoff.to.cloned())
    }

    mod check {
        use super::*;

        #[test]
        fn refuses_a_writer_without_control_and_spends_nothing() {
            let mut index = index();
            let holder = index.gate.open(writer("a", 10), None, at(0));
            let waiter = index.gate.open(writer("b", 5), None, at(0));
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
            let holder = index.gate.open(writer("a", 10), None, at(0));
            let waiter = index.gate.open(writer("b", 5), None, at(0));
            assert_eq!(write(&mut index, holder, &[5], at(1)), Ok(0..1));
            assert_eq!(
                write(&mut index, waiter, &[4], at(2)),
                Err(Refusal::Control(control::Error::Waiting))
            );
        }

        #[test]
        fn refuses_a_backwards_stamp_with_the_order_error() {
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
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
            let key = index.gate.open(writer("a", 10), None, at(0));
            let series = stamps(&[1, 2]);
            let first = index.check(key, Path::Live, &series, at(1), mesh());
            let again = index.check(key, Path::Live, &series, at(2), mesh());
            assert_eq!(first.map(|accepted| accepted.seq()), Ok(0..2));
            assert_eq!(again.map(|accepted| accepted.seq()), Ok(0..2));
        }

        #[test]
        fn hands_control_on_when_the_holder_lease_ran_out() {
            let mut index = index();
            let _ = index.gate.open(writer("a", 10), Some(lease(10)), at(0));
            let waiter = index.gate.open(writer("b", 5), None, at(0));
            index.gate.recorded();
            assert_eq!(write(&mut index, waiter, &[1], at(20)), Ok(0..1));
            assert_eq!(handed_to(&index), Some(writer("b", 5)));
        }

        #[test]
        fn does_not_renew_the_lease_for_a_refused_frame() {
            let mut index = index();
            let holder = index.gate.open(writer("a", 10), Some(lease(10)), at(0));
            let waiter = index.gate.open(writer("b", 5), None, at(0));
            assert_eq!(write(&mut index, holder, &[5], at(1)), Ok(0..1));
            let refused = write(&mut index, holder, &[4], at(9));
            assert!(matches!(refused, Err(Refusal::Order(_))), "{refused:?}");
            assert_eq!(
                write(&mut index, holder, &[6], at(12)),
                Err(Refusal::Control(control::Error::Expired))
            );
            assert_eq!(write(&mut index, waiter, &[6], at(13)), Ok(1..2));
        }

        #[test]
        fn does_not_renew_the_lease_until_advance() {
            let mut index = index();
            let holder = index.gate.open(writer("a", 10), Some(lease(10)), at(0));
            let accepted =
                index.check(holder, Path::Live, &stamps(&[1]), at(8), mesh());
            assert_eq!(index.gate.deadline(), Some(at(10)));
            let _ = index.advance(accepted.expect("a holder's frame in order"));
            assert_eq!(index.gate.deadline(), Some(at(18)));
        }

        #[test]
        fn keeps_a_handoff_from_a_refused_write() {
            let mut index = index();
            let holder = index.gate.open(writer("a", 10), Some(lease(10)), at(0));
            let _ = index.gate.open(writer("b", 5), None, at(0));
            index.gate.recorded();
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
            let key = index.gate.open(writer("a", 10), None, at(0));
            let session = index.readers.open_latest(None, s(0)).key;
            let series = stamps(&[2, 3]);
            assert_eq!(write(&mut index, key, &[1], at(1)), Ok(0..1));
            let mut accepted = index
                .check(key, Path::Live, &series, at(2), mesh())
                .expect("a holder's frame in order");
            let frame = accepted.freeze(frames.draft(&series), 0).clone();
            assert_eq!(index.advance(accepted), &[session]);
            let taken = index.readers.take(session).expect("the newest frame");
            assert_eq!(taken.path(), Path::Live);
            assert_eq!(taken.range(0), Some(frame::Range { seq: 1, count: 2 }));
            assert_eq!(taken.series(0), frame.series(0));
        }

        #[test]
        fn keeps_a_backfill_frame_from_latest_sessions() {
            let frames = Frames::new();
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            let session = index.readers.open_latest(None, s(0)).key;
            let series = stamps(&[1, 2]);
            let mut accepted = index
                .check(key, Path::Backfill, &series, at(1), mesh())
                .expect("a holder's frame in order");
            let frame = accepted.freeze(frames.draft(&series), 0);
            assert_eq!(frame.path(), Path::Backfill);
            assert_eq!(frame.range(0), Some(frame::Range { seq: 0, count: 2 }));
            assert_eq!(index.advance(accepted), &[]);
            assert!(index.readers.take(session).is_none());
            assert_eq!(write(&mut index, key, &[3], at(2)), Ok(0..1));
        }

        #[test]
        fn spends_the_seq_of_a_frame_the_pool_had_no_room_for() {
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            let session = index.readers.open_latest(None, s(0)).key;
            assert_eq!(write(&mut index, key, &[1, 2], at(1)), Ok(0..2));
            assert!(index.readers.take(session).is_none());
            assert_eq!(write(&mut index, key, &[3], at(2)), Ok(2..3));
        }

        #[test]
        #[should_panic(expected = "invariant: the order moved after the check")]
        fn panics_when_the_path_moved_after_the_check() {
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            let first = index.check(key, Path::Live, &stamps(&[1]), at(1), mesh());
            let first = first.expect("a holder's frame in order");
            // At the same time, so the gate still takes the first permit.
            assert_eq!(write(&mut index, key, &[1], at(1)), Ok(0..1));
            let _ = index.advance(first);
        }

        #[test]
        #[should_panic(
            expected = "invariant: the gate changed after the check of writer 0"
        )]
        fn panics_when_the_holder_changed_after_the_check() {
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            let first = index.check(key, Path::Live, &stamps(&[1]), at(1), mesh());
            let first = first.expect("a holder's frame in order");
            let _ = index.gate.open(writer("b", 20), None, at(1));
            let _ = index.advance(first);
        }

        #[test]
        #[should_panic(expected = "invariant: a frame is frozen once")]
        fn panics_when_a_frame_is_frozen_twice() {
            let frames = Frames::new();
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            let series = stamps(&[1]);
            let mut accepted = index
                .check(key, Path::Live, &series, at(1), mesh())
                .expect("a holder's frame in order");
            let _ = accepted.freeze(frames.draft(&series), 0);
            let _ = accepted.freeze(frames.draft(&series), 0);
        }
    }

    mod handoff {
        use proptest::prelude::*;

        use super::*;

        #[test]
        fn is_kept_until_recorded() {
            let mut index = index();
            let first = index.gate.open(writer("a", 5), None, at(0));
            assert_eq!(handed_to(&index), Some(writer("a", 5)));
            index.gate.recorded();
            assert_eq!(index.gate.handoff(), None);
            let _ = index.gate.open(writer("b", 1), None, at(1));
            assert_eq!(index.gate.handoff(), None);
            let _ = index.gate.open(writer("c", 9), None, at(2));
            assert_eq!(handed_to(&index), Some(writer("c", 9)));
            index.gate.close(first, at(3));
            assert_eq!(handed_to(&index), Some(writer("c", 9)));
        }

        #[test]
        fn names_an_empty_gate_after_the_last_close() {
            let mut index = index();
            let key = index.gate.open(writer("a", 5), None, at(0));
            index.gate.recorded();
            index.gate.close(key, at(1));
            assert_eq!(index.gate.handoff(), Some(Handoff { to: None }));
        }

        #[test]
        fn is_none_when_the_gate_returns_to_the_logged_holder() {
            let mut index = index();
            let _ = index.gate.open(writer("a", 5), None, at(0));
            index.gate.recorded();
            let b = index.gate.open(writer("b", 9), None, at(1));
            assert_eq!(handed_to(&index), Some(writer("b", 9)));
            index.gate.close(b, at(2));
            assert_eq!(index.gate.holder(), Some(&writer("a", 5)));
            assert_eq!(index.gate.handoff(), None);
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
            /// names the gate's holder after every input. No record repeats the one
            /// before it.
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
                            open.push(index.gate.open(writer, lease, now));
                        }
                        Input::Close(n) if !open.is_empty() => {
                            let key = open.swap_remove(n % open.len());
                            index.gate.close(key, now);
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
                            if let Some(handoff) = index.gate.handoff() {
                                prop_assert_ne!(handoff.to, logged.as_ref());
                                logged = handoff.to.cloned();
                            }
                            index.gate.recorded();
                        }
                        Input::Close(_) | Input::Write(_) => {}
                    }
                    let named = index.gate.handoff().map_or(logged.as_ref(), |h| h.to);
                    prop_assert_eq!(named, index.gate.holder());
                }
            }
        }
    }
}

//! One index of a shard: who may write it and the order of its samples.

use std::ops::Range;

use control::{Gate, Handoff, Permit};
use types::frame::{Draft, Frame, Path};
use types::time::{Monotonic, Stamp};

use crate::order::{self, Order, Tail};
use crate::{Refusal, split};

/// One index of a shard. It reads no clock: each input takes the time.
#[derive(Debug)]
pub(crate) struct Index {
    /// Who may write the index.
    pub(crate) gate: Gate,
    order: Order,
}

/// A frame that passed [`Index::check`]. [`Index::spend`] spends it.
#[derive(Debug)]
pub(crate) struct Accepted {
    order: order::Accepted,
    permit: Permit,
}

impl Accepted {
    /// The seq the frame's samples take.
    pub(crate) fn seq(&self) -> Range<u64> {
        self.order.seq.clone()
    }

    /// The newest stamp of the frame, or `None` when it is empty.
    pub(crate) fn last(&self) -> Option<Stamp> {
        self.order.last
    }

    /// Sets the seq of `group` in `draft` and freezes it on the accepted path.
    ///
    /// # Panics
    ///
    /// If `group` is absent from `draft`.
    pub(crate) fn freeze(&self, mut draft: Draft, group: u32) -> Frame {
        draft.set_seq(group, self.order.seq.start);
        draft.freeze(self.order.path)
    }
}

impl Index {
    /// An index whose paths stand at `live` and `backfill`, with an empty gate.
    pub(crate) fn new(limits: order::Limits, live: Tail, backfill: Tail) -> Self {
        Self {
            gate: Gate::new(),
            order: Order::new(limits, live, backfill),
        }
    }

    /// Checks a frame from `key` whose index series on `path` gives `stamps`, or the
    /// error of its first series that does not fit, at monotonic time `now` and mesh
    /// time `mesh`. Only the gate changes: a lease that ran out by `now` hands
    /// control on.
    ///
    /// # Errors
    ///
    /// In this order: a control refusal when `key` does not hold control,
    /// [`Refusal::Codec`] with the error of `stamps`, and [`Refusal::Order`] when a
    /// stamp breaks a rule.
    ///
    /// # Panics
    ///
    /// If `key` is not open.
    pub(crate) fn check(
        &mut self,
        key: control::Key,
        path: Path,
        stamps: Result<split::Stamps<'_>, split::Error>,
        now: Monotonic,
        mesh: Stamp,
    ) -> Result<Accepted, Refusal> {
        let permit = self.gate.check(key, now).map_err(Refusal::control)?;
        let mut stamps = stamps?;
        // A codec error comes first, so the vectors after an order error still decode.
        let mut order = Ok(self.order.check(path, mesh));
        while let Some(vector) = stamps.next() {
            let vector = vector?;
            order = order.and_then(|order| order.push(vector));
        }
        let order = order.map_err(Refusal::Order)?;
        Ok(Accepted {
            order: order.end(),
            permit,
        })
    }

    /// The gate's handoff to record, and the seq it goes in at: the live tail.
    pub(crate) fn handoff(&self) -> Option<(Handoff<'_>, u64)> {
        let first = self.live_tail();
        self.gate.handoff().map(|handoff| (handoff, first))
    }

    /// The seq of the next live frame.
    pub(crate) fn live_tail(&self) -> u64 {
        self.order.tail(Path::Live).seq
    }

    /// Spends the seq of `accepted` and renews the writer's control lease.
    ///
    /// # Panics
    ///
    /// If either path moved, or the holder changed, after the check.
    pub(crate) fn spend(&mut self, accepted: Accepted) {
        self.gate.renew(accepted.permit);
        self.order.advance(accepted.order);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use control::{Lease, Writer};
    use types::authority::Authority;
    use types::channel;
    use types::frame::key_set::{Group, Interner, KeySet};
    use types::frame::{self, Form};
    use types::time::Span;

    use super::*;
    use crate::common::pool;

    /// Index frames of one index with no data channels.
    struct Frames {
        pool: block::Pool,
        set: Arc<KeySet>,
    }

    impl Frames {
        fn new() -> Self {
            let index = Group {
                index: channel::Key::from_u128(1),
                data: &[],
            };
            Self {
                pool: pool(4096),
                set: Interner::new().intern(&[index]),
            }
        }

        fn draft(&self, stamps: &[[u8; 8]]) -> Draft {
            let series = [(0, stamps.len() * 8)];
            let mut draft = Draft::new(&self.pool, &self.set, Form::Raw, &series)
                .expect("the pool has room");
            let bytes = draft.series_mut(0).expect("the index is present");
            bytes.copy_from_slice(stamps.as_flattened());
            let count = u32::try_from(stamps.len()).expect("a short test frame");
            draft.set_count(0, count);
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

    /// Checks a frame on `path` from `key` at `now`, whose index series holds
    /// `seconds`.
    fn check(
        index: &mut Index,
        key: control::Key,
        path: Path,
        seconds: &[i64],
        now: Monotonic,
    ) -> Result<Accepted, Refusal> {
        let frames = Frames::new();
        let mut scratch = split::Scratch::default();
        let mut split = scratch.split(&frames.set, frames.draft(&stamps(seconds)));
        let (_, stamps) = split.next().expect("the group of the index");
        index.check(key, path, stamps, now, mesh())
    }

    /// Mesh time in the tests: the latest stamp accepted is `s(61)`.
    fn mesh() -> Stamp {
        s(60)
    }

    fn index() -> Index {
        let limits = order::Limits {
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
        let accepted = check(index, key, Path::Live, seconds, now)?;
        let seq = accepted.seq();
        index.spend(accepted);
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
            assert_eq!(refusal, Refusal::Waiting);
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
                Err(Refusal::Waiting)
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
        fn spends_no_seq_until_spend() {
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            let first = check(&mut index, key, Path::Live, &[1, 2], at(1));
            let again = check(&mut index, key, Path::Live, &[1, 2], at(2));
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
                Err(Refusal::Expired)
            );
            assert_eq!(write(&mut index, waiter, &[6], at(13)), Ok(1..2));
        }

        #[test]
        fn does_not_renew_the_lease_until_spend() {
            let mut index = index();
            let holder = index.gate.open(writer("a", 10), Some(lease(10)), at(0));
            let accepted = check(&mut index, holder, Path::Live, &[1], at(8))
                .expect("a holder's frame in order");
            assert_eq!(index.gate.deadline(), Some(at(10)));
            index.spend(accepted);
            assert_eq!(index.gate.deadline(), Some(at(18)));
        }

        #[test]
        fn keeps_a_handoff_from_a_refused_write() {
            let mut index = index();
            let holder = index.gate.open(writer("a", 10), Some(lease(10)), at(0));
            let _ = index.gate.open(writer("b", 5), None, at(0));
            index.gate.recorded();
            let refusal = write(&mut index, holder, &[1], at(20));
            assert_eq!(refusal, Err(Refusal::Expired));
            assert_eq!(handed_to(&index), Some(writer("b", 5)));
        }
    }

    mod freeze {
        use super::*;

        #[test]
        fn freezes_a_live_frame_at_its_seq() {
            let frames = Frames::new();
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            assert_eq!(write(&mut index, key, &[1], at(1)), Ok(0..1));
            let accepted = check(&mut index, key, Path::Live, &[2, 3], at(2))
                .expect("a holder's frame in order");
            let frame = accepted.freeze(frames.draft(&stamps(&[2, 3])), 0);
            assert_eq!(frame.path(), Path::Live);
            assert_eq!(frame.range(0), Some(frame::Range { seq: 1, count: 2 }));
        }

        #[test]
        fn freezes_a_backfill_frame_at_its_seq() {
            let frames = Frames::new();
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            assert_eq!(write(&mut index, key, &[5], at(1)), Ok(0..1));
            let accepted = check(&mut index, key, Path::Backfill, &[1, 2], at(2))
                .expect("a holder's frame in order");
            let frame = accepted.freeze(frames.draft(&stamps(&[1, 2])), 0);
            assert_eq!(frame.path(), Path::Backfill);
            assert_eq!(frame.range(0), Some(frame::Range { seq: 0, count: 2 }));
        }
    }

    mod spend {
        use super::*;

        #[test]
        fn moves_only_the_accepted_path() {
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            let accepted = check(&mut index, key, Path::Backfill, &[1, 2], at(1))
                .expect("a holder's frame in order");
            index.spend(accepted);
            assert_eq!(index.live_tail(), 0);
            assert_eq!(write(&mut index, key, &[3], at(2)), Ok(0..1));
            assert_eq!(index.live_tail(), 1);
        }

        #[test]
        #[should_panic(expected = "invariant: the order moved after the check")]
        fn panics_when_the_path_moved_after_the_check() {
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            let first = check(&mut index, key, Path::Live, &[1], at(1));
            let first = first.expect("a holder's frame in order");
            // At the same time, so the gate still takes the first permit.
            assert_eq!(write(&mut index, key, &[1], at(1)), Ok(0..1));
            index.spend(first);
        }

        #[test]
        #[should_panic(
            expected = "invariant: the gate changed after the check of writer 0"
        )]
        fn panics_when_the_holder_changed_after_the_check() {
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            let first = check(&mut index, key, Path::Live, &[1], at(1));
            let first = first.expect("a holder's frame in order");
            let _ = index.gate.open(writer("b", 20), None, at(1));
            index.spend(first);
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
                                    matches!(
                                        refusal,
                                        Refusal::Waiting
                                            | Refusal::Reserved
                                            | Refusal::Expired
                                    ),
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

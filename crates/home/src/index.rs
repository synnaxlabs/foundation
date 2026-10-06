//! One index of a shard: who may write it, the order of its samples, and who reads
//! it.

use std::ops::Range;

use control::{Gate, Handoff, Permit};
use delivery::{Position, Reader, Readers, Start};
use types::frame::{Draft, Frame, Path};
use types::time::{Interval, Monotonic, Stamp};

use crate::order::{self, Order, Tail};
use crate::{Refusal, split};

/// One index of a shard. It reads no clock: each input takes the time.
#[derive(Debug)]
pub(crate) struct Index {
    /// Who may write the index.
    pub(crate) gate: Gate,
    order: Order,
    /// Who reads the index. The shard opens and closes readers, takes their frames,
    /// and releases each live frame to complete readers once it is on disk.
    pub(crate) readers: Readers,
}

/// A frame that passed [`Index::check`]. [`Index::advance`] or [`Index::lose`] spends
/// it.
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

    /// The newest stamp of the frame, or `None` when it is empty.
    pub(crate) fn last(&self) -> Option<Stamp> {
        self.order.last
    }

    /// Sets the seq of `group` in `draft` and freezes it on the accepted path. A live
    /// frame becomes the newest frame when the index spends it.
    ///
    /// # Panics
    ///
    /// If `group` is absent from `draft`, or a frame was frozen already.
    pub(crate) fn freeze(&mut self, mut draft: Draft, group: u32) -> &Frame {
        assert!(self.frame.is_none(), "invariant: a frame is frozen once");
        draft.set_seq(group, self.order.seq.start);
        self.frame.insert(draft.freeze(self.order.path))
    }
}

impl Index {
    /// An index whose paths stand at `live` and `backfill`, with an empty gate.
    pub(crate) fn new(limits: order::Config, live: Tail, backfill: Tail) -> Self {
        Self {
            gate: Gate::new(),
            readers: Readers::new(live.seq),
            order: Order::new(limits, live, backfill),
        }
    }

    /// Checks a frame from `key` whose index series on `path` is `stamps`, or the
    /// error of its first series that does not fit, at monotonic time `now` and mesh
    /// time `mesh`. Only the gate changes: a lease that ran out by `now` hands
    /// control on.
    ///
    /// # Errors
    ///
    /// In this order: [`Refusal::Control`] when `key` does not hold control,
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
        stamps: Result<&[[u8; 8]], split::Error>,
        now: Monotonic,
        mesh: Interval,
    ) -> Result<Accepted, Refusal> {
        let permit = self.gate.check(key, now).map_err(Refusal::Control)?;
        let stamps = stamps.map_err(Refusal::Codec)?;
        let order = self
            .order
            .check(path, mesh)
            .push(stamps)
            .map_err(Refusal::Order)?
            .end();
        Ok(Accepted {
            order,
            permit,
            frame: None,
        })
    }

    /// The gate's handoff to record, and the seq it goes in at: the live tail.
    pub(crate) fn handoff(&self) -> Option<(Handoff<'_>, u64)> {
        let first = self.order.tail(Path::Live).seq;
        self.gate.handoff().map(|handoff| (handoff, first))
    }

    /// Opens a complete reader at the live tail, with a credit of `limit_bytes`.
    pub(crate) fn open_complete(&mut self, limit_bytes: u64) -> delivery::Key {
        let live = self.order.tail(Path::Live).seq;
        let start = Start::At(Position {
            live,
            backfill: None,
        });
        self.readers.open(Reader::Unnamed, start, limit_bytes).key
    }

    /// Spends the seq of `accepted`, whose frame is in the buffer, and renews the
    /// writer's control lease. A live frame becomes the newest frame, and is queued for
    /// complete readers until it is on disk. Returns whether the frame was queued, and
    /// the latest sessions to wake.
    ///
    /// # Panics
    ///
    /// If either path moved, or the holder changed, after the check, or no frame was
    /// frozen.
    pub(crate) fn advance(&mut self, accepted: Accepted) -> (bool, &[delivery::Key]) {
        let (path, seq, frame) = self.spend(accepted);
        let frame = frame.expect("invariant: a stored frame was frozen");
        match path {
            Path::Live => {
                self.readers.queue(&frame, seq);
                (true, self.readers.put(frame))
            }
            Path::Backfill => (false, &[]),
        }
    }

    /// Spends the seq of `accepted`, which found no room in the buffer, as a gap, and
    /// renews the writer's control lease. A frozen live frame becomes the newest frame.
    /// Returns the latest sessions to wake.
    ///
    /// # Panics
    ///
    /// If either path moved, or the holder changed, after the check.
    pub(crate) fn lose(&mut self, accepted: Accepted) -> &[delivery::Key] {
        match self.spend(accepted) {
            (Path::Live, _, Some(frame)) => self.readers.put(frame),
            (Path::Live, _, None) | (Path::Backfill, ..) => &[],
        }
    }

    fn spend(&mut self, accepted: Accepted) -> (Path, Range<u64>, Option<Frame>) {
        let Accepted {
            order,
            permit,
            frame,
        } = accepted;
        let (path, seq) = (order.path, order.seq.clone());
        self.gate.renew(permit);
        self.order.advance(order);
        (path, seq, frame)
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

    /// Checks a live frame of `seconds` from `key` at `now`, and spends its seq with
    /// no frame.
    fn write(
        index: &mut Index,
        key: control::Key,
        seconds: &[i64],
        now: Monotonic,
    ) -> Result<Range<u64>, Refusal> {
        let accepted =
            index.check(key, Path::Live, Ok(&stamps(seconds)), now, mesh())?;
        let seq = accepted.seq();
        let _ = index.lose(accepted);
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
            let first = index.check(key, Path::Live, Ok(&series), at(1), mesh());
            let again = index.check(key, Path::Live, Ok(&series), at(2), mesh());
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
            let frames = Frames::new();
            let mut index = index();
            let holder = index.gate.open(writer("a", 10), Some(lease(10)), at(0));
            let series = stamps(&[1]);
            let mut accepted = index
                .check(holder, Path::Live, Ok(&series), at(8), mesh())
                .expect("a holder's frame in order");
            let _ = accepted.freeze(frames.draft(&series), 0);
            assert_eq!(index.gate.deadline(), Some(at(10)));
            let _ = index.advance(accepted);
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
                .check(key, Path::Live, Ok(&series), at(2), mesh())
                .expect("a holder's frame in order");
            let frame = accepted.freeze(frames.draft(&series), 0).clone();
            assert_eq!(index.advance(accepted), (true, &[session][..]));
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
                .check(key, Path::Backfill, Ok(&series), at(1), mesh())
                .expect("a holder's frame in order");
            let frame = accepted.freeze(frames.draft(&series), 0);
            assert_eq!(frame.path(), Path::Backfill);
            assert_eq!(frame.range(0), Some(frame::Range { seq: 0, count: 2 }));
            assert_eq!(index.advance(accepted), (false, &[][..]));
            assert!(index.readers.take(session).is_none());
            assert_eq!(write(&mut index, key, &[3], at(2)), Ok(0..1));
        }

        #[test]
        fn queues_a_live_frame_for_complete_readers_until_it_is_on_disk() {
            let frames = Frames::new();
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            let start = Start::At(Position {
                live: 0,
                backfill: None,
            });
            let session = index.readers.open(Reader::Unnamed, start, u64::MAX).key;
            let series = stamps(&[1, 2]);
            let mut accepted = index
                .check(key, Path::Live, Ok(&series), at(1), mesh())
                .expect("a holder's frame in order");
            let _ = accepted.freeze(frames.draft(&series), 0);
            assert_eq!(index.advance(accepted), (true, &[][..]));
            assert_eq!(index.readers.release(1), &[]);
            assert_eq!(index.readers.release(2), &[session]);
            let taken = index.readers.take(session).expect("the stored frame");
            assert_eq!(taken.range(0), Some(frame::Range { seq: 0, count: 2 }));
        }

        #[test]
        #[should_panic(expected = "invariant: a stored frame was frozen")]
        fn panics_when_a_stored_frame_was_not_frozen() {
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            let accepted =
                index.check(key, Path::Live, Ok(&stamps(&[1])), at(1), mesh());
            let _ = index.advance(accepted.expect("a holder's frame in order"));
        }

        #[test]
        #[should_panic(expected = "invariant: the order moved after the check")]
        fn panics_when_the_path_moved_after_the_check() {
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            let first = index.check(key, Path::Live, Ok(&stamps(&[1])), at(1), mesh());
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
            let first = index.check(key, Path::Live, Ok(&stamps(&[1])), at(1), mesh());
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
                .check(key, Path::Live, Ok(&series), at(1), mesh())
                .expect("a holder's frame in order");
            let _ = accepted.freeze(frames.draft(&series), 0);
            let _ = accepted.freeze(frames.draft(&series), 0);
        }
    }

    mod lose {
        use super::*;

        #[test]
        fn makes_a_live_frame_the_newest_and_spends_its_seq_as_a_gap() {
            let frames = Frames::new();
            let mut index = index();
            let key = index.gate.open(writer("a", 10), None, at(0));
            let latest = index.readers.open_latest(None, s(0)).key;
            let start = Start::At(Position {
                live: 0,
                backfill: None,
            });
            index.readers.open(Reader::Unnamed, start, u64::MAX);
            let series = stamps(&[1, 2]);
            let mut accepted = index
                .check(key, Path::Live, Ok(&series), at(1), mesh())
                .expect("a holder's frame in order");
            let _ = accepted.freeze(frames.draft(&series), 0);
            assert_eq!(index.lose(accepted), &[latest]);
            let taken = index.readers.take(latest).expect("the newest frame");
            assert_eq!(taken.range(0), Some(frame::Range { seq: 0, count: 2 }));
            assert_eq!(index.readers.release(2), &[]);
            assert_eq!(write(&mut index, key, &[3], at(2)), Ok(2..3));
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

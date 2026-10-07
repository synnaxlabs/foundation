//! The order of an index's samples: stamp checks and seq, per write path.

use std::fmt;
use std::ops::Range;

use types::frame::Path;
use types::time::{Span, Stamp};

/// The stamps an index accepts.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The earliest stamp accepted. A clock that was never set reads near 1970.
    pub earliest: Stamp,
    /// How far past mesh time a stamp may be.
    pub ahead: Span,
}

/// A frame whose stamps passed a [`Check`] on one path.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Accepted {
    /// The path of the frame.
    pub(crate) path: Path,
    /// The seq of the frame's samples.
    pub(crate) seq: Range<u64>,
    /// The newest stamp of the frame, or `None` when it is empty.
    pub(crate) last: Option<Stamp>,
    /// The seq of the live and backfill paths at the check.
    checked: [u64; 2],
}

/// Where one path of an index stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Tail {
    /// The newest accepted stamp, or `None` before the first sample.
    pub(crate) stamp: Option<Stamp>,
    /// The seq of the next sample.
    pub(crate) seq: u64,
}

/// The order of one index's samples: checks each frame's stamps and gives its samples
/// their seq, per path.
#[derive(Debug)]
pub(crate) struct Order {
    limits: Limits,
    live: Tail,
    backfill: Tail,
}

impl Order {
    /// Starts an index whose paths stand at `live` and `backfill`.
    pub(crate) fn new(limits: Limits, live: Tail, backfill: Tail) -> Self {
        Self {
            limits,
            live,
            backfill,
        }
    }

    /// Starts the check of one frame's index series on `path` at mesh time `now`.
    /// [`Check::push`] takes the series in order, in chunks of any length, and
    /// [`Check::end`] gives the seq its samples take. Changes nothing:
    /// [`Order::advance`] spends the seq.
    pub(crate) fn check(&self, path: Path, now: Stamp) -> Check<'_> {
        let latest =
            Stamp::from_nanos(now.nanos().saturating_add(self.limits.ahead.nanos()));
        Check {
            order: self,
            path,
            latest,
            last: None,
            count: 0,
        }
    }

    /// Spends the seq of a frame that a [`Check`] accepted. Later frames on its
    /// path must follow it.
    ///
    /// # Panics
    ///
    /// If either path moved after the check.
    pub(crate) fn advance(&mut self, accepted: Accepted) {
        let Accepted {
            path,
            seq,
            last,
            checked,
        } = accepted;
        assert_eq!(
            [self.live.seq, self.backfill.seq],
            checked,
            "invariant: the order moved after the check of {path:?} seq {seq:?}",
        );
        let tail = match path {
            Path::Live => &mut self.live,
            Path::Backfill => &mut self.backfill,
        };
        if let Some(last) = last {
            tail.seq = seq.end;
            tail.stamp = Some(last);
        }
    }

    /// Where `path` stands.
    pub(crate) fn tail(&self, path: Path) -> Tail {
        match path {
            Path::Live => self.live,
            Path::Backfill => self.backfill,
        }
    }
}

/// The check of one frame's index series on one path, from [`Order::check`].
#[derive(Debug)]
pub(crate) struct Check<'a> {
    order: &'a Order,
    path: Path,
    /// The latest stamp accepted now.
    latest: Stamp,
    /// The newest stamp checked, or `None` before the first.
    last: Option<Stamp>,
    count: u64,
}

impl Check<'_> {
    /// Checks the next stamps of the series, in place: each stamp is little-endian
    /// `i64` nanoseconds since the epoch. Any split of a series gives the same result.
    ///
    /// # Errors
    ///
    /// [`Error`] names the first stamp that breaks a rule. Each stamp is checked
    /// against the limits, then the order on its path, then the other path.
    pub(crate) fn push(mut self, stamps: &[[u8; 8]]) -> Result<Self, Error> {
        let (Some(first), Some(last)) = (stamps.first(), stamps.last()) else {
            return Ok(self);
        };
        let (first, last) = (stamp(*first), stamp(*last));
        let before = self.before();
        let (floor, ceiling) = match self.path {
            Path::Live => (before.max(self.other()), None),
            Path::Backfill => (before, self.other()),
        };
        // States the rules a second time, as one pass with no early exit, so that it
        // vectorizes. `breach` names the error only for stamps that fail it.
        let increasing = stamps.array_windows().fold(true, |increasing, [a, b]| {
            increasing & (i64::from_le_bytes(*a) < i64::from_le_bytes(*b))
        });
        if increasing
            && first >= self.order.limits.earliest
            && last <= self.latest
            && floor.is_none_or(|floor| first > floor)
            && ceiling.is_none_or(|ceiling| last < ceiling)
        {
            self.last = Some(last);
            self.count += stamps.len() as u64;
            return Ok(self);
        }
        Err(self.breach(stamps))
    }

    /// Ends the check and returns the seq the series takes. The seq counts only the
    /// stamps pushed, so end after the last chunk. An empty series gets an empty range.
    ///
    /// # Panics
    ///
    /// If the path's seq would pass `u64::MAX`.
    pub(crate) fn end(self) -> Accepted {
        let start = self.order.tail(self.path).seq;
        let end = start
            .checked_add(self.count)
            .expect("invariant: a path takes fewer than 2^64 samples");
        Accepted {
            path: self.path,
            seq: start..end,
            last: self.last,
            checked: [self.order.live.seq, self.order.backfill.seq],
        }
    }

    /// The stamp that the next stamp must follow on its path.
    fn before(&self) -> Option<Stamp> {
        self.last.or(self.order.tail(self.path).stamp)
    }

    /// The newest stamp on the other path.
    fn other(&self) -> Option<Stamp> {
        match self.path {
            Path::Live => self.order.backfill.stamp,
            Path::Backfill => self.order.live.stamp,
        }
    }

    /// The first rule that `stamps` break, in the order that [`Check::push`] states.
    fn breach(&self, stamps: &[[u8; 8]]) -> Error {
        let (path, earliest, latest) =
            (self.path, self.order.limits.earliest, self.latest);
        let other = self.other();
        let mut before = self.before();
        for stamp in stamps.iter().copied().map(stamp) {
            if stamp < earliest {
                return Error::Early { stamp, earliest };
            }
            if stamp > latest {
                return Error::Ahead { stamp, latest };
            }
            if let Some(before) = before
                && stamp <= before
            {
                return Error::Backwards {
                    path,
                    before,
                    stamp,
                };
            }
            if let Some(newest) = other
                && match path {
                    Path::Live => stamp <= newest,
                    Path::Backfill => stamp >= newest,
                }
            {
                return Error::Overlap {
                    path,
                    stamp,
                    newest,
                };
            }
            before = Some(stamp);
        }
        panic!("invariant: stamps that fail the check break a rule: {stamps:?}")
    }
}

fn stamp(bytes: [u8; 8]) -> Stamp {
    Stamp::from_nanos(i64::from_le_bytes(bytes))
}

/// A frame's stamps break a rule. The frame is rejected whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A stamp is not after the stamp before it on its path.
    Backwards {
        /// The path of the frame.
        path: Path,
        /// The stamp before it.
        before: Stamp,
        /// The stamp.
        stamp: Stamp,
    },
    /// A stamp is before the earliest stamp accepted.
    Early {
        /// The stamp.
        stamp: Stamp,
        /// The earliest stamp accepted.
        earliest: Stamp,
    },
    /// A stamp is past the latest stamp accepted now.
    Ahead {
        /// The stamp.
        stamp: Stamp,
        /// Mesh time plus the limit.
        latest: Stamp,
    },
    /// A stamp is on the wrong side of the other path: backfill ends before the newest
    /// live stamp, and live starts after the newest backfill stamp.
    Overlap {
        /// The path of the frame.
        path: Path,
        /// The stamp.
        stamp: Stamp,
        /// The newest stamp on the other path.
        newest: Stamp,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backwards {
                path: Path::Live,
                before,
                stamp,
            } => write!(
                f,
                "live stamp {stamp} is not after {before}: write late data on the \
                 backfill path"
            ),
            Self::Backwards {
                path: Path::Backfill,
                before,
                stamp,
            } => write!(
                f,
                "backfill stamp {stamp} is not after {before}: write backfill in time \
                 order"
            ),
            Self::Early { stamp, earliest } => {
                write!(
                    f,
                    "stamp {stamp} is before {earliest}: set the source's clock"
                )
            }
            Self::Ahead { stamp, latest } => write!(
                f,
                "stamp {stamp} is after {latest}, the latest this home accepts now: \
                 check the source's clock"
            ),
            Self::Overlap {
                path: Path::Backfill,
                stamp,
                newest,
            } => write!(
                f,
                "backfill stamp {stamp} is not before the newest live stamp {newest}: \
                 backfill only data older than live data"
            ),
            Self::Overlap {
                path: Path::Live,
                stamp,
                newest,
            } => write!(
                f,
                "live stamp {stamp} is not after the newest backfill stamp {newest}: \
                 check the source's clock"
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Second `seconds` of 2026-10-05.
    fn s(seconds: i64) -> Stamp {
        let day: Stamp = "2026-10-05T00:00:00Z".parse().expect("a valid stamp");
        day + Span::from_nanos(seconds * Span::SECOND.nanos())
    }

    fn stamps(seconds: &[i64]) -> Vec<[u8; 8]> {
        bytes(&seconds.iter().map(|&n| s(n)).collect::<Vec<_>>())
    }

    fn bytes(stamps: &[Stamp]) -> Vec<[u8; 8]> {
        stamps.iter().map(|s| s.nanos().to_le_bytes()).collect()
    }

    fn limits() -> Limits {
        Limits {
            earliest: "2000-01-01T00:00:00Z".parse().expect("a valid stamp"),
            ahead: Span::SECOND,
        }
    }

    /// Mesh time in the unit tests: the latest stamp accepted is `s(61)`.
    fn now() -> Stamp {
        s(60)
    }

    /// Checks a frame and spends its seq, as the home does after a stored frame.
    fn accept(
        order: &mut Order,
        path: Path,
        stamps: &[[u8; 8]],
        now: Stamp,
    ) -> Result<Range<u64>, Error> {
        accept_in(order, path, stamps, usize::MAX, now)
    }

    /// [`accept`], with the stamps pushed in chunks of `size`.
    fn accept_in(
        order: &mut Order,
        path: Path,
        stamps: &[[u8; 8]],
        size: usize,
        now: Stamp,
    ) -> Result<Range<u64>, Error> {
        let mut check = order.check(path, now);
        for chunk in stamps.chunks(size) {
            check = check.push(chunk)?;
        }
        let accepted = check.end();
        let seq = accepted.seq.clone();
        order.advance(accepted);
        Ok(seq)
    }

    fn order() -> Order {
        Order::new(limits(), Tail::default(), Tail::default())
    }

    fn tail(stamp: i64, seq: u64) -> Tail {
        Tail {
            stamp: Some(s(stamp)),
            seq,
        }
    }

    mod accept {
        use super::*;

        #[test]
        fn gives_a_frame_the_next_seq_on_its_path() {
            let mut order = order();
            assert_eq!(
                accept(&mut order, Path::Live, &stamps(&[1, 2, 3]), now()),
                Ok(0..3)
            );
            assert_eq!(
                accept(&mut order, Path::Live, &stamps(&[4, 5]), now()),
                Ok(3..5)
            );
            assert_eq!(order.tail(Path::Live), tail(5, 5));
        }

        #[test]
        fn counts_each_path_apart() {
            let mut order = order();
            assert_eq!(
                accept(&mut order, Path::Live, &stamps(&[10, 11]), now()),
                Ok(0..2)
            );
            let backfill =
                accept(&mut order, Path::Backfill, &stamps(&[1, 2, 3]), now());
            assert_eq!(backfill, Ok(0..3));
            assert_eq!(
                accept(&mut order, Path::Live, &stamps(&[12]), now()),
                Ok(2..3)
            );
            assert_eq!(
                accept(&mut order, Path::Backfill, &stamps(&[4]), now()),
                Ok(3..4)
            );
            assert_eq!(order.tail(Path::Live), tail(12, 3));
            assert_eq!(order.tail(Path::Backfill), tail(4, 4));
        }

        #[test]
        fn rejects_a_stamp_not_after_the_last_frame() {
            let mut order = order();
            assert_eq!(
                accept(&mut order, Path::Live, &stamps(&[5, 6]), now()),
                Ok(0..2)
            );
            let error = accept(&mut order, Path::Live, &stamps(&[4]), now())
                .expect_err("a stamp before the last frame");
            assert_eq!(
                error,
                Error::Backwards {
                    path: Path::Live,
                    before: s(6),
                    stamp: s(4),
                }
            );
            assert_eq!(
                error.to_string(),
                "live stamp 2026-10-05T00:00:04.000000000Z is not after \
                 2026-10-05T00:00:06.000000000Z: write late data on the backfill path"
            );
        }

        #[test]
        fn rejects_a_tie_inside_a_frame() {
            let error =
                accept(&mut order(), Path::Backfill, &stamps(&[1, 2, 2]), now())
                    .expect_err("a tie");
            assert_eq!(
                error,
                Error::Backwards {
                    path: Path::Backfill,
                    before: s(2),
                    stamp: s(2),
                }
            );
            assert_eq!(
                error.to_string(),
                "backfill stamp 2026-10-05T00:00:02.000000000Z is not after \
                 2026-10-05T00:00:02.000000000Z: write backfill in time order"
            );
        }

        #[test]
        fn rejects_a_stamp_before_the_earliest_before_its_order() {
            let mut order = order();
            assert_eq!(
                accept(&mut order, Path::Live, &stamps(&[5]), now()),
                Ok(0..1)
            );
            let error = accept(&mut order, Path::Live, &bytes(&[Stamp::EPOCH]), now())
                .expect_err("a clock that was never set");
            assert_eq!(
                error,
                Error::Early {
                    stamp: Stamp::EPOCH,
                    earliest: limits().earliest,
                }
            );
            assert_eq!(
                error.to_string(),
                "stamp 1970-01-01T00:00:00.000000000Z is before \
                 2000-01-01T00:00:00.000000000Z: set the source's clock"
            );
        }

        #[test]
        fn accepts_a_stamp_at_the_earliest() {
            let earliest = bytes(&[limits().earliest]);
            assert_eq!(accept(&mut order(), Path::Live, &earliest, now()), Ok(0..1));
        }

        #[test]
        fn rejects_a_stamp_past_mesh_time_and_ahead() {
            let mut order = order();
            assert_eq!(
                accept(&mut order, Path::Live, &stamps(&[60, 61]), now()),
                Ok(0..2)
            );
            let error = accept(&mut order, Path::Live, &stamps(&[62, 63]), now())
                .expect_err("a stamp past the limit");
            assert_eq!(
                error,
                Error::Ahead {
                    stamp: s(62),
                    latest: s(61),
                }
            );
            assert_eq!(
                error.to_string(),
                "stamp 2026-10-05T00:01:02.000000000Z is after \
                 2026-10-05T00:01:01.000000000Z, the latest this home accepts now: \
                 check the source's clock"
            );
        }

        #[test]
        fn rejects_backfill_at_or_after_the_newest_live_stamp() {
            let mut order = order();
            assert_eq!(
                accept(&mut order, Path::Live, &stamps(&[10]), now()),
                Ok(0..1)
            );
            assert_eq!(
                accept(&mut order, Path::Backfill, &stamps(&[8, 9]), now()),
                Ok(0..2)
            );
            let error = accept(&mut order, Path::Backfill, &stamps(&[10]), now())
                .expect_err("backfill at the newest live stamp");
            assert_eq!(
                error,
                Error::Overlap {
                    path: Path::Backfill,
                    stamp: s(10),
                    newest: s(10),
                }
            );
            assert_eq!(
                error.to_string(),
                "backfill stamp 2026-10-05T00:00:10.000000000Z is not before the \
                 newest live stamp 2026-10-05T00:00:10.000000000Z: backfill only \
                 data older than live data"
            );
        }

        #[test]
        fn accepts_backfill_before_any_live_sample() {
            let mut order = order();
            assert_eq!(
                accept(&mut order, Path::Backfill, &stamps(&[1, 2]), now()),
                Ok(0..2)
            );
            assert_eq!(order.tail(Path::Live), Tail::default());
        }

        #[test]
        fn rejects_live_at_or_before_the_newest_backfill_stamp() {
            let mut order = order();
            assert_eq!(
                accept(&mut order, Path::Backfill, &stamps(&[5, 6]), now()),
                Ok(0..2)
            );
            let error = accept(&mut order, Path::Live, &stamps(&[6]), now())
                .expect_err("live at the newest backfill stamp");
            assert_eq!(
                error,
                Error::Overlap {
                    path: Path::Live,
                    stamp: s(6),
                    newest: s(6),
                }
            );
            assert_eq!(
                error.to_string(),
                "live stamp 2026-10-05T00:00:06.000000000Z is not after the newest \
                 backfill stamp 2026-10-05T00:00:06.000000000Z: check the source's \
                 clock"
            );
            assert_eq!(
                accept(&mut order, Path::Live, &stamps(&[7]), now()),
                Ok(0..1)
            );
        }

        #[test]
        fn spends_the_seq_only_at_advance() {
            let mut order = order();
            let accepted = order.check(Path::Live, now()).push(&stamps(&[1, 2]));
            let accepted = accepted.expect("stamps in order").end();
            assert_eq!(accepted.seq, 0..2);
            assert_eq!(order.tail(Path::Live), Tail::default());
            order.advance(accepted);
            assert_eq!(order.tail(Path::Live), tail(2, 2));
        }

        #[test]
        fn gives_a_series_in_chunks_one_seq() {
            let mut order = order();
            let check = order.check(Path::Live, now()).push(&stamps(&[1]));
            let check = check.and_then(|check| check.push(&[]));
            let check = check.and_then(|check| check.push(&stamps(&[2, 3])));
            let accepted = check.expect("stamps in order").end();
            assert_eq!(accepted.seq, 0..3);
            order.advance(accepted);
            assert_eq!(order.tail(Path::Live), tail(3, 3));
        }

        #[test]
        fn checks_each_chunk_against_the_one_before() {
            let order = order();
            let check = order.check(Path::Backfill, now()).push(&stamps(&[1, 2]));
            let check = check.and_then(|check| check.push(&[]));
            let error = check
                .and_then(|check| check.push(&stamps(&[2, 3])))
                .expect_err("a tie across chunks");
            assert_eq!(
                error,
                Error::Backwards {
                    path: Path::Backfill,
                    before: s(2),
                    stamp: s(2),
                }
            );
        }

        #[test]
        #[should_panic(expected = "invariant: the order moved after the check")]
        fn panics_when_the_other_path_moved_after_the_check() {
            let mut order = order();
            let live = order.check(Path::Live, now()).push(&stamps(&[5]));
            let live = live.expect("stamps in order").end();
            let backfill = order.check(Path::Backfill, now()).push(&stamps(&[7]));
            order.advance(backfill.expect("backfill before any live sample").end());
            order.advance(live);
        }

        #[test]
        fn changes_nothing_for_a_rejected_frame() {
            let mut order = order();
            assert_eq!(
                accept(&mut order, Path::Live, &stamps(&[1, 2]), now()),
                Ok(0..2)
            );
            let rejected = accept(&mut order, Path::Live, &stamps(&[3, 3]), now());
            assert_eq!(
                rejected,
                Err(Error::Backwards {
                    path: Path::Live,
                    before: s(3),
                    stamp: s(3),
                })
            );
            assert_eq!(order.tail(Path::Live), tail(2, 2));
            assert_eq!(
                accept(&mut order, Path::Live, &stamps(&[3]), now()),
                Ok(2..3)
            );
        }

        #[test]
        fn gives_an_empty_frame_no_seq() {
            let mut order = order();
            assert_eq!(
                accept(&mut order, Path::Live, &stamps(&[1]), now()),
                Ok(0..1)
            );
            let epoch = Stamp::EPOCH;
            assert_eq!(accept(&mut order, Path::Live, &[], epoch), Ok(1..1));
            assert_eq!(order.tail(Path::Live), tail(1, 1));
        }

        #[test]
        fn continues_from_the_tails_it_starts_at() {
            let mut order = Order::new(limits(), tail(5, 7), tail(2, 3));
            let live = accept(&mut order, Path::Live, &stamps(&[5]), now());
            assert_eq!(
                live,
                Err(Error::Backwards {
                    path: Path::Live,
                    before: s(5),
                    stamp: s(5),
                })
            );
            assert_eq!(
                accept(&mut order, Path::Live, &stamps(&[6]), now()),
                Ok(7..8)
            );
            let backfill = accept(&mut order, Path::Backfill, &stamps(&[2]), now());
            assert_eq!(
                backfill,
                Err(Error::Backwards {
                    path: Path::Backfill,
                    before: s(2),
                    stamp: s(2),
                })
            );
            assert_eq!(
                accept(&mut order, Path::Backfill, &stamps(&[3]), now()),
                Ok(3..4)
            );
        }

        #[test]
        fn saturates_the_latest_stamp_at_both_ends() {
            let at = Stamp::from_nanos;
            let limits = Limits {
                earliest: Stamp::from_nanos(i64::MIN),
                ahead: Span::SECOND,
            };
            let mut order = Order::new(limits, Tail::default(), Tail::default());
            let last = Stamp::from_nanos(i64::MAX);
            assert_eq!(
                accept(&mut order, Path::Live, &bytes(&[last]), at(i64::MAX - 1)),
                Ok(0..1)
            );
            let behind = Limits {
                ahead: Span::from_nanos(-1),
                ..limits
            };
            let mut order = Order::new(behind, Tail::default(), Tail::default());
            assert_eq!(
                accept(
                    &mut order,
                    Path::Live,
                    &bytes(&[Stamp::EPOCH]),
                    at(i64::MIN)
                ),
                Err(Error::Ahead {
                    stamp: Stamp::EPOCH,
                    latest: Stamp::from_nanos(i64::MIN),
                })
            );
        }

        #[test]
        #[should_panic(expected = "invariant: a path takes fewer than 2^64 samples")]
        fn panics_past_the_last_seq() {
            let full = Tail {
                stamp: None,
                seq: u64::MAX,
            };
            let accepted = accept(
                &mut Order::new(limits(), full, Tail::default()),
                Path::Live,
                &stamps(&[1]),
                now(),
            );
            panic!("accepted past the last seq: {accepted:?}");
        }
    }

    mod rules {
        use proptest::prelude::*;

        use super::*;

        /// The rules, stated over every stamp accepted on each path.
        struct Model {
            limits: Limits,
            accepted: [Vec<Stamp>; 2],
            seq: [u64; 2],
        }

        fn index(path: Path) -> usize {
            match path {
                Path::Live => 0,
                Path::Backfill => 1,
            }
        }

        impl Model {
            fn new(limits: Limits, live: Tail, backfill: Tail) -> Self {
                Self {
                    limits,
                    accepted: [live, backfill].map(|t| t.stamp.into_iter().collect()),
                    seq: [live.seq, backfill.seq],
                }
            }

            fn accept(
                &mut self,
                path: Path,
                stamps: &[Stamp],
                now: Stamp,
            ) -> Result<Range<u64>, Error> {
                let p = index(path);
                let earliest = self.limits.earliest;
                let limit =
                    i128::from(now.nanos()) + i128::from(self.limits.ahead.nanos());
                let latest =
                    Stamp::from_nanos(i64::try_from(limit).unwrap_or(if limit > 0 {
                        i64::MAX
                    } else {
                        i64::MIN
                    }));
                let live = self.accepted[0].iter().max().copied();
                let backfill = self.accepted[1].iter().max().copied();
                for (i, &stamp) in stamps.iter().enumerate() {
                    let before = self.accepted[p].iter().chain(&stamps[..i]).max();
                    if stamp < earliest {
                        return Err(Error::Early { stamp, earliest });
                    }
                    if i128::from(stamp.nanos()) > limit {
                        return Err(Error::Ahead { stamp, latest });
                    }
                    if let Some(&before) = before
                        && stamp <= before
                    {
                        return Err(Error::Backwards {
                            path,
                            before,
                            stamp,
                        });
                    }
                    if let Some(newest) = backfill
                        && path == Path::Live
                        && stamp <= newest
                    {
                        return Err(Error::Overlap {
                            path,
                            stamp,
                            newest,
                        });
                    }
                    if let Some(newest) = live
                        && path == Path::Backfill
                        && stamp >= newest
                    {
                        return Err(Error::Overlap {
                            path,
                            stamp,
                            newest,
                        });
                    }
                }
                self.accepted[p].extend_from_slice(stamps);
                let start = self.seq[p];
                self.seq[p] += stamps.len() as u64;
                Ok(start..self.seq[p])
            }

            fn tail(&self, path: Path) -> Tail {
                Tail {
                    stamp: self.accepted[index(path)].iter().max().copied(),
                    seq: self.seq[index(path)],
                }
            }
        }

        fn tails(order: &Order) -> [Tail; 2] {
            [order.tail(Path::Live), order.tail(Path::Backfill)]
        }

        /// One frame: its path, its stamps, the length of the chunks it is checked in,
        /// and mesh time.
        type Frame = (Path, Vec<Stamp>, usize, Stamp);

        fn check(limits: Limits, start: [Tail; 2], frames: Vec<Frame>) {
            let mut order = Order::new(limits, start[0], start[1]);
            let mut model = Model::new(limits, start[0], start[1]);
            for (path, stamps, size, now) in frames {
                let before = tails(&order);
                let series = bytes(&stamps);
                let restarted = Order::new(limits, before[0], before[1])
                    .check(path, now)
                    .push(&series)
                    .map(|check| check.end().seq);
                let accepted = accept_in(&mut order, path, &series, size, now);
                assert_eq!(accepted, model.accept(path, &stamps, now));
                assert_eq!(
                    restarted, accepted,
                    "one chunk from a restart at the tails differs"
                );
                if accepted.is_err() {
                    assert_eq!(
                        tails(&order),
                        before,
                        "a rejected frame changed a tail"
                    );
                }
                assert_eq!(
                    tails(&order),
                    [Path::Live, Path::Backfill].map(|p| model.tail(p))
                );
            }
        }

        fn tail() -> impl Strategy<Value = Tail> {
            (proptest::option::of(0..200_i64), 0..1_000_u64).prop_map(|(stamp, seq)| {
                Tail {
                    stamp: stamp.map(Stamp::from_nanos),
                    seq,
                }
            })
        }

        /// Stamps that mostly increase, near each other, so that every rule breaks
        /// sometimes.
        fn frame() -> impl Strategy<Value = Frame> {
            (
                prop_oneof![Just(Path::Live), Just(Path::Backfill)],
                0..240_i64,
                proptest::collection::vec(-2..12_i64, 0..5),
                1..6_usize,
                prop_oneof![8 => 0..240_i64, 1 => Just(i64::MIN), 1 => Just(i64::MAX)],
            )
                .prop_map(|(path, first, steps, size, now)| {
                    let mut stamp = first;
                    let mut stamps = vec![Stamp::from_nanos(first)];
                    for step in steps {
                        stamp += step;
                        stamps.push(Stamp::from_nanos(stamp));
                    }
                    (path, stamps, size, Stamp::from_nanos(now))
                })
        }

        proptest! {
            #[test]
            fn hold_against_a_model(
                earliest in 0..20_i64,
                ahead in prop_oneof![
                    8 => -20..20_i64,
                    1 => Just(i64::MIN),
                    1 => Just(i64::MAX),
                ],
                live in tail(),
                backfill in tail(),
                frames in proptest::collection::vec(frame(), 0..40),
                empty in any::<bool>(),
            ) {
                let limits = Limits {
                    earliest: Stamp::from_nanos(earliest),
                    ahead: Span::from_nanos(ahead),
                };
                let mut frames = frames;
                if empty {
                    frames.push((Path::Live, Vec::new(), 1, Stamp::EPOCH));
                }
                check(limits, [live, backfill], frames);
            }
        }
    }
}

//! The limit on stateless resets.

use std::time::{Duration, Instant};

/// The least time between two resets for one group of connection IDs.
pub(super) const INTERVAL: Duration = Duration::from_millis(20);

/// Limits stateless resets to one for each group of issued connection IDs in each
/// [`INTERVAL`], from any source address. [`super::cid::group`] gives the group.
pub(super) struct Limit {
    last: [Option<Instant>; 256],
}

impl Limit {
    pub(super) fn new() -> Self {
        Self { last: [None; 256] }
    }

    /// Whether a reset for an ID in `group` may go at `now`. Records it when it may.
    pub(super) fn admit(&mut self, now: Instant, group: u8) -> bool {
        let last = &mut self.last[usize::from(group)];
        if last.is_some_and(|last| last + INTERVAL > now) {
            return false;
        }
        *last = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use types::time::Span;

    use super::*;
    use crate::quic::testing;

    fn epoch() -> Instant {
        testing::run(1, |shard| {
            shard
                .config(testing::SERVER_KEY, Span::SECOND)
                .clock
                .epoch()
        })
    }

    #[test]
    fn admits_one_reset_for_a_group_in_each_interval() {
        let (epoch, mut limit) = (epoch(), Limit::new());
        let nano = Duration::from_nanos(1);
        let at = [nano, nano, INTERVAL, INTERVAL + nano, INTERVAL * 2];
        let admitted = at.map(|at| limit.admit(epoch + at, 7));
        assert_eq!(admitted, [true, false, false, true, false]);
    }

    #[test]
    fn admits_each_group_on_its_own() {
        let (epoch, mut limit) = (epoch(), Limit::new());
        let admitted = [7, 8, 7, 0, 255].map(|group| limit.admit(epoch, group));
        assert_eq!(admitted, [true, true, false, true, true]);
    }

    proptest! {
        #[test]
        fn admits_a_reset_only_when_its_group_had_none_for_an_interval(
            steps in prop::collection::vec((0..30_000_000u64, 0..4u8), 1..64),
        ) {
            let (epoch, mut limit) = (epoch(), Limit::new());
            let mut now = epoch;
            let mut last: [Option<Instant>; 4] = [None; 4];
            for (gap, group) in steps {
                now += Duration::from_nanos(gap);
                let quiet = last[usize::from(group)]
                    .is_none_or(|last| now.duration_since(last) >= INTERVAL);
                prop_assert_eq!(limit.admit(now, group), quiet);
                if quiet {
                    last[usize::from(group)] = Some(now);
                }
            }
        }
    }
}

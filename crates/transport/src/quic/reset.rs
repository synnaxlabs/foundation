//! The limit on stateless resets.

use std::time::{Duration, Instant};

/// The least time between two resets for one connection ID.
pub(super) const INTERVAL: Duration = Duration::from_millis(20);

/// Limits stateless resets to one for each slot of issued connection IDs in each
/// [`INTERVAL`], from any source address. IDs share 256 slots by their first random
/// byte, so memory is fixed. A stranger that keeps the ID of a failed dial cannot
/// make the node send resets to spoofed addresses without bound, and takes a peer's
/// resets only while it holds an ID in the peer's slot.
pub(super) struct Limit {
    last: [Option<Instant>; 256],
}

impl Limit {
    pub(super) fn new() -> Self {
        Self { last: [None; 256] }
    }

    /// The slot of `datagram`'s connection ID when it has a short header, the only
    /// header that a reset answers. An issued ID starts with its shard, then random
    /// bytes.
    pub(super) fn slot(datagram: &[u8]) -> Option<u8> {
        match *datagram {
            [form, _shard, random, ..] if form & 0x80 == 0 => Some(random),
            _ => None,
        }
    }

    /// Whether a reset for an ID in `slot` may go at `now`. Records it when it may.
    pub(super) fn admit(&mut self, now: Instant, slot: u8) -> bool {
        let last = &mut self.last[usize::from(slot)];
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
    fn admits_one_reset_for_a_slot_in_each_interval() {
        let (epoch, mut limit) = (epoch(), Limit::new());
        let nano = Duration::from_nanos(1);
        let at = [nano, nano, INTERVAL, INTERVAL + nano, INTERVAL * 2];
        let admitted = at.map(|at| limit.admit(epoch + at, 7));
        assert_eq!(admitted, [true, false, false, true, false]);
    }

    #[test]
    fn admits_each_slot_on_its_own() {
        let (epoch, mut limit) = (epoch(), Limit::new());
        let admitted = [7, 8, 7, 0, 255].map(|slot| limit.admit(epoch, slot));
        assert_eq!(admitted, [true, true, false, true, true]);
    }

    #[test]
    fn takes_the_slot_from_the_first_random_byte_of_a_short_header() {
        let slots = [
            [0x40, 3, 9, 1].as_slice(),
            &[0x40, 3, 9],
            &[0x40, 3],
            &[0xc0, 3, 9, 1],
            &[],
        ]
        .map(Limit::slot);
        assert_eq!(slots, [Some(9), Some(9), None, None, None]);
    }

    proptest! {
        #[test]
        fn admits_a_reset_only_when_its_slot_had_none_for_an_interval(
            steps in prop::collection::vec((0..30_000_000u64, 0..4u8), 1..64),
        ) {
            let (epoch, mut limit) = (epoch(), Limit::new());
            let mut now = epoch;
            let mut last: [Option<Instant>; 4] = [None; 4];
            for (gap, slot) in steps {
                now += Duration::from_nanos(gap);
                let quiet = last[usize::from(slot)]
                    .is_none_or(|last| now.duration_since(last) >= INTERVAL);
                prop_assert_eq!(limit.admit(now, slot), quiet);
                if quiet {
                    last[usize::from(slot)] = Some(now);
                }
            }
        }
    }
}

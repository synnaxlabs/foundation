//! The limit on stateless resets.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use aws_lc_rs::hmac;
use env::entropy::Entropy;

/// The length of one window of [`Limit`].
pub(super) const WINDOW: Duration = Duration::from_millis(20);

/// The buckets that addresses hash into.
const BUCKETS: usize = 1 << 16;

/// Limits stateless resets to one for each address in each window. An IPv6 address
/// counts by its first 64 bits. A window lasts [`WINDOW`] and starts at the first
/// reset after the last window, so two resets to one address can be closer than
/// [`WINDOW`] at the edge of a window.
///
/// Addresses hash into [`BUCKETS`] buckets with a secret key, so a sender cannot
/// choose an address that shares a peer's bucket.
pub(super) struct Limit {
    key: hmac::Key,
    start: Option<Instant>,
    /// One bit for each bucket: set when a reset went to it in this window.
    sent: Box<[u64; BUCKETS / 64]>,
}

impl Limit {
    /// A limit with a hash key from `entropy`.
    pub(super) fn new(entropy: &Entropy) -> Self {
        let mut key = [0; 32];
        entropy.fill(&mut key);
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, &key),
            start: None,
            sent: Box::new([0; BUCKETS / 64]),
        }
    }

    /// Whether a reset to `ip` may go at `now`. Records it when it may.
    pub(super) fn admit(&mut self, now: Instant, ip: IpAddr) -> bool {
        if self.start.is_none_or(|start| start + WINDOW <= now) {
            self.start = Some(now);
            self.sent.fill(0);
        }
        let bucket = self.bucket(ip);
        let (word, bit) = (bucket / 64, 1 << (bucket % 64));
        let sent = self.sent[word] & bit != 0;
        self.sent[word] |= bit;
        !sent
    }

    fn bucket(&self, ip: IpAddr) -> usize {
        let tag = match ip.to_canonical() {
            IpAddr::V4(ip) => hmac::sign(&self.key, &ip.octets()),
            IpAddr::V6(ip) => hmac::sign(&self.key, &ip.octets()[..8]),
        };
        match *tag.as_ref() {
            [low, high, ..] => usize::from(u16::from_le_bytes([low, high])),
            _ => unreachable!("invariant: an HMAC-SHA256 tag is 32 bytes"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::net::{Ipv4Addr, Ipv6Addr};

    use proptest::prelude::*;
    use types::time::Span;

    use super::*;
    use crate::quic::pair;
    use crate::testing;

    const V4: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));

    /// The clock's epoch, and a limit with the simulated entropy.
    fn create() -> (Instant, Limit) {
        testing::run(1, |shard| {
            let config = shard.config(pair::SERVER_KEY, Span::SECOND);
            (config.clock.epoch(), Limit::new(&config.entropy))
        })
    }

    fn v6(segments: [u16; 8]) -> IpAddr {
        IpAddr::V6(Ipv6Addr::from(segments))
    }

    #[test]
    fn admits_one_reset_for_an_address_in_each_window() {
        let (epoch, mut limit) = create();
        let nano = Duration::from_nanos(1);
        let at = [nano, nano, WINDOW, WINDOW + nano, WINDOW * 2];
        let admitted = at.map(|at| limit.admit(epoch + at, V4));
        assert_eq!(admitted, [true, false, false, true, false]);
    }

    #[test]
    fn admits_each_address_on_its_own() {
        let (epoch, mut limit) = create();
        let other = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));
        let ips = [V4, other, V4, v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1])];
        let admitted = ips.map(|ip| limit.admit(epoch, ip));
        assert_eq!(admitted, [true, true, false, true]);
    }

    #[test]
    fn counts_an_ipv6_address_by_its_first_64_bits() {
        let (epoch, mut limit) = create();
        let ips = [
            v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1]),
            v6([0x2001, 0xdb8, 0, 0, 9, 9, 9, 9]),
            v6([0x2001, 0xdb8, 0, 1, 0, 0, 0, 1]),
        ];
        let admitted = ips.map(|ip| limit.admit(epoch, ip));
        assert_eq!(admitted, [true, false, true]);
    }

    #[test]
    fn counts_a_mapped_ipv4_address_as_the_ipv4_address() {
        let (epoch, mut limit) = create();
        let mapped = v6([0, 0, 0, 0, 0, 0xffff, 0xc000, 0x0201]);
        let admitted = [mapped, V4].map(|ip| limit.admit(epoch, ip));
        assert_eq!(admitted, [true, false]);
    }

    #[test]
    fn hashes_with_a_key_from_entropy() {
        let buckets = |value| {
            testing::run(value, |shard| {
                let config = shard.config(pair::SERVER_KEY, Span::SECOND);
                Limit::new(&config.entropy).bucket(V4)
            })
        };
        assert_ne!(buckets(1), buckets(2));
    }

    proptest! {
        #[test]
        fn admits_one_reset_for_each_bucket_in_a_window(
            hosts in prop::collection::vec(any::<u32>(), 1..512),
        ) {
            let (epoch, mut limit) = create();
            let mut sent = BTreeSet::new();
            for host in hosts {
                let ip = IpAddr::V4(Ipv4Addr::from(host));
                prop_assert_eq!(limit.admit(epoch, ip), sent.insert(limit.bucket(ip)));
            }
        }

        #[test]
        fn admits_a_reset_to_one_address_only_after_a_window_without_one(
            steps in prop::collection::vec((0..30_000_000u64, any::<bool>()), 1..64),
        ) {
            let (epoch, mut limit) = create();
            let mapped = v6([0, 0, 0, 0, 0, 0xffff, 0xc000, 0x0201]);
            let (mut now, mut last) = (epoch, None);
            for (gap, v4) in steps {
                now += Duration::from_nanos(gap);
                let quiet =
                    last.is_none_or(|last: Instant| now.duration_since(last) >= WINDOW);
                prop_assert_eq!(limit.admit(now, if v4 { V4 } else { mapped }), quiet);
                if quiet {
                    last = Some(now);
                }
            }
        }
    }
}

//! Entry points for the crate's benchmark, behind the `bench` feature. Not a contract:
//! the public surface of the crate replaces it.

use std::ops::Range;

use types::time::{Span, Stamp};

use crate::order::{Config, Order, Path, Tail};

/// Accepts `stamps` as a live frame at `now` on an index whose newest live stamp is the
/// epoch, and returns the seq of its samples.
///
/// # Panics
///
/// If the stamps are not after the epoch, do not strictly increase, or are past `now`.
#[must_use]
pub fn accept(stamps: &[Stamp], now: Stamp) -> Range<u64> {
    let config = Config {
        earliest: Stamp::EPOCH,
        ahead: Span::ZERO,
    };
    let live = Tail {
        stamp: Some(Stamp::EPOCH),
        seq: 0,
    };
    Order::new(config, live, Tail::default())
        .accept(Path::Live, stamps, now)
        .expect("invariant: the benchmark's stamps follow the rules")
}

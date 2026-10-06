//! The links between nodes: one direction of a path, with its delay and faults.

use std::num::NonZeroU64;

use types::time::Span;

use crate::chance;

/// One direction of a path between two nodes. Build it with `..Config::default()`:
/// fields get added.
///
/// ```
/// let lossy = sim::link::Config { loss: 0.01, ..sim::link::Config::default() };
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    /// The one-way delay. Not negative.
    pub delay: Span,
    /// The most extra delay, uniform per packet. It reorders datagrams, but not the
    /// segments of one direction of a TCP stream. Not negative.
    pub jitter: Span,
    /// The chance, from 0 to 1, that a datagram is lost. 1 cuts the link. A TCP send
    /// on a link with loss panics: sim does not simulate it yet.
    pub loss: f64,
    /// The chance, from 0 to 1, that a datagram arrives twice.
    pub duplication: f64,
    /// The largest IP packet in bytes. A datagram is lost when it is larger with its
    /// headers: 28 bytes on IPv4, 48 on IPv6. A TCP segment carries at most the MTU
    /// less 40 bytes on IPv4, 60 on IPv6.
    pub mtu: usize,
    /// The most bytes per second that the link sends, counted as IP packets with
    /// the headers that [`mtu`](Self::mtu) gives. The link sends one packet at a
    /// time, in the order they were sent, and each then takes the delay and the
    /// jitter. `None` has no limit.
    pub rate: Option<NonZeroU64>,
}

impl Default for Config {
    /// 250 us of delay, no jitter, loss, or duplication, an MTU of 1,500 bytes, and
    /// no rate.
    fn default() -> Self {
        Self {
            delay: Span::from_nanos(250 * Span::MICROSECOND.nanos()),
            jitter: Span::ZERO,
            loss: 0.0,
            duplication: 0.0,
            mtu: 1_500,
            rate: None,
        }
    }
}

impl Config {
    /// Panics with the config when a span is negative or a chance is not from 0 to 1.
    pub(crate) fn check(&self) {
        let spans = [self.delay, self.jitter];
        let chances = [self.loss, self.duplication];
        let valid = spans.iter().all(|&span| span >= Span::ZERO)
            && chances.into_iter().all(chance::valid);
        assert!(
            valid,
            "{self:?} has a negative span or a chance outside 0 to 1"
        );
    }
}

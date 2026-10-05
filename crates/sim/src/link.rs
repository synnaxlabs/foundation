//! The links between nodes: one direction of a path, with its delay and faults.

use types::time::Span;

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
    /// The most extra delay, uniform per packet. It reorders packets. Not negative.
    pub jitter: Span,
    /// The chance, from 0 to 1, that a packet is lost. 1 cuts the link.
    pub loss: f64,
    /// The chance, from 0 to 1, that a datagram arrives twice.
    pub duplication: f64,
    /// The largest IP packet in bytes. A datagram is lost when it is larger with its
    /// headers: 28 bytes on IPv4, 48 on IPv6.
    pub mtu: usize,
}

impl Default for Config {
    /// 250 us of delay, no jitter, loss, or duplication, and an MTU of 1,500 bytes.
    fn default() -> Self {
        Self {
            delay: Span::from_nanos(250 * Span::MICROSECOND.nanos()),
            jitter: Span::ZERO,
            loss: 0.0,
            duplication: 0.0,
            mtu: 1_500,
        }
    }
}

impl Config {
    /// Panics with the config when a span is negative or a chance is not from 0 to 1.
    pub(crate) fn check(&self) {
        let spans = [self.delay, self.jitter];
        let chances = [self.loss, self.duplication];
        let valid = spans.iter().all(|&span| span >= Span::ZERO)
            && chances.iter().all(|chance| (0.0..=1.0).contains(chance));
        assert!(
            valid,
            "{self:?} has a negative span or a chance outside 0 to 1"
        );
    }
}

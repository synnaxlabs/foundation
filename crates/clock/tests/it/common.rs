use sim::Sim;
use sim::node::{self, Node};
use types::time::Span;

/// 36500 days, the error of an unknown measurement.
pub(crate) const UNKNOWN: Span = Span::from_nanos(36_500 * Span::DAY.nanos());

pub(crate) fn ms(n: i64) -> Span {
    Span::from_nanos(n * 1_000_000)
}

/// A simulated node whose OS gives a bound of 10 ms.
pub(crate) fn node() -> (Sim, Node) {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(node::Config::default());
    (sim, node)
}

use estimate::Measurement;
use sim::Sim;
use sim::node::{self, Node};
use types::time::{Monotonic, Span};

/// The error of an unknown measurement.
pub(crate) const UNKNOWN: Span = Measurement::unknown(Monotonic(0), Span::ZERO).error();

pub(crate) fn ms(n: i64) -> Span {
    Span::from_nanos(n * 1_000_000)
}

/// A simulated node whose OS gives a bound of 10 ms.
pub(crate) fn node() -> (Sim, Node) {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(node::Config::default());
    (sim, node)
}

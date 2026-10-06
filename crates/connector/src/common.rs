//! Test helpers for the modules of this crate.

use env::clock::Clock;
use env::entropy::Entropy;
use env::tasks::Tasks;

/// Runs `main` on a shard of one simulated node and returns its output.
pub(crate) fn run<T, F>(
    main: impl FnOnce(Clock, Tasks, Entropy) -> F + Send + 'static,
) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| {
        main(node.clock(), tasks, node.entropy())
    })
    .expect("the run ends")
}

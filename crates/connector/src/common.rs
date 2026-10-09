//! Test helpers for the modules of this crate.

use env::clock::Clock;
use env::entropy::Entropy;
use env::tasks::Tasks;
use types::time::Stamp;

use crate::kind::Table;
use crate::{supervisor, testing};

/// Runs `main` on a shard of one simulated node and returns its output.
pub(crate) fn run<T, F>(
    main: impl FnOnce(Clock, Tasks, Entropy) -> F + Send + 'static,
) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    run_on(|node, tasks| main(node.clock(), tasks, node.entropy()))
}

/// Runs `main` on a shard of one simulated node, given the node, and returns its
/// output.
pub(crate) fn run_on<T, F>(
    main: impl FnOnce(sim::node::Node, Tasks) -> F + Send + 'static,
) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, main).expect("the run ends")
}

/// The inputs of a supervisor of `kinds` on a shard of `node`, with a hub on a new
/// shard, and the mesh time once the clock has one.
pub(crate) async fn create_config(
    node: &sim::node::Node,
    tasks: Tasks,
    kinds: Table,
) -> (supervisor::Config, Stamp) {
    let env = hub::testing::Env {
        files: node.files(),
        clock: node.clock(),
        wall: node.wall(),
        entropy: node.entropy(),
        tasks,
    };
    testing::create_config(env, node.net(), kinds).await
}

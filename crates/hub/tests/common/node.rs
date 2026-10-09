//! The seams of a sim node, for a test shard.

use env::tasks::Tasks;
use hub::testing::Env;

/// The seams of `node`, with `tasks`.
pub(crate) fn env(node: &sim::node::Node, tasks: Tasks) -> Env {
    Env {
        files: node.files(),
        clock: node.clock(),
        wall: node.wall(),
        entropy: node.entropy(),
        tasks,
    }
}

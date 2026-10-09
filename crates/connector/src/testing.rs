//! The inputs of a supervisor, for the tests of `connector` and of the kinds.

use std::sync::Arc;

use env::net::Net;
use types::time::Stamp;

use crate::kind::Table;
use crate::supervisor;

/// A supervisor's inputs for the tests of `connector` and the kinds: `kinds`, the
/// seams of `env` and `net`, and a hub on a new shard in `env`. Also gives the mesh
/// time once the clock has one.
///
/// # Panics
///
/// As [`hub::testing::open`].
pub async fn create_config(
    env: hub::testing::Env,
    net: Net,
    kinds: Table,
) -> (supervisor::Config, Stamp) {
    let (clock, entropy, tasks) =
        (env.clock.clone(), env.entropy.clone(), env.tasks.clone());
    let (hub, now) = hub::testing::open(env).await;
    let config = supervisor::Config {
        kinds: Arc::new(kinds),
        clock,
        entropy,
        net,
        tasks,
        hub,
    };
    (config, now)
}

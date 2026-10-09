//! A hub for the tests and benches of `hub` and the crates above it.

pub use home::testing::Env;
use types::time::Stamp;

use crate::{Config, Hub};

/// A hub on the shard that [`home::testing::shard`] gives in `env`, and the mesh time
/// that it gives. It defines no channel and has no region, so its node,
/// `node::Key::from_u128(1)`, is the home of each index.
///
/// # Panics
///
/// When the ring does not open.
pub async fn open(env: Env) -> (Hub, Stamp) {
    let tasks = env.tasks.clone();
    let entropy = env.entropy.clone();
    let (home, interner, now, time) = home::testing::shard(env).await;
    let hub = Hub::new(Config {
        home,
        interner,
        tasks,
        node: types::node::Key::from_u128(1),
        time,
        entropy,
        region: None,
    });
    (hub, now)
}

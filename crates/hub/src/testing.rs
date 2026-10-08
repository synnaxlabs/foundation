//! A hub for the tests and benches of `hub` and the crates above it.

pub use home::testing::Env;
use types::time::Stamp;

use crate::{Config, Hub};

/// A hub on the shard that [`home::testing::shard`] gives in `env`, and the mesh time
/// that it gives. It defines no channel.
///
/// # Panics
///
/// When the ring does not open.
pub async fn open(env: Env) -> (Hub, Stamp) {
    let tasks = env.tasks.clone();
    let (home, interner, now) = home::testing::shard(env).await;
    let hub = Hub::new(Config {
        home,
        interner,
        tasks,
    });
    (hub, now)
}

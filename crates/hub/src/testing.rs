//! A hub for the tests and benches of `hub` and the crates above it.

pub use home::testing::Env;
use types::time::Stamp;

use crate::{Config, Hub};

/// A hub on a new shard in `env`, and the mesh time once the clock has one. It defines
/// no channel. [`home::testing::shard`] states the shard's ring.
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

//! A hub for the tests and benches of `hub` and the crates above it.

pub use home::testing::Env;
use types::time::{Span, Stamp};

use crate::{Config, Hub};

/// A hub on the shard that [`home::testing::shard`] gives in `env`, and the mesh time
/// that it gives. It defines no channel and has no region, so its node,
/// `node::Key::from_u128(1)`, is the home of each index.
///
/// # Panics
///
/// When the ring does not open.
pub async fn open(env: Env) -> (Hub, Stamp) {
    let (tasks, entropy) = (env.tasks.clone(), env.entropy.clone());
    let (home, interner, now, time) = home::testing::shard(env).await;
    (create_hub(home, interner, time, tasks, entropy), now)
}

/// A hub as [`open`] gives, whose mesh clock starts `delay` after the call, so the
/// node has no mesh time until then and [`Hub::writer`] waits for it.
///
/// # Panics
///
/// When the ring does not open.
pub async fn open_unsynced(env: Env, delay: Span) -> Hub {
    let (tasks, entropy) = (env.tasks.clone(), env.entropy.clone());
    let (home, interner, time) = home::testing::unsynced_shard(env, delay).await;
    create_hub(home, interner, time, tasks, entropy)
}

/// The hub of the node `node::Key::from_u128(1)` on `home`, with no region.
fn create_hub(
    home: home::Shard,
    interner: types::frame::key_set::Interner,
    time: clock::Reader,
    tasks: env::tasks::Tasks,
    entropy: env::entropy::Entropy,
) -> Hub {
    Hub::new(Config {
        home,
        interner,
        tasks,
        node: types::node::Key::from_u128(1),
        time,
        entropy,
        region: None,
    })
}

//! Test helpers for the modules of this crate.

use std::sync::{Arc, Mutex};

use env::clock::Clock;
use env::tasks::Tasks;

/// Runs `main` on a shard of one simulated node and returns its output.
pub(crate) fn run<T, F>(main: impl FnOnce(Clock, Tasks) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let clock = node.clock();
    let out = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&out);
    let config = env::shards::Config {
        name: "shard-0".into(),
        core: Some(0),
    };
    let handle = node
        .shards()
        .start(config, move |tasks| async move {
            let value = main(clock, tasks).await;
            *slot.lock().expect("no panic under the lock") = Some(value);
        })
        .expect("the shard starts");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ends");
    let value = out.lock().expect("no panic under the lock").take();
    value.expect("main returned")
}

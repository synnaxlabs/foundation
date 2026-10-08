//! A shard for the tests and benches of `home` and the crates above it.

use std::path::PathBuf;
use std::rc::Rc;

use block::{Heap, Pool};
use types::frame::key_set::Interner;
use types::time::{Span, Stamp};

use crate::Shard;

/// The seams that a test shard runs on.
#[derive(Debug)]
pub struct Env {
    /// Where the shard keeps its ring.
    pub files: env::files::Files,
    /// The monotonic clock.
    pub clock: env::clock::Clock,
    /// The wall clock that the mesh clock reads.
    pub wall: env::wall::Wall,
    /// The randomness of the ring.
    pub entropy: env::entropy::Entropy,
    /// Where the shard spawns the mesh clock and the ring's tasks.
    pub tasks: env::tasks::Tasks,
}

/// A shard on a new ring in `env`, its interner, and the mesh time once the clock has
/// one.
///
/// The ring is 4 MiB in `shard-0`, with record bodies of at most 64 KiB and a commit
/// each 10 ms. A commit takes whole 4 KiB blocks (one for a frame, three for 64), and
/// nothing frees the ring until #160, so a run fills it at 1023 one-frame commits.
///
/// # Panics
///
/// When the ring does not open.
pub async fn shard(env: Env) -> (Shard, Interner, Stamp) {
    let config = block::Config { budget: 1 << 23 };
    let pool = Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
    let (clock, mesh) = clock::Clock::new(env.clock.clone());
    let wall = env.wall;
    env.tasks.spawn(async move { clock.run(wall).await });
    let mut interner = Interner::new();
    let config = buffer::Config {
        files: env.files,
        dir: PathBuf::from("shard-0"),
        pool,
        clock: env.clock.clone(),
        tasks: env.tasks,
        entropy: env.entropy,
        layout: buffer::Layout::new(1 << 22, 1 << 16).expect("a ring"),
        commit: Span::from_nanos(10_000_000),
    };
    let buffer = buffer::Buffer::open(config, interner.slots())
        .await
        .expect("opens");
    let shard = Shard::new(crate::Config {
        shard: 0,
        buffer,
        clock: mesh.clone(),
        limits: crate::order::Limits {
            earliest: Stamp::from_nanos(1),
            ahead: Span::from_nanos(1_000_000_000),
        },
    });
    loop {
        if let Some(now) = mesh.now().mesh {
            return (shard, interner, now.latest);
        }
        env.clock.sleep(Span::from_nanos(1)).await;
    }
}

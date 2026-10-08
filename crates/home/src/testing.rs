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
/// one: the midpoint of the clock's interval, as the shard stamps it.
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
            let midpoint = now.earliest.nanos().midpoint(now.latest.nanos());
            return (shard, interner, Stamp::from_nanos(midpoint));
        }
        env.clock.sleep(Span::from_nanos(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use env::files::Mode;
    use types::authority::Authority;
    use types::channel;
    use types::frame::key_set::Group;
    use types::frame::{Draft, Form, Label, Path as Route};
    use types::sample::{Scalar, Type};

    use super::*;
    use crate::Outcome;

    /// Runs `body` on a shard of a sim node made with `config`.
    fn run<T: Send + 'static, F: Future<Output = T> + 'static>(
        config: sim::node::Config,
        body: impl FnOnce(sim::node::Node, Shard, Interner, Stamp) -> F + Send + 'static,
    ) -> T {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(config);
        sim.run_on(&node, |node, tasks| async move {
            let env = Env {
                files: node.files(),
                clock: node.clock(),
                wall: node.wall(),
                entropy: node.entropy(),
                tasks,
            };
            let (shard, interner, now) = shard(env).await;
            body(node, shard, interner, now).await
        })
        .expect("the run ends")
    }

    #[test]
    fn opens_a_ring_of_4_mib_in_shard_0() {
        let len = run(sim::node::Config::default(), |node, _, _, _| async move {
            let files = node.files();
            let ring = files.open(Path::new("shard-0/ring"), Mode::Read).await;
            ring.expect("the ring exists").len()
        });

        // The ring follows two 4 KiB header blocks.
        assert_eq!(len, (1 << 22) + 2 * 4096);
    }

    #[test]
    fn applies_a_frame_at_the_mesh_time_it_gives_while_the_wall_error_is_unknown() {
        let config = sim::node::Config {
            wall_error: None,
            ..sim::node::Config::default()
        };
        let outcomes = run(config, |_, mut shard, mut interner, now| async move {
            let index = channel::Key::from_u128(1);
            let data = channel::Key::from_u128(2);
            let channels = [(data, Type::Scalar(Scalar::I64))];
            let set = interner.intern(&[Group {
                index,
                data: &channels,
            }]);
            shard.carry(set.entries()[0].slot);
            let writer = shard
                .open_writer(crate::writer::Writer {
                    subject: "a".parse().expect("a valid name"),
                    authority: Authority(1),
                    lease: None,
                    set: Arc::clone(&set),
                })
                .expect("opens");
            let series = [(0, 8), (1, 8)];
            let mut draft =
                Draft::new(shard.pool(), &set, Form::Raw, &series).expect("a frame");
            for (entry, _) in series {
                let bytes = draft.series_mut(entry).expect("the series is present");
                bytes.copy_from_slice(&now.nanos().to_le_bytes());
            }
            draft.set_count(0, 1);
            let outcomes = shard.write(writer, Label::Path(Route::Live), draft);
            outcomes.expect("the home takes it").to_vec()
        });

        assert!(
            matches!(outcomes[..], [Outcome::Applied { .. }]),
            "{outcomes:?}"
        );
    }
}

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

/// A shard on the ring in `shard-0` of `env.files`, its interner, and the mesh time
/// once the clock has one: the midpoint of the clock's interval, as the shard stamps
/// it.
///
/// It makes a ring of 4 MiB when `shard-0` holds none, and opens the one there
/// otherwise. A write waits at most 10 ms for its commit to start, and longer while an
/// earlier commit runs. A commit takes whole 4 KiB blocks, and each open takes one. A
/// commit of one frame of a stamp and an `i64` sample takes one block, and of 64 such
/// frames takes three. Nothing frees the ring until #160, so a new ring fills at 1023
/// commits of one such frame.
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
    use std::ops::Range;
    use std::path::Path;
    use std::sync::Arc;

    use env::files::Mode;
    use types::authority::Authority;
    use types::channel;
    use types::channel::Slot;
    use types::frame::key_set::{Group, KeySet};
    use types::frame::{Draft, Form, Label, Path as Route};
    use types::sample::{Scalar, Type};

    use super::*;
    use crate::{Outcome, Refusal, order};

    /// Runs `body` on a shard of a sim node made with `config`.
    fn run<T, F, B>(config: sim::node::Config, body: B) -> T
    where
        T: Send + 'static,
        F: Future<Output = T> + 'static,
        B: FnOnce(sim::node::Node, Shard, Interner, Stamp) -> F + Send + 'static,
    {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(config);
        sim.run_on(&node, |node, tasks| async move {
            let (shard, interner, now) = shard(env(&node, tasks)).await;
            body(node, shard, interner, now).await
        })
        .expect("the run ends")
    }

    fn env(node: &sim::node::Node, tasks: env::tasks::Tasks) -> Env {
        Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks,
        }
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

    /// A writer on one index with one data channel, and its key set.
    fn open_writer(
        shard: &mut Shard,
        interner: &mut Interner,
    ) -> (crate::writer::Key, Arc<KeySet>) {
        let index = channel::Key::from_u128(1);
        let channels = [(channel::Key::from_u128(2), Type::Scalar(Scalar::I64))];
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
        (writer, set)
    }

    /// The outcomes of a frame of one sample at `stamp`.
    fn write(
        shard: &mut Shard,
        writer: crate::writer::Key,
        set: &KeySet,
        stamp: Stamp,
    ) -> Vec<Outcome> {
        let series = [(0, 8), (1, 8)];
        let mut draft =
            Draft::new(shard.pool(), set, Form::Raw, &series).expect("a frame");
        for (entry, _) in series {
            let bytes = draft.series_mut(entry).expect("the series is present");
            bytes.copy_from_slice(&stamp.nanos().to_le_bytes());
        }
        draft.set_count(0, 1);
        let outcomes = shard.write(writer, Label::Path(Route::Live), draft);
        outcomes.expect("the home takes it").to_vec()
    }

    /// Writes a frame at each stamp of `stamps` and waits for their commit, or gives
    /// false when the ring loses one.
    async fn commit(
        shard: &mut Shard,
        writer: crate::writer::Key,
        set: &KeySet,
        stamps: Range<i64>,
    ) -> bool {
        for stamp in stamps {
            let outcomes = write(shard, writer, set, Stamp::from_nanos(stamp));
            if !matches!(outcomes[..], [Outcome::Applied { .. }]) {
                assert!(
                    matches!(outcomes[..], [Outcome::Lost { .. }]),
                    "{outcomes:?}"
                );
                return false;
            }
        }
        shard.committed().await.expect("commits");
        true
    }

    /// The count of commits of `frames` frames each that a new ring takes.
    fn fill(frames: i64) -> u64 {
        run(
            sim::node::Config::default(),
            move |_, mut shard, mut interner, now| async move {
                let (writer, set) = open_writer(&mut shard, &mut interner);
                let mut commits = 0;
                let mut stamp = now.nanos();
                while commit(&mut shard, writer, &set, stamp..stamp + frames).await {
                    commits += 1;
                    stamp += frames;
                }
                commits
            },
        )
    }

    #[test]
    fn fills_a_new_ring_at_1023_one_frame_commits() {
        assert_eq!(fill(1), 1023);
    }

    #[test]
    fn fills_a_new_ring_at_341_commits_of_64_frames() {
        assert_eq!(fill(64), 341);
    }

    #[test]
    fn takes_a_block_of_the_ring_for_each_open() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let commits = sim
            .run_on(&node, |node, tasks| async move {
                let (mut shard, mut interner, now) =
                    shard(env(&node, tasks.clone())).await;
                let (writer, set) = open_writer(&mut shard, &mut interner);
                let mut stamp = now.nanos();
                for _ in 0..10 {
                    assert!(commit(&mut shard, writer, &set, stamp..stamp + 1).await);
                    stamp += 1;
                }
                let ended = shard.committed();
                drop(shard);
                ended.await.expect("the buffer ends");
                let (mut shard, mut interner, _) =
                    super::shard(env(&node, tasks)).await;
                let (writer, set) = open_writer(&mut shard, &mut interner);
                let mut commits = 10;
                while commit(&mut shard, writer, &set, stamp..stamp + 1).await {
                    commits += 1;
                    stamp += 1;
                }
                commits
            })
            .expect("the run ends");

        assert_eq!(commits, 1022);
    }

    #[test]
    fn gives_the_midpoint_of_the_clock_while_the_wall_error_is_unknown() {
        let config = sim::node::Config {
            wall_error: None,
            ..sim::node::Config::default()
        };
        let (ahead, applied, now) =
            run(config, |_, mut shard, mut interner, now| async move {
                let (writer, set) = open_writer(&mut shard, &mut interner);
                let late = Stamp::from_nanos(now.nanos() + 1_000_000_001);
                let ahead = write(&mut shard, writer, &set, late);
                (ahead, write(&mut shard, writer, &set, now), now)
            });

        let refusal = Refusal::Order(order::Error::Ahead {
            stamp: Stamp::from_nanos(now.nanos() + 1_000_000_001),
            latest: Stamp::from_nanos(now.nanos() + 1_000_000_000),
        });
        assert_eq!(
            ahead,
            [Outcome::Refused {
                slot: Slot::new(0),
                refusal
            }]
        );
        assert!(
            matches!(applied[..], [Outcome::Applied { .. }]),
            "{applied:?}"
        );
    }

    #[test]
    fn starts_the_commit_of_a_lone_frame_10_ms_after_its_write() {
        let elapsed = run(
            sim::node::Config::default(),
            |node, mut shard, mut interner, now| async move {
                let (writer, set) = open_writer(&mut shard, &mut interner);
                let start = node.clock().now();
                write(&mut shard, writer, &set, now);
                shard.committed().await.expect("commits");
                node.clock().now() - start
            },
        );

        let commit = Span::from_nanos(10_000_000);
        // The disk takes under 1 ms of it.
        let disk = Span::from_nanos(1_000_000);
        assert!(
            commit <= elapsed && elapsed.nanos() < commit.nanos() + disk.nanos(),
            "{elapsed:?}"
        );
    }
}

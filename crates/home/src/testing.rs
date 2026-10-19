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

/// A shard on the ring in `shard-0` of `env.files`, its interner, the mesh time once
/// the clock has one ([`Shard::now`]), and the reader of that time.
///
/// It makes a ring of 4 MiB when `shard-0` holds none, and opens the one there
/// otherwise. A write waits at most 10 ms for its commit to start, and longer while an
/// earlier commit runs. A commit takes whole 4 KiB blocks, and nothing frees the ring
/// until #160, so a new ring fills after about a thousand commits of one frame of a
/// stamp and an `i64` sample.
///
/// # Panics
///
/// When the ring does not open.
pub async fn shard(env: Env) -> (Shard, Interner, Stamp, clock::Reader) {
    let sleep = env.clock.clone();
    let (shard, interner, mesh) = unsynced_shard(env, Span::ZERO).await;
    loop {
        if let Some(now) = shard.now() {
            return (shard, interner, now, mesh);
        }
        sleep.sleep(Span::from_nanos(1)).await;
    }
}

/// A shard as [`shard`] gives, whose mesh clock starts `delay` after the call, so the
/// shard has no mesh time until then, its interner, and the reader of that time.
///
/// # Panics
///
/// When the ring does not open.
pub async fn unsynced_shard(env: Env, delay: Span) -> (Shard, Interner, clock::Reader) {
    let config = block::Config::new(1 << 23).expect("the budget fits");
    let pool = Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
    let (clock, mesh) = clock::Clock::new(env.clock.clone());
    let (wall, sleep) = (env.wall, env.clock.sleep(delay));
    env.tasks.spawn(async move {
        sleep.await;
        clock.run(wall).await;
    });
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
        trimmed: false,
    };
    let buffer = buffer::Buffer::open(config, interner.slots())
        .await
        .expect("opens");
    let shard = Shard::open(crate::Config {
        shard: 0,
        buffer,
        clock: mesh.clone(),
        limits: crate::order::Limits {
            earliest: Stamp::from_nanos(1),
            ahead: Span::from_nanos(1_000_000_000),
        },
    });
    (shard, interner, mesh)
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
            let (shard, interner, now, _) = shard(env(&node, tasks)).await;
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

    #[test]
    fn gives_no_mesh_time_before_the_delay_of_an_unsynced_shard() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let (before, at) = sim
            .run_on(&node, |node, tasks| async move {
                let clock = node.clock();
                let start = clock.now();
                let delay = Span::from_nanos(2_000_000_000);
                let (shard, _, _) = unsynced_shard(env(&node, tasks), delay).await;
                clock.sleep_until(start + delay - Span::from_nanos(1)).await;
                let before = shard.now();
                while shard.now().is_none() {
                    clock.sleep(Span::from_nanos(1_000_000)).await;
                }
                (before, clock.now() - start)
            })
            .expect("the run ends");
        assert_eq!(before, None);
        assert!(at >= Span::from_nanos(2_000_000_000), "mesh time at {at:?}");
        assert!(at < Span::from_nanos(2_100_000_000), "mesh time at {at:?}");
    }

    /// A writer on one index with `channels` data channels of `scalar`, and its key
    /// set.
    fn open_writer(
        shard: &mut Shard,
        interner: &mut Interner,
        scalar: Scalar,
        channels: u128,
    ) -> (crate::writer::Key, Arc<KeySet>) {
        let index = channel::Key::from_u128(1);
        let channels: Vec<_> = (2..channels + 2)
            .map(|key| (channel::Key::from_u128(key), Type::Scalar(scalar)))
            .collect();
        let set = interner.intern(&[Group {
            index,
            data: &channels,
        }]);
        let entry = set.entries()[0];
        assert!(shard.carry(entry.slot, entry.key));
        let writer = shard
            .open_writer(crate::writer::Writer {
                subject: "a".parse().expect("a valid name"),
                authority: Authority(1),
                via: types::node::Key::from_u128(1),
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
        let series: Vec<_> = set
            .entries()
            .iter()
            .enumerate()
            .map(|(entry, channel)| (entry, channel.data_type.width().expect("fixed")))
            .collect();
        let mut draft =
            Draft::new(shard.pool(), set, Form::Raw, &series).expect("a frame");
        let stamp = stamp.nanos().to_le_bytes();
        for (_, bytes) in draft.iter_mut() {
            bytes.fill(0);
            let width = bytes.len().min(8);
            bytes[..width].copy_from_slice(&stamp[..width]);
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

    #[test]
    fn fills_a_new_ring_at_1000_one_frame_commits() {
        let commits = run(
            sim::node::Config::default(),
            |_, mut shard, mut interner, now| async move {
                let (writer, set) =
                    open_writer(&mut shard, &mut interner, Scalar::I64, 1);
                let mut commits = 0;
                let mut stamp = now.nanos();
                while commit(&mut shard, writer, &set, stamp..stamp + 1).await {
                    commits += 1;
                    stamp += 1;
                }
                commits
            },
        );

        assert_eq!(commits, 1000);
    }

    /// The polls of 256 channels of `scalar` that a new ring takes, with a wait for
    /// their commit after each `batch` of them.
    fn polls(scalar: Scalar, batch: i64) -> i64 {
        run(
            sim::node::Config::default(),
            move |_, mut shard, mut interner, now| async move {
                let (writer, set) = open_writer(&mut shard, &mut interner, scalar, 256);
                let mut taken = 0;
                loop {
                    for _ in 0..batch {
                        let stamp = Stamp::from_nanos(now.nanos() + taken);
                        let outcomes = write(&mut shard, writer, &set, stamp);
                        if !matches!(outcomes[..], [Outcome::Applied { .. }]) {
                            assert!(
                                matches!(outcomes[..], [Outcome::Lost { .. }]),
                                "{outcomes:?}"
                            );
                            shard.committed().await.expect("commits");
                            return taken;
                        }
                        taken += 1;
                    }
                    shard.committed().await.expect("commits");
                }
            },
        )
    }

    #[test]
    fn fills_a_new_ring_at_the_polls_of_256_channels_of_each_type() {
        let filled: Vec<_> = crate::common::SCALARS
            .into_iter()
            .map(|scalar| (scalar, polls(scalar, 1), polls(scalar, 16)))
            .collect();

        // A poll a commit takes 3 blocks, 48 B a data sample, and 4 of uuid. A record
        // of the test ring holds at most 64 KiB, so 16 polls go in more than one
        // commit: a sample of 4 bytes or less takes about 37 B, one of 8 bytes 43 B,
        // and a uuid 60 B.
        assert_eq!(
            filled,
            [
                (Scalar::Bool, 333, 432),
                (Scalar::I8, 333, 432),
                (Scalar::I16, 333, 432),
                (Scalar::I32, 333, 432),
                (Scalar::I64, 333, 372),
                (Scalar::U8, 333, 432),
                (Scalar::U16, 333, 432),
                (Scalar::U32, 333, 432),
                (Scalar::U64, 333, 372),
                (Scalar::F32, 333, 432),
                (Scalar::F64, 333, 372),
                (Scalar::Stamp, 333, 372),
                (Scalar::Span, 333, 372),
                (Scalar::Uuid, 250, 266),
            ]
        );
    }

    #[test]
    fn opens_the_ring_that_shard_0_holds() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let commits = sim
            .run_on(&node, |node, tasks| async move {
                let (mut shard, mut interner, now, _) =
                    shard(env(&node, tasks.clone())).await;
                let (writer, set) =
                    open_writer(&mut shard, &mut interner, Scalar::I64, 1);
                let mut stamp = now.nanos();
                for _ in 0..10 {
                    assert!(commit(&mut shard, writer, &set, stamp..stamp + 1).await);
                    stamp += 1;
                }
                let ended = shard.committed();
                drop(shard);
                ended.await.expect("the buffer ends");
                let (mut shard, mut interner, _, _) =
                    super::shard(env(&node, tasks)).await;
                let (writer, set) =
                    open_writer(&mut shard, &mut interner, Scalar::I64, 1);
                let mut commits = 0;
                while commit(&mut shard, writer, &set, stamp..stamp + 1).await {
                    commits += 1;
                    stamp += 1;
                }
                commits
            })
            .expect("the run ends");

        // The 10 commits before the open keep their blocks.
        assert!(commits <= 1000 - 10, "{commits}");
    }

    #[test]
    fn gives_the_midpoint_of_the_clock_while_the_wall_error_is_unknown() {
        let config = sim::node::Config {
            wall_error: None,
            ..sim::node::Config::default()
        };
        let (ahead, applied, now) =
            run(config, |_, mut shard, mut interner, now| async move {
                let (writer, set) =
                    open_writer(&mut shard, &mut interner, Scalar::I64, 1);
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
    fn gives_the_reader_of_the_mesh_time_it_gives() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let (now, read) = sim
            .run_on(&node, |node, tasks| async move {
                let (_, _, now, reader) = shard(env(&node, tasks)).await;
                let mesh = reader.now().mesh.expect("mesh time");
                let midpoint = mesh.earliest.nanos().midpoint(mesh.latest.nanos());
                (now, Stamp::from_nanos(midpoint))
            })
            .expect("the run ends");

        assert_eq!(read, now);
    }

    #[test]
    fn starts_the_commit_of_a_lone_frame_10_ms_after_its_write() {
        let elapsed = run(
            sim::node::Config::default(),
            |node, mut shard, mut interner, now| async move {
                let (writer, set) =
                    open_writer(&mut shard, &mut interner, Scalar::I64, 1);
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

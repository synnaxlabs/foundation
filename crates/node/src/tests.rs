use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use env::thread;
use sim::shard::Fault;
use types::time::Span;

use crate::{Config, Error, Node};

struct Run {
    seed: u64,
    sim: sim::Sim,
    host: sim::node::Node,
    node: Node,
}

type Memory = Box<dyn FnMut(usize) -> Result<block::Heap, os::memory::Error>>;

/// Memory for a shard's pool, from the heap.
#[expect(
    clippy::unnecessary_wraps,
    reason = "it is the memory seam of `Config`"
)]
fn heap(len: usize) -> Result<block::Heap, os::memory::Error> {
    Ok(block::Heap::new(len))
}

/// Memory from the heap for each shard but the one on core `refused`, which gets
/// `error`. `node` asks once per shard, in order of core.
fn refuse(refused: usize, error: os::memory::Error) -> Memory {
    let mut core = 0;
    Box::new(move |len| {
        core += 1;
        if core - 1 == refused {
            Err(error)
        } else {
            heap(len)
        }
    })
}

/// The seams of `host`, with a pool budget of `budget` from `memory`.
fn config(
    host: &sim::node::Node,
    budget: usize,
    memory: Memory,
) -> Config<block::Heap> {
    Config {
        shards: host.shards(),
        clock: host.clock(),
        wall: host.wall(),
        budget,
        memory,
        files: {
            let host = host.clone();
            Arc::new(move || host.files())
        },
        entropy: host.entropy(),
    }
}

/// Starts a node on `cores` cores of a `sim` host, after `faults` aim at its shards.
fn start(seed: u64, cores: usize, faults: &[(usize, Fault)]) -> Run {
    start_with(seed, cores, faults, 1 << 20, Box::new(heap))
}

fn start_with(
    seed: u64,
    cores: usize,
    faults: &[(usize, Fault)],
    budget: usize,
    memory: Memory,
) -> Run {
    let host = sim::node::Config {
        cores: NonZeroUsize::new(cores).unwrap(),
        ..sim::node::Config::default()
    };
    start_on(seed, host, faults, budget, memory)
}

fn start_on(
    seed: u64,
    host: sim::node::Config,
    faults: &[(usize, Fault)],
    budget: usize,
    memory: Memory,
) -> Run {
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let host = sim.node(host);
    for &(core, fault) in faults {
        host.fail_shard(core, fault);
    }
    let node = Node::start(config(&host, budget, memory));
    Run {
        seed,
        sim,
        host,
        node,
    }
}

fn starts(run: &Run) -> Vec<(String, Option<usize>)> {
    let starts = run.host.shard_starts();
    starts.into_iter().map(|c| (c.name, c.core)).collect()
}

fn named(cores: &[usize]) -> Vec<(String, Option<usize>)> {
    cores
        .iter()
        .map(|&c| (format!("shard-{c}"), Some(c)))
        .collect()
}

#[test]
fn starts_one_pinned_shard_per_core_and_runs_until_stopped() {
    let mut run = start(7, 3, &[]);
    assert_eq!(starts(&run), named(&[0, 1, 2]));
    assert_eq!(run.sim.run_for(Span::HOUR), Ok(()));
    assert_eq!(run.node.stop.waiting(), 3);
    run.node.stop();
    assert_eq!(run.sim.run(), Ok(()));
    assert_eq!(run.node.join(), Ok(()));
}

#[test]
fn shard_0_runs_the_mesh_clock_on_the_os_clock() {
    let mut run = start(7, 3, &[]);
    run.host.set_wall_error(Some(Span::from_nanos(-1)));
    assert_eq!(
        run.sim.run(),
        Err(sim::Error::Panicked {
            thread: "shard-0".into(),
            message: "invariant: the OS error bound -1ns is negative".into(),
            seed: 7,
        })
    );
    assert_eq!(run.sim.run(), Ok(()));
    assert_eq!(
        run.node.join(),
        Err(Error::Panicked(thread::Panicked {
            name: "shard-0".into()
        }))
    );
}

#[test]
fn stop_before_the_shards_run_ends_them_and_repeats_freely() {
    let mut run = start(7, 2, &[]);
    run.node.stop();
    run.node.stop();
    assert_eq!(run.sim.run(), Ok(()));
    assert_eq!(run.node.join(), Ok(()));
}

#[test]
fn a_panic_in_one_shard_stops_the_others() {
    for seed in 0..32 {
        let mut run = start(seed, 3, &[(1, Fault::Panic)]);
        assert_eq!(
            run.sim.run(),
            Err(sim::Error::Panicked {
                thread: "shard-1".into(),
                message: "injected".into(),
                seed,
            })
        );
        assert_eq!(run.sim.run(), Ok(()), "seed {seed}");
        let e = run.node.join().unwrap_err();
        assert_eq!(
            e,
            Error::Panicked(thread::Panicked {
                name: "shard-1".into()
            })
        );
        assert_eq!(e.to_string(), "thread shard-1 panicked");
    }
}

#[test]
fn a_shard_that_cannot_start_stops_the_started_shards() {
    let mut run = start(7, 4, &[(2, Fault::Start)]);
    assert_eq!(starts(&run), named(&[0, 1, 2]));
    assert_eq!(run.sim.run(), Ok(()));
    let e = run.node.join().unwrap_err();
    assert_eq!(
        e,
        Error::Start(thread::Error::Start {
            name: "shard-2".into(),
            reason: "injected".into()
        })
    );
    assert_eq!(e.to_string(), "cannot start thread shard-2: injected");
}

#[test]
fn a_shard_that_cannot_pin_stops_the_node() {
    let mut run = start(7, 2, &[(0, Fault::Pin)]);
    assert_eq!(starts(&run), named(&[0]));
    assert_eq!(run.sim.run(), Ok(()));
    let e = run.node.join().unwrap_err();
    assert_eq!(
        e,
        Error::Start(thread::Error::Pin {
            name: "shard-0".into(),
            core: 0,
            reason: "injected".into()
        })
    );
    assert_eq!(
        e.to_string(),
        "cannot pin thread shard-0 to core 0: injected"
    );
}

/// Runs the sim until it ends, and returns the thread of each panic in order.
fn panics(run: &mut Run) -> Vec<String> {
    let mut threads = Vec::new();
    loop {
        match run.sim.run() {
            Ok(()) => return threads,
            Err(sim::Error::Panicked {
                thread,
                message,
                seed,
            }) => {
                assert_eq!((message.as_str(), seed), ("injected", run.seed));
                threads.push(thread);
            }
            Err(e) => panic!("{e}"),
        }
    }
}

#[test]
fn join_gives_the_first_shard_by_core_that_panicked() {
    for seed in 0..32 {
        let mut run = start(seed, 3, &[(0, Fault::Panic), (2, Fault::Panic)]);
        let mut threads = panics(&mut run);
        threads.sort();
        assert_eq!(threads, ["shard-0", "shard-2"], "seed {seed}");
        assert_eq!(
            run.node.join(),
            Err(Error::Panicked(thread::Panicked {
                name: "shard-0".into()
            })),
            "seed {seed}"
        );
    }
}

#[test]
fn join_gives_a_shard_that_could_not_start_over_one_that_panicked() {
    for seed in 0..32 {
        let mut run = start(seed, 3, &[(0, Fault::Panic), (1, Fault::Start)]);
        assert_eq!(starts(&run), named(&[0, 1]));
        assert_eq!(panics(&mut run), ["shard-0"], "seed {seed}");
        assert_eq!(
            run.node.join(),
            Err(Error::Start(thread::Error::Start {
                name: "shard-1".into(),
                reason: "injected".into()
            })),
            "seed {seed}"
        );
    }
}

#[test]
fn each_shard_reserves_its_part_of_the_budget_and_core_0_takes_the_rest() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&calls);
    // Each part is a multiple of the pool's alignment, so only the remainder moves
    // core 0 to a larger reservation.
    let budget = (9 << 20) + 2;
    let mut run = start_with(
        7,
        3,
        &[],
        budget,
        Box::new(move |len| {
            record.lock().unwrap().push(len);
            heap(len)
        }),
    );
    run.node.stop();
    assert_eq!(run.sim.run(), Ok(()));
    assert_eq!(run.node.join(), Ok(()));
    let part = budget / 3;
    assert_eq!(part * 3 + 2, budget);
    let reservation = |budget| block::Config { budget }.reservation();
    assert_eq!(
        *calls.lock().unwrap(),
        [reservation(part + 2), reservation(part), reservation(part)]
    );
}

#[test]
fn a_shard_with_no_memory_stops_the_node_before_later_shards_start() {
    for seed in 0..32 {
        let refused = os::memory::Error::Refused;
        let mut run = start_with(seed, 3, &[], 1 << 20, refuse(1, refused));
        assert_eq!(starts(&run), named(&[0]));
        assert_eq!(run.sim.run(), Ok(()), "seed {seed}");
        let e = run.node.join().unwrap_err();
        assert_eq!(
            e,
            Error::Memory {
                core: 1,
                error: refused
            }
        );
        assert_eq!(
            e.to_string(),
            "no memory for the pool of shard-1: the OS refused memory for the first \
             page of a reserve; free memory or raise the commit limit of the system"
        );
    }
}

#[test]
fn join_gives_a_shard_with_no_memory_over_one_that_panicked() {
    for seed in 0..32 {
        let len = block::Config {
            budget: (1 << 20) / 3,
        }
        .reservation();
        let error = os::memory::Error::Reserve { len, code: 12 };
        let mut run =
            start_with(seed, 3, &[(0, Fault::Panic)], 1 << 20, refuse(2, error));
        assert_eq!(starts(&run), named(&[0, 1]));
        assert_eq!(panics(&mut run), ["shard-0"], "seed {seed}");
        assert_eq!(
            run.node.join(),
            Err(Error::Memory { core: 2, error }),
            "seed {seed}"
        );
    }
}

#[test]
fn a_config_shows_its_budget_and_entropy_but_not_its_memory_or_files() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = sim.node(sim::node::Config::default());
    let config = config(&host, 4096, Box::new(heap));
    let (clock, wall, entropy) = (&config.clock, &config.wall, &config.entropy);
    assert_eq!(
        format!("{config:?}"),
        format!(
            "Config {{ shards: Shards {{ .. }}, clock: {clock:?}, wall: {wall:?}, \
             budget: 4096, entropy: {entropy:?}, .. }}"
        )
    );
}

#[test]
#[should_panic(expected = "pool budget 18446744073709551615 is too large")]
fn a_budget_past_the_address_space_panics_at_start() {
    drop(start_with(7, 1, &[], usize::MAX, Box::new(heap)));
}

#[test]
fn a_host_that_cannot_pin_starts_shards_on_no_core() {
    let host = sim::node::Config {
        cores: NonZeroUsize::new(2).unwrap(),
        unpinnable: true,
        ..sim::node::Config::default()
    };
    let mut run = start_on(7, host, &[], 1 << 20, Box::new(heap));
    assert_eq!(
        starts(&run),
        [("shard-0".to_string(), None), ("shard-1".to_string(), None)]
    );
    assert_eq!(run.sim.run_for(Span::HOUR), Ok(()));
    assert_eq!(run.node.stop.waiting(), 2);
    run.node.stop();
    assert_eq!(run.sim.run(), Ok(()));
    assert_eq!(run.node.join(), Ok(()));
}

/// Starts a node on `host`, runs it for an hour, stops it, and gives what `join`
/// gives.
fn run_on(sim: &mut sim::Sim, host: &sim::node::Node) -> Result<(), Error> {
    let node = Node::start(config(host, 1 << 20, Box::new(heap)));
    assert_eq!(sim.run_for(Span::HOUR), Ok(()));
    node.stop();
    assert_eq!(sim.run(), Ok(()));
    node.join()
}

/// The entries of directory `dir` of `host`'s data directory, sorted.
fn listed(sim: &mut sim::Sim, host: &sim::node::Node, dir: &str) -> Vec<PathBuf> {
    let dir = PathBuf::from(dir);
    let mut listed = sim
        .run_on(host, move |host, _| async move {
            host.files().list(&dir).await.expect("the directory lists")
        })
        .expect("the run ends");
    listed.sort();
    listed
}

fn host(sim: &mut sim::Sim, cores: usize) -> sim::node::Node {
    sim.node(sim::node::Config {
        cores: NonZeroUsize::new(cores).unwrap(),
        ..sim::node::Config::default()
    })
}

mod buffer {
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    use ::buffer::{Buffer, Entry};
    use types::channel::{Key, Slots};
    use types::frame::Path as Stream;
    use types::frame::key_set::Interner;
    use types::time::Stamp;

    use super::*;

    /// Writes one entry of index `key` to the ring of shard `core` of `host`.
    fn write(sim: &mut sim::Sim, host: &sim::node::Node, core: usize, key: u128) {
        sim.run_on(host, move |host, tasks| async move {
            let config = block::Config { budget: 1 << 20 };
            let memory = block::Heap::new(config.reservation());
            let pool = Rc::new(block::Pool::new(config, memory));
            let config = ::buffer::Config {
                files: host.files(),
                dir: PathBuf::from(format!("shard-{core}")),
                pool: Rc::clone(&pool),
                clock: host.clock(),
                tasks,
                entropy: host.entropy(),
                layout: ::buffer::Layout::new(crate::AREA, crate::BODY_MAX)
                    .expect("the ring sizes of node make a ring"),
                commit: crate::COMMIT,
            };
            let mut slots = Slots::new();
            let buffer = Buffer::open(config, &mut slots).await.expect("opens");
            let index = Key::from_u128(key);
            let entry = Entry {
                index,
                slot: slots.assign(index),
                path: Stream::Live,
                first: 0,
                len: 1,
                stored_at: Stamp::from_nanos(1),
                last: Some(Stamp::from_nanos(1)),
                tag: 0,
                parts: pool.alloc(8).expect("a block").freeze().into(),
            };
            buffer.append([entry]).expect("the ring has room");
            buffer.committed().await.expect("commits");
        })
        .expect("the run ends");
    }

    /// What the node's interner gives now.
    fn taken(node: &mut Node) -> Poll<Option<Interner>> {
        let mut cx = Context::from_waker(Waker::noop());
        Pin::new(&mut node.interner).poll(&mut cx)
    }

    #[test]
    fn the_shards_assign_the_indexes_they_recover_in_one_table() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        write(&mut sim, &host, 0, 1);
        write(&mut sim, &host, 1, 2);
        let mut node = Node::start(config(&host, 1 << 20, Box::new(heap)));
        assert_eq!(sim.run_for(Span::HOUR), Ok(()));
        let Poll::Ready(Some(mut interner)) = taken(&mut node) else {
            panic!("the last shard gave the interner");
        };
        let slots = interner.slots();
        let assigned = [3, 2, 1].map(|key| slots.assign(Key::from_u128(key)).get());
        assert_eq!(assigned, [2, 1, 0]);
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
    }

    /// A shard's part of the budget must hold the block that its buffer's open
    /// takes first. A shard that waits for the interner does not open after it.
    #[test]
    fn a_shard_part_too_small_for_the_buffer_stops_the_node() {
        let mut run = start_with(7, 32, &[], 1 << 20, Box::new(heap));
        assert_eq!(run.sim.run(), Ok(()));
        let e = run.node.join().unwrap_err();
        let pool = block::Error::TooLarge {
            requested: 52186,
            largest: 28672,
        };
        assert_eq!(
            e,
            Error::Buffer {
                core: 0,
                error: ::buffer::Error::Pool(pool),
            }
        );
        assert_eq!(
            e.to_string(),
            "cannot open the buffer of shard-0: the pool has no block: block of 52186 \
             bytes is above the largest block of 28672 bytes"
        );
    }

    #[test]
    fn each_shard_opens_a_ring_in_its_own_directory() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        assert_eq!(run_on(&mut sim, &host), Ok(()));
        let shards = ["shard-0", "shard-1"];
        let made = ["shard-0", "shard-1", "shards-2"];
        assert_eq!(listed(&mut sim, &host, ""), made.map(PathBuf::from));
        for shard in shards {
            assert_eq!(listed(&mut sim, &host, shard), [PathBuf::from("ring")]);
        }
        let len = sim
            .run_on(&host, |host, _| async move {
                let ring = Path::new("shard-1/ring");
                let file = host.files().open(ring, env::files::Mode::Read).await;
                file.expect("the ring opens").len()
            })
            .expect("the run ends");
        // Two header blocks of 4 KiB, then the area.
        assert_eq!(len, 8192 + (64 << 20));
    }

    /// A crash at any point of the claim and the first opens leaves a data directory
    /// that the next start opens.
    #[test]
    fn a_crash_during_the_opens_leaves_rings_the_next_start_opens() {
        for crash in [sim::Crash::Process, sim::Crash::Power] {
            // The claim ends at about 250 us, and the opens at about 1.3 ms.
            for step in 0..60 {
                let mut sim = sim::Sim::new(sim::Config::default());
                let host = host(&mut sim, 2);
                let node = Node::start(config(&host, 1 << 20, Box::new(heap)));
                let after = Span::from_nanos(step * 25_000);
                assert_eq!(sim.run_for(after), Ok(()), "{crash:?} at {after:?}");
                sim.crash(&host, crash);
                drop(node);
                assert_eq!(run_on(&mut sim, &host), Ok(()), "{crash:?} at {after:?}");
            }
        }
    }

    #[test]
    fn a_restart_opens_the_rings_it_left() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        assert_eq!(run_on(&mut sim, &host), Ok(()));
        // A ring or a record that was not there would need its directory made.
        for dir in ["shard-0", "shard-1", "shards-2"] {
            host.fail_file(Path::new(dir), env::files::Operation::CreateDir);
        }
        assert_eq!(run_on(&mut sim, &host), Ok(()));
    }

    /// Starts a node of `cores` shards whose ring on `core` does not open, after
    /// `faults` aim at its shards, and gives what `join` gives.
    fn refused(
        seed: u64,
        cores: usize,
        core: usize,
        faults: &[(usize, Fault)],
    ) -> Error {
        let mut run = start(seed, cores, faults);
        let ring = format!("shard-{core}/ring");
        run.host
            .fail_file(Path::new(&ring), env::files::Operation::Open);
        panics(&mut run);
        run.node.join().unwrap_err()
    }

    fn opened(core: usize) -> Error {
        Error::Buffer {
            core,
            error: ::buffer::Error::Files(env::files::Error::Io {
                path: PathBuf::from(format!("shard-{core}/ring")),
                operation: env::files::Operation::Open,
                code: 5,
            }),
        }
    }

    #[test]
    fn no_shard_opens_after_a_ring_that_does_not_open() {
        let mut run = start(7, 3, &[]);
        run.host
            .fail_file(Path::new("shard-1/ring"), env::files::Operation::Open);
        assert_eq!(panics(&mut run), Vec::<String>::new());
        assert!(matches!(taken(&mut run.node), Poll::Ready(None)));
        assert_eq!(run.node.join(), Err(opened(1)));
        let made = ["shard-0", "shards-3"].map(PathBuf::from);
        assert_eq!(listed(&mut run.sim, &run.host, ""), made);
    }

    #[test]
    fn a_shard_with_no_memory_keeps_the_interner_from_the_node() {
        let refused = os::memory::Error::Refused;
        let mut run = start_with(7, 3, &[], 1 << 20, refuse(1, refused));
        assert_eq!(run.sim.run(), Ok(()));
        assert!(matches!(taken(&mut run.node), Poll::Ready(None)));
    }

    #[test]
    fn a_ring_that_does_not_open_stops_the_node() {
        for seed in 0..32 {
            let e = refused(seed, 3, 1, &[]);
            assert_eq!(e, opened(1), "seed {seed}");
            assert_eq!(
                e.to_string(),
                "cannot open the buffer of shard-1: a file call failed: open of \
                 shard-1/ring failed with OS error 5"
            );
        }
    }

    #[test]
    fn join_gives_a_ring_that_did_not_open_over_a_shard_that_panicked() {
        for seed in 0..32 {
            let e = refused(seed, 3, 1, &[(2, Fault::Panic)]);
            assert_eq!(e, opened(1), "seed {seed}");
        }
    }

    #[test]
    fn join_gives_a_shard_that_could_not_start_over_a_ring_that_did_not_open() {
        for seed in 0..32 {
            let e = refused(seed, 3, 1, &[(2, Fault::Start)]);
            let start = thread::Error::Start {
                name: "shard-2".into(),
                reason: "injected".into(),
            };
            assert_eq!(e, Error::Start(start), "seed {seed}");
        }
    }
}

mod directory {
    use super::*;

    /// Makes the record of a node of `stored` shards in `host`'s data directory.
    fn record(sim: &mut sim::Sim, host: &sim::node::Node, stored: usize) {
        sim.run_on(host, move |host, _| async move {
            let record = PathBuf::from(format!("shards-{stored}"));
            host.files().create_dir(&record).await.expect("makes");
        })
        .expect("the run ends");
    }

    #[test]
    fn a_data_directory_made_for_another_shard_count_is_refused() {
        for (cores, text) in [
            (
                2,
                "the data directory holds 3 shards, but this node starts 2; start it \
                 on 3 cores",
            ),
            (
                4,
                "the data directory holds 3 shards, but this node starts 4; start it \
                 on 3 cores",
            ),
        ] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, cores);
            record(&mut sim, &host, 3);
            let e = run_on(&mut sim, &host).unwrap_err();
            assert_eq!(e, Error::Shards { stored: 3, cores });
            assert_eq!(e.to_string(), text);
            let listed = listed(&mut sim, &host, "");
            assert_eq!(listed, [PathBuf::from("shards-3")], "{cores} cores");
        }
    }

    /// With more than one record of another count, the error gives the smallest, in
    /// any order of the list.
    #[test]
    fn the_smallest_other_count_is_the_stored_one() {
        for (records, stored) in [(&[5, 3][..], 3), (&[3, 5], 3), (&[2, 4], 4)] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            for &k in records {
                record(&mut sim, &host, k);
            }
            let e = run_on(&mut sim, &host).unwrap_err();
            assert_eq!(e, Error::Shards { stored, cores: 2 }, "{records:?}");
        }
    }

    #[test]
    fn a_name_that_is_not_a_count_is_not_a_record() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        sim.run_on(&host, |host, _| async move {
            let files = host.files();
            let names = ["shards-x", "shards", "other-3", "shards-03", "shards-+3"];
            for name in names {
                files.create_dir(Path::new(name)).await.expect("makes");
            }
        })
        .expect("the run ends");
        assert_eq!(run_on(&mut sim, &host), Ok(()));
        let listed = listed(&mut sim, &host, "");
        let made = [
            "other-3",
            "shard-0",
            "shard-1",
            "shards",
            "shards-+3",
            "shards-03",
            "shards-2",
            "shards-x",
        ];
        assert_eq!(listed, made.map(PathBuf::from));
    }

    #[test]
    fn a_failed_file_call_of_the_claim_stops_the_node_before_any_ring() {
        use env::files::Operation::{CreateDir, List, SyncDir};
        for (path, operation, text, made) in [
            ("", List, "list of  failed with OS error 5", &[][..]),
            (
                "shards-2",
                CreateDir,
                "create_dir of shards-2 failed with OS error 5",
                &[][..],
            ),
            (
                "",
                SyncDir,
                "sync_dir of  failed with OS error 5",
                &["shards-2"][..],
            ),
        ] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            host.fail_file(Path::new(path), operation);
            let e = run_on(&mut sim, &host).unwrap_err();
            let io = env::files::Error::Io {
                path: PathBuf::from(path),
                operation,
                code: 5,
            };
            assert_eq!(e, Error::Directory(io), "{operation:?}");
            assert_eq!(
                e.to_string(),
                format!(
                    "cannot read or record the shard count of the data directory: \
                     {text}"
                )
            );
            let made: Vec<PathBuf> = made.iter().map(PathBuf::from).collect();
            assert_eq!(listed(&mut sim, &host, ""), made, "{operation:?}");
        }
    }

    #[test]
    fn join_gives_a_refused_data_directory_over_a_shard_that_panicked() {
        for seed in 0..32 {
            let mut sim = sim::Sim::new(sim::Config {
                seed,
                ..sim::Config::default()
            });
            let host = host(&mut sim, 3);
            record(&mut sim, &host, 2);
            host.fail_shard(2, Fault::Panic);
            let node = Node::start(config(&host, 1 << 20, Box::new(heap)));
            let mut run = Run {
                seed,
                sim,
                host,
                node,
            };
            panics(&mut run);
            let e = run.node.join().unwrap_err();
            assert_eq!(
                e,
                Error::Shards {
                    stored: 2,
                    cores: 3
                },
                "seed {seed}"
            );
        }
    }
}

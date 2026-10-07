use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use env::thread;
use sim::shard::Fault;
use types::byte::Size;
use types::time::Span;

use crate::{Config, Error, Node};

struct Run {
    seed: u64,
    sim: sim::Sim,
    host: sim::node::Node,
    node: Node,
}

/// A disk budget of two rings of 64 MiB, with their header blocks.
const DISK: Size = Size::from_bytes(2 * (8192 + (64 << 20)));

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

/// The seams of `host`, with a pool budget of `budget` from `memory`, and a disk
/// budget of [`DISK`].
fn config(host: &sim::node::Node, budget: Size, memory: Memory) -> Config<block::Heap> {
    Config {
        shards: host.shards(),
        clock: host.clock(),
        wall: host.wall(),
        budget,
        memory,
        files: {
            let host = host.clone();
            Box::new(move || {
                let host = host.clone();
                Box::new(move || host.files())
            })
        },
        entropy: host.entropy(),
        disk: DISK,
    }
}

/// Starts a node on `cores` cores of a `sim` host, after `faults` aim at its shards.
fn start(seed: u64, cores: usize, faults: &[(usize, Fault)]) -> Run {
    start_with(seed, cores, faults, Size::MEBIBYTE, Box::new(heap))
}

fn start_with(
    seed: u64,
    cores: usize,
    faults: &[(usize, Fault)],
    budget: Size,
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
    budget: Size,
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
    let budget = Size::from_bytes((9 << 20) + 2);
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
    let part = usize::try_from(budget.bytes() / 3).unwrap();
    assert_eq!(part * 3 + 2, (9 << 20) + 2);
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
        let mut run = start_with(seed, 3, &[], Size::MEBIBYTE, refuse(1, refused));
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
        let mut run = start_with(
            seed,
            3,
            &[(0, Fault::Panic)],
            Size::MEBIBYTE,
            refuse(2, error),
        );
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
    let config = config(&host, Size::from_bytes(4096), Box::new(heap));
    let (clock, wall, entropy) = (&config.clock, &config.wall, &config.entropy);
    assert_eq!(
        format!("{config:?}"),
        format!(
            "Config {{ shards: Shards {{ .. }}, clock: {clock:?}, wall: {wall:?}, \
             budget: Size(4096), entropy: {entropy:?}, disk: {DISK:?}, .. }}"
        )
    );
}

#[test]
#[should_panic(expected = "pool budget 18446744073709551615 is too large")]
fn a_budget_past_the_address_space_panics_at_start() {
    drop(start_with(
        7,
        1,
        &[],
        Size::from_bytes(u64::MAX),
        Box::new(heap),
    ));
}

#[test]
fn a_host_that_cannot_pin_starts_shards_on_no_core() {
    let host = sim::node::Config {
        cores: NonZeroUsize::new(2).unwrap(),
        unpinnable: true,
        ..sim::node::Config::default()
    };
    let mut run = start_on(7, host, &[], Size::MEBIBYTE, Box::new(heap));
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
    run_on_disk(sim, host, DISK.bytes())
}

/// As [`run_on`], with a disk budget of `disk`.
fn run_on_disk(
    sim: &mut sim::Sim,
    host: &sim::node::Node,
    disk: u64,
) -> Result<(), Error> {
    let node = Node::start(Config {
        disk: Size::from_bytes(disk),
        ..config(host, Size::MEBIBYTE, Box::new(heap))
    });
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

/// `join` gives the node's own failure, else the first shard error by core, else the
/// first panic by core, and joins every shard.
#[test]
fn join_gives_errors_in_order_of_precedence() {
    let panicked = |core: usize| thread::Panicked {
        name: format!("shard-{core}"),
    };
    let memory = Error::Memory {
        core: 2,
        error: os::memory::Error::Refused,
    };
    let shards = |stored: usize| Error::Shards { stored, cores: 3 };
    let mut joined = 0;
    let all = [
        (Err(panicked(0)), Some(shards(2))),
        (Ok(()), Some(shards(4))),
    ];
    let all = all.into_iter().inspect(|_| joined += 1);
    assert_eq!(crate::error(Some(memory.clone()), all), Err(memory));
    assert_eq!(joined, 2);
    let cases = [
        (
            vec![(Err(panicked(0)), None), (Ok(()), Some(shards(2)))],
            shards(2),
        ),
        (
            vec![(Ok(()), Some(shards(2))), (Ok(()), Some(shards(4)))],
            shards(2),
        ),
        (
            vec![
                (Ok(()), None),
                (Err(panicked(1)), None),
                (Err(panicked(2)), None),
            ],
            Error::Panicked(panicked(1)),
        ),
    ];
    for (shards, error) in cases {
        assert_eq!(crate::error(None, shards.into_iter()), Err(error));
    }
    assert_eq!(crate::error(None, [(Ok(()), None)].into_iter()), Ok(()));
}

mod buffer {
    use std::cell::RefCell;
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
                layout: ::buffer::Layout::new(64 << 20, crate::BODY_MAX)
                    .expect("the test sizes make a ring"),
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

    /// A data directory that a node of 3 shards left before the record existed is
    /// made for 3 shards, so a start on 2 cores is refused.
    #[test]
    fn rings_of_three_shards_with_no_record_are_refused_on_two_cores() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        for core in 0..3 {
            write(&mut sim, &host, core, 1 + core as u128);
        }
        let e = run_on(&mut sim, &host);
        let listed = listed(&mut sim, &host, "");
        assert_eq!(
            e,
            Err(Error::Shards {
                stored: 3,
                cores: 2
            }),
            "{listed:?}"
        );
    }

    #[test]
    fn the_shards_assign_the_indexes_they_recover_in_one_table() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        write(&mut sim, &host, 0, 1);
        write(&mut sim, &host, 1, 2);
        let mut node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
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
        let mut run = start_with(7, 32, &[], Size::MEBIBYTE, Box::new(heap));
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
        // Two header blocks of 4 KiB, then the area.
        assert_eq!(ring_len(&mut sim, &host, 1), 8192 + (64 << 20));
    }

    /// The length of the ring file of shard `core` of `host`.
    fn ring_len(sim: &mut sim::Sim, host: &sim::node::Node, core: usize) -> u64 {
        sim.run_on(host, move |host, _| async move {
            let ring = PathBuf::from(format!("shard-{core}/ring"));
            let file = host.files().open(&ring, env::files::Mode::Read).await;
            file.expect("the ring opens").len()
        })
        .expect("the run ends")
    }

    /// A ring file of an area of 8 MiB, with its two header blocks.
    const RING: u64 = 8192 + (8 << 20);

    /// Each part is a ring and 4095 bytes, and shard 0 takes one byte more: whole
    /// blocks only.
    #[test]
    fn each_ring_takes_its_part_of_the_disk_budget_and_shard_0_the_rest() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let disk = 2 * (RING + 4095) + 1;
        assert_eq!(run_on_disk(&mut sim, &host, disk), Ok(()));
        assert_eq!(ring_len(&mut sim, &host, 0), RING + 4096);
        assert_eq!(ring_len(&mut sim, &host, 1), RING);
    }

    /// A ring already there keeps its size, larger or smaller than its new part.
    #[test]
    fn a_restart_with_another_disk_budget_opens_the_rings_at_their_sizes() {
        for (first, ring, then) in
            [(2 * RING, RING, 4 * RING), (4 * RING, 2 * RING, 2 * RING)]
        {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            assert_eq!(run_on_disk(&mut sim, &host, first), Ok(()));
            assert_eq!(run_on_disk(&mut sim, &host, then), Ok(()), "{then}");
            for core in 0..2 {
                let len = ring_len(&mut sim, &host, core);
                assert_eq!(len, ring, "shard-{core} at {then}");
            }
        }
    }

    /// With one least ring no part holds a ring; one byte short of two, shard 0's part
    /// fits and shard 1's does not. Two least rings start.
    #[test]
    fn a_disk_budget_that_holds_no_ring_on_each_shard_starts_no_shard() {
        let smallest = ::buffer::Layout::fit(0, crate::BODY_MAX).unwrap_err().min;
        let min = Size::from_bytes(4_227_072);
        let cases = [(smallest, "2064KiB"), (2 * smallest - 1, "4227071B")];
        for (bytes, shown) in cases {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            let disk = Size::from_bytes(bytes);
            let node = Node::start(Config {
                disk,
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            assert_eq!(host.shard_starts(), [], "{shown}");
            assert_eq!(sim.run(), Ok(()));
            let e = node.join().unwrap_err();
            assert_eq!(
                e,
                Error::Disk {
                    disk,
                    cores: 2,
                    min
                },
                "{shown}"
            );
            assert_eq!(
                e.to_string(),
                format!(
                    "the disk budget {shown} holds no ring on each of 2 shards; it \
                     needs at least 4128KiB"
                )
            );
        }
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        assert_eq!(run_on_disk(&mut sim, &host, 2 * smallest), Ok(()));
    }

    /// The core count comes from the host. With 2^43 cores no `u64` budget holds a
    /// ring on each shard, the largest too, so `min` is the largest budget.
    #[test]
    fn a_least_disk_budget_past_a_u64_is_the_largest_budget() {
        let cores = 1_usize << 43;
        let min = Size::from_bytes(u64::MAX);
        for disk in [Size::from_bytes(1 << 30), min] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, cores);
            let node = Node::start(Config {
                disk,
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            assert_eq!(host.shard_starts(), []);
            assert_eq!(node.join(), Err(Error::Disk { disk, cores, min }));
        }
    }

    /// `node` calls the maker on the start thread just before it starts each shard,
    /// so call `k` is for shard `k`, and each shard runs the function it gets once.
    #[test]
    fn the_maker_makes_each_shard_its_files_before_the_shard_starts() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 3);
        let ran = Arc::new(Mutex::new(Vec::new()));
        let made = Rc::new(RefCell::new(Vec::new()));
        let node = Node::start(Config {
            files: {
                let host = host.clone();
                let ran = Arc::clone(&ran);
                let made = Rc::clone(&made);
                Box::new(move || {
                    let call = made.borrow().len();
                    made.borrow_mut().push(host.shard_starts().len());
                    let (host, ran) = (host.clone(), Arc::clone(&ran));
                    Box::new(move || {
                        ran.lock().unwrap().push(call);
                        host.files()
                    })
                })
            },
            ..config(&host, Size::MEBIBYTE, Box::new(heap))
        });
        assert_eq!(*made.borrow(), [0, 1, 2], "shards started before each call");
        assert_eq!(sim.run_for(Span::HOUR), Ok(()));
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
        let mut ran = ran.lock().unwrap().clone();
        ran.sort_unstable();
        assert_eq!(ran, [0, 1, 2]);
    }

    /// Starts a node of 3 shards, after `faults` aim at its shards, with memory from
    /// `memory`. Gives the result of `join`, the calls of the files maker, and the
    /// functions that ran.
    fn made(
        faults: &[(usize, Fault)],
        memory: Memory,
    ) -> (Result<(), Error>, usize, usize) {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 3);
        for &(core, fault) in faults {
            host.fail_shard(core, fault);
        }
        let ran = Arc::new(Mutex::new(0));
        let made = Rc::new(RefCell::new(0));
        let node = Node::start(Config {
            files: {
                let (host, ran, made) =
                    (host.clone(), Arc::clone(&ran), Rc::clone(&made));
                Box::new(move || {
                    *made.borrow_mut() += 1;
                    let (host, ran) = (host.clone(), Arc::clone(&ran));
                    Box::new(move || {
                        *ran.lock().unwrap() += 1;
                        host.files()
                    })
                })
            },
            ..config(&host, Size::MEBIBYTE, memory)
        });
        assert_eq!(sim.run(), Ok(()));
        (node.join(), *made.borrow(), *ran.lock().unwrap())
    }

    /// `node` makes the files of a shard after its memory and before its start. A
    /// shard that does not start drops its function unrun.
    #[test]
    fn a_shard_that_does_not_start_drops_its_files_unrun() {
        let error = os::memory::Error::Refused;
        let memory = Err(Error::Memory { core: 1, error });
        assert_eq!(made(&[], refuse(1, error)), (memory, 1, 1), "no memory");
        let start = Err(Error::Start(thread::Error::Start {
            name: "shard-1".into(),
            reason: "injected".into(),
        }));
        let made = made(&[(1, Fault::Start)], Box::new(heap));
        assert_eq!(made, (start, 2, 1), "no start");
    }

    #[test]
    fn a_stop_before_the_opens_makes_no_ring() {
        let mut run = start(7, 3, &[]);
        run.node.stop();
        assert_eq!(run.sim.run(), Ok(()));
        assert_eq!(run.node.join(), Ok(()));
        assert_eq!(listed(&mut run.sim, &run.host, ""), Vec::<PathBuf>::new());
    }

    /// A stop at any point of the claim and the opens ends each step that started
    /// and starts no other, so the data directory holds the claim and the rings of
    /// the first shards, each whole. A stop between the claim and the first open
    /// leaves the claim alone.
    #[test]
    fn a_stop_ends_the_steps_that_started_and_starts_no_other() {
        let all = ["shard-0", "shard-1", "shard-2", "shards-3"].map(PathBuf::from);
        let mut seen = [false; 5];
        for step in 0..200 {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 3);
            let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
            let after = Span::from_nanos(step * 10_000);
            assert_eq!(sim.run_for(after), Ok(()), "at {after:?}");
            node.stop();
            assert_eq!(sim.run(), Ok(()), "at {after:?}");
            assert_eq!(node.join(), Ok(()), "at {after:?}");
            let listed = listed(&mut sim, &host, "");
            let rings = listed.len().saturating_sub(1);
            let made: Vec<PathBuf> = if listed.is_empty() {
                Vec::new()
            } else {
                all[..rings].iter().chain(&all[3..]).cloned().collect()
            };
            assert_eq!(listed, made, "at {after:?}");
            seen[listed.len()] = true;
            assert_eq!(run_on(&mut sim, &host), Ok(()), "at {after:?}");
        }
        assert_eq!(seen[1..], [true; 4], "a stop after each step");
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
                let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
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
        let mut run = start_with(7, 3, &[], Size::MEBIBYTE, refuse(1, refused));
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

    /// A shard that panics as it starts stops the node before shard 1 starts its
    /// open, so `join` gives the panic.
    #[test]
    fn a_shard_that_panics_before_an_open_skips_it() {
        for seed in 0..32 {
            let e = refused(seed, 3, 1, &[(2, Fault::Panic)]);
            let panicked = thread::Panicked {
                name: "shard-2".into(),
            };
            assert_eq!(e, Error::Panicked(panicked), "seed {seed}");
        }
    }

    #[test]
    fn a_shard_that_cannot_start_skips_a_ring_that_would_not_open() {
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
    /// any order of the list. `shards-10` lists before `shards-3`.
    #[test]
    fn the_smallest_other_count_is_the_stored_one() {
        let cases = [(&[5, 3][..], 3), (&[3, 5], 3), (&[2, 4], 4), (&[10, 3], 3)];
        for (records, stored) in cases {
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
            let names = [
                "shards-x",
                "shards",
                "other-3",
                "shards-03",
                "shards-+3",
                "shards-0",
            ];
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
            "shards-0",
            "shards-03",
            "shards-2",
            "shards-x",
        ];
        assert_eq!(listed, made.map(PathBuf::from));
    }

    /// With no record, rings up to `shard-<k>` are a record of `k + 1`, and a name
    /// that is not a plain count is not a ring.
    #[test]
    fn rings_with_no_record_are_a_record() {
        let refused = |cores| Err(Error::Shards { stored: 3, cores });
        for (cores, made) in [(2, refused(2)), (3, Ok(())), (4, refused(4))] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, cores);
            sim.run_on(&host, |host, _| async move {
                let files = host.files();
                for name in ["shard-0", "shard-2", "shard-03", "shard-x"] {
                    files.create_dir(Path::new(name)).await.expect("makes");
                }
            })
            .expect("the run ends");
            assert_eq!(run_on(&mut sim, &host), made, "{cores} cores");
        }
    }

    /// The count of rings with no record is one over the largest index, in any order
    /// of the list. `shard-10` lists before `shard-9`.
    #[test]
    fn the_largest_ring_gives_the_count() {
        let (stored, cores) = (11, 2);
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, cores);
        sim.run_on(&host, |host, _| async move {
            let files = host.files();
            for name in ["shard-10", "shard-9"] {
                files.create_dir(Path::new(name)).await.expect("makes");
            }
        })
        .expect("the run ends");
        let e = run_on(&mut sim, &host).unwrap_err();
        assert_eq!(e, Error::Shards { stored, cores });
        assert_eq!(
            e.to_string(),
            "the data directory holds 11 shards, but this node starts 2; start it \
             on 11 cores"
        );
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

    /// No node has a shard of index `usize::MAX`, so that name is not a ring.
    #[test]
    fn a_ring_of_the_largest_index_is_not_a_ring() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let name = format!("shard-{}", usize::MAX);
        let made = name.clone();
        sim.run_on(&host, |host, _| async move {
            host.files()
                .create_dir(Path::new(&made))
                .await
                .expect("makes");
        })
        .expect("the run ends");
        assert_eq!(run_on(&mut sim, &host), Ok(()));
        let listed = listed(&mut sim, &host, "");
        let made = ["shard-0", "shard-1", &name, "shards-2"];
        assert_eq!(listed, made.map(PathBuf::from));
    }

    /// A count in plain decimal that does not fit a `usize` is not a ring and not a
    /// record.
    #[test]
    fn a_count_that_does_not_fit_a_usize_is_not_a_count() {
        let over = u128::try_from(usize::MAX).expect("fits") + 1;
        for prefix in ["shard", "shards"] {
            let name = format!("{prefix}-{over}");
            let made = name.clone();
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            sim.run_on(&host, move |host, _| async move {
                host.files()
                    .create_dir(Path::new(&made))
                    .await
                    .expect("makes");
            })
            .expect("the run ends");
            assert_eq!(run_on(&mut sim, &host), Ok(()), "{name}");
            let mut made = ["shard-0", "shard-1", &name, "shards-2"];
            made.sort_unstable();
            let made = made.map(PathBuf::from);
            assert_eq!(listed(&mut sim, &host, ""), made, "{name}");
        }
    }

    /// The largest count that fits: a record of `usize::MAX`, and a ring of index
    /// `usize::MAX - 1` with no record.
    #[test]
    fn the_largest_count_that_fits_is_a_count() {
        for name in [
            format!("shards-{}", usize::MAX),
            format!("shard-{}", usize::MAX - 1),
        ] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            let made = name.clone();
            sim.run_on(&host, move |host, _| async move {
                host.files()
                    .create_dir(Path::new(&made))
                    .await
                    .expect("makes");
            })
            .expect("the run ends");
            let refused = Err(Error::Shards {
                stored: usize::MAX,
                cores: 2,
            });
            assert_eq!(run_on(&mut sim, &host), refused, "{name}");
        }
    }

    /// The claim reads names only, so a file with the name of a record or a ring
    /// counts.
    #[test]
    fn a_file_named_as_a_count_is_a_count() {
        for name in ["shards-3", "shard-2"] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            sim.run_on(&host, move |host, _| async move {
                let mode = env::files::Mode::Create { len: 0 };
                let file = host.files().open(Path::new(name), mode).await;
                file.expect("makes").close().await;
            })
            .expect("the run ends");
            let refused = Err(Error::Shards {
                stored: 3,
                cores: 2,
            });
            assert_eq!(run_on(&mut sim, &host), refused, "{name}");
        }
    }

    /// A crash at the sync of the record: a power cut loses the record, and a
    /// process crash keeps it. The next start syncs the record before `shard-0`.
    #[test]
    fn a_crash_at_the_sync_of_the_record_leaves_it_whole_or_absent() {
        use env::files::Operation::SyncDir;
        let io = env::files::Error::Io {
            path: PathBuf::new(),
            operation: SyncDir,
            code: 5,
        };
        for (crash, kept) in [
            (sim::Crash::Power, &[][..]),
            (sim::Crash::Process, &["shards-2"][..]),
        ] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            host.fail_file(Path::new(""), SyncDir);
            let e = run_on(&mut sim, &host);
            assert_eq!(e, Err(Error::Directory(io.clone())), "{crash:?}");
            sim.crash(&host, crash);
            let kept: Vec<PathBuf> = kept.iter().map(PathBuf::from).collect();
            assert_eq!(listed(&mut sim, &host, ""), kept, "{crash:?}");
            host.fail_file(Path::new(""), SyncDir);
            let e = run_on(&mut sim, &host);
            assert_eq!(e, Err(Error::Directory(io.clone())), "{crash:?}");
            let made = [PathBuf::from("shards-2")];
            assert_eq!(listed(&mut sim, &host, ""), made, "{crash:?}");
        }
    }

    /// A shard with no memory stops the node before the claim starts, so the claim
    /// records no shard count.
    #[test]
    fn a_shard_with_no_memory_skips_the_claim() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let refused = os::memory::Error::Refused;
        let node = Node::start(config(&host, Size::MEBIBYTE, refuse(1, refused)));
        assert_eq!(sim.run(), Ok(()));
        let e = node.join();
        assert_eq!(
            e,
            Err(Error::Memory {
                core: 1,
                error: refused
            })
        );
        assert_eq!(listed(&mut sim, &host, ""), Vec::<PathBuf>::new());
    }

    /// A claim that started before a shard panicked runs to its end, and `join` gives
    /// its error over the panic. A panic before the claim skips it.
    #[test]
    fn join_gives_a_refused_data_directory_over_a_shard_that_panicked() {
        let shards = Error::Shards {
            stored: 2,
            cores: 3,
        };
        let panicked = Error::Panicked(thread::Panicked {
            name: "shard-2".into(),
        });
        let mut seen = [false; 2];
        for seed in 0..32 {
            let mut sim = sim::Sim::new(sim::Config {
                seed,
                ..sim::Config::default()
            });
            let host = host(&mut sim, 3);
            record(&mut sim, &host, 2);
            host.fail_shard(2, Fault::Panic);
            let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
            let mut run = Run {
                seed,
                sim,
                host,
                node,
            };
            panics(&mut run);
            let e = run.node.join().unwrap_err();
            let claimed = e == shards;
            assert!(claimed || e == panicked, "seed {seed}: {e:?}");
            seen[usize::from(claimed)] = true;
        }
        assert_eq!(
            seen, [true; 2],
            "a panic before and after the claim started"
        );
    }
}

mod home {
    use std::rc::Rc;
    use std::sync::OnceLock;

    use ::home::{Outcome, Refusal, order, writer};
    use types::authority::Authority;
    use types::channel::{Key, Slot};
    use types::frame::key_set::{Group, Interner};
    use types::frame::{Draft, Form, Label, Path as Stream, Range};
    use types::sample::{Scalar, Type};
    use types::time::Stamp;

    use super::*;
    use crate::stop::Stop;
    use crate::{BODY_MAX, Open, handoff};

    /// What the home that `Open::run` gives for shard `shard` of `host` does: the key
    /// of a writer of one index, at `slot`, and the outcomes of a frame of one sample
    /// at each stamp that `stamps` gives for mesh time `now`.
    struct Written {
        key: writer::Key,
        slot: Slot,
        now: Stamp,
        outcomes: Vec<Vec<Outcome>>,
    }

    fn written(
        sim: &mut sim::Sim,
        host: &sim::node::Node,
        shard: u32,
        stamps: fn(Stamp) -> Vec<Stamp>,
    ) -> Written {
        sim.run_on(host, move |host, tasks| async move {
            let (driver, clock) = clock::Clock::new(host.clock());
            let wall = host.wall();
            tasks.spawn(async move { driver.run(wall).await });
            let (give, take) = handoff::pair();
            give.give(Interner::new());
            let (give, next) = handoff::pair();
            let config = block::Config { budget: 1 << 22 };
            let memory = block::Heap::new(config.reservation());
            let pool = block::Pool::new(config, memory);
            let open = Open {
                shard,
                take,
                give,
                monotonic: host.clock(),
                clock: clock.clone(),
                entropy: host.entropy(),
                layout: ::buffer::Layout::new(64 << 20, BODY_MAX).expect("a ring"),
                failed: Arc::new(OnceLock::new()),
                stop: Stop::default(),
            };
            let opened = open.run(host.files(), Rc::new(pool), tasks).await;
            let mut home = opened.expect("the buffer opens");
            let mut interner = next.await.expect("the open gives the interner");
            let (index, values) = (Key::from_u128(1), Key::from_u128(2));
            let slot = interner.slots().assign(index);
            interner.slots().assign(values);
            let set = interner.intern(&[Group {
                index,
                data: &[(values, Type::Scalar(Scalar::I64))],
            }]);
            home.carry(slot);
            let now = mesh_now(&clock, &host.clock()).await;
            let key = home
                .open_writer(writer::Writer {
                    subject: "a".parse().expect("a name"),
                    authority: Authority(1),
                    lease: None,
                    set: Arc::clone(&set),
                })
                .expect("the writer opens");
            let mut outcomes = Vec::new();
            for stamp in stamps(now) {
                let mut frame =
                    Draft::new(home.pool(), &set, Form::Raw, &[(0, 8), (1, 8)])
                        .expect("a frame");
                for (entry, sample) in [(0, stamp.nanos()), (1, 7)] {
                    let series =
                        frame.series_mut(entry).expect("the series is present");
                    series.copy_from_slice(&sample.to_le_bytes());
                }
                frame.set_count(0, 1);
                let written = home.write(key, Label::Path(Stream::Live), frame);
                outcomes.push(written.expect("the write runs").to_vec());
            }
            Written {
                key,
                slot,
                now,
                outcomes,
            }
        })
        .expect("the run ends")
    }

    /// The midpoint of mesh time, once `clock` has one.
    async fn mesh_now(clock: &clock::Reader, monotonic: &env::clock::Clock) -> Stamp {
        loop {
            if let Some(mesh) = clock.now().mesh {
                let (earliest, latest) = (mesh.earliest.nanos(), mesh.latest.nanos());
                return Stamp::from_nanos(earliest.midpoint(latest));
            }
            monotonic.sleep(Span::MILLISECOND).await;
        }
    }

    /// Each shard's home numbers its writers with the shard's core, and accepts
    /// stamps from 2000-01-01 to 10 s past mesh time.
    #[test]
    fn each_shard_builds_its_home_with_its_core_and_the_stamp_limits() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let earliest = Stamp::from_nanos(946_684_800_000_000_000);
        let ahead = Span::from_nanos(10 * Span::SECOND.nanos());
        let at = |now: Stamp| {
            let latest = now.checked_add(Span::from_nanos(10 * Span::SECOND.nanos()));
            let latest = latest.expect("mesh time is far from the end");
            let past = latest.checked_add(Span::NANOSECOND).expect("in range");
            let early = Stamp::from_nanos(946_684_800_000_000_000 - 1);
            vec![early, past, latest]
        };
        let zero = written(&mut sim, &host, 0, at);
        let one = written(&mut sim, &host, 1, at);
        // The first writer of each shard, which differ only by the shard's number.
        assert_ne!(zero.key, one.key);
        let latest = one.now.checked_add(ahead).expect("in range");
        let refused = |error| Outcome::Refused {
            slot: one.slot,
            refusal: Refusal::Order(error),
        };
        let early = Stamp::from_nanos(earliest.nanos() - 1);
        let past = latest.checked_add(Span::NANOSECOND).expect("in range");
        assert_eq!(
            one.outcomes,
            [
                vec![refused(order::Error::Early {
                    stamp: early,
                    earliest,
                })],
                vec![refused(order::Error::Ahead {
                    stamp: past,
                    latest,
                })],
                vec![Outcome::Applied {
                    slot: one.slot,
                    range: Range { seq: 0, count: 1 },
                }],
            ]
        );
    }
}

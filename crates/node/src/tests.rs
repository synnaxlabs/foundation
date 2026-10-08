use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use env::thread;
use sim::shard::Fault;
use types::byte::Size;
use types::ed25519::PrivateKey;
use types::time::Span;

use crate::{Config, Error, Node};

struct Run {
    seed: u64,
    sim: sim::Sim,
    host: sim::node::Node,
    node: Node,
}

/// The port of [`config`].
const PORT: u16 = 7000;
/// The node's private key in [`config`].
const KEY: PrivateKey = PrivateKey([2; 32]);
/// The node's key in [`config`].
const OWN: types::node::Key = types::node::Key::from_u128(1);

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

/// The seams of `host`, with a pool budget of `budget` from `memory`, a disk budget
/// of [`DISK`], keys [`OWN`] and [`KEY`], and the port at [`listen`].
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
        net: host.net(),
        listen: listen(host),
        private_key: KEY,
        key: OWN,
        region: None,
    }
}

/// Port [`PORT`] at the first address of `host`.
fn listen(host: &sim::node::Node) -> SocketAddr {
    SocketAddr::new(host.addresses()[0], PORT)
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
    assert_eq!(part * 3 + 2, usize::try_from(budget.bytes()).unwrap());
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
             budget: Size(4096), entropy: {entropy:?}, disk: {DISK:?}, \
             listen: {listen:?}, key: {OWN:?}, region: None, .. }}",
            listen = config.listen,
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

/// What became of a task given to [`Node::spawn`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    Waiting,
    Ran,
    Dropped,
}

/// Sets its fate to [`Fate::Dropped`] when it drops before its task runs.
struct Probe(Arc<Mutex<Fate>>);

impl Drop for Probe {
    fn drop(&mut self) {
        let mut fate = self.0.lock().unwrap();
        if *fate == Fate::Waiting {
            *fate = Fate::Dropped;
        }
    }
}

/// Gives `node` a task that does nothing, and gives its fate.
fn probe(node: &Node) -> Arc<Mutex<Fate>> {
    let fate = Arc::new(Mutex::new(Fate::Waiting));
    let probe = Probe(Arc::clone(&fate));
    node.spawn(move |_| {
        *probe.0.lock().unwrap() = Fate::Ran;
        async {}
    });
    fate
}

fn fate(fate: &Mutex<Fate>) -> Fate {
    *fate.lock().unwrap()
}

/// A private call: in `sim` a shard panics before its open or after each open, so no
/// run gives this order. On real threads, the mesh clock of shard 0 can panic while a
/// later shard opens.
#[test]
fn join_gives_a_later_shard_error_over_an_earlier_panic() {
    let panicked = thread::Panicked {
        name: "shard-0".to_owned(),
    };
    let shards = Error::Shards {
        stored: 2,
        cores: 3,
    };
    let all = vec![(Err(panicked), None), (Ok(()), Some(shards.clone()))];
    assert_eq!(crate::error(None, all), Err(shards));
}

mod buffer {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ::buffer::{Buffer, Entry};
    use types::channel::{Key, Slots};
    use types::frame::Path as Stream;
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

    /// True when each open of a node has ended, `after` its start, by the fate of
    /// `probe`, which the node got at its start.
    ///
    /// # Panics
    ///
    /// When the node dropped `probe`, or when the opens go on after 10 ms.
    fn all_opened(probe: &Mutex<Fate>, after: Span) -> bool {
        let fate = fate(probe);
        assert_ne!(fate, Fate::Dropped, "{after:?}");
        assert!(after < Span::from_nanos(10_000_000), "the opens go on");
        fate == Fate::Ran
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
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        let assigned = Arc::new(Mutex::new(Vec::new()));
        let out = Arc::clone(&assigned);
        let clock = host.clock();
        node.spawn(move |hub| async move {
            let names = ["c", "b", "a"];
            for (key, name) in [3, 2, 1].into_iter().zip(names) {
                super::hub::define(&hub, key, name, super::hub::STAMP, key);
            }
            let writer = super::hub::writer(&hub, &clock, &names).await;
            let entries = writer.set().entries();
            let slot = |key| entries.iter().find(|e| e.key == Key::from_u128(key));
            let slots = [3, 2, 1].map(|key| slot(key).expect("an entry").slot.get());
            out.lock().unwrap().extend(slots);
        });
        assert_eq!(sim.run_for(Span::HOUR), Ok(()));
        assert_eq!(*assigned.lock().unwrap(), [2, 1, 0]);
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
    }

    /// A shard's part of the budget must hold the block that its buffer's open
    /// takes first. A shard that waits for the interner does not open after it.
    #[test]
    fn a_shard_part_too_small_for_the_buffer_stops_the_node() {
        let budget = Size::from_bytes(512 << 10);
        let mut run = start_with(7, 16, &[], budget, Box::new(heap));
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
        let made = ["lock", "shard-0", "shard-1", "shards-2"];
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

    /// A ring with a checkpoint keeps its size, larger or smaller than its new part.
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
    /// fits and shard 1's does not. Two least rings start. A task given to a node that
    /// started no shard is dropped unrun.
    #[test]
    fn a_disk_budget_that_holds_no_ring_on_each_shard_starts_no_shard() {
        let smallest = ::buffer::Layout::fit(0, crate::BODY_MAX).unwrap_err().min;
        let min = Size::from_bytes(8_437_760);
        let cases = [(smallest, "4120KiB"), (2 * smallest - 1, "8437759B")];
        for (bytes, shown) in cases {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            let disk = Size::from_bytes(bytes);
            let node = Node::start(Config {
                disk,
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            assert_eq!(host.shard_starts(), [], "{shown}");
            assert_eq!(fate(&probe(&node)), Fate::Dropped, "{shown}");
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
                     needs at least 8240KiB"
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
        let all = ["lock", "shard-0", "shard-1", "shard-2", "shards-3"];
        let all = all.map(PathBuf::from);
        let mut seen = [false; 6];
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
            let made: Vec<PathBuf> = if listed.is_empty() {
                Vec::new()
            } else {
                let held = listed.len() - 1;
                all[..held].iter().chain(&all[4..]).cloned().collect()
            };
            assert_eq!(listed, made, "at {after:?}");
            seen[listed.len()] = true;
            // A smaller budget makes a ring with no checkpoint again at its new part,
            // so only a whole ring keeps its size.
            let restart = run_on_disk(&mut sim, &host, 3 * RING);
            assert_eq!(restart, Ok(()), "at {after:?}");
            for core in 0..3 {
                let dir = PathBuf::from(format!("shard-{core}"));
                // `DISK` splits into three whole parts, with no remainder.
                let len = if listed.contains(&dir) {
                    DISK.bytes() / 3
                } else {
                    RING
                };
                let ring = ring_len(&mut sim, &host, core);
                assert_eq!(ring, len, "{dir:?} at {after:?}");
            }
        }
        assert_eq!(seen[2..], [true; 4], "a stop after each step");
    }

    /// A crash at any point of the claim and the first opens leaves a data directory
    /// that the next start opens.
    #[test]
    fn a_crash_during_the_opens_leaves_rings_the_next_start_opens() {
        for crash in [sim::Crash::Process, sim::Crash::Power] {
            for after in (0..).step_by(25_000).map(Span::from_nanos) {
                let mut sim = sim::Sim::new(sim::Config::default());
                let host = host(&mut sim, 2);
                let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
                let probe = probe(&node);
                assert_eq!(sim.run_for(after), Ok(()), "{crash:?} at {after:?}");
                let opened = all_opened(&probe, after);
                sim.crash(&host, crash);
                drop(node);
                assert_eq!(run_on(&mut sim, &host), Ok(()), "{crash:?} at {after:?}");
                if opened {
                    break;
                }
            }
        }
    }

    /// A crash at any point of the first opens, then a restart with another disk
    /// budget: a ring with a checkpoint keeps its size and a ring with none takes its
    /// new part, so the restart opens.
    #[test]
    fn a_crash_during_the_opens_then_another_disk_budget_opens() {
        let mut failed = Vec::new();
        for crash in [sim::Crash::Process, sim::Crash::Power] {
            for after in (0..).step_by(25_000).map(Span::from_nanos) {
                let mut sim = sim::Sim::new(sim::Config::default());
                let host = host(&mut sim, 2);
                let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
                let probe = probe(&node);
                assert_eq!(sim.run_for(after), Ok(()), "{crash:?} at {after:?}");
                let opened = all_opened(&probe, after);
                sim.crash(&host, crash);
                drop(node);
                let lens = run_on_disk(&mut sim, &host, 2 * RING)
                    .map(|()| [0, 1].map(|core| ring_len(&mut sim, &host, core)));
                let fits = |len: &u64| [RING, DISK.bytes() / 2].contains(len);
                if !lens.as_ref().is_ok_and(|lens| lens.iter().all(fits)) {
                    failed.push(format!("{crash:?} at {after:?}: {lens:?}"));
                }
                if opened {
                    assert_eq!(lens, Ok([DISK.bytes() / 2; 2]), "{crash:?}");
                    break;
                }
            }
        }
        assert_eq!(failed, Vec::<String>::new());
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
        let probe = probe(&run.node);
        run.host
            .fail_file(Path::new("shard-1/ring"), env::files::Operation::Open);
        assert_eq!(panics(&mut run), Vec::<String>::new());
        assert_eq!(fate(&probe), Fate::Dropped);
        assert_eq!(run.node.join(), Err(opened(1)));
        let made = ["lock", "shard-0", "shards-3"].map(PathBuf::from);
        assert_eq!(listed(&mut run.sim, &run.host, ""), made);
    }

    #[test]
    fn a_shard_with_no_memory_drops_the_tasks_of_the_node() {
        let refused = os::memory::Error::Refused;
        let mut run = start_with(7, 3, &[], Size::MEBIBYTE, refuse(1, refused));
        let probe = probe(&run.node);
        assert_eq!(run.sim.run(), Ok(()));
        assert_eq!(fate(&probe), Fate::Dropped);
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
    use std::cell::RefCell;
    use std::rc::Rc;

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
            let made = ["lock", "shards-3"].map(PathBuf::from);
            assert_eq!(listed, made, "{cores} cores");
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
            "lock",
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
        use env::files::Operation::{CreateDir, List, Open, SyncDir};
        for (path, operation, text, made) in [
            ("lock", Open, "open of lock failed with OS error 5", &[][..]),
            ("", List, "list of  failed with OS error 5", &["lock"][..]),
            (
                "shards-2",
                CreateDir,
                "create_dir of shards-2 failed with OS error 5",
                &["lock"][..],
            ),
            (
                "",
                SyncDir,
                "sync_dir of  failed with OS error 5",
                &["lock", "shards-2"][..],
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
                format!("cannot claim the data directory: {text}")
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
        let made = ["lock", "shard-0", "shard-1", &name, "shards-2"];
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
            let mut made = ["lock", "shard-0", "shard-1", &name, "shards-2"];
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

    /// A crash at the sync of the record: a power cut keeps a prefix of the creates
    /// of the lock and the record, and a process crash keeps both. The next start
    /// syncs the record before `shard-0`.
    #[test]
    fn a_crash_at_the_sync_of_the_record_leaves_it_whole_or_absent() {
        use env::files::Operation::SyncDir;
        let io = env::files::Error::Io {
            path: PathBuf::new(),
            operation: SyncDir,
            code: 5,
        };
        let made = ["lock", "shards-2"].map(PathBuf::from);
        let prefixes = BTreeSet::from([vec![], made[..1].to_vec(), made.to_vec()]);
        for (crash, kept) in [
            (sim::Crash::Power, prefixes),
            (sim::Crash::Process, BTreeSet::from([made.to_vec()])),
        ] {
            let mut listings = BTreeSet::new();
            for seed in 0..32 {
                let mut sim = sim::Sim::new(sim::Config {
                    seed,
                    ..sim::Config::default()
                });
                let host = host(&mut sim, 2);
                host.fail_file(Path::new(""), SyncDir);
                let e = run_on(&mut sim, &host);
                assert_eq!(e, Err(Error::Directory(io.clone())), "{crash:?}");
                sim.crash(&host, crash);
                listings.insert(listed(&mut sim, &host, ""));
                host.fail_file(Path::new(""), SyncDir);
                let e = run_on(&mut sim, &host);
                assert_eq!(e, Err(Error::Directory(io.clone())), "{crash:?}");
                assert_eq!(listed(&mut sim, &host, ""), made, "{crash:?}");
            }
            assert_eq!(listings, kept, "{crash:?}");
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

    /// On the real OS, shard 0 runs while the later shards get their memory, so its
    /// claim can fail before a shard gets no memory. `join` gives the shard with no
    /// memory.
    #[test]
    fn join_gives_a_shard_with_no_memory_over_a_claim_that_failed_first() {
        let error = os::memory::Error::Refused;
        for seed in 0..32 {
            let sim = Rc::new(RefCell::new(sim::Sim::new(sim::Config {
                seed,
                ..sim::Config::default()
            })));
            let host = host(&mut sim.borrow_mut(), 3);
            record(&mut sim.borrow_mut(), &host, 2);
            let (mut core, run) = (0, Rc::clone(&sim));
            let memory: Memory = Box::new(move |len| {
                core += 1;
                if core < 3 {
                    return heap(len);
                }
                let claim = Span::from_nanos(5_000_000);
                assert_eq!(run.borrow_mut().run_for(claim), Ok(()));
                Err(error)
            });
            let node = Node::start(config(&host, Size::MEBIBYTE, memory));
            assert_eq!(sim.borrow_mut().run(), Ok(()), "seed {seed}");
            // The claim took the lock, then refused the count: no `shards-3`.
            let claimed = listed(&mut sim.borrow_mut(), &host, "");
            let made = ["lock", "shards-2"].map(PathBuf::from);
            assert_eq!(claimed, made, "seed {seed}");
            let memory = Error::Memory { core: 2, error };
            assert_eq!(node.join(), Err(memory), "seed {seed}");
        }
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

    /// An `Open` of shard `shard` of `host`, given the first interner, with a new pool,
    /// the end that takes the interner the open gives, and the node's clocks, which run
    /// on `tasks`.
    pub(super) fn create_open(
        host: &sim::node::Node,
        tasks: &env::tasks::Tasks,
        shard: u32,
        stop: Stop,
    ) -> (Open, block::Pool, handoff::Take<Interner>, clock::Reader) {
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
            stop,
        };
        (open, pool, next, clock)
    }

    fn written(
        sim: &mut sim::Sim,
        host: &sim::node::Node,
        shard: u32,
        stamps: fn(Stamp) -> Vec<Stamp>,
    ) -> Written {
        sim.run_on(host, move |host, tasks| async move {
            let (open, pool, next, clock) =
                create_open(&host, &tasks, shard, Stop::default());
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

mod lock {
    use super::*;

    /// The error of a start on a data directory that another node holds.
    fn busy() -> Error {
        Error::Directory(env::files::Error::Busy {
            path: PathBuf::from("lock"),
        })
    }

    /// Starts a node on the cores and the data directory of `host`, with its port at
    /// [`PORT`] plus `n`, so two nodes on one host do not share a port.
    fn start_on(host: &sim::node::Node, n: u16) -> Node {
        Node::start(Config {
            listen: SocketAddr::new(host.addresses()[0], PORT + n),
            ..config(host, Size::MEBIBYTE, Box::new(heap))
        })
    }

    #[test]
    fn a_second_node_on_the_data_directory_of_a_running_node_is_refused() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let first = start_on(&host, 0);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        let second = start_on(&host, 1);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        let e = second.join();
        assert_eq!(e, Err(busy()));
        assert_eq!(
            busy().to_string(),
            "cannot claim the data directory: file lock is open for writing in \
             another handle"
        );
        first.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(first.join(), Ok(()));
        let made = ["lock", "shard-0", "shard-1", "shards-2"].map(PathBuf::from);
        assert_eq!(listed(&mut sim, &host, ""), made);
    }

    /// Two nodes that start at once on a new data directory: one claims it, and the
    /// other stops before it reads a name or opens a ring.
    #[test]
    fn of_two_nodes_that_start_at_once_one_claims_the_data_directory() {
        let mut seen = [false; 2];
        for seed in 0..16 {
            let mut sim = sim::Sim::new(sim::Config {
                seed,
                ..sim::Config::default()
            });
            let host = host(&mut sim, 2);
            let nodes = [start_on(&host, 0), start_on(&host, 1)];
            assert_eq!(sim.run_for(Span::SECOND), Ok(()), "seed {seed}");
            for node in &nodes {
                node.stop();
            }
            assert_eq!(sim.run(), Ok(()), "seed {seed}");
            let joined = nodes.map(Node::join);
            let claimed = usize::from(joined[0].is_err());
            assert_eq!(joined[claimed], Ok(()), "seed {seed}");
            assert_eq!(joined[1 - claimed], Err(busy()), "seed {seed}");
            seen[claimed] = true;
            let made = ["lock", "shard-0", "shard-1", "shards-2"].map(PathBuf::from);
            assert_eq!(listed(&mut sim, &host, ""), made, "seed {seed}");
        }
        assert_eq!(seen, [true; 2]);
    }

    /// A stop at any point of the claim and the opens, then a second node at once:
    /// a node holds the lock until each of its rings has closed, so the other never
    /// finds a ring open.
    #[test]
    fn the_lock_outlives_each_ring() {
        let mut seen = [false; 3];
        // The opens end at about 1.3 ms; a stop at 760 us finds `shard-1` opening.
        for step in 0..400 {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            let first = start_on(&host, 0);
            let after = Span::from_nanos(step * 5_000);
            assert_eq!(sim.run_for(after), Ok(()), "at {after:?}");
            first.stop();
            let second = start_on(&host, 1);
            assert_eq!(sim.run_for(Span::SECOND), Ok(()), "at {after:?}");
            second.stop();
            assert_eq!(sim.run(), Ok(()), "at {after:?}");
            let joined = [first.join(), second.join()];
            let outcome = [
                [Ok(()), Ok(())],
                [Ok(()), Err(busy())],
                [Err(busy()), Ok(())],
            ]
            .iter()
            .position(|o| *o == joined);
            let Some(outcome) = outcome else {
                panic!("at {after:?}: {joined:?}");
            };
            seen[outcome] = true;
        }
        assert_eq!(seen, [true; 3]);
    }

    /// A stop at any point of the opens of three shards, then a probe that takes the
    /// lock as soon as it is free: each ring has closed by then.
    #[test]
    fn the_lock_outlives_the_ring_of_each_shard() {
        let mut held = false;
        for step in 0..500 {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 3);
            let node = start_on(&host, 0);
            let after = Span::from_nanos(step * 5_000);
            assert_eq!(sim.run_for(after), Ok(()), "at {after:?}");
            node.stop();
            let (waited, rings) = sim
                .run_on(&host, |host, _| async move {
                    let files = host.files();
                    let clock = host.clock();
                    let mut waited = false;
                    let lock = loop {
                        let mode = env::files::Mode::Create { len: 0 };
                        match files.open(Path::new("lock"), mode).await {
                            Err(env::files::Error::Busy { .. }) => {
                                waited = true;
                                clock.sleep(Span::from_nanos(1_000)).await;
                            }
                            opened => break opened.expect("the lock opens"),
                        }
                    };
                    let mut rings = Vec::new();
                    for core in 0..3 {
                        let ring = crate::directory::shard(core).join("ring");
                        let opened = files.open(&ring, env::files::Mode::Write).await;
                        rings.push(opened.map(drop));
                    }
                    drop(lock);
                    (waited, rings)
                })
                .expect("the probe ends");
            held |= waited;
            for ring in rings {
                let busy = matches!(ring, Err(env::files::Error::Busy { .. }));
                assert!(!busy, "at {after:?}: {ring:?}");
            }
            // A probe that takes the lock before the claim refuses the node.
            let joined = node.join();
            assert!(joined == Ok(()) || joined == Err(busy()), "at {after:?}");
        }
        assert!(held);
    }

    /// A process crash frees the lock, so the next start claims the data directory.
    #[test]
    fn a_restart_after_a_process_crash_claims_the_data_directory() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let node = start_on(&host, 0);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        sim.crash(&host, sim::Crash::Process);
        drop(node);
        assert_eq!(run_on(&mut sim, &host), Ok(()));
    }
}

mod hub {
    use ::hub::reader::{Mode, Received};
    use ::hub::writer::{self, Writer};
    use ::hub::{Channel, Hub};
    use types::authority::Authority;
    use types::channel::Key;
    use types::frame::key_set::KeySet;
    use types::frame::{Form, Label, Path as Stream};
    use types::name::Name;
    use types::sample::{Scalar, Type};

    use super::*;

    pub(super) const STAMP: Type = Type::Scalar(Scalar::Stamp);
    const I64: Type = Type::Scalar(Scalar::I64);
    /// The wall time when a host is added, which is before the node's mesh time.
    const WALL: i64 = 1_767_225_600_000_000_000;

    fn name(name: &str) -> Name {
        name.parse().expect("a valid name")
    }

    /// Defines channel `key`, named `name`, of `data_type` on index `index`.
    pub(super) fn define(
        hub: &Hub,
        key: u128,
        name: &str,
        data_type: Type,
        index: u128,
    ) {
        hub.define(Channel {
            key: Key::from_u128(key),
            name: self::name(name),
            data_type,
            index: Key::from_u128(index),
        });
    }

    /// A writer on `channels`, opened again each millisecond of `clock` until the node
    /// has mesh time.
    pub(super) async fn writer(
        hub: &Hub,
        clock: &env::clock::Clock,
        channels: &[&str],
    ) -> Writer {
        let config = writer::Config {
            subject: name("a"),
            authority: Authority(1),
            lease: None,
            channels: channels.iter().map(|n| name(n)).collect(),
        };
        loop {
            match hub.writer(config.clone()).await {
                Err(writer::Error::Home(::hub::home::writer::Error::Unsynced)) => {
                    clock.sleep(Span::MILLISECOND).await;
                }
                opened => return opened.expect("the writer opens"),
            }
        }
    }

    /// The position of channel `key` in `set`.
    fn entry(set: &KeySet, key: u128) -> usize {
        let key = Key::from_u128(key);
        let entries = set.entries();
        entries.iter().position(|e| e.key == key).expect("an entry")
    }

    /// Writes one sample at `stamp` to `time` and `value` to `value`.
    fn write(writer: &mut Writer, stamp: i64, value: i64) {
        let set = writer.set();
        let (time, data) = (entry(set, 1), entry(set, 2));
        let group = set.entries()[time].group;
        let mut series = [(time, 8), (data, 8)];
        series.sort_unstable();
        let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
        for (entry, sample) in [(time, stamp), (data, value)] {
            let bytes = draft.series_mut(entry).expect("the series is present");
            bytes.copy_from_slice(&sample.to_le_bytes());
        }
        draft.set_count(group, 1);
        let outcomes = writer.write(Label::Path(Stream::Live), draft);
        assert_eq!(outcomes.map(<[_]>::len), Ok(1));
    }

    /// The samples of channel `key` in `received`.
    fn samples(received: &Received<'_>, key: u128) -> Vec<i64> {
        let entry = entry(received.set, key);
        let entries = received.set.entries();
        let range = received.view.range(entries[entry].group).expect("a range");
        let count = usize::try_from(range.count).expect("a count");
        let (_, bytes) = received
            .view
            .iter()
            .find(|&(present, _)| present == entry)
            .expect("the view holds the series");
        let mut out = vec![0; count * 8];
        codec::decode(entries[entry].data_type, count, bytes, &mut out)
            .expect("decodes");
        let (chunks, _) = out.as_chunks::<8>();
        chunks.iter().map(|c| i64::from_le_bytes(*c)).collect()
    }

    /// A node of `cores` shards on a new host, with the host.
    fn node(sim: &mut sim::Sim, cores: usize) -> (sim::node::Node, Node) {
        let host = host(sim, cores);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        (host, node)
    }

    /// A task writes a sample through the hub of shard 0, at the node's mesh time,
    /// and reads it back. The task does not run before the node does.
    #[test]
    fn a_task_writes_and_reads_through_the_hub_of_shard_0() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let (host, node) = node(&mut sim, 2);
        let read = Arc::new(Mutex::new(None));
        let out = Arc::clone(&read);
        let clock = host.clock();
        let probe = probe(&node);
        node.spawn(move |hub| async move {
            define(&hub, 1, "time", STAMP, 1);
            define(&hub, 2, "value", I64, 1);
            let reader = hub.reader(&[name("value")], Mode::Complete).await;
            let mut reader = reader.expect("the reader opens");
            let mut writer = writer(&hub, &clock, &["value"]).await;
            let stamp = WALL;
            write(&mut writer, stamp, 7);
            let received = reader.next().await.expect("a frame");
            let samples = (samples(&received, 1), samples(&received, 2));
            *out.lock().unwrap() = Some((stamp, samples));
        });
        assert_eq!(fate(&probe), Fate::Waiting, "a spawn does not wait");
        assert_eq!(sim.run_for(Span::HOUR), Ok(()));
        let read = read.lock().unwrap().take();
        let (stamp, samples) = read.expect("the task read the sample");
        assert_eq!(samples, (vec![stamp], vec![7]));
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
    }

    /// Tasks are called in the order of their calls, given before or after the opens.
    #[test]
    fn tasks_are_called_in_the_order_of_their_calls() {
        for seed in 0..16 {
            let mut sim = sim::Sim::new(sim::Config {
                seed,
                ..sim::Config::default()
            });
            let (_host, node) = node(&mut sim, 3);
            let started = Arc::new(Mutex::new(Vec::new()));
            let spawn = |n: usize| {
                let started = Arc::clone(&started);
                node.spawn(move |_| {
                    started.lock().unwrap().push(n);
                    async {}
                });
            };
            (0..8).for_each(spawn);
            assert_eq!(sim.run_for(Span::HOUR), Ok(()), "seed {seed}");
            (8..16).for_each(spawn);
            assert_eq!(sim.run_for(Span::HOUR), Ok(()), "seed {seed}");
            let all: Vec<_> = (0..16).collect();
            assert_eq!(*started.lock().unwrap(), all, "seed {seed}");
            node.stop();
            assert_eq!(sim.run(), Ok(()), "seed {seed}");
            assert_eq!(node.join(), Ok(()), "seed {seed}");
        }
    }

    /// A task given before or after a stop, before it starts, is dropped unrun.
    #[test]
    fn a_task_given_around_a_stop_is_dropped_unrun() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let (_host, node) = node(&mut sim, 2);
        let before = probe(&node);
        node.stop();
        let after = probe(&node);
        assert_eq!(sim.run(), Ok(()));
        let late = probe(&node);
        assert_eq!(
            [&before, &after, &late].map(|p| fate(p)),
            [Fate::Dropped; 3]
        );
        assert_eq!(node.join(), Ok(()));
    }

    /// A task given once the hub runs, and still in the inbox at a stop, is dropped
    /// unrun.
    #[test]
    fn a_task_given_with_a_stop_after_the_opens_is_dropped_unrun() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let (_host, node) = node(&mut sim, 2);
        let ran = probe(&node);
        assert_eq!(sim.run_for(Span::HOUR), Ok(()));
        assert_eq!(fate(&ran), Fate::Ran);
        let given = probe(&node);
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(fate(&given), Fate::Dropped);
        assert_eq!(node.join(), Ok(()));
    }

    /// Running futures each keep what they hold until the stop drops them all.
    #[test]
    fn running_futures_each_hold_until_the_stop() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let (_host, node) = node(&mut sim, 2);
        let dropped: Vec<_> = (0..3).map(|_| Arc::new(Mutex::new(false))).collect();
        for flag in &dropped {
            let held = Dropped(Arc::clone(flag));
            node.spawn(move |_| async move {
                let _held = held;
                std::future::pending::<()>().await;
            });
        }
        assert_eq!(sim.run_for(Span::HOUR), Ok(()));
        let fates = || {
            dropped
                .iter()
                .map(|d| *d.lock().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(fates(), [false; 3]);
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(fates(), [true; 3]);
        assert_eq!(node.join(), Ok(()));
    }

    /// A task that completes drops what it holds, before the node stops.
    #[test]
    fn a_task_that_completes_drops_what_it_holds() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let (_host, node) = node(&mut sim, 2);
        let dropped = Arc::new(Mutex::new(false));
        let held = Dropped(Arc::clone(&dropped));
        node.spawn(move |_| {
            std::future::poll_fn(move |_| {
                let _held = &held;
                std::task::Poll::Ready(())
            })
        });
        assert_eq!(sim.run_for(Span::HOUR), Ok(()));
        assert!(*dropped.lock().unwrap(), "the task dropped what it held");
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
    }

    /// Records its drop.
    struct Dropped(Arc<Mutex<bool>>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = true;
        }
    }

    /// A stop drops a task that holds a reader and waits for a frame, so the ring
    /// closes, the node ends, and the next start opens.
    #[test]
    fn a_stop_drops_a_running_task_and_its_sessions() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let (host, node) = node(&mut sim, 2);
        let dropped = Arc::new(Mutex::new(false));
        let task = Dropped(Arc::clone(&dropped));
        node.spawn(move |hub| async move {
            let _task = task;
            define(&hub, 1, "time", STAMP, 1);
            let reader = hub.reader(&[name("time")], Mode::Complete).await;
            let mut reader = reader.expect("the reader opens");
            drop(reader.next().await);
            unreachable!("no frame comes");
        });
        assert_eq!(sim.run_for(Span::HOUR), Ok(()));
        assert!(!*dropped.lock().unwrap(), "the task runs");
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert!(*dropped.lock().unwrap(), "the stop dropped the task");
        assert_eq!(node.join(), Ok(()));
        assert_eq!(run_on(&mut sim, &host), Ok(()));
    }

    /// `keep` returns only once the ring under a hub with a writer has closed, so a
    /// write open of it right after gives no `Busy`. This pins the order in `keep`;
    /// `hub` tests its own drop of a commit.
    #[test]
    fn keep_returns_once_the_ring_under_a_hub_has_closed() {
        use crate::directory;
        use crate::stop::Stop;

        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 1);
        let opened = sim.run_on(&host, move |host, tasks| async move {
            let stop = Stop::default();
            let (open, pool, next, _) =
                super::home::create_open(&host, &tasks, 0, stop.clone());
            let monotonic = host.clock();
            let spawn = tasks.clone();
            let hold = async move |home, guard| {
                let interner = next.await.expect("the open gives the interner");
                let hub = Hub::new(::hub::Config {
                    home,
                    interner,
                    tasks: spawn,
                });
                define(&hub, 1, "time", STAMP, 1);
                define(&hub, 2, "value", I64, 1);
                let writer = writer(&hub, &monotonic, &["value"]).await;
                drop(guard);
                drop((writer, hub));
            };
            let files = host.files();
            open.keep(
                host.files(),
                std::rc::Rc::new(pool),
                tasks,
                stop.guard(),
                hold,
            )
            .await;
            let ring = directory::shard(0).join("ring");
            files.open(&ring, env::files::Mode::Write).await.map(drop)
        });
        assert_eq!(opened, Ok(Ok(())));
    }

    /// A panic in a task ends shard 0 and fails the node.
    #[test]
    fn a_panic_in_a_task_fails_the_node() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let (_host, node) = node(&mut sim, 2);
        node.spawn(|_| async { panic!("a task panics") });
        let panicked = sim::Error::Panicked {
            thread: "shard-0".into(),
            message: "a task panics".into(),
            seed: 0,
        };
        assert_eq!(sim.run(), Err(panicked));
        assert_eq!(sim.run(), Ok(()));
        let shard = thread::Panicked {
            name: "shard-0".into(),
        };
        assert_eq!(node.join(), Err(Error::Panicked(shard)));
    }

    /// A panic in a task's closure body fails the node, as one in its future does.
    #[test]
    fn a_panic_in_a_task_body_fails_the_node() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let (_host, node) = node(&mut sim, 2);
        node.spawn(|_| -> std::future::Ready<()> { panic!("a task body panics") });
        let panicked = sim::Error::Panicked {
            thread: "shard-0".into(),
            message: "a task body panics".into(),
            seed: 0,
        };
        assert_eq!(sim.run(), Err(panicked));
        assert_eq!(sim.run(), Ok(()));
        let shard = thread::Panicked {
            name: "shard-0".into(),
        };
        assert_eq!(node.join(), Err(Error::Panicked(shard)));
    }

    /// A task given after a task whose body stops the node is dropped uncalled.
    #[test]
    fn a_task_given_after_a_body_that_stops_the_node_is_dropped_uncalled() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let (_host, node) = node(&mut sim, 2);
        let node = Arc::new(Mutex::new(node));
        let stopper = Arc::clone(&node);
        node.lock().unwrap().spawn(move |_| {
            stopper.lock().unwrap().stop();
            async {}
        });
        let after = probe(&node.lock().unwrap());
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(fate(&after), Fate::Dropped);
        let node = Arc::try_unwrap(node).expect("one owner");
        assert_eq!(node.into_inner().unwrap().join(), Ok(()));
    }

    /// A task that a body gives after it stops the node is dropped uncalled.
    #[test]
    fn a_task_given_by_a_body_after_it_stops_the_node_is_dropped_uncalled() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let (_host, node) = node(&mut sim, 2);
        let node = Arc::new(Mutex::new(node));
        let stopper = Arc::clone(&node);
        let after = Arc::new(Mutex::new(None));
        let out = Arc::clone(&after);
        node.lock().unwrap().spawn(move |_| {
            let node = stopper.lock().unwrap();
            node.stop();
            *out.lock().unwrap() = Some(probe(&node));
            async {}
        });
        assert_eq!(sim.run(), Ok(()));
        let after = after.lock().unwrap().take().expect("the body ran");
        assert_eq!(fate(&after), Fate::Dropped);
        let node = Arc::try_unwrap(node).expect("one owner");
        assert_eq!(node.into_inner().unwrap().join(), Ok(()));
    }

    /// `Node`'s derive is the one caller of `Queue`'s `Debug`.
    #[test]
    fn a_node_shows_its_queue() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let (_host, node) = node(&mut sim, 1);
        node.spawn(|_| async {});
        assert!(format!("{node:?}").contains("queue: Queue"), "{node:?}");
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
    }
}

mod port {
    use std::num::NonZeroU32;
    use std::rc::Rc;

    use transport::{Address, Class, Code, Peer, Transport};

    use super::*;

    /// The key of the peer that dials the node.
    const CLIENT: PrivateKey = PrivateKey([1; 32]);

    /// What the peer saw of the node.
    #[derive(Debug, PartialEq)]
    struct Seen {
        /// The session's peer.
        peer: Peer,
        /// The largest message the node takes on the stream.
        bytes_max: usize,
        /// The error of the stream's first send that failed.
        sent: transport::Error,
        /// The read of the stream's reply half.
        read: Result<Option<Vec<u8>>, transport::Error>,
    }

    /// A transport on `host` with `key`, at a free port, with its pool.
    fn transport(
        host: &sim::node::Node,
        tasks: env::tasks::Tasks,
        key: PrivateKey,
    ) -> (Transport, Rc<block::Pool>) {
        let pool = block::Config { budget: 1 << 20 };
        let memory = block::Heap::new(pool.reservation());
        let pool = Rc::new(block::Pool::new(pool, memory));
        let config = transport::Config {
            private_key: key,
            message_bytes_max: NonZeroUsize::new(1 << 16).unwrap(),
            window_bytes: 1 << 20,
            streams_max: NonZeroU32::new(16).unwrap(),
            idle: Span::from_nanos(10 * Span::SECOND.nanos()),
            clock: host.clock(),
            entropy: host.entropy(),
            tasks,
            pool: Rc::clone(&pool),
        };
        let at = SocketAddr::new(host.addresses()[0], 0);
        let bound = transport::Port::bind(&host.net(), at).expect("a port");
        let part = bound.split(NonZeroUsize::MIN).pop().expect("one part");
        (Transport::new(config, part).expect("a transport"), pool)
    }

    /// Starts a peer on a new host of `sim` that dials the node at `listen` of
    /// `host`, opens a two-way stream, sends `header` as its first message, then
    /// sends until a send fails, and reads the reply half. Gives what the peer saw
    /// once the run reaches it, or the error of the dial. The peer holds its session
    /// until the session closes.
    fn dial(
        sim: &mut sim::Sim,
        host: &sim::node::Node,
        header: &[u8],
    ) -> Arc<Mutex<Option<Result<Seen, transport::Error>>>> {
        let peer = sim.node(sim::node::Config::default());
        let listen = listen(host);
        let header = header.to_vec();
        let out = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&out);
        let shard = env::shards::Config {
            name: "peer".into(),
            core: None,
        };
        let own = peer.clone();
        let started = peer.shards().start(shard, move |tasks| async move {
            let (transport, pool) = transport(&own, tasks, CLIENT);
            let dialed = transport.dial(KEY.public(), &[Address::Udp(listen)]).await;
            let session = match dialed {
                Ok(session) => session,
                Err(error) => {
                    *seen.lock().unwrap() = Some(Err(error));
                    return;
                }
            };
            let (mut sender, mut receiver) =
                session.open(Class::Complete).await.expect("a stream");
            let message = |bytes: &[u8]| {
                let mut block = pool.alloc(bytes.len()).unwrap();
                block.copy_from_slice(bytes);
                block.freeze()
            };
            let bytes_max = sender.bytes_max();
            let mut sent = sender.send(message(&header)).await;
            while sent.is_ok() {
                own.clock().sleep(Span::MILLISECOND).await;
                sent = sender.send(message(b"after")).await;
            }
            let read = receiver.recv().await.map(|m| m.map(|b| b.to_vec()));
            let sent = sent.unwrap_err();
            let peer = session.peer();
            *seen.lock().unwrap() = Some(Ok(Seen {
                peer,
                bytes_max,
                sent,
                read,
            }));
            session.closed().await;
        });
        drop(started.expect("the peer starts"));
        out
    }

    /// What a peer sees when it sends `header` to a running node with `memory`, which
    /// then stops cleanly.
    fn rejected(header: &[u8], memory: Size) -> Seen {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let node = Node::start(config(&host, memory, Box::new(heap)));
        let seen = dial(&mut sim, &host, header);
        assert_eq!(sim.run_for(Span::HOUR), Ok(()));
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
        let seen = seen.lock().unwrap().take();
        seen.expect("the peer ran")
            .expect("the dial reaches the node")
    }

    /// The node stops a stream it serves no protocol for, and resets its reply half,
    /// with the code of a rejected header. The node proves its key.
    #[test]
    fn a_stream_of_a_known_protocol_is_rejected() {
        let key = KEY.public();
        let header = wire::header::encode(wire::Protocol::Mesh);
        let code = Code(wire::header::REJECTED);
        let Seen {
            peer,
            bytes_max,
            sent,
            read,
        } = rejected(&header, Size::MEBIBYTE);
        assert_eq!(peer, Peer::Node(key));
        assert_eq!(bytes_max, 65_536);
        assert_eq!(sent, transport::Error::Stopped { code });
        assert_eq!(read, Err(transport::Error::Reset { code }));
    }

    /// A header with an unknown protocol number gets the same stop and reset.
    #[test]
    fn a_stream_of_an_unknown_protocol_is_rejected() {
        let mut header = wire::header::encode(wire::Protocol::Mesh);
        header[2] = 9;
        assert_eq!(
            wire::header::decode(&header),
            Err(wire::header::Error::Protocol { number: 9 })
        );
        let code = Code(wire::header::REJECTED);
        let Seen { sent, read, .. } = rejected(&header, Size::MEBIBYTE);
        assert_eq!(sent, transport::Error::Stopped { code });
        assert_eq!(read, Err(transport::Error::Reset { code }));
    }

    /// A node whose pool's largest block is below 64 KiB takes messages of that block.
    #[test]
    fn a_node_whose_largest_block_is_below_64_kib_serves_its_port() {
        let header = wire::header::encode(wire::Protocol::Mesh);
        let seen = rejected(&header, Size::from_bytes(128 << 10));
        assert_eq!(seen.bytes_max, 57_344);
        let code = Code(wire::header::REJECTED);
        assert_eq!(seen.sent, transport::Error::Stopped { code });
    }

    /// A session that stays open delays no stream of another session.
    #[test]
    fn a_held_session_delays_no_other_session() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        let header = wire::header::encode(wire::Protocol::Mesh);
        let first = dial(&mut sim, &host, &header);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        let second = dial(&mut sim, &host, &header);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        let sent = |seen: &Arc<Mutex<Option<Result<Seen, _>>>>| {
            let seen = seen.lock().unwrap().take();
            seen.map(|seen| seen.map(|seen| seen.sent))
        };
        let code = Code(wire::header::REJECTED);
        let stopped = Some(Ok(transport::Error::Stopped { code }));
        assert_eq!(sent(&first), stopped);
        assert_eq!(sent(&second), stopped);
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
    }

    /// `join` gives a disk budget that holds no ring before a port in use.
    #[test]
    fn join_gives_a_small_disk_over_a_port_in_use() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let udp = env::net::udp::Config {
            local: listen(&host),
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 16,
        };
        let held = host.net().udp(&udp).expect("the port binds");
        let disk = Size::from_bytes(1);
        let node = Node::start(Config {
            disk,
            ..config(&host, Size::MEBIBYTE, Box::new(heap))
        });
        assert_eq!(sim.run(), Ok(()));
        let min = Size::from_bytes(8_437_760);
        assert_eq!(
            node.join(),
            Err(Error::Disk {
                disk,
                cores: 2,
                min
            })
        );
        drop(held);
    }

    /// A port that does not bind starts no shard, and `join` gives why.
    #[test]
    fn a_port_that_does_not_bind_starts_no_shard() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let listen = listen(&host);
        let udp = env::net::udp::Config {
            local: listen,
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 16,
        };
        let held = host.net().udp(&udp).expect("the port binds");
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        assert_eq!(sim.run(), Ok(()));
        let error = Error::Port {
            listen,
            error: env::net::Error::AddressInUse { local: listen },
        };
        assert_eq!(node.join(), Err(error.clone()));
        assert_eq!(host.shard_starts().len(), 0);
        assert_eq!(
            error.to_string(),
            format!(
                "cannot bind the node's port at {listen}: address {listen} is in use"
            )
        );
        drop(held);
    }

    /// The node takes no session when a buffer does not open.
    #[test]
    fn a_node_whose_buffer_does_not_open_takes_no_session() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        host.fail_file(Path::new("shard-1/ring"), env::files::Operation::Open);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        let seen = dial(&mut sim, &host, &wire::header::encode(wire::Protocol::Mesh));
        assert_eq!(sim.run(), Ok(()));
        let seen = seen.lock().unwrap().take().expect("the peer ran");
        let unreachable = transport::Error::Unreachable {
            peer: KEY.public(),
            attempts: vec![(Address::Udp(listen(&host)), transport::Error::TimedOut)],
        };
        assert_eq!(seen, Err(unreachable));
        let error = ::buffer::Error::Files(env::files::Error::Io {
            path: PathBuf::from("shard-1/ring"),
            operation: env::files::Operation::Open,
            code: 5,
        });
        assert_eq!(node.join(), Err(Error::Buffer { core: 1, error }));
    }

    /// A transport that stops stops the node, and `join` gives why.
    #[test]
    fn a_transport_that_stops_stops_the_node() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        host.fail_udp(listen(&host));
        assert_eq!(sim.run(), Ok(()));
        let error = transport::Error::Network {
            error: env::net::Error::Io { code: 5 },
        };
        assert_eq!(node.join(), Err(Error::Transport(error.clone())));
        assert_eq!(
            Error::Transport(error).to_string(),
            "the node's transport stopped: the socket broke: network call failed \
             with OS error 5"
        );
    }

    /// A stop of the node at the instant its transport stops is not a failure.
    #[test]
    fn a_stop_as_the_transport_stops_gives_no_error() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        host.fail_udp(listen(&host));
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
    }

    /// A task whose drop panics as the transport stops: `join` ranks the transport's
    /// error above the panic.
    #[test]
    fn a_panic_as_the_transport_stops_gives_the_transport_error() {
        struct Panics;
        impl Drop for Panics {
            fn drop(&mut self) {
                panic!("a task's drop panics");
            }
        }
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        node.spawn(|_| {
            let panics = Panics;
            async move {
                std::future::pending::<()>().await;
                drop(panics);
            }
        });
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        host.fail_udp(listen(&host));
        assert_eq!(
            sim.run(),
            Err(sim::Error::Panicked {
                thread: "shard-0".into(),
                message: "a task's drop panics".into(),
                seed: 0,
            })
        );
        assert_eq!(sim.run(), Ok(()));
        let error = transport::Error::Network {
            error: env::net::Error::Io { code: 5 },
        };
        assert_eq!(node.join(), Err(Error::Transport(error)));
    }

    /// A stream whose header is late delays no other stream of the same session. On a
    /// slow link, stream A sends a first message of 60 KiB and stream B a header
    /// only: B's reply half resets before A's.
    #[test]
    fn a_late_header_delays_no_other_stream() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        let peer = sim.node(sim::node::Config::default());
        let slow = sim::link::Config {
            rate: Some(std::num::NonZeroU64::new(20_000).unwrap()),
            ..sim::link::Config::default()
        };
        sim.link(&peer, &host, slow);
        let listen = listen(&host);
        let order = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&order);
        let shard = env::shards::Config {
            name: "peer".into(),
            core: None,
        };
        let own = peer.clone();
        let started = peer.shards().start(shard, move |tasks| async move {
            let (transport, pool) = transport(&own, tasks.clone(), CLIENT);
            let session = transport
                .dial(KEY.public(), &[Address::Udp(listen)])
                .await
                .expect("a session");
            let message = |bytes: &[u8]| {
                let mut block = pool.alloc(bytes.len()).unwrap();
                block.copy_from_slice(bytes);
                block.freeze()
            };
            let header = wire::header::encode(wire::Protocol::Mesh);
            let mut late = header.to_vec();
            late.resize(60 << 10, 0);
            let (mut a, a_reply) = session.open(Class::Complete).await.expect("a");
            a.send(message(&late)).await.expect("a sends");
            let (mut b, b_reply) = session.open(Class::Complete).await.expect("b");
            b.send(message(&header)).await.expect("b sends");
            for (name, mut reply) in [("a", a_reply), ("b", b_reply)] {
                let seen = Arc::clone(&seen);
                tasks.spawn(async move {
                    let read = reply.recv().await.map(|m| m.map(|b| b.to_vec()));
                    seen.lock().unwrap().push((name, read));
                });
            }
            session.closed().await;
            drop((a, b, transport));
        });
        drop(started.expect("the peer starts"));
        assert_eq!(
            sim.run_for(Span::from_nanos(8 * Span::SECOND.nanos())),
            Ok(())
        );
        let code = Code(wire::header::REJECTED);
        let reset = Err(transport::Error::Reset { code });
        assert_eq!(*order.lock().unwrap(), [("b", reset.clone()), ("a", reset)]);
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
    }

    mod mesh {
        use std::collections::BTreeMap;

        use ::mesh::card::addresses::Addresses;
        use ::mesh::card::{self, Card};
        use ::mesh::status::Status;
        use std::future::poll_fn;
        use std::pin::pin;
        use std::task::Poll;

        use ::mesh::Member;
        use types::channel;
        use types::node::SealKey;

        use super::*;
        use crate::{Endpoint, Region, route};

        /// The key and private key of a second node.
        const OTHER: (types::node::Key, PrivateKey) =
            (types::node::Key::from_u128(2), PrivateKey([3; 32]));
        const INDEX: channel::Key = channel::Key::from_u128(7);
        /// A channel whose home the peer sets after the node starts again.
        const AFTER: channel::Key = channel::Key::from_u128(8);
        const TEN: Span = Span::from_nanos(10 * Span::SECOND.nanos());
        const TWENTY: Span = Span::from_nanos(20 * Span::SECOND.nanos());

        /// The code of a mesh message that the mesh refuses.
        const REFUSED: Code = Code(16);

        /// The first file of the mesh's log.
        const LOG: &str = "mesh/log/log-0";

        /// The member `key` with `private_key`, whose node listens on `host`.
        fn member(
            key: types::node::Key,
            private_key: &PrivateKey,
            host: &sim::node::Node,
        ) -> Member {
            let card = Card {
                name: format!("plant.node{key}").parse().unwrap(),
                public_key: private_key.public(),
                seal_key: SealKey::new([9; 32]).unwrap(),
                addresses: Addresses::new(vec![Address::Udp(listen(host))]).unwrap(),
                version: 1,
            };
            Member {
                card: card::Signed::sign(key, card, private_key),
                admission: [0; 64],
                ephemeral: None,
                status: Status::new(BTreeMap::new()).unwrap(),
            }
        }

        /// The region `plant`, where each of `members` is a voter.
        fn region(members: &[Member]) -> Region {
            Region {
                prefix: "plant".parse().unwrap(),
                members: members.to_vec(),
                voters: members.iter().map(|member| member.card.key()).collect(),
            }
        }

        /// Starts the node `key` on `host` with `private_key` and `region`.
        fn start(
            host: &sim::node::Node,
            (key, private_key): (types::node::Key, PrivateKey),
            region: Region,
        ) -> Node {
            Node::start(Config {
                key,
                private_key,
                region: Some(region),
                ..config(host, Size::MEBIBYTE, Box::new(heap))
            })
        }

        /// The node with key [`OWN`] on `host`, the one member and voter of its region.
        fn start_alone(host: &sim::node::Node) -> Node {
            start(host, (OWN, KEY), region(&[member(OWN, &KEY, host)]))
        }

        /// Starts the member [`OTHER`] of `members` on `host`: an endpoint with no
        /// node, which opens and serves the mesh as a node's does. Runs `act` with the
        /// mesh, then drops the mesh and its port.
        fn peer<F: Future<Output = ()> + 'static>(
            host: &sim::node::Node,
            members: Vec<Member>,
            act: impl FnOnce(::mesh::Mesh, sim::node::Node) -> F + Send + 'static,
        ) {
            let shard = env::shards::Config {
                name: "peer".into(),
                core: None,
            };
            let own = host.clone();
            let started = host.shards().start(shard, move |tasks| async move {
                let bound =
                    transport::Port::bind(&own.net(), listen(&own)).expect("a port");
                let endpoint = Endpoint {
                    part: bound.split(NonZeroUsize::MIN).pop().expect("one part"),
                    private_key: OTHER.1,
                    key: OTHER.0,
                    region: Some(region(&members)),
                    clock: own.clock(),
                    entropy: own.entropy(),
                };
                let pool = block::Config { budget: 1 << 20 };
                let memory = block::Heap::new(pool.reservation());
                let pool = Rc::new(block::Pool::new(pool, memory));
                let (transport, mesh) = endpoint
                    .open(own.files(), pool, tasks.clone())
                    .await
                    .expect("the mesh opens");
                let mesh = mesh.expect("the peer has a region");
                let port = route::accept(transport, Some(mesh.clone()), tasks);
                let (mut port, mut act) = (pin!(port), pin!(act(mesh, own)));
                poll_fn(|cx| {
                    let stopped = port.as_mut().poll(cx);
                    assert!(stopped.is_pending(), "the peer's transport stopped");
                    act.as_mut().poll(cx)
                })
                .await;
            });
            drop(started.expect("the peer starts"));
        }

        /// The members of the region of the node [`OWN`] on `hosts[0]` and the peer
        /// [`OTHER`] on `hosts[1]`, which are its two voters.
        fn pair(hosts: &[sim::node::Node; 2]) -> Vec<Member> {
            vec![
                member(OWN, &KEY, &hosts[0]),
                member(OTHER.0, &OTHER.1, &hosts[1]),
            ]
        }

        /// A home that the peer sets commits only with the node's vote: the node
        /// serves the mesh streams of its port, and its mesh answers on its
        /// transport.
        #[test]
        fn a_home_commits_with_the_vote_of_the_node() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [host(&mut sim, 2), host(&mut sim, 2)];
            let members = pair(&hosts);
            let node = start(&hosts[0], (OWN, KEY), region(&members));
            let set = Arc::new(Mutex::new(Vec::new()));
            let out = Arc::clone(&set);
            peer(&hosts[1], members, move |mesh, _| async move {
                let set = mesh.set_home(INDEX, OTHER.0).await;
                out.lock().unwrap().push(set);
            });
            assert_eq!(sim.run_for(TEN), Ok(()));
            assert_eq!(*set.lock().unwrap(), [Ok(())]);
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
        }

        /// What a peer that is not a member sees when it sends `header` to a node
        /// with a region.
        fn sent(header: &[u8]) -> Seen {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            let node = start_alone(&host);
            let seen = dial(&mut sim, &host, header);
            assert_eq!(sim.run_for(Span::HOUR), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let seen = seen.lock().unwrap().take();
            seen.expect("the peer ran")
                .expect("the dial reaches the node")
        }

        /// The mesh serves a mesh stream: at the first message that is not a mesh
        /// message, it stops the stream and resets its reply half with the code of
        /// a malformed message.
        #[test]
        fn the_mesh_serves_a_mesh_stream() {
            let header = wire::header::encode(wire::Protocol::Mesh);
            let Seen { sent, read, .. } = sent(&header);
            let code = Code(wire::header::MALFORMED);
            assert_eq!(sent, transport::Error::Stopped { code });
            assert_eq!(read, Err(transport::Error::Reset { code }));
        }

        /// The error of the first send that fails, when a peer that is not a member
        /// opens a one-way mesh stream to the node [`OWN`] of a region with [`OTHER`],
        /// then sends `message` up to 100 times, or `None` when no send fails.
        #[expect(
            clippy::unwrap_in_result,
            reason = "a sim error is a test failure, not a send that did not fail"
        )]
        fn sent_one_way(message: &[u8]) -> Option<transport::Error> {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            let peer = sim.node(sim::node::Config::default());
            let members = [member(OWN, &KEY, &host), member(OTHER.0, &OTHER.1, &peer)];
            let node = start(&host, (OWN, KEY), region(&members));
            let listen = listen(&host);
            let message = message.to_vec();
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            let shard = env::shards::Config {
                name: "peer".into(),
                core: None,
            };
            let own = peer.clone();
            let started = peer.shards().start(shard, move |tasks| async move {
                let (transport, pool) = transport(&own, tasks, CLIENT);
                let dialed =
                    transport.dial(KEY.public(), &[Address::Udp(listen)]).await;
                let session = dialed.expect("the dial reaches the node");
                let sender = session.open_sender(Class::Complete).await;
                let mut sender = sender.expect("a stream");
                let block = |bytes: &[u8]| {
                    let mut block = pool.alloc(bytes.len()).unwrap();
                    block.copy_from_slice(bytes);
                    block.freeze()
                };
                let header = wire::header::encode(wire::Protocol::Mesh);
                let mut sent = sender.send(block(&header)).await;
                for _ in 0..100 {
                    if sent.is_err() {
                        break;
                    }
                    own.clock().sleep(Span::MILLISECOND).await;
                    sent = sender.send(block(&message)).await;
                }
                *seen.lock().unwrap() = Some(sent.err());
                session.closed().await;
            });
            drop(started.expect("the peer starts"));
            assert_eq!(sim.run_for(Span::HOUR), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let sent = out.lock().unwrap().take();
            sent.expect("the peer ran")
        }

        /// A peer outside the region that sends a mesh message in the name of a member
        /// is refused: the mesh stops the stream with the code of a refused message.
        #[test]
        fn a_message_in_the_name_of_a_member_is_refused() {
            // A `raft` heartbeat reply from [`OTHER`] to [`OWN`] in term 0, which
            // needs no proof, with no chain.
            let mut reply = vec![1];
            reply.extend(OTHER.0.as_u128().to_le_bytes());
            reply.extend(OWN.as_u128().to_le_bytes());
            reply.extend(0_u64.to_le_bytes());
            reply.push(0);
            reply.extend(0_u64.to_le_bytes());
            reply.push(6);
            let code = REFUSED;
            assert_eq!(
                sent_one_way(&reply),
                Some(transport::Error::Stopped { code })
            );
        }

        /// A header with a byte after it names no protocol, so a node with a mesh
        /// rejects the stream too.
        #[test]
        fn a_header_with_a_byte_after_it_is_rejected() {
            let mut header = wire::header::encode(wire::Protocol::Mesh).to_vec();
            header.push(0);
            let code = Code(wire::header::REJECTED);
            let Seen { sent, read, .. } = sent(&header);
            assert_eq!(sent, transport::Error::Stopped { code });
            assert_eq!(read, Err(transport::Error::Reset { code }));
        }

        /// A mesh that does not open stops the node, and `join` gives why.
        #[test]
        fn a_mesh_that_does_not_open_stops_the_node() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            host.fail_file(Path::new(LOG), env::files::Operation::Open);
            let node = start_alone(&host);
            assert_eq!(sim.run(), Ok(()));
            let error =
                ::mesh::Error::Log(::mesh::log::Error::Files(env::files::Error::Io {
                    path: PathBuf::from(LOG),
                    operation: env::files::Operation::Open,
                    code: 5,
                }));
            assert_eq!(node.join(), Err(Error::Mesh(error.clone())));
            assert_eq!(
                Error::Mesh(error.clone()).to_string(),
                format!("the node's mesh did not open: {error}")
            );
        }

        /// A chunk store that does not open stops the node, and `join` gives why.
        #[test]
        fn a_chunk_store_that_does_not_open_stops_the_node() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            host.fail_file(Path::new("blob"), env::files::Operation::List);
            let node = start_alone(&host);
            assert_eq!(sim.run(), Ok(()));
            let error =
                ::mesh::Error::Blob(blob::Error::Files(env::files::Error::Io {
                    path: PathBuf::from("blob"),
                    operation: env::files::Operation::List,
                    code: 5,
                }));
            assert_eq!(node.join(), Err(Error::Mesh(error)));
        }

        /// Takes the lock of `host` as soon as it is free, then opens the mesh's log
        /// to write. Gives whether the lock was held, and the open of the log.
        fn probe(
            sim: &mut sim::Sim,
            host: &sim::node::Node,
        ) -> (bool, Result<(), env::files::Error>) {
            sim.run_on(host, |host, _| async move {
                let files = host.files();
                let clock = host.clock();
                let mut waited = false;
                let lock = loop {
                    let mode = env::files::Mode::Create { len: 0 };
                    match files.open(Path::new("lock"), mode).await {
                        Err(env::files::Error::Busy { .. }) => {
                            waited = true;
                            clock.sleep(Span::from_nanos(1_000)).await;
                        }
                        opened => break opened.expect("the lock opens"),
                    }
                };
                let mode = env::files::Mode::Write;
                let log = files.open(Path::new(LOG), mode).await.map(drop);
                drop(lock);
                (waited, log)
            })
            .expect("the probe ends")
        }

        /// A stop at any point of the start and the run of a node whose mesh writes
        /// its log for each home that the peer sets, then a probe that takes the
        /// lock as soon as it is free: the mesh's log is never busy then. The order
        /// of the two closes waits on #1835. The peer ends once a set waits [`TEN`],
        /// so the probe's run ends.
        #[test]
        fn the_log_of_the_mesh_is_free_once_the_lock_is() {
            let (mut held, mut logged) = (false, false);
            // The mesh opens at about 1.5 ms, and from 1.6 s the log writes a home
            // about each 0.7 ms.
            let opens = (0..200).map(|step| step * 13_000);
            let writes = (0..100).map(|step| 1_700_000_000 + step * 7_000);
            for after in opens.chain(writes) {
                let mut sim = sim::Sim::new(sim::Config::default());
                let hosts = [host(&mut sim, 2), host(&mut sim, 2)];
                let members = pair(&hosts);
                let node = start(&hosts[0], (OWN, KEY), region(&members));
                peer(&hosts[1], members, |mesh, host| async move {
                    let clock = host.clock();
                    for key in 100.. {
                        let key = channel::Key::from_u128(key);
                        let mut set = pin!(mesh.set_home(key, OTHER.0));
                        let mut late = pin!(clock.sleep(TEN));
                        let set = poll_fn(|cx| match set.as_mut().poll(cx) {
                            Poll::Ready(set) => Poll::Ready(set.is_ok()),
                            Poll::Pending => late.as_mut().poll(cx).map(|()| false),
                        });
                        if !set.await {
                            break;
                        }
                    }
                });
                let after = Span::from_nanos(after);
                assert_eq!(sim.run_for(after), Ok(()), "at {after:?}");
                node.stop();
                let (waited, log) = probe(&mut sim, &hosts[0]);
                held |= waited;
                logged |= log.is_ok();
                let busy = matches!(log, Err(env::files::Error::Busy { .. }));
                assert!(!busy, "at {after:?}: {log:?}");
                // A probe that takes the lock before the claim refuses the node.
                let refused = Error::Directory(env::files::Error::Busy {
                    path: PathBuf::from("lock"),
                });
                let joined = node.join();
                assert!(joined == Ok(()) || joined == Err(refused), "at {after:?}");
            }
            assert!(held && logged);
        }

        /// A node that starts again with its region, at once after a stop or a power
        /// cut, opens its mesh and votes again: a home that the peer sets after the
        /// restart commits.
        #[test]
        fn a_restart_opens_the_mesh_and_votes_again() {
            for cut in [false, true] {
                let mut sim = sim::Sim::new(sim::Config::default());
                let hosts = [host(&mut sim, 2), host(&mut sim, 2)];
                let members = pair(&hosts);
                let node = start(&hosts[0], (OWN, KEY), region(&members));
                let set = Arc::new(Mutex::new(Vec::new()));
                let out = Arc::clone(&set);
                peer(&hosts[1], members.clone(), move |mesh, host| async move {
                    let set = mesh.set_home(INDEX, OTHER.0).await;
                    out.lock().unwrap().push(set);
                    host.clock().sleep(TEN).await;
                    let set = mesh.set_home(AFTER, OTHER.0).await;
                    out.lock().unwrap().push(set);
                });
                assert_eq!(sim.run_for(TEN), Ok(()), "cut {cut}");
                assert_eq!(*set.lock().unwrap(), [Ok(())], "cut {cut}");
                if cut {
                    sim.crash(&hosts[0], sim::Crash::Power);
                } else {
                    node.stop();
                    assert_eq!(sim.run_for(Span::SECOND), Ok(()), "cut {cut}");
                    assert_eq!(node.join(), Ok(()), "cut {cut}");
                }
                let node = start(&hosts[0], (OWN, KEY), region(&members));
                assert_eq!(sim.run_for(TWENTY), Ok(()), "cut {cut}");
                assert_eq!(*set.lock().unwrap(), [Ok(()), Ok(())], "cut {cut}");
                node.stop();
                assert_eq!(sim.run(), Ok(()), "cut {cut}");
                assert_eq!(node.join(), Ok(()), "cut {cut}");
            }
        }
    }
}

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

use crate::identity::{self, Identity};
use crate::{Config, Error, Node};

struct Run {
    seed: u64,
    sim: sim::Sim,
    host: sim::node::Node,
    node: Node,
}

/// The port of [`config`].
const PORT: u16 = 7000;
/// The private key in the data directory of [`keyed`].
const KEY: PrivateKey = PrivateKey([2; 32]);
/// The node's key in the data directory of [`keyed`].
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
/// of [`DISK`], and the port at [`listen`].
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
             listen: {listen:?}, region: None, .. }}",
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

/// A host of `sim` with `cores` cores, whose data directory holds the key [`OWN`]
/// and the private key [`KEY`].
fn keyed(sim: &mut sim::Sim, cores: usize) -> sim::node::Node {
    let host = host(sim, cores);
    let identity = Identity {
        key: OWN,
        private_key: KEY,
    };
    write_key(sim, &host, identity::encode(&identity).to_vec());
    host
}

/// Writes `bytes` to a new file `node.key` on `host`.
fn write_key(sim: &mut sim::Sim, host: &sim::node::Node, bytes: Vec<u8>) {
    sim.run_on(host, move |host, _| async move {
        let files = host.files();
        let mode = env::files::Mode::Create {
            len: bytes.len() as u64,
        };
        let path = Path::new(identity::FILE);
        let file = files.open(path, mode).await.expect("opens");
        let pool = block::Pool::heap(block::Config { budget: 4096 });
        let block = pool.copy(&bytes).expect("a block");
        file.write_at(0, &[block]).await.expect("writes");
        file.sync().await.expect("syncs");
    })
    .expect("the run ends");
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
            let channels = [3, 2, 1].map(super::hub::index);
            let channels: Vec<_> = names.into_iter().zip(channels).collect();
            super::hub::define(&hub, &channels);
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
        let made = ["lock", "node.key", "shard-0", "shard-1", "shards-2"];
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
            let mut listed = listed(&mut sim, &host, "");
            let key = PathBuf::from("node.key");
            // Shard 0 reads the key once each ring has opened.
            if listed.contains(&key) {
                listed.retain(|name| *name != key);
                assert_eq!(listed, all, "at {after:?}");
            }
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
            "node.key",
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
        let made = ["lock", "node.key", "shard-0", "shard-1", &name, "shards-2"];
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
            let mut made =
                ["lock", "node.key", "shard-0", "shard-1", &name, "shards-2"];
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
        let made = ["lock", "node.key", "shard-0", "shard-1", "shards-2"];
        assert_eq!(listed(&mut sim, &host, ""), made.map(PathBuf::from));
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
            let made = ["lock", "node.key", "shard-0", "shard-1", "shards-2"];
            let made = made.map(PathBuf::from);
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
    use ::hub::Hub;
    use ::hub::reader::{Mode, Received};
    use ::hub::writer::{self, Writer};
    use spec::channel::{Channel, Data, Kind};
    use spec::data_type::DataType;
    use spec::definition::Definition;
    use types::authority::Authority;
    use types::channel::Key;
    use types::frame::key_set::KeySet;
    use types::frame::{Form, Label, Path as Stream};
    use types::name::Name;
    use types::sample::{Scalar, Type};

    use super::*;

    pub(super) const I64: Type = Type::Scalar(Scalar::I64);
    /// The wall time when a host is added, which is before the node's mesh time.
    pub(super) const WALL: i64 = 1_767_225_600_000_000_000;

    fn name(name: &str) -> Name {
        name.parse().expect("a valid name")
    }

    /// The index `key`.
    pub(super) fn index(key: u128) -> Channel {
        Channel {
            key: Key::from_u128(key),
            kind: Kind::Index {
                error: None,
                control: None,
            },
        }
    }

    /// The data channel `key` of `data_type` on the index `index`.
    pub(super) fn data(key: u128, data_type: Type, index: u128) -> Channel {
        let index = Key::from_u128(index);
        let data = Data::new(index, None, DataType::Sample(data_type), None);
        Channel {
            key: Key::from_u128(key),
            kind: Kind::Data(data.expect("no unit")),
        }
    }

    /// Defines each of `channels`, by its name, in one call. It passes them in their
    /// order: a map would sort them by name, and a test of slots needs the order.
    pub(super) fn define(hub: &Hub, channels: &[(&str, Channel)]) {
        let definitions: Vec<_> = channels
            .iter()
            .map(|(n, c)| (name(n), Definition::Channel(c.clone())))
            .collect();
        hub.define(
            definitions
                .iter()
                .map(|(name, definition)| (name, definition)),
        );
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
    pub(super) fn write(writer: &mut Writer, stamp: i64, value: i64) {
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
    pub(super) fn samples(received: &Received<'_>, key: u128) -> Vec<i64> {
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
            define(&hub, &[("time", index(1)), ("value", data(2, I64, 1))]);
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
            define(&hub, &[("time", index(1))]);
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
            let (open, pool, next, time) =
                super::home::create_open(&host, &tasks, 0, stop.clone());
            let monotonic = host.clock();
            let entropy = host.entropy();
            let spawn = tasks.clone();
            let hold = async move |home, guard| {
                let interner = next.await.expect("the open gives the interner");
                let hub = Hub::new(::hub::Config {
                    home,
                    interner,
                    tasks: spawn,
                    node: types::node::Key::from_u128(1),
                    time,
                    entropy,
                    mesh: None,
                });
                define(&hub, &[("time", index(1)), ("value", data(2, I64, 1))]);
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

    /// Who dials the node.
    enum Dialer {
        /// A node with this key.
        Node(PrivateKey),
        /// A program.
        Program,
    }

    /// A pool of 1 MiB to send on.
    fn pool() -> Rc<block::Pool> {
        let config = block::Config { budget: 1 << 20 };
        let memory = block::Heap::new(config.reservation());
        Rc::new(block::Pool::new(config, memory))
    }

    /// A part of a port at a free address of `host`, and a pool of 1 MiB to send on.
    fn port(host: &sim::node::Node) -> (Rc<block::Pool>, transport::port::Part) {
        let pool = pool();
        let at = SocketAddr::new(host.addresses()[0], 0);
        let bound = transport::Port::bind(&host.net(), at).expect("a port");
        let part = bound.split(NonZeroUsize::MIN).pop().expect("one part");
        (pool, part)
    }

    /// A transport on `host` with `key`, at a free port, with its pool.
    fn transport(
        host: &sim::node::Node,
        tasks: env::tasks::Tasks,
        key: PrivateKey,
    ) -> (Transport, Rc<block::Pool>) {
        let (pool, part) = port(host);
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
        (Transport::new(config, part).expect("a transport"), pool)
    }

    /// A program's transport on `host`, at a free port, with its pool.
    fn program(
        host: &sim::node::Node,
        tasks: env::tasks::Tasks,
    ) -> (transport::Client, Rc<block::Pool>) {
        let (pool, part) = port(host);
        let config = transport::client::Config {
            clock: host.clock(),
            entropy: host.entropy(),
            tasks,
            pool: Rc::clone(&pool),
        };
        (
            transport::Client::new(config, part).expect("a client"),
            pool,
        )
    }

    /// Starts a peer on a new host of `sim` that dials the node at `listen` of `host`
    /// as `dialer`, opens a two-way stream, sends each of `first`, then sends until a
    /// send fails, and reads the reply half. Gives what the peer saw once the run
    /// reaches it, or the error of the dial. The peer holds its session until the
    /// session closes.
    fn dial(
        sim: &mut sim::Sim,
        host: &sim::node::Node,
        dialer: Dialer,
        first: &[&[u8]],
    ) -> Arc<Mutex<Option<Result<Seen, transport::Error>>>> {
        let peer = sim.node(sim::node::Config::default());
        let listen = listen(host);
        let first: Vec<Vec<u8>> = first.iter().map(|bytes| bytes.to_vec()).collect();
        let out = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&out);
        let shard = env::shards::Config {
            name: "peer".into(),
            core: None,
        };
        let own = peer.clone();
        let started = peer.shards().start(shard, move |tasks| async move {
            let at = [Address::Udp(listen)];
            let (mut node, mut client) = (None, None);
            let (session, pool) = match dialer {
                Dialer::Node(key) => {
                    let (transport, pool) = transport(&own, tasks, key);
                    let session = transport.dial(KEY.public(), &at).await;
                    node = Some(transport);
                    (session, pool)
                }
                Dialer::Program => {
                    let (program, pool) = program(&own, tasks);
                    let session = program.dial(KEY.public(), &at).await;
                    client = Some(program);
                    (session, pool)
                }
            };
            let session = match session {
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
            let mut sent = Ok(());
            for bytes in &first {
                if sent.is_ok() {
                    sent = sender.send(message(bytes)).await;
                }
            }
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
            drop((node, client));
        });
        drop(started.expect("the peer starts"));
        out
    }

    /// Runs `sim` for an hour, stops `node` cleanly, and gives what the peer of
    /// [`dial`] saw.
    fn watch(
        mut sim: sim::Sim,
        node: Node,
        seen: &Mutex<Option<Result<Seen, transport::Error>>>,
    ) -> Seen {
        assert_eq!(sim.run_for(Span::HOUR), Ok(()));
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
        let seen = seen.lock().unwrap().take();
        seen.expect("the peer ran")
            .expect("the dial reaches the node")
    }

    /// What `dialer` sees when it sends `first` to a running node with `memory`.
    fn sees(dialer: Dialer, first: &[&[u8]], memory: Size) -> Seen {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        let node = Node::start(config(&host, memory, Box::new(heap)));
        let seen = dial(&mut sim, &host, dialer, first);
        watch(sim, node, &seen)
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
        } = sees(Dialer::Node(CLIENT), &[&header], Size::MEBIBYTE);
        assert_eq!(peer, Peer::Node(key));
        assert_eq!(bytes_max, 65_536);
        assert_eq!(sent, transport::Error::Stopped { code });
        assert_eq!(read, Err(transport::Error::Reset { code }));
    }

    /// The header of a hub stream, then an open of the channel 9, which no node
    /// knows.
    fn open_unknown() -> [Vec<u8>; 3] {
        let header = wire::header::encode(wire::Protocol::Hub);
        let open = wire::hub::Open {
            mode: wire::hub::Mode::Complete { limit_bytes: 0 },
            channels: 1,
        };
        let mut opened = vec![0; open.encoded_len()];
        open.encode(&mut opened);
        let mut keys = [0; wire::hub::keys::LEN];
        wire::hub::keys::encode(&[types::channel::Key::from_u128(9)], &mut keys);
        [header.to_vec(), opened, keys.to_vec()]
    }

    /// A node with no region has no members, so it stops a hub stream of a node, and
    /// resets its reply half, with the code of a rejected header.
    #[test]
    fn rejects_a_hub_stream_of_a_node_with_no_region() {
        let first = open_unknown();
        let first: Vec<&[u8]> = first.iter().map(Vec::as_slice).collect();
        let Seen { sent, read, .. } =
            sees(Dialer::Node(CLIENT), &first, Size::MEBIBYTE);
        let code = Code(wire::header::REJECTED);
        assert_eq!(sent, transport::Error::Stopped { code });
        assert_eq!(read, Err(transport::Error::Reset { code }));
    }

    /// The node stops a hub stream of a program, and resets its reply half, with the
    /// code of a rejected header.
    #[test]
    fn rejects_a_hub_stream_of_a_program() {
        let header = wire::header::encode(wire::Protocol::Hub);
        let Seen {
            peer, sent, read, ..
        } = sees(Dialer::Program, &[&header], Size::MEBIBYTE);
        let code = Code(wire::header::REJECTED);
        assert_eq!(peer, Peer::Node(KEY.public()));
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
        let Seen { sent, read, .. } =
            sees(Dialer::Node(CLIENT), &[&header], Size::MEBIBYTE);
        assert_eq!(sent, transport::Error::Stopped { code });
        assert_eq!(read, Err(transport::Error::Reset { code }));
    }

    /// A node whose pool's largest block is below 64 KiB takes messages of that block.
    #[test]
    fn a_node_whose_largest_block_is_below_64_kib_serves_its_port() {
        let header = wire::header::encode(wire::Protocol::Mesh);
        let seen = sees(
            Dialer::Node(CLIENT),
            &[&header],
            Size::from_bytes(128 << 10),
        );
        assert_eq!(seen.bytes_max, 57_344);
        let code = Code(wire::header::REJECTED);
        assert_eq!(seen.sent, transport::Error::Stopped { code });
    }

    /// A session that stays open delays no stream of another session.
    #[test]
    fn a_held_session_delays_no_other_session() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        let header = wire::header::encode(wire::Protocol::Mesh);
        let first = dial(&mut sim, &host, Dialer::Node(CLIENT), &[&header]);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        let second = dial(&mut sim, &host, Dialer::Node(CLIENT), &[&header]);
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
        let host = keyed(&mut sim, 2);
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
        let host = keyed(&mut sim, 2);
        host.fail_file(Path::new("shard-1/ring"), env::files::Operation::Open);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        let seen = dial(
            &mut sim,
            &host,
            Dialer::Node(CLIENT),
            &[&wire::header::encode(wire::Protocol::Mesh)],
        );
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
        let host = keyed(&mut sim, 2);
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
        let host = keyed(&mut sim, 2);
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
        let host = keyed(&mut sim, 2);
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
        let host = keyed(&mut sim, 2);
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

    mod key {
        use super::*;
        use crate::identity::{FILE, LEN};

        /// The bytes of `node.key` on `host`.
        fn read(sim: &mut sim::Sim, host: &sim::node::Node) -> Vec<u8> {
            sim.run_on(host, |host, _| async move {
                let files = host.files();
                let mode = env::files::Mode::Read;
                let file = files.open(Path::new(FILE), mode).await.expect("opens");
                let len = usize::try_from(file.len()).unwrap();
                let pool = block::Pool::heap(block::Config { budget: 4096 });
                let into = pool.alloc(len).expect("a block");
                file.read_at(0, into).await.expect("reads").to_vec()
            })
            .expect("the run ends")
        }

        /// Whether `node.key` holds a key.
        fn key_written(sim: &mut sim::Sim, host: &sim::node::Node) -> bool {
            sim.run_on(host, |host, _| async move {
                let mode = env::files::Mode::Create { len: LEN as u64 };
                let file = host.files().open(Path::new(FILE), mode).await;
                let pool = block::Pool::heap(block::Config { budget: 4096 });
                let into = pool.alloc(LEN).expect("a block");
                let read = file.expect("opens").read_at(0, into).await;
                read.expect("reads").starts_with(b"foundation/key/1")
            })
            .expect("the run ends")
        }

        /// The bytes of the identity [`OWN`], [`KEY`].
        fn own() -> Vec<u8> {
            let identity = Identity {
                key: OWN,
                private_key: KEY,
            };
            identity::encode(&identity).to_vec()
        }

        /// What a peer that dials the node on `host` and pins `key` gets within a
        /// second.
        #[expect(
            clippy::unwrap_in_result,
            reason = "a test helper panics on a peer that does not run"
        )]
        fn dial(
            sim: &mut sim::Sim,
            host: &sim::node::Node,
            key: types::ed25519::PublicKey,
        ) -> Result<Peer, transport::Error> {
            let peer = sim.node(sim::node::Config::default());
            let listen = listen(host);
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            let shard = env::shards::Config {
                name: "peer".into(),
                core: None,
            };
            let own = peer.clone();
            let started = peer.shards().start(shard, move |tasks| async move {
                let (transport, _pool) = transport(&own, tasks, CLIENT);
                let dialed = transport.dial(key, &[Address::Udp(listen)]).await;
                *seen.lock().unwrap() = Some(dialed.map(|session| session.peer()));
            });
            drop(started.expect("the peer starts"));
            assert_eq!(sim.run_for(Span::SECOND), Ok(()));
            let dialed = out.lock().unwrap().take();
            dialed.expect("the dial ends")
        }

        /// Starts a node on `host`, runs `sim` for a second, then stops it and gives
        /// the error of its join.
        fn start_and_stop(
            sim: &mut sim::Sim,
            host: &sim::node::Node,
        ) -> Result<(), Error> {
            let node = Node::start(config(host, Size::MEBIBYTE, Box::new(heap)));
            assert_eq!(sim.run_for(Span::SECOND), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            node.join()
        }

        /// The first start makes a UUIDv7 key at mesh time and a private key,
        /// and a node started again on the data directory proves the same key.
        #[test]
        fn a_node_started_twice_proves_the_key_of_its_first_start() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            assert_eq!(start_and_stop(&mut sim, &host), Ok(()));
            let made = read(&mut sim, &host);
            assert_eq!(made.len(), LEN);
            assert_eq!(&made[..16], b"foundation/key/1");
            let key = u128::from_be_bytes(made[16..32].try_into().unwrap());
            assert_eq!(key >> 76 & 0xf, 7, "version 7");
            let millis = i64::try_from(key >> 80).unwrap();
            let wall = super::super::hub::WALL / 1_000_000;
            assert!((wall..wall + 1_000).contains(&millis), "{millis} at {wall}");
            let public = PrivateKey(made[32..64].try_into().unwrap()).public();
            let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
            assert_eq!(dial(&mut sim, &host, public), Ok(Peer::Node(public)));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            assert_eq!(read(&mut sim, &host), made, "the second start keeps it");
        }

        /// Two hosts make two keys.
        #[test]
        fn each_node_makes_its_own_key() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [host(&mut sim, 1), host(&mut sim, 1)];
            let made = hosts.map(|host| {
                assert_eq!(start_and_stop(&mut sim, &host), Ok(()));
                read(&mut sim, &host)
            });
            assert_ne!(made[0][16..32], made[1][16..32], "keys");
            assert_ne!(made[0][32..64], made[1][32..64], "private keys");
        }

        #[test]
        fn a_node_proves_the_key_in_its_data_directory() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            write_key(&mut sim, &host, own());
            let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
            let public = KEY.public();
            assert_eq!(dial(&mut sim, &host, public), Ok(Peer::Node(public)));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            assert_eq!(read(&mut sim, &host), own());
        }

        /// 68 zero bytes are a key that a crash kept from being written.
        #[test]
        fn a_node_makes_a_key_over_zero_bytes() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            write_key(&mut sim, &host, vec![0; LEN]);
            assert_eq!(start_and_stop(&mut sim, &host), Ok(()));
            assert_eq!(&read(&mut sim, &host)[..16], b"foundation/key/1");
        }

        /// A `node.key` with no bytes is a key not yet written.
        #[test]
        fn a_node_makes_a_key_over_an_empty_file() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            sim.run_on(&host, |host, _| async move {
                let files = host.files();
                let mode = env::files::Mode::Create { len: 0 };
                let file = files.open(Path::new(FILE), mode).await.expect("opens");
                file.sync().await.expect("syncs");
                files.sync_dir(Path::new("")).await.expect("syncs");
            })
            .expect("the run ends");
            assert_eq!(read(&mut sim, &host), Vec::<u8>::new());
            assert_eq!(start_and_stop(&mut sim, &host), Ok(()));
            assert_eq!(&read(&mut sim, &host)[..16], b"foundation/key/1");
        }

        /// A file of another length that is not 0, or of another tag or checksum, stops
        /// the node, which keeps the file.
        #[test]
        fn a_key_that_is_not_valid_stops_the_node() {
            let mut tag = own();
            tag[15] = b'2';
            let crc = crc32c::crc32c(&tag[..64]);
            tag[64..].copy_from_slice(&crc.to_le_bytes());
            let mut changed = own();
            changed[40] ^= 1;
            let short = own()[..LEN - 1].to_vec();
            let long = [own(), vec![0]].concat();
            for bytes in [short, long, tag, changed] {
                let mut sim = sim::Sim::new(sim::Config::default());
                let host = host(&mut sim, 2);
                write_key(&mut sim, &host, bytes.clone());
                let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
                assert_eq!(sim.run(), Ok(()));
                assert_eq!(node.join(), Err(Error::Key));
                assert_eq!(read(&mut sim, &host), bytes, "keeps the file");
            }
            assert_eq!(
                Error::Key.to_string(),
                "the file node.key in the data directory is not a node key; restore \
                 it from a backup of this node"
            );
        }

        /// What [`crate::create_key`] gives on `host` for [`OWN`] and `private_key`.
        #[expect(
            clippy::unwrap_in_result,
            reason = "a test helper panics on a run that does not end"
        )]
        fn create(
            sim: &mut sim::Sim,
            host: &sim::node::Node,
            private_key: PrivateKey,
        ) -> Result<(), Error> {
            sim.run_on(host, move |host, _| async move {
                crate::create_key(&host.files(), OWN, private_key).await
            })
            .expect("the run ends")
        }

        /// Each start proves the key that `create_key` writes, also over 68 zero
        /// bytes.
        #[test]
        fn a_node_proves_the_key_that_create_key_writes() {
            for before in [None, Some(vec![0; LEN])] {
                let mut sim = sim::Sim::new(sim::Config::default());
                let host = host(&mut sim, 2);
                if let Some(bytes) = before {
                    write_key(&mut sim, &host, bytes);
                }
                assert_eq!(create(&mut sim, &host, KEY), Ok(()));
                assert_eq!(read(&mut sim, &host), own());
                let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
                let public = KEY.public();
                assert_eq!(dial(&mut sim, &host, public), Ok(Peer::Node(public)));
                node.stop();
                assert_eq!(sim.run(), Ok(()));
                assert_eq!(node.join(), Ok(()));
                assert_eq!(read(&mut sim, &host), own());
            }
        }

        /// `create_key` writes nothing over a file that holds a key, valid or not.
        #[test]
        fn create_key_writes_nothing_over_a_key() {
            let mut changed = own();
            changed[40] ^= 1;
            let exists = env::files::Error::Exists {
                path: PathBuf::from("node.key"),
            };
            for bytes in [own(), changed] {
                let mut sim = sim::Sim::new(sim::Config::default());
                let host = host(&mut sim, 2);
                write_key(&mut sim, &host, bytes.clone());
                let created = create(&mut sim, &host, PrivateKey([9; 32]));
                assert_eq!(created, Err(Error::Directory(exists.clone())));
                assert_eq!(read(&mut sim, &host), bytes, "keeps the file");
            }
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            let short = own()[..LEN - 1].to_vec();
            write_key(&mut sim, &host, short.clone());
            let length = env::files::Error::Length {
                path: PathBuf::from("node.key"),
                expected: 68,
                found: 67,
            };
            assert_eq!(create(&mut sim, &host, KEY), Err(Error::Directory(length)));
            assert_eq!(read(&mut sim, &host), short, "keeps the file");
            assert_eq!(
                Error::Directory(exists).to_string(),
                "cannot claim the data directory: path node.key is already there"
            );
        }

        /// `create_key` writes its key into a `node.key` with no bytes, as into no
        /// file.
        #[test]
        fn create_key_writes_into_an_empty_file() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            sim.run_on(&host, |host, _| async move {
                let files = host.files();
                let mode = env::files::Mode::Create { len: 0 };
                let file = files.open(Path::new(FILE), mode).await.expect("opens");
                file.sync().await.expect("syncs");
                files.sync_dir(Path::new("")).await.expect("syncs");
            })
            .expect("the run ends");
            assert_eq!(read(&mut sim, &host), Vec::<u8>::new());
            assert_eq!(create(&mut sim, &host, KEY), Ok(()));
            assert_eq!(read(&mut sim, &host), own());
        }

        /// A power cut after `create_key` keeps the key.
        #[test]
        fn a_created_key_survives_a_power_cut() {
            for seed in 0..16 {
                let mut sim = sim::Sim::new(sim::Config {
                    seed,
                    ..sim::Config::default()
                });
                let host = host(&mut sim, 2);
                assert_eq!(create(&mut sim, &host, KEY), Ok(()));
                sim.crash(&host, sim::Crash::Power);
                assert_eq!(read(&mut sim, &host), own(), "seed {seed}");
            }
        }

        /// A failed file call of `create_key` gives its error.
        #[test]
        fn a_failed_file_call_of_create_key_gives_its_error() {
            use env::files::Operation::{Open, ReadAt, Sync, SyncDir, WriteAt};
            let calls = [Open, ReadAt, WriteAt, Sync].map(|call| (FILE, call));
            for (path, operation) in calls.into_iter().chain([("", SyncDir)]) {
                let mut sim = sim::Sim::new(sim::Config::default());
                let host = host(&mut sim, 2);
                host.fail_file(Path::new(path), operation);
                let error = env::files::Error::Io {
                    path: PathBuf::from(path),
                    operation,
                    code: 5,
                };
                let created = create(&mut sim, &host, KEY);
                assert_eq!(created, Err(Error::Directory(error)), "{operation:?}");
            }
        }

        /// A failed file call on a new key stops the node, and the next start makes
        /// one.
        #[test]
        fn a_failed_file_call_on_the_key_stops_the_node() {
            use env::files::Operation::{Open, ReadAt, Sync, WriteAt};
            for operation in [Open, ReadAt, WriteAt, Sync] {
                let mut sim = sim::Sim::new(sim::Config::default());
                let host = host(&mut sim, 2);
                host.fail_file(Path::new(FILE), operation);
                let error = env::files::Error::Io {
                    path: PathBuf::from(FILE),
                    operation,
                    code: 5,
                };
                assert_eq!(
                    start_and_stop(&mut sim, &host),
                    Err(Error::Directory(error))
                );
                assert_eq!(start_and_stop(&mut sim, &host), Ok(()));
                assert_eq!(&read(&mut sim, &host)[..16], b"foundation/key/1");
            }
        }

        /// The first start makes its key durable before it serves.
        #[test]
        fn a_new_key_survives_a_power_cut() {
            for seed in 0..16 {
                let mut sim = sim::Sim::new(sim::Config {
                    seed,
                    ..sim::Config::default()
                });
                let host = host(&mut sim, 2);
                assert_eq!(start_and_stop(&mut sim, &host), Ok(()));
                let made = read(&mut sim, &host);
                sim.crash(&host, sim::Crash::Power);
                assert_eq!(read(&mut sim, &host), made, "seed {seed}");
            }
        }

        /// A key made while the OS clock reads before 1970 has the time 0.
        #[test]
        fn a_clock_before_1970_makes_a_key_at_the_epoch() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = sim.node(sim::node::Config {
                cores: NonZeroUsize::new(2).unwrap(),
                wall: types::time::Stamp::EPOCH - Span::HOUR,
                ..sim::node::Config::default()
            });
            assert_eq!(start_and_stop(&mut sim, &host), Ok(()));
            let made = read(&mut sim, &host);
            let key = u128::from_be_bytes(made[16..32].try_into().unwrap());
            assert_eq!(key >> 80, 0, "the millis of the key");
        }

        /// A load makes no key until its clock has mesh time. It calls the load
        /// directly, because no seam of `Node::start` delays mesh time: the sim's wall
        /// source syncs at once.
        #[test]
        fn a_load_makes_its_key_once_it_has_mesh_time() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 1);
            let (early, loaded) = sim
                .run_on(&host, |host, tasks| async move {
                    let (driver, clock) = ::clock::Clock::new(host.clock());
                    let loaded = std::rc::Rc::new(std::cell::RefCell::new(None));
                    let out = std::rc::Rc::clone(&loaded);
                    let (files, entropy) = (host.files(), host.entropy());
                    tasks.spawn(async move {
                        let identity = identity::load(&files, &clock, &entropy).await;
                        *out.borrow_mut() = Some(identity.map(|identity| identity.key));
                    });
                    host.clock().sleep(Span::SECOND).await;
                    let early = loaded.borrow().is_some();
                    let wall = host.wall();
                    tasks.spawn(async move { driver.run(wall).await });
                    host.clock().sleep(Span::SECOND).await;
                    (early, loaded.take())
                })
                .expect("the run ends");
            assert!(!early, "a key before mesh time");
            let key = loaded.expect("a key at mesh time").expect("a key");
            let made = read(&mut sim, &host);
            assert_eq!(made[16..32], key.as_u128().to_be_bytes());
        }

        /// A failed sync of the directory after the key's write stops the node, so
        /// no key is proven before its name is durable. The fault comes at each time
        /// of the first start, because the claim and the buffers sync the directory
        /// first.
        #[test]
        fn a_failed_sync_of_the_directory_stops_the_node() {
            let error = Err(Error::Directory(env::files::Error::Io {
                path: PathBuf::new(),
                operation: env::files::Operation::SyncDir,
                code: 5,
            }));
            let stopped = (0..200).any(|step| {
                let mut sim = sim::Sim::new(sim::Config::default());
                let host = host(&mut sim, 2);
                let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
                assert_eq!(sim.run_for(Span::from_nanos(step * 25_000)), Ok(()));
                host.fail_file(Path::new(""), env::files::Operation::SyncDir);
                assert_eq!(sim.run_for(Span::SECOND), Ok(()));
                node.stop();
                assert_eq!(sim.run(), Ok(()));
                node.join() == error && key_written(&mut sim, &host)
            });
            assert!(stopped, "no start stopped at the sync of the directory");
        }

        /// A failed sync of a new key stops the node, and can leave the key in the
        /// cache only. A start that proves that key makes it durable, so a power cut
        /// after it keeps the key.
        #[test]
        fn a_key_proven_after_a_failed_sync_survives_a_power_cut() {
            let mut tried = Vec::new();
            for seed in 0..16 {
                let mut sim = sim::Sim::new(sim::Config {
                    seed,
                    ..sim::Config::default()
                });
                let host = host(&mut sim, 2);
                host.fail_file(Path::new(FILE), env::files::Operation::Sync);
                let error = env::files::Error::Io {
                    path: PathBuf::from(FILE),
                    operation: env::files::Operation::Sync,
                    code: 5,
                };
                assert_eq!(
                    start_and_stop(&mut sim, &host),
                    Err(Error::Directory(error))
                );
                let made = read(&mut sim, &host);
                if made == [0; LEN] {
                    continue;
                }
                let public = PrivateKey(made[32..64].try_into().unwrap()).public();
                let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
                let dialed = dial(&mut sim, &host, public);
                node.stop();
                assert_eq!(sim.run(), Ok(()));
                assert_eq!(node.join(), Ok(()));
                if dialed.is_err() {
                    continue;
                }
                tried.push(seed);
                sim.crash(&host, sim::Crash::Power);
                assert_eq!(start_and_stop(&mut sim, &host), Ok(()));
                assert_eq!(read(&mut sim, &host), made, "seed {seed}");
            }
            assert!(!tried.is_empty(), "no seed kept the key in the cache");
        }
    }

    mod mesh {
        use std::collections::BTreeMap;
        use std::sync::atomic::{AtomicBool, Ordering};

        use ::mesh::card::addresses::Addresses;
        use ::mesh::card::{self, Card};
        use ::mesh::region::Founding;
        use ::mesh::status::Status;
        use std::future::poll_fn;
        use std::pin::pin;
        use std::task::Poll;

        use ::mesh::Member;
        use spec::definition::{Definition, Kind};
        use spec::subject::Subject;
        use spec::tree::Chunks;
        use types::channel;
        use types::node::SealKey;

        use super::*;
        use crate::{Endpoint, route};

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
        /// A time by which the mesh of a node opens, and before its first write after
        /// the open.
        const OPEN: Span = Span::from_nanos(10_000_000);
        /// The time after [`OPEN`], in nanoseconds, at which a write of [`LOG`] that
        /// fails from [`OPEN`] stops the group in the sim.
        const WRITE: i64 = 1_792_553_323;

        /// Why the group stops when a write of [`LOG`] fails.
        fn write_failed() -> ::mesh::Stopped {
            ::mesh::Stopped::Write(::mesh::log::Error::Files(env::files::Error::Io {
                path: PathBuf::from(LOG),
                operation: env::files::Operation::WriteAt,
                code: 5,
            }))
        }

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
        fn region(members: &[Member]) -> Founding {
            Founding {
                prefix: "plant".parse().unwrap(),
                members: members.to_vec(),
                voters: members.iter().map(|member| member.card.key()).collect(),
                definitions: BTreeMap::new(),
                homes: BTreeMap::new(),
            }
        }

        /// Starts the node on `host`, which [`keyed`] made, with `region`.
        fn start(host: &sim::node::Node, region: Founding) -> Node {
            Node::start(Config {
                region: Some(region),
                ..config(host, Size::MEBIBYTE, Box::new(heap))
            })
        }

        /// The region of the node [`OWN`] on `host` alone, whose founding spec holds
        /// the index `plant.time` (key 1), with its home at [`OWN`], and the data
        /// channel `plant.value` (key 2).
        fn founded(host: &sim::node::Node) -> Founding {
            use super::super::hub::{I64, data, index};
            let mut founding = region(&[member(OWN, &KEY, host)]);
            let channels = [("plant.time", index(1)), ("plant.value", data(2, I64, 1))];
            for (name, channel) in channels {
                let name = name.parse().unwrap();
                founding
                    .definitions
                    .insert(name, Definition::Channel(channel));
            }
            founding
                .homes
                .insert(types::channel::Key::from_u128(1), OWN);
            founding
        }

        /// Starts the node [`OWN`] on `host` with the region [`founded`], and gives
        /// what a task reads back after it writes 7 at `stamp` to `plant.value`, by
        /// name: the samples of keys 1 and 2.
        fn round_trip(
            sim: &mut sim::Sim,
            host: &sim::node::Node,
            stamp: i64,
        ) -> Option<(Vec<i64>, Vec<i64>)> {
            use super::super::hub::{samples, write, writer};
            let node = start(host, founded(host));
            let read = Arc::new(Mutex::new(None));
            let out = Arc::clone(&read);
            let clock = host.clock();
            node.spawn(move |hub| async move {
                let value = "plant.value".parse().unwrap();
                let reader = hub.reader(&[value], ::hub::reader::Mode::Complete).await;
                let mut reader = reader.expect("the reader opens");
                let mut writer = writer(&hub, &clock, &["plant.value"]).await;
                write(&mut writer, stamp, 7);
                let received = reader.next().await.expect("a frame");
                *out.lock().unwrap() =
                    Some((samples(&received, 1), samples(&received, 2)));
            });
            assert_eq!(sim.run_for(TEN), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            read.lock().unwrap().take()
        }

        /// The hub knows each channel of the founding spec, so a task opens a writer
        /// and a reader on them by name.
        #[test]
        fn a_task_opens_sessions_on_the_channels_of_the_founding_spec() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let stamp = super::super::hub::WALL;
            let read = round_trip(&mut sim, &host, stamp);
            assert_eq!(read, Some((vec![stamp], vec![7])));
        }

        /// A founding with a data channel whose index the spec does not hold fails the
        /// node at its first open.
        #[test]
        fn a_founding_with_a_dangling_index_fails_the_node() {
            use super::super::hub::{I64, data};
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let mut founding = region(&[member(OWN, &KEY, &host)]);
            let name = "plant.value".parse().unwrap();
            let channel = Definition::Channel(data(2, I64, 1));
            founding.definitions.insert(name, channel);
            let node = start(&host, founding);
            let index = types::channel::Key::from_u128(1);
            assert_eq!(
                sim.run(),
                Err(sim::Error::Panicked {
                    thread: "shard-0".into(),
                    message: format!(
                        "the index {index} of channel plant.value is not a known index"
                    ),
                    seed: 0,
                })
            );
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(
                node.join(),
                Err(Error::Panicked(thread::Panicked {
                    name: "shard-0".into()
                }))
            );
        }

        /// The node defines the founding channels at each open, not only at the
        /// first.
        #[test]
        fn a_node_that_opens_again_knows_the_founding_channels() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let stamp = super::super::hub::WALL;
            assert!(
                round_trip(&mut sim, &host, stamp).is_some(),
                "the first open"
            );
            let later = stamp + TEN.nanos();
            let read = round_trip(&mut sim, &host, later);
            assert_eq!(read, Some((vec![later], vec![7])));
        }

        /// The hub reads the homes of the node's mesh, so a reader of a channel whose
        /// index has its home at another node gets that home.
        #[test]
        fn a_reader_of_a_channel_whose_home_is_another_node_gets_the_home() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let mut founding = founded(&host);
            founding.members.push(member(OTHER.0, &OTHER.1, &host));
            founding.voters.insert(OTHER.0);
            founding
                .homes
                .insert(types::channel::Key::from_u128(1), OTHER.0);
            let node = start(&host, founding);
            let opened = Arc::new(Mutex::new(None));
            let out = Arc::clone(&opened);
            node.spawn(move |hub| async move {
                let value = "plant.value".parse().unwrap();
                let reader = hub.reader(&[value], ::hub::reader::Mode::Complete).await;
                *out.lock().unwrap() = Some(reader.err());
            });
            assert_eq!(sim.run_for(TEN), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let remote = ::hub::reader::Error::Remote { home: OTHER.0 };
            assert_eq!(opened.lock().unwrap().take(), Some(Some(remote)));
        }

        /// The node with key [`OWN`] on `host`, the one member and voter of its region.
        fn start_alone(host: &sim::node::Node) -> Node {
            start(host, region(&[member(OWN, &KEY, host)]))
        }

        /// Starts the member [`OTHER`] of `members` on `host` without a `Node`: an
        /// endpoint and a hub with its key, which serve the mesh as a node's do. Runs
        /// `act` with the mesh, then drops the mesh and its port.
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
                let identity = Identity {
                    key: OTHER.0,
                    private_key: OTHER.1,
                };
                let endpoint = Endpoint {
                    part: bound.split(NonZeroUsize::MIN).pop().expect("one part"),
                    region: Some(region(&members)),
                    clock: own.clock(),
                    entropy: own.entropy(),
                };
                let pool = super::pool();
                let stop = crate::stop::Stop::default();
                let (open, buffer, next, time) =
                    super::super::home::create_open(&own, &tasks, 0, stop);
                let opened =
                    open.run(own.files(), Rc::new(buffer), tasks.clone()).await;
                let home = opened.expect("the buffer opens");
                let interner = next.await.expect("the open gives the interner");
                let (key, entropy) = (identity.key, endpoint.entropy.clone());
                let (transport, mesh) = endpoint
                    .open(identity, own.files(), pool, tasks.clone())
                    .await
                    .expect("the mesh opens");
                let mesh = mesh.expect("the peer has a region");
                let hub = ::hub::Hub::new(::hub::Config {
                    home,
                    interner,
                    tasks: tasks.clone(),
                    node: key,
                    time,
                    entropy,
                    mesh: Some(mesh.clone()),
                });
                hub.define(&region(&members).definitions);
                let port = route::accept(transport, Some(mesh.clone()), hub, tasks);
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
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let members = pair(&hosts);
            let node = start(&hosts[0], region(&members));
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
            let host = keyed(&mut sim, 2);
            let node = start_alone(&host);
            let seen = dial(&mut sim, &host, Dialer::Node(CLIENT), &[header]);
            watch(sim, node, &seen)
        }

        /// What `dialer` sees when it sends [`open_unknown`] to the node [`OWN`] of a
        /// region with [`OTHER`].
        ///
        /// [`open_unknown`]: super::open_unknown
        fn opened(dialer: Dialer) -> Seen {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start(&hosts[0], region(&pair(&hosts)));
            let first = super::open_unknown();
            let first: Vec<&[u8]> = first.iter().map(Vec::as_slice).collect();
            let seen = dial(&mut sim, &hosts[0], dialer, &first);
            watch(sim, node, &seen)
        }

        /// The node gives a hub stream of a member to its hub, which stops an open of
        /// a channel that the node does not know, and resets its reply half, with
        /// `UNKNOWN`.
        #[test]
        fn a_hub_stream_of_a_member_goes_to_the_hub() {
            let Seen { sent, read, .. } = opened(Dialer::Node(OTHER.1));
            let code = Code(wire::hub::UNKNOWN);
            assert_eq!(sent, transport::Error::Stopped { code });
            assert_eq!(read, Err(transport::Error::Reset { code }));
        }

        /// The node stops a hub stream of a node that is not a member, and resets its
        /// reply half, with the code of a rejected header.
        #[test]
        fn rejects_a_hub_stream_of_a_node_that_is_not_a_member() {
            let Seen { sent, read, .. } = opened(Dialer::Node(CLIENT));
            let code = Code(wire::header::REJECTED);
            assert_eq!(sent, transport::Error::Stopped { code });
            assert_eq!(read, Err(transport::Error::Reset { code }));
        }

        /// The node stops a hub stream of a program, and resets its reply half, with
        /// the code of a rejected header.
        #[test]
        fn rejects_a_hub_stream_of_a_program_of_a_region() {
            let Seen { sent, read, .. } = opened(Dialer::Program);
            let code = Code(wire::header::REJECTED);
            assert_eq!(sent, transport::Error::Stopped { code });
            assert_eq!(read, Err(transport::Error::Reset { code }));
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
            let host = keyed(&mut sim, 2);
            let peer = sim.node(sim::node::Config::default());
            let members = [member(OWN, &KEY, &host), member(OTHER.0, &OTHER.1, &peer)];
            let node = start(&host, region(&members));
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
            let host = keyed(&mut sim, 2);
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

        /// A mesh whose group stops stops the node, and `join` gives why.
        #[test]
        fn a_mesh_whose_group_stops_stops_the_node() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = start_alone(&host);
            assert_eq!(sim.run_for(OPEN), Ok(()));
            host.fail_file(Path::new(LOG), env::files::Operation::WriteAt);
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Err(Error::Group(write_failed())));
            assert_eq!(
                Error::Group(write_failed()).to_string(),
                format!("the group of the node's mesh stopped: {}", write_failed())
            );
        }

        /// A task whose drop panics as the mesh's group stops: `join` ranks the
        /// group's stop above the panic.
        #[test]
        fn a_panic_as_the_group_stops_gives_the_group_error() {
            struct Panics;
            impl Drop for Panics {
                fn drop(&mut self) {
                    panic!("a task's drop panics");
                }
            }
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = start_alone(&host);
            node.spawn(|_| {
                let panics = Panics;
                async move {
                    std::future::pending::<()>().await;
                    drop(panics);
                }
            });
            assert_eq!(sim.run_for(OPEN), Ok(()));
            host.fail_file(Path::new(LOG), env::files::Operation::WriteAt);
            assert_eq!(
                sim.run(),
                Err(sim::Error::Panicked {
                    thread: "shard-0".into(),
                    message: "a task's drop panics".into(),
                    seed: 0,
                })
            );
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Err(Error::Group(write_failed())));
        }

        /// A task that panics one nanosecond before the group stops: `join` gives the
        /// panic.
        #[test]
        fn a_panic_before_the_group_stops_gives_the_panic() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = start_alone(&host);
            let at = sim::node::Config::default().monotonic
                + OPEN
                + Span::from_nanos(WRITE - 1);
            let own = host.clone();
            node.spawn(move |_| async move {
                own.clock().sleep_until(at).await;
                panic!("a task panics");
            });
            assert_eq!(sim.run_for(OPEN), Ok(()));
            host.fail_file(Path::new(LOG), env::files::Operation::WriteAt);
            assert_eq!(
                sim.run(),
                Err(sim::Error::Panicked {
                    thread: "shard-0".into(),
                    message: "a task panics".into(),
                    seed: 0,
                })
            );
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(
                node.join(),
                Err(Error::Panicked(thread::Panicked {
                    name: "shard-0".into()
                }))
            );
        }

        /// A task that panics after the group stops, before the node sees the stop:
        /// `join` gives the panic. At [`WRITE`], the sim runs the group's stop first.
        #[test]
        fn a_panic_before_the_node_sees_the_group_stop_gives_the_panic() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = start_alone(&host);
            let at =
                sim::node::Config::default().monotonic + OPEN + Span::from_nanos(WRITE);
            let own = host.clone();
            node.spawn(move |_| async move {
                own.clock().sleep_until(at).await;
                panic!("a task panics");
            });
            assert_eq!(sim.run_for(OPEN), Ok(()));
            host.fail_file(Path::new(LOG), env::files::Operation::WriteAt);
            assert_eq!(
                sim.run(),
                Err(sim::Error::Panicked {
                    thread: "shard-0".into(),
                    message: "a task panics".into(),
                    seed: 0,
                })
            );
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(
                node.join(),
                Err(Error::Panicked(thread::Panicked {
                    name: "shard-0".into()
                }))
            );
        }

        /// Breaks the node's UDP socket from a task on shard 0 at `OPEN + after`, with
        /// each write of the log failing from `OPEN`. Gives whether the task ran, and
        /// the node's `join`.
        fn udp_fault_at(after: Span) -> (bool, Result<(), Error>) {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = start_alone(&host);
            let at = sim::node::Config::default().monotonic + OPEN + after;
            let ran = Arc::new(AtomicBool::new(false));
            let (own, done) = (host.clone(), Arc::clone(&ran));
            node.spawn(move |_| async move {
                own.clock().sleep_until(at).await;
                own.fail_udp(listen(&own));
                done.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            });
            assert_eq!(sim.run_for(OPEN), Ok(()));
            host.fail_file(Path::new(LOG), env::files::Operation::WriteAt);
            assert_eq!(sim.run(), Ok(()));
            (ran.load(Ordering::SeqCst), node.join())
        }

        /// Of a transport and a group that stop, `join` gives the one that the node
        /// sees first, which at one instant can be either.
        #[test]
        fn join_gives_the_stop_that_the_node_sees_first() {
            let error = transport::Error::Network {
                error: env::net::Error::Io { code: 5 },
            };
            assert_eq!(
                udp_fault_at(Span::from_nanos(WRITE - 1)),
                (true, Err(Error::Transport(error)))
            );
            assert_eq!(
                udp_fault_at(Span::from_nanos(WRITE)),
                (true, Err(Error::Group(write_failed())))
            );
            assert_eq!(
                udp_fault_at(Span::from_nanos(WRITE + 1)),
                (false, Err(Error::Group(write_failed())))
            );
        }

        /// A stop of the node from a task on shard 0, in the poll that breaks the
        /// node's UDP socket: the node sees its stop and the transport's at one poll,
        /// with the group still pending, and its stop ranks first.
        #[test]
        fn a_stop_as_the_transport_of_a_mesh_stops_gives_no_error() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = Arc::new(std::sync::Mutex::new(start_alone(&host)));
            let at = sim::node::Config::default().monotonic + OPEN;
            let (own, stops) = (host.clone(), Arc::clone(&node));
            node.lock()
                .expect("no panic holds the lock")
                .spawn(move |_| async move {
                    own.clock().sleep_until(at).await;
                    own.fail_udp(listen(&own));
                    stops.lock().expect("no panic holds the lock").stop();
                });
            assert_eq!(sim.run(), Ok(()));
            let node = Arc::into_inner(node).expect("shard 0 dropped the task");
            let node = node.into_inner().expect("no panic holds the lock");
            assert_eq!(node.join(), Ok(()));
        }

        /// A chunk store that does not open stops the node, and `join` gives why.
        #[test]
        fn a_chunk_store_that_does_not_open_stops_the_node() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            host.fail_file(Path::new("blob"), env::files::Operation::List);
            let node = start_alone(&host);
            assert_eq!(sim.run(), Ok(()));
            let error = blob::Error::Files(env::files::Error::Io {
                path: PathBuf::from("blob"),
                operation: env::files::Operation::List,
                code: 5,
            });
            assert_eq!(node.join(), Err(Error::Blob(error.clone())));
            assert_eq!(
                Error::Blob(error.clone()).to_string(),
                format!("cannot open the node's chunk store: {error}")
            );
        }

        /// Takes the lock of `host` as soon as it is free, then binds the node's port
        /// and opens the mesh's log to write. Gives whether the lock was held, the
        /// bind, and the open of the log.
        fn probe(
            sim: &mut sim::Sim,
            host: &sim::node::Node,
        ) -> (
            bool,
            Result<(), env::net::Error>,
            Result<(), env::files::Error>,
        ) {
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
                let udp = env::net::udp::Config {
                    local: listen(&host),
                    send_buffer_bytes: 1 << 16,
                    recv_buffer_bytes: 1 << 16,
                };
                let port = host.net().udp(&udp).map(drop);
                let mode = env::files::Mode::Write;
                let log = files.open(Path::new(LOG), mode).await.map(drop);
                drop(lock);
                (waited, port, log)
            })
            .expect("the probe ends")
        }

        /// Starts the peer [`OTHER`] of `members` on `host`, which sets a home in the
        /// mesh, one after another, until a set waits [`TEN`].
        fn set_homes(host: &sim::node::Node, members: Vec<Member>) {
            peer(host, members, |mesh, host| async move {
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
        }

        /// A stop at any point of the start and the run of a node whose mesh writes
        /// its log for each home that the peer sets, then a probe that takes the
        /// lock as soon as it is free: the mesh's log is never busy then, and the port
        /// of a node that held the lock binds once the lock is free. A node stopped
        /// before its claim holds no lock, so its port can still be bound. The order
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
                let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
                let members = pair(&hosts);
                let node = start(&hosts[0], region(&members));
                set_homes(&hosts[1], members);
                let after = Span::from_nanos(after);
                assert_eq!(sim.run_for(after), Ok(()), "at {after:?}");
                node.stop();
                let (waited, port, log) = probe(&mut sim, &hosts[0]);
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
                if waited {
                    assert_eq!(port, Ok(()), "at {after:?}");
                }
            }
            assert!(held && logged);
        }

        /// When the mesh's group stops the node while a peer holds a session with it,
        /// the port binds once the lock is free.
        #[test]
        fn the_port_is_free_once_the_lock_is_after_the_group_stops() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let members = pair(&hosts);
            let node = start(&hosts[0], region(&members));
            set_homes(&hosts[1], members);
            assert_eq!(sim.run_for(Span::from_nanos(1_700_000_000)), Ok(()));
            hosts[0].fail_file(Path::new(LOG), env::files::Operation::WriteAt);
            let (waited, port, _) = probe(&mut sim, &hosts[0]);
            assert_eq!((waited, port), (true, Ok(())));
            assert_eq!(node.join(), Err(Error::Group(write_failed())));
        }

        /// A node started with a region with founding definitions puts the chunks of
        /// their tree in its chunk store.
        #[test]
        fn the_chunk_store_holds_the_tree_of_the_founding_definitions() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let mut region = region(&[member(OWN, &KEY, &host)]);
            let subject = Subject::new([KEY.public()].into()).unwrap();
            let label = Kind::Subject.key("plant.operator").unwrap();
            region.definitions = [(label, Definition::Subject(subject))].into();
            let mut chunks = Chunks::default();
            let tree = spec::region::tree(&mut chunks, &region.definitions);
            let listed = tree.chunks.clone();
            let node = start(&host, region);
            assert_eq!(sim.run_for(Span::SECOND), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let shard = env::shards::Config {
                name: "store".into(),
                core: None,
            };
            let own = host.clone();
            let held = Arc::new(Mutex::new(Vec::new()));
            let out = Arc::clone(&held);
            let started = host.shards().start(shard, move |_| async move {
                let pool = block::Config { budget: 1 << 20 };
                let memory = block::Heap::new(pool.reservation());
                let pool = Rc::new(block::Pool::new(pool, memory));
                // A node gives no read of its chunk store, so the test opens the store
                // at its private path.
                let store = blob::Store::open(blob::Config {
                    files: own.files(),
                    dir: crate::directory::blob(),
                    pool,
                })
                .await
                .expect("the store opens");
                for digest in tree.chunks {
                    let block = store.get(digest).await.expect("a read");
                    let bytes = block.map(|block| block.to_vec());
                    out.lock().unwrap().push((digest, bytes));
                }
            });
            drop(started.expect("the store starts"));
            assert_eq!(sim.run(), Ok(()));
            assert!(listed.contains(&tree.root));
            let expected: Vec<_> = listed
                .into_iter()
                .map(|digest| (digest, chunks.get(digest).map(<[u8]>::to_vec)))
                .collect();
            assert_eq!(*held.lock().unwrap(), expected);
        }

        /// A node that starts again with its region, at once after a stop or a power
        /// cut, opens its mesh and votes again: a home that the peer sets after the
        /// restart commits.
        #[test]
        fn a_restart_opens_the_mesh_and_votes_again() {
            for cut in [false, true] {
                let mut sim = sim::Sim::new(sim::Config::default());
                let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
                let members = pair(&hosts);
                let node = start(&hosts[0], region(&members));
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
                let node = start(&hosts[0], region(&members));
                assert_eq!(sim.run_for(TWENTY), Ok(()), "cut {cut}");
                assert_eq!(*set.lock().unwrap(), [Ok(()), Ok(())], "cut {cut}");
                node.stop();
                assert_eq!(sim.run(), Ok(()), "cut {cut}");
                assert_eq!(node.join(), Ok(()), "cut {cut}");
            }
        }

        /// A line that `ssh-keygen -t ed25519` wrote, and its key.
        const OPERATOR: &str = concat!(
            "ssh-ed25519 ",
            "AAAAC3NzaC1lZDI1NTE5AAAAIGVVuOR8JKYpAcWLMUveadmJ1wUAmYGgIDtqlhFe7Yhg",
        );
        const OPERATOR_KEY: [u8; 32] = [
            0x65, 0x55, 0xb8, 0xe4, 0x7c, 0x24, 0xa6, 0x29, 0x01, 0xc5, 0x8b, 0x31,
            0x4b, 0xde, 0x69, 0xd9, 0x89, 0xd7, 0x05, 0x00, 0x99, 0x81, 0xa0, 0x20,
            0x3b, 0x6a, 0x96, 0x11, 0x5e, 0xed, 0x88, 0x60,
        ];

        /// A node whose region has founding definitions plans no change for files
        /// with the same definitions, at its first open and at a second open with
        /// the same region.
        #[test]
        fn a_plan_of_the_founding_definitions_gives_no_change() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let mut region = region(&[member(OWN, &KEY, &host)]);
            let key = types::ed25519::PublicKey::new(OPERATOR_KEY).unwrap();
            let subject = Subject::new([key].into()).unwrap();
            let label = Kind::Subject.key("plant.operator").unwrap();
            region.definitions = [(label, Definition::Subject(subject))].into();
            let text =
                format!("subject \"plant.operator\" {{ keys = [\"{OPERATOR}\"] }}\n");
            for open in ["first", "second"] {
                let node = start(&host, region.clone());
                let planned = Arc::new(Mutex::new(None));
                let out = Arc::clone(&planned);
                let files = vec![(std::path::PathBuf::from("plant.hcl"), text.clone())];
                node.operate(move |ops| async move {
                    let plan = ops.plan(files).await;
                    *out.lock().unwrap() = Some(plan.map(|(_, output)| output));
                });
                assert_eq!(sim.run_for(Span::SECOND), Ok(()), "{open} open");
                node.stop();
                assert_eq!(sim.run(), Ok(()), "{open} open");
                assert_eq!(node.join(), Ok(()), "{open} open");
                let output = planned.lock().unwrap().take().expect("a plan");
                let output =
                    output.unwrap_or_else(|error| panic!("{open} open: {error}"));
                let changes = output["changes"].as_array().map(Vec::len);
                assert_eq!(changes, Some(0), "{open} open: {output}");
            }
        }

        /// A channel that an apply makes has a UUIDv7 key at mesh time.
        #[test]
        fn an_applied_channel_has_a_key_at_mesh_time() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = start(&host, region(&[member(OWN, &KEY, &host)]));
            let made = Arc::new(Mutex::new(None));
            let out = Arc::clone(&made);
            let text = format!(
                "channel \"plant.time\" {{ kind = \"index\" }}\n\
                 placement \"plant\" {{\n  select = \"plant.*\"\n  home = \"plant.node{OWN}\"\n}}\n"
            );
            node.operate(move |ops| async move {
                let files = vec![(std::path::PathBuf::from("plant.hcl"), text)];
                let (plan, _) = ops.plan(files).await.expect("a plan");
                let path = std::path::Path::new("plant.plan");
                ops.apply(path, &plan).await.expect("an apply");
                let spec = ops.mesh().spec().await.expect("a spec");
                let label = Kind::Channel.key("plant.time").unwrap();
                let Some(Definition::Channel(channel)) = spec.definitions.get(&label)
                else {
                    panic!("a channel at plant.time");
                };
                *out.lock().unwrap() = Some(channel.key);
            });
            assert_eq!(
                sim.run_for(Span::from_nanos(30 * Span::SECOND.nanos())),
                Ok(())
            );
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let key = made.lock().unwrap().take().expect("a channel");
            let millis = i64::try_from(key.as_u128() >> 80).unwrap();
            let start = sim::node::Config::default().wall.nanos() / 1_000_000;
            assert!(
                (start..start + 1_000).contains(&millis),
                "{millis} ms is not in the first second of {start} ms"
            );
        }

        /// A node with a region whose port does not bind drops a task of `operate`
        /// uncalled, as `spawn` does, and `join` gives the port error.
        #[test]
        fn operate_on_a_regional_node_that_failed_drops_the_task() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let udp = env::net::udp::Config {
                local: listen(&host),
                send_buffer_bytes: 1 << 16,
                recv_buffer_bytes: 1 << 16,
            };
            let held = host.net().udp(&udp).expect("the port binds");
            let node = start(&host, region(&[member(OWN, &KEY, &host)]));
            node.operate(|_| async {});
            assert_eq!(sim.run(), Ok(()));
            let listen = listen(&host);
            let error = Error::Port {
                listen,
                error: env::net::Error::AddressInUse { local: listen },
            };
            assert_eq!(node.join(), Err(error));
            drop(held);
        }

        /// `operate` refuses a node with no region, which has no mesh to operate on.
        #[test]
        #[should_panic(expected = "`operate` on a node with no region")]
        fn operate_refuses_a_node_with_no_region() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
            node.operate(|_| async {});
        }
    }
}

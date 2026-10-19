use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use env::thread;
use sim::shard::Fault;
use types::byte::Size;
use types::ed25519::PrivateKey;
use types::time::Span;

use secret::seal::Opener;

use crate::identity::{self, Identity};
use crate::{Budget, Config, Error, Node, Region};

mod address;
mod card;
mod log;
mod region;

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
/// The bytes of the seal key in the data directory of [`keyed`].
const SEAL: [u8; 32] = [3; 32];

/// The DER bytes of a CA certificate, the one root of [`config`].
const CA: &[u8] = include_bytes!("tests/ca.der");

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
fn config<M>(
    host: &sim::node::Node,
    budget: Size,
    memory: Box<dyn FnMut(usize) -> Result<M, os::memory::Error>>,
) -> Config<M> {
    Config {
        shards: host.shards(),
        clock: host.clock(),
        wall: host.wall(),
        budget: Budget {
            pool: budget,
            disk: DISK,
        },
        memory,
        files: {
            let host = host.clone();
            Box::new(move || {
                let host = host.clone();
                Box::new(move || host.files())
            })
        },
        entropy: host.entropy(),
        net: host.net(),
        listen: listen(host),
        region: Region::Absent,
        name: "edge".parse().expect("a name"),
        settings: crate::Settings::default(),
        vars: std::collections::BTreeMap::new(),
        roots: os::roots::Roots {
            certificates: vec![CA.to_vec()],
            ..os::roots::Roots::default()
        },
        log: crate::Log {
            writer: Box::new(std::io::sink()),
            format: ops::Log::default(),
        },
        threads: host.threads(),
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
fn a_stopper_from_another_thread_stops_its_node() {
    fn sendable<T: Send + Sync + 'static>(_: &T) {}
    let mut run = start(7, 2, &[]);
    let stopper = run.node.stopper();
    sendable(&stopper);
    assert_eq!(run.sim.run_for(Span::HOUR), Ok(()));
    stopper.clone().stop();
    assert_eq!(run.sim.run(), Ok(()));
    assert_eq!(run.node.join(), Ok(()));
}

#[test]
fn a_stop_after_the_node_ended_does_nothing() {
    let mut run = start(7, 2, &[]);
    let stopper = run.node.stopper();
    run.node.stop();
    assert_eq!(run.sim.run(), Ok(()));
    assert_eq!(run.node.join(), Ok(()));
    stopper.stop();
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
    let part = budget.bytes() / 3;
    assert_eq!(part * 3 + 2, budget.bytes());
    let reservation = |budget| {
        block::Config::new(budget)
            .expect("the budget fits")
            .reservation()
    };
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
        let len = block::Config::new((1 << 20) / 3)
            .expect("the budget fits")
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
fn a_config_shows_its_budget_entropy_settings_and_log_but_not_its_memory_or_files() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = sim.node(sim::node::Config::default());
    let config = config(&host, Size::from_bytes(4096), Box::new(heap));
    let (clock, wall, entropy) = (&config.clock, &config.wall, &config.entropy);
    assert_eq!(
        format!("{config:?}"),
        format!(
            "Config {{ shards: Shards {{ .. }}, clock: {clock:?}, wall: {wall:?}, \
             budget: Budget {{ pool: Size(4096), disk: {DISK:?} }}, entropy: {entropy:?}, \
             listen: {listen:?}, region: Absent, name: Name(\"edge\"), \
             settings: {settings:?}, log: Log {{ format: Log {{ json: false, level: Level(Info) }}, .. }}, \
             threads: Threads {{ .. }}, \
             .. }}",
            listen = config.listen,
            settings = config.settings,
        )
    );
}

/// No part of the largest pool budget fits a shard's reservation. No shard starts,
/// and a task given to the node is dropped unrun.
#[test]
fn a_pool_reservation_past_usize_max_starts_no_shard() {
    let pool = Size::from_bytes(u64::MAX);
    for cores in [1, 2] {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, cores);
        let node = Node::start(config(&host, pool, Box::new(heap)));
        assert_eq!(host.shard_starts(), [], "{cores} cores");
        assert_eq!(fate(&probe(&node)), Fate::Dropped, "{cores} cores");
        assert_eq!(sim.run(), Ok(()));
        let e = node.join().unwrap_err();
        assert_eq!(e, Error::Pool { pool, cores });
        assert_eq!(
            e.to_string(),
            format!(
                "the pool budget 18446744073709551615B gives one of {cores} shards a \
                 pool that needs more address space than this host has"
            )
        );
    }
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

/// Starts a node on `host`, runs it until its opens end, stops it, and gives what
/// `join` gives.
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
        budget: Budget {
            pool: Size::MEBIBYTE,
            disk: Size::from_bytes(disk),
        },
        ..config(host, Size::MEBIBYTE, Box::new(heap))
    });
    // The task runs once each file that a caller reads is there, or drops as the
    // node fails.
    let probe = probe(&node);
    let mut ms = 0;
    while fate(&probe) == Fate::Waiting {
        let after = Span::from_nanos(ms * Span::MILLISECOND.nanos());
        assert!(after < Span::HOUR, "the opens go on");
        assert_eq!(sim.run_for(Span::MILLISECOND), Ok(()));
        ms += 1;
    }
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

/// Writes `bytes` to a new file `path` in the data directory of `host`.
fn write(sim: &mut sim::Sim, host: &sim::node::Node, path: &str, bytes: Vec<u8>) {
    let path = PathBuf::from(path);
    sim.run_on(host, move |host, _| async move {
        let files = host.files();
        let mode = env::files::Mode::Create {
            len: bytes.len() as u64,
        };
        let file = files.open(&path, mode).await.expect("opens");
        if !bytes.is_empty() {
            let pool =
                block::Pool::heap(block::Config::new(4096).expect("the budget fits"));
            let block = pool.copy(&bytes).expect("a block");
            file.write_at(0, &[block]).await.expect("writes");
        }
        file.sync().await.expect("syncs");
        files.sync_dir(Path::new("")).await.expect("syncs");
    })
    .expect("the run ends");
}

/// The bytes of the file `path` in the data directory of `host`, or `None` when it
/// is not there.
fn bytes(sim: &mut sim::Sim, host: &sim::node::Node, path: &str) -> Option<Vec<u8>> {
    let path = PathBuf::from(path);
    sim.run_on(host, |host, _| async move {
        let files = host.files();
        let file = match files.open(&path, env::files::Mode::Read).await {
            Err(env::files::Error::NotFound { .. }) => return None,
            file => file.expect("opens"),
        };
        let len = usize::try_from(file.len()).unwrap();
        if len == 0 {
            return Some(Vec::new());
        }
        let pool =
            block::Pool::heap(block::Config::new(4096).expect("the budget fits"));
        let into = pool.alloc(len).expect("a block");
        Some(file.read_at(0, into).await.expect("reads").to_vec())
    })
    .expect("the run ends")
}

fn host(sim: &mut sim::Sim, cores: usize) -> sim::node::Node {
    sim.node(sim::node::Config {
        cores: NonZeroUsize::new(cores).unwrap(),
        ..sim::node::Config::default()
    })
}

/// Takes the lock of the data directory of `host` once the node frees it. Gives the
/// lock, and whether it waited.
async fn take_lock(host: &sim::node::Node) -> (env::files::File, bool) {
    let (files, clock) = (host.files(), host.clock());
    let mut waited = false;
    loop {
        let mode = env::files::Mode::Create { len: 0 };
        match files.open(Path::new("lock"), mode).await {
            Err(env::files::Error::Busy { .. }) => {
                waited = true;
                clock.sleep(Span::from_nanos(1_000)).await;
            }
            opened => break (opened.expect("the lock opens"), waited),
        }
    }
}

/// A host of `sim` with `cores` cores, whose data directory holds the key [`OWN`]
/// and the private key [`KEY`].
fn keyed(sim: &mut sim::Sim, cores: usize) -> sim::node::Node {
    keyed_on(
        sim,
        sim::node::Config {
            cores: NonZeroUsize::new(cores).unwrap(),
            ..sim::node::Config::default()
        },
    )
}

/// As [`keyed`], on a host of `config`.
fn keyed_on(sim: &mut sim::Sim, config: sim::node::Config) -> sim::node::Node {
    let host = sim.node(config);
    let identity = Identity {
        key: OWN,
        private_key: KEY,
        opener: Opener::from_bytes(SEAL),
    };
    write_key(sim, &host, identity::encode(&identity).to_vec());
    host
}

/// Writes `bytes` to a new file `node.key` on `host`, durably.
fn write_key(sim: &mut sim::Sim, host: &sim::node::Node, bytes: Vec<u8>) {
    sim.run_on(host, move |host, _| async move {
        let files = host.files();
        let mode = env::files::Mode::Create {
            len: bytes.len() as u64,
        };
        let path = Path::new(identity::FILE);
        let file = files.open(path, mode).await.expect("opens");
        let pool =
            block::Pool::heap(block::Config::new(4096).expect("the budget fits"));
        let block = pool.copy(&bytes).expect("a block");
        file.write_at(0, &[block]).await.expect("writes");
        file.sync().await.expect("syncs");
        files.sync_dir(Path::new("")).await.expect("syncs");
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
        let mut fate = self.0.lock().unwrap_or_else(PoisonError::into_inner);
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

#[test]
fn a_probe_with_a_poisoned_lock_does_not_abort_a_test_that_panics() {
    let fate = Arc::new(Mutex::new(Fate::Waiting));
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = fate.lock().unwrap();
        panic!("poison the lock");
    }));
    poisoned.expect_err("the lock is poisoned");
    let probe = Probe(fate);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _probe = probe;
        panic!("the test broke");
    }));
    unwound.expect_err("the test panics");
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

/// An unnamed reader on the channel named `channel`.
fn unnamed(channel: &str, mode: ::hub::reader::Mode) -> ::hub::reader::Config {
    ::hub::reader::Config {
        select: types::name::Selector::new([channel]).expect("a selector"),
        mode,
        subject: "test".parse().expect("a name"),
        name: None,
        hold: Span::ZERO,
    }
}

mod buffer {
    use std::cell::RefCell;
    use std::num::NonZeroU32;
    use std::rc::Rc;

    use ::buffer::{Buffer, Entry, Samples};
    use types::channel::{Key, Slots};
    use types::frame::Path as Stream;
    use types::time::Stamp;

    use super::*;

    /// Writes one entry of index `key` to the ring of shard `core` of `host`.
    fn write(sim: &mut sim::Sim, host: &sim::node::Node, core: usize, key: u128) {
        sim.run_on(host, move |host, tasks| async move {
            let config = block::Config::new(1 << 20).expect("the budget fits");
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
                trimmed: false,
            };
            let mut slots = Slots::new();
            let buffer = Buffer::open(config, &mut slots).await.expect("opens");
            let index = Key::from_u128(key);
            let entry = Entry {
                index,
                slot: slots.index(index),
                path: Stream::Live,
                first: 0,
                samples: Some(Samples {
                    len: NonZeroU32::MIN,
                    last: Some(Stamp::from_nanos(1)),
                }),
                stored_at: Stamp::from_nanos(1),
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
    pub(super) fn all_opened(probe: &Mutex<Fate>, after: Span) -> bool {
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
        node.spawn(move |hub| async move {
            let names = ["c", "b", "a"];
            let channels = [3, 2, 1].map(super::hub::index);
            let channels: Vec<_> = names.into_iter().zip(channels).collect();
            super::hub::define(&hub, &channels);
            let writer = super::hub::writer(&hub, &names).await;
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

    /// A shard's part of the budget must hold the least budget of its buffer. A shard
    /// that waits for the interner does not open after it.
    #[test]
    fn a_shard_part_too_small_for_the_buffer_stops_the_node() {
        let budget = Size::from_bytes(512 << 10);
        let mut run = start_with(7, 16, &[], budget, Box::new(heap));
        assert_eq!(run.sim.run(), Ok(()));
        let e = run.node.join().unwrap_err();
        assert_eq!(
            e,
            Error::Small {
                pool: budget,
                cores: 16,
                min: Size::from_bytes(996_352),
            }
        );
        assert_eq!(
            e.to_string(),
            "the pool budget 512KiB opens no buffer on each of 16 shards; it needs at \
             least 973KiB"
        );
        let made = ["lock", "name", "shards-16"].map(PathBuf::from);
        assert_eq!(listed(&mut run.sim, &run.host, ""), made);
    }

    /// The least pool budget that [`Error::Small`] names opens the buffer of each
    /// shard. One byte less gives shard 0 enough with its remainder, and shard 1 too
    /// little.
    #[test]
    fn the_least_pool_budget_opens_the_buffer_of_each_shard() {
        let least = Size::from_bytes(996_352);
        let mut run = start_with(7, 16, &[], least, Box::new(heap));
        let opened = Arc::new(Mutex::new(false));
        let set = Arc::clone(&opened);
        run.node
            .spawn(move |_| async move { *set.lock().unwrap() = true });
        assert_eq!(run.sim.run_for(Span::HOUR), Ok(()));
        assert!(*opened.lock().unwrap());
        run.node.stop();
        assert_eq!(run.sim.run(), Ok(()));
        assert_eq!(run.node.join(), Ok(()));
        let under = Size::from_bytes(least.bytes() - 1);
        let mut run = start_with(7, 16, &[], under, Box::new(heap));
        assert_eq!(run.sim.run(), Ok(()));
        let small = Error::Small {
            pool: under,
            cores: 16,
            min: least,
        };
        assert_eq!(run.node.join(), Err(small));
    }

    #[test]
    fn each_shard_opens_a_ring_in_its_own_directory() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        assert_eq!(run_on(&mut sim, &host), Ok(()));
        let shards = ["shard-0", "shard-1"];
        let made = [
            "address", "budget", "lock", "name", "node.key", "shard-0", "shard-1",
            "shards-2",
        ];
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
        let min = Size::from_bytes(8_626_176);
        let cases = [(smallest, "4212KiB"), (2 * smallest - 1, "8626175B")];
        for (bytes, shown) in cases {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            let disk = Size::from_bytes(bytes);
            let node = Node::start(Config {
                budget: Budget {
                    pool: Size::MEBIBYTE,
                    disk,
                },
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
                     needs at least 8424KiB"
                )
            );
        }
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        assert_eq!(run_on_disk(&mut sim, &host, 2 * smallest), Ok(()));
    }

    /// The test rig of `tests/it` gives each core 8 MiB of disk.
    #[test]
    fn a_disk_budget_of_8_mib_for_each_core_starts_a_host_of_any_size() {
        for cores in [1, 64, 1024] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, cores);
            let per_core =
                |bytes: u64| Size::from_bytes(u64::try_from(cores).unwrap() * bytes);
            let node = Node::start(Config {
                budget: Budget {
                    pool: per_core(1 << 20),
                    disk: per_core(8 << 20),
                },
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            assert_eq!(host.shard_starts().len(), cores, "{cores} cores");
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()), "{cores} cores");
        }
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
                budget: Budget {
                    pool: Size::MEBIBYTE,
                    disk,
                },
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            assert_eq!(host.shard_starts(), []);
            assert_eq!(node.join(), Err(Error::Disk { disk, cores, min }));
        }
    }

    /// With two least rings less one byte, shard 0's part of the disk fits, and its
    /// part of the pool does not.
    #[test]
    fn a_disk_budget_that_holds_no_ring_wins_over_a_pool_reservation_past_usize_max() {
        let smallest = ::buffer::Layout::fit(0, crate::BODY_MAX).unwrap_err().min;
        for (cores, bytes, min) in
            [(1, 1, smallest), (2, 2 * smallest - 1, 2 * smallest)]
        {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, cores);
            let disk = Size::from_bytes(bytes);
            let node = Node::start(Config {
                budget: Budget {
                    pool: Size::from_bytes(u64::MAX),
                    disk,
                },
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            assert_eq!(host.shard_starts(), [], "{cores} cores");
            assert_eq!(sim.run(), Ok(()));
            let min = Size::from_bytes(min);
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
        let all = ["lock", "name", "shard-0", "shard-1", "shard-2", "shards-3"];
        let all = all.map(PathBuf::from);
        let mut seen = [false; 7];
        for step in 0..250 {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 3);
            let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
            let after = Span::from_nanos(step * 10_000);
            assert_eq!(sim.run_for(after), Ok(()), "at {after:?}");
            node.stop();
            assert_eq!(sim.run(), Ok(()), "at {after:?}");
            assert_eq!(node.join(), Ok(()), "at {after:?}");
            let mut listed = listed(&mut sim, &host, "");
            let late = ["budget", "node.key", "address"].map(PathBuf::from);
            // Shard 0 keeps the budgets, then reads the key, then writes `address`,
            // once each ring has opened.
            if listed.contains(&late[0]) {
                listed.retain(|name| !late.contains(name));
                assert_eq!(listed, all, "at {after:?}");
            }
            let made: Vec<PathBuf> = if listed.is_empty() {
                Vec::new()
            } else {
                let held = listed.len() - 1;
                all[..held].iter().chain(&all[5..]).cloned().collect()
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
        assert_eq!(seen[3..], [true; 4], "a stop after each step");
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
        let made = ["lock", "name", "shard-0", "shards-3"].map(PathBuf::from);
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
            "address",
            "budget",
            "lock",
            "name",
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
                format!("cannot use the data directory: {text}")
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
        let made = [
            "address", "budget", "lock", "name", "node.key", "shard-0", "shard-1",
            &name, "shards-2",
        ];
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
            let mut made = [
                "address", "budget", "lock", "name", "node.key", "shard-0", "shard-1",
                &name, "shards-2",
            ];
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
        let config = block::Config::new(1 << 22).expect("the budget fits");
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
            budget: Size::from_bytes(1 << 22),
            cores: 1,
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
            let slot = interner.slots().index(index);
            interner.slots().data(values, Type::Scalar(Scalar::I64));
            let set = interner.intern(&[Group {
                index,
                data: &[(values, Type::Scalar(Scalar::I64))],
            }]);
            assert!(home.carry(slot, index));
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

    /// A read of the ring that fails at a restart over a handoff record fails the
    /// node with its error, in the open of the buffer and in the open of the home
    /// over it: the fault lands at each step of 25 us until the opens are done. Then
    /// the node runs a task.
    #[test]
    fn a_failed_read_at_a_restart_over_a_handoff_fails_the_node() {
        use super::hub::{I64, data, define, index, write, writer};

        let ring = Path::new("shard-0/ring");
        let error = Error::Buffer {
            core: 0,
            error: ::buffer::Error::Files(env::files::Error::Io {
                path: ring.to_owned(),
                operation: env::files::Operation::ReadAt,
                code: 5,
            }),
        };
        for after in (0..).step_by(25_000).map(Span::from_nanos) {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 1);
            let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
            node.spawn(move |hub| async move {
                define(&hub, &[("time", index(1)), ("value", data(2, I64, 1))]);
                let mut writer = writer(&hub, &["value"]).await;
                let stamp = writer.now().nanos();
                write(&mut writer, stamp, 7);
                std::future::pending::<()>().await;
            });
            assert_eq!(sim.run_for(Span::SECOND), Ok(()));
            sim.crash(&host, sim::Crash::Process);
            drop(node);
            let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
            let probe = probe(&node);
            assert_eq!(sim.run_for(after), Ok(()), "{after:?}");
            host.fail_file(ring, env::files::Operation::ReadAt);
            assert_eq!(sim.run_for(Span::SECOND), Ok(()), "{after:?}");
            node.stop();
            assert_eq!(sim.run(), Ok(()), "{after:?}");
            match node.join() {
                Ok(()) => {
                    assert_eq!(fate(&probe), Fate::Ran, "{after:?}");
                    break;
                }
                joined => assert_eq!(joined, Err(error.clone()), "{after:?}"),
            }
            assert!(after < Span::from_nanos(10_000_000), "the opens go on");
        }
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
            "cannot use the data directory: file lock is open for writing in \
             another handle"
        );
        first.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(first.join(), Ok(()));
        let made = [
            "address", "budget", "lock", "name", "node.key", "shard-0", "shard-1",
            "shards-2",
        ];
        assert_eq!(listed(&mut sim, &host, ""), made.map(PathBuf::from));
    }

    /// A second node at the port of a node that serves on the same data directory: the
    /// claim comes before the bind, so the data directory refuses it.
    #[test]
    fn a_second_node_at_the_port_of_a_running_node_is_refused_by_the_lock() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let first = start_on(&host, 0);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        let second = start_on(&host, 0);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        assert_eq!(second.join(), Err(busy()));
        first.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(first.join(), Ok(()));
    }

    /// A stop at any point of the start or while the node serves, then a second node
    /// at once at the same port: the stop holds the lock until the port is free, so
    /// each node serves or finds the lock held, one of them claims, and no port fails
    /// to bind.
    #[test]
    fn a_start_during_the_stop_of_a_node_at_its_port_never_fails_on_the_port() {
        let mut seen = [false; 2];
        let steps = (0..400).map(|step| Span::from_nanos(step * 5_000));
        for after in steps.chain([Span::SECOND]) {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            let first = start_on(&host, 0);
            assert_eq!(sim.run_for(after), Ok(()), "at {after:?}");
            first.stop();
            let second = start_on(&host, 0);
            assert_eq!(sim.run_for(Span::SECOND), Ok(()), "at {after:?}");
            second.stop();
            assert_eq!(sim.run(), Ok(()), "at {after:?}");
            let outcomes = [first, second].map(|node| {
                let joined = node.join();
                let outcome = [Ok(()), Err(busy())].iter().position(|o| *o == joined);
                outcome.unwrap_or_else(|| panic!("at {after:?}: {joined:?}"))
            });
            assert_ne!(outcomes, [1, 1], "at {after:?}");
            seen[outcomes[1]] = true;
        }
        assert_eq!(seen, [true; 2]);
    }

    /// Which of the stop of a node that serves and the start of a second node at its
    /// port comes first.
    #[derive(Clone, Copy)]
    enum Order {
        Stop,
        Start,
    }

    /// Runs a node that serves and a second node at its port on `seed`, with the
    /// second of `order` 5 µs times `step` after the first. Returns whether the second
    /// node served, and asserts that it gave `Err(busy())` when it did not.
    fn second_served(order: Order, seed: u64, step: i64) -> bool {
        let mut sim = sim::Sim::new(sim::Config {
            seed,
            ..sim::Config::default()
        });
        let host = host(&mut sim, 2);
        let first = start_on(&host, 0);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()), "seed {seed} at {step}");
        let delay = Span::from_nanos(step * 5_000);
        let (second, probe) = match order {
            Order::Stop => {
                first.stop();
                assert_eq!(sim.run_for(delay), Ok(()), "seed {seed} at {step}");
                let second = start_on(&host, 0);
                let probe = probe(&second);
                (second, probe)
            }
            Order::Start => {
                let second = start_on(&host, 0);
                let probe = probe(&second);
                assert_eq!(sim.run_for(delay), Ok(()), "seed {seed} at {step}");
                first.stop();
                (second, probe)
            }
        };
        assert_eq!(sim.run_for(Span::SECOND), Ok(()), "seed {seed} at {step}");
        second.stop();
        assert_eq!(sim.run(), Ok(()), "seed {seed} at {step}");
        assert_eq!(first.join(), Ok(()), "seed {seed} at {step}");
        let served = fate(&probe) == Fate::Ran;
        let expected = if served { Ok(()) } else { Err(busy()) };
        assert_eq!(second.join(), expected, "seed {seed} at {step}");
        served
    }

    /// A node that serves stops, and a second node at its port starts at a step of
    /// that stop, on seeds 0 to 3: the second node finds the lock held or serves, and
    /// never fails on the port. One seed can give one outcome at each step.
    #[test]
    fn a_start_at_each_step_of_the_stop_of_a_node_at_its_port_never_fails_on_the_port()
    {
        let mut seen = [false; 2];
        for (seed, step) in
            (0..4).flat_map(|seed| (0..400).step_by(4).map(move |step| (seed, step)))
        {
            seen[usize::from(second_served(Order::Stop, seed, step))] = true;
        }
        assert_eq!(seen, [true; 2]);
    }

    /// A second node starts at the port of a node that serves, and the first node
    /// stops at a step of that start, on seeds 0 to 3: the second node finds the lock
    /// held or serves, and never fails on the port.
    #[test]
    fn a_stop_at_each_step_of_a_start_at_the_port_of_a_node_never_fails_on_the_port() {
        let mut seen = [false; 2];
        for (seed, step) in
            (0..4).flat_map(|seed| (0..400).step_by(4).map(move |step| (seed, step)))
        {
            seen[usize::from(second_served(Order::Start, seed, step))] = true;
        }
        assert_eq!(seen, [true; 2]);
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
            let made = [
                "address", "budget", "lock", "name", "node.key", "shard-0", "shard-1",
                "shards-2",
            ];
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
                    let (lock, waited) = take_lock(&host).await;
                    let files = host.files();
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
        hub.set_definitions(
            definitions
                .iter()
                .map(|(name, definition)| (name, definition)),
        );
    }

    /// A writer on `channels`.
    pub(super) async fn writer(hub: &Hub, channels: &[&str]) -> Writer {
        let config = writer::Config {
            subject: name("a"),
            authority: Authority(1),
            lease: None,
            channels: channels.iter().map(|n| name(n)).collect(),
        };
        hub.writer(config).await.expect("the writer opens")
    }

    /// The position of channel `key` in `set`.
    pub(super) fn entry(set: &KeySet, key: u128) -> usize {
        let key = Key::from_u128(key);
        let entries = set.entries();
        entries.iter().position(|e| e.key == key).expect("an entry")
    }

    /// Writes one sample at `stamp` to `time` and `value` to `value`.
    pub(super) fn write(writer: &mut Writer, stamp: i64, value: i64) {
        write_at(writer, [1, 2], stamp, value);
    }

    /// Writes one sample at `stamp` to the index of key `time` and `value` to the
    /// data channel of key `data`.
    pub(super) fn write_at(
        writer: &mut Writer,
        [time, data]: [u128; 2],
        stamp: i64,
        value: i64,
    ) {
        let set = writer.set();
        let (time, data) = (entry(set, time), entry(set, data));
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
        let entry = entry(received.set(), key);
        let entries = received.set().entries();
        let range = received
            .view()
            .range(entries[entry].group)
            .expect("a range");
        let count = usize::try_from(range.count).expect("a count");
        let (_, bytes) = received
            .view()
            .iter()
            .find(|&(present, _)| present == entry)
            .expect("the view holds the series");
        let mut out = Vec::new();
        codec::decode(entries[entry].data_type, count, bytes, &mut out)
            .expect("decodes");
        let (chunks, _) = out.as_chunks::<8>();
        chunks.iter().map(|c| i64::from_le_bytes(*c)).collect()
    }

    /// Each sample of the `u8` series of `key` in `received`.
    pub(super) fn bytes(received: &Received<'_>, key: u128) -> Vec<u8> {
        let entry = entry(received.set(), key);
        let entries = received.set().entries();
        let range = received
            .view()
            .range(entries[entry].group)
            .expect("a range");
        let count = usize::try_from(range.count).expect("a count");
        let (_, bytes) = received
            .view()
            .iter()
            .find(|&(present, _)| present == entry)
            .expect("the view holds the series");
        let mut out = vec![0; count];
        codec::decode(entries[entry].data_type, count, bytes, &mut out)
            .expect("decodes");
        out
    }

    /// The seq of the first sample of the group of `key` in `received`.
    pub(super) fn seq(received: &Received<'_>, key: u128) -> u64 {
        let group = received.set().entries()[entry(received.set(), key)].group;
        received.view().range(group).expect("a range").seq
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
        let (_, node) = node(&mut sim, 2);
        let read = Arc::new(Mutex::new(None));
        let out = Arc::clone(&read);
        let probe = probe(&node);
        node.spawn(move |hub| async move {
            define(&hub, &[("time", index(1)), ("value", data(2, I64, 1))]);
            let reader = hub.reader(unnamed("value", Mode::Complete)).await;
            let mut reader = reader.expect("the reader opens");
            let mut writer = writer(&hub, &["value"]).await;
            let stamp = writer.now().nanos();
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
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) = true;
        }
    }

    #[test]
    fn a_dropped_with_a_poisoned_lock_does_not_abort_a_test_that_panics() {
        let flag = Arc::new(Mutex::new(false));
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = flag.lock().unwrap();
            panic!("poison the lock");
        }));
        poisoned.expect_err("the lock is poisoned");
        let dropped = Dropped(flag);
        let unwound =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _dropped = dropped;
                panic!("the test broke");
            }));
        unwound.expect_err("the test panics");
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
            let reader = hub.reader(unnamed("time", Mode::Complete)).await;
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
                    region: None,
                });
                define(&hub, &[("time", index(1)), ("value", data(2, I64, 1))]);
                let writer = writer(&hub, &["value"]).await;
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

mod pool {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ::hub::writer::Writer;
    use block::testing::{Scarce, Switch};
    use types::frame::{self, Form};
    use types::time::Monotonic;

    use super::hub::{I64, data, define, entry, index, writer};
    use super::*;

    const SECOND_NS: i64 = Span::SECOND.nanos();

    /// A shard gives the pages of a block size back to the system 10 to 20 s after
    /// the last block of that size returned. The last return falls at the open of
    /// the writer and at each of 1 to 9 s after the start. A return at the open also
    /// keeps its pages until 20 s after the start, before which the timer cannot
    /// purge twice.
    #[test]
    fn a_shard_gives_back_the_pages_of_an_idle_block_size_in_10_to_20_s() {
        for phase in 0..10 {
            let phase = Span::from_nanos(phase * SECOND_NS);
            let early = Span::from_nanos(10 * SECOND_NS - 1);
            let held = kept(phase, Since::Return, early);
            assert!(held, "the size keeps its pages at {phase:?}");
            let late = Span::from_nanos(20 * SECOND_NS + 1);
            let idle = !kept(phase, Since::Return, late);
            assert!(idle, "the pages went back at {phase:?}");
        }
        let early = Span::from_nanos(20 * SECOND_NS - 1);
        let held = kept(Span::ZERO, Since::Start, early);
        assert!(held, "the size keeps its pages until 20 s after the start");
    }

    /// The timer runs from before the open of the buffer, whose reads take blocks of
    /// 4 KiB. The timer starts less than 1 ms after the start of the node, so that
    /// size goes back by 20 s and 1 ms after it.
    #[test]
    fn a_size_that_the_open_of_the_buffer_used_goes_back_20_s_after_the_start() {
        let refused = frame::Error::Pool(block::Error::Refused { requested: 4056 });
        let early = probe(4000, Span::ZERO);
        assert_eq!(early, Ok(()), "the open of the buffer carved the size");
        let late = probe(4000, Span::from_nanos(20 * SECOND_NS + 1_000_000));
        assert_eq!(late, Err(refused), "the pages of the size went back");
    }

    /// The instant from which [`kept`] counts its wait.
    enum Since {
        /// The start of the node.
        Start,
        /// The return of the last block of the size.
        Return,
    }

    /// Whether a block size keeps its pages `wait` after the instant `since`. The last
    /// block of the size returns at `phase` after the start, or at the open of the
    /// writer when it comes later. The size keeps its pages when a take of its block
    /// succeeds while the memory refuses. The memory shows a purge only through its
    /// next commit: while it refuses, a commit of bytes that the system still holds
    /// succeeds, and a commit of purged bytes gives `Refused`.
    fn kept(phase: Span, since: Since, wait: Span) -> bool {
        let (new, carved, again, later) = on_writer(4096, async move |rig| {
            let phase = rig.start.checked_add(phase).expect("in range");
            rig.clock.sleep_until(phase).await;
            rig.switch.refuse();
            let new = rig.take();
            rig.switch.allow();
            let carved = rig.take();
            rig.switch.refuse();
            let again = rig.take();
            let base = match since {
                Since::Start => rig.start,
                Since::Return => rig.clock.now(),
            };
            rig.clock
                .sleep_until(base.checked_add(wait).expect("in range"))
                .await;
            let later = rig.take();
            rig.switch.allow();
            (new, carved, again, later)
        });
        let refused = frame::Error::Pool(block::Error::Refused { requested: 4152 });
        assert_eq!(new, Err(refused.clone()), "a new size needs a commit");
        assert_eq!(carved, Ok(()), "the pool carves the block");
        assert_eq!(again, Ok(()), "a carved size needs no commit");
        if let Err(error) = &later {
            assert_eq!(error, &refused, "the memory refused the take");
        }
        later.is_ok()
    }

    /// The take of a block for `bytes` of samples, at `at` after the start of the
    /// node, while the memory refuses.
    fn probe(bytes: usize, at: Span) -> Result<(), frame::Error> {
        on_writer(bytes, async move |rig| {
            let at = rig.start.checked_add(at).expect("in range");
            rig.clock.sleep_until(at).await;
            rig.switch.refuse();
            let taken = rig.take();
            rig.switch.allow();
            taken
        })
    }

    /// A writer on a node of one shard whose memory is `Scarce`.
    struct Rig {
        writer: Writer,
        series: [(usize, usize); 2],
        switch: Switch,
        clock: env::clock::Clock,
        start: Monotonic,
    }

    impl Rig {
        /// Takes a block for a frame of the rig's samples and drops it at once.
        fn take(&self) -> Result<(), frame::Error> {
            self.writer.draft(Form::Raw, &self.series).map(drop)
        }
    }

    /// Runs `task` on a writer of 8 bytes of times and `bytes` of values, on a node
    /// of one shard whose memory is `Scarce`. Returns its output after 40 s of the
    /// sim, and after the node stops.
    fn on_writer<T: Send + 'static>(
        bytes: usize,
        task: impl AsyncFnOnce(Rig) -> T + Send + 'static,
    ) -> T {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 1);
        let switch = Rc::new(RefCell::new(None));
        let made = Rc::clone(&switch);
        let memory = Box::new(move |len| {
            let (memory, switch) = Scarce::new(len);
            *made.borrow_mut() = Some(switch);
            Ok(memory)
        });
        let start = host.clock().now();
        let node = Node::start(config(&host, Size::MEBIBYTE, memory));
        let switch = switch.take().expect("the shard got its memory");
        let seen = Arc::new(Mutex::new(None));
        let (out, clock) = (Arc::clone(&seen), host.clock());
        node.spawn(move |hub| async move {
            define(&hub, &[("time", index(1)), ("value", data(2, I64, 1))]);
            let writer = writer(&hub, &["value"]).await;
            let (time, value) = (entry(writer.set(), 1), entry(writer.set(), 2));
            let mut series = [(time, 8), (value, bytes)];
            series.sort_unstable();
            let rig = Rig {
                writer,
                series,
                switch,
                clock,
                start,
            };
            *out.lock().unwrap() = Some(task(rig).await);
        });
        assert_eq!(sim.run_for(Span::from_nanos(40 * SECOND_NS)), Ok(()));
        let seen = seen.lock().unwrap().take();
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
        seen.expect("the task ran to its end")
    }
}

pub(crate) mod port {
    use std::num::NonZeroU32;
    use std::rc::Rc;

    use transport::{Address, Class, Code, Peer, Transport};

    use super::*;

    mod client;
    mod settings;

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
        let config = block::Config::new(1 << 20).expect("the budget fits");
        let memory = block::Heap::new(config.reservation());
        Rc::new(block::Pool::new(config, memory))
    }

    /// A part of a port of `host` at `at`, and a pool of 1 MiB to send on.
    fn port(
        host: &sim::node::Node,
        at: SocketAddr,
    ) -> (Rc<block::Pool>, transport::port::Part) {
        let pool = pool();
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
        let at = SocketAddr::new(host.addresses()[0], 0);
        transport_at(host, tasks, key, at)
    }

    /// A transport on `host` with `key`, at `at`, with its pool.
    fn transport_at(
        host: &sim::node::Node,
        tasks: env::tasks::Tasks,
        key: PrivateKey,
        at: SocketAddr,
    ) -> (Transport, Rc<block::Pool>) {
        let (pool, part) = port(host, at);
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

    /// Runs `act` on a new shard of `host`, with the host and the shard's tasks.
    fn shard<F: Future<Output = ()> + 'static>(
        host: &sim::node::Node,
        act: impl FnOnce(sim::node::Node, env::tasks::Tasks) -> F + Send + 'static,
    ) {
        let shard = env::shards::Config {
            name: "peer".into(),
            core: None,
        };
        let own = host.clone();
        let started = host.shards().start(shard, move |tasks| act(own, tasks));
        drop(started.expect("the peer starts"));
    }

    /// A program's transport on `host`, at a free port, with its pool.
    fn program(
        host: &sim::node::Node,
        tasks: env::tasks::Tasks,
    ) -> (transport::Client, Rc<block::Pool>) {
        let pool = pool();
        let config = transport::client::Config {
            net: host.net(),
            clock: host.clock(),
            entropy: host.entropy(),
            tasks,
            pool: Rc::clone(&pool),
        };
        (transport::Client::new(config).expect("a client"), pool)
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
            let message = |bytes: &[u8]| pool.copy(bytes).unwrap();
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

    /// A node with no region has no store, so it stops a blob stream of a node, and
    /// resets its reply half, with the code of a rejected header.
    #[test]
    fn rejects_a_blob_stream_of_a_node_with_no_region() {
        let header = wire::header::encode(wire::Protocol::Blob);
        let Seen { sent, read, .. } =
            sees(Dialer::Node(CLIENT), &[&header], Size::MEBIBYTE);
        let code = Code(wire::header::REJECTED);
        assert_eq!(sent, transport::Error::Stopped { code });
        assert_eq!(read, Err(transport::Error::Reset { code }));
    }

    /// A node with no region stops a hub stream of a program, and resets its reply
    /// half, with the code of a rejected header.
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
            budget: Budget {
                pool: Size::MEBIBYTE,
                disk,
            },
            ..config(&host, Size::MEBIBYTE, Box::new(heap))
        });
        assert_eq!(sim.run(), Ok(()));
        let min = Size::from_bytes(8_626_176);
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

    /// The pool check comes before the bind, which shard 0 does after its claim.
    #[test]
    fn a_pool_reservation_past_usize_max_wins_over_a_port_that_does_not_bind() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 1);
        let listen = listen(&host);
        let udp = env::net::udp::Config {
            local: listen,
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 16,
        };
        let held = host.net().udp(&udp).expect("the port binds");
        let pool = Size::from_bytes(u64::MAX);
        let node = Node::start(config(&host, pool, Box::new(heap)));
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Err(Error::Pool { pool, cores: 1 }));
        drop(held);
    }

    /// A port that does not bind stops the node once shard 0 claimed the data
    /// directory, and no buffer opens. A later start there at a free port serves.
    #[test]
    fn a_port_that_does_not_bind_opens_no_buffer() {
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
        let claimed = ["lock", "name", "shards-2"].map(PathBuf::from);
        assert_eq!(listed(&mut sim, &host, ""), claimed);
        assert_eq!(
            error.to_string(),
            format!(
                "cannot bind the node's port at {listen}: address {listen} is in use"
            )
        );
        drop(held);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        let probe = probe(&node);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
        assert_eq!(fate(&probe), Fate::Ran);
    }

    /// With a file `name`, a held port gives [`Error::Port`] before the load reads a
    /// `node.key` that a node did not write.
    #[test]
    fn a_port_that_does_not_bind_wins_over_a_node_key_that_is_not_a_key() {
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
        assert!(matches!(node.join(), Err(Error::Port { .. })));
        write_key(&mut sim, &host, vec![0; identity::LEN]);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        assert_eq!(sim.run(), Ok(()));
        assert!(matches!(node.join(), Err(Error::Port { .. })));
        drop(held);
    }

    /// A multicast IP or the IPv4 broadcast IP, also as an IPv4-mapped IPv6 address,
    /// starts no shard and makes no file.
    #[test]
    fn a_listen_that_no_peer_reaches_starts_no_shard() {
        let refused = [
            "224.0.0.1:0",
            "239.255.255.255:7000",
            "255.255.255.255:0",
            "[ff02::1]:0",
            "[ff0e::1]:7000",
            "[::ffff:224.0.0.1]:0",
            "[::ffff:255.255.255.255]:0",
        ];
        for text in refused {
            let listen: SocketAddr = text.parse().expect("a socket address");
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            let node = Node::start(Config {
                listen,
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Err(Error::Listen { listen }), "{text}");
            assert_eq!(host.shard_starts().len(), 0, "{text}");
            assert_eq!(listed(&mut sim, &host, ""), Vec::<PathBuf>::new(), "{text}");
        }
        let listen = "[::ffff:224.0.0.1]:0".parse().expect("a socket address");
        assert_eq!(
            Error::Listen { listen }.to_string(),
            "cannot bind the node's port at [::ffff:224.0.0.1]:0: a multicast or \
             broadcast IP is not an address of a host"
        );
    }

    /// An IP next to the refused ones goes to the bind.
    #[test]
    fn a_listen_next_to_a_refused_ip_goes_to_the_bind() {
        let bound = [
            "10.255.255.255:0",
            "192.168.1.255:0",
            "223.255.255.255:0",
            "240.0.0.0:0",
            "255.255.255.254:0",
            "[feff::1]:0",
            "[::ffff:223.255.255.255]:0",
            "[::ffff:240.0.0.0]:0",
        ];
        for text in bound {
            let listen: SocketAddr = text.parse().expect("a socket address");
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            let node = Node::start(Config {
                listen,
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            assert_eq!(sim.run(), Ok(()));
            let error = env::net::Error::Io { code: 99 };
            assert_eq!(node.join(), Err(Error::Port { listen, error }), "{text}");
        }
    }

    /// The refused listen comes after a reserved name and before a disk budget that
    /// holds no ring.
    #[test]
    fn a_refused_listen_comes_between_a_reserved_name_and_a_small_disk() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let listen = "224.0.0.1:0".parse().expect("a socket address");
        let small = || Config {
            listen,
            budget: Budget {
                pool: Size::MEBIBYTE,
                disk: Size::from_bytes(1),
            },
            ..config(&host, Size::MEBIBYTE, Box::new(heap))
        };
        let reserved: types::name::Name = "@x".parse().expect("a name");
        let named = Node::start(Config {
            name: reserved.clone(),
            ..small()
        });
        let unnamed = Node::start(small());
        assert_eq!(sim.run(), Ok(()));
        let name = reserved;
        assert_eq!(named.join(), Err(Error::Reserved { name }));
        assert_eq!(unnamed.join(), Err(Error::Listen { listen }));
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
            let message = |bytes: &[u8]| pool.copy(bytes).unwrap();
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

    /// The node rejects a stream whose header has not arrived 10 s after the stream
    /// did. On a link of 4 KB/s, a first message of 60 KiB takes about 16 s, so the
    /// stream stops, and its reply half resets, before the message arrives.
    #[test]
    fn a_header_late_by_10_s_is_rejected() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        let peer = sim.node(sim::node::Config::default());
        let slow = sim::link::Config {
            rate: Some(std::num::NonZeroU64::new(4_000).unwrap()),
            ..sim::link::Config::default()
        };
        sim.link(&peer, &host, slow);
        let listen = listen(&host);
        let out = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&out);
        let shard = env::shards::Config {
            name: "peer".into(),
            core: None,
        };
        let own = peer.clone();
        let started = peer.shards().start(shard, move |tasks| async move {
            let (transport, pool) = transport(&own, tasks, CLIENT);
            let session = transport
                .dial(KEY.public(), &[Address::Udp(listen)])
                .await
                .expect("a session");
            let mut late = wire::header::encode(wire::Protocol::Mesh).to_vec();
            late.resize(60 << 10, 0);
            let block = pool.copy(&late).unwrap();
            let (mut sender, mut reply) =
                session.open(Class::Complete).await.expect("a stream");
            let start = own.clock().now();
            sender.send(block).await.expect("it sends");
            let read = reply.recv().await.map(|m| m.map(|b| b.to_vec()));
            let waited = own.clock().now() - start;
            let sent = loop {
                let block = pool.copy(b"a").unwrap();
                if let Err(error) = sender.send(block).await {
                    break error;
                }
                own.clock().sleep(Span::MILLISECOND).await;
            };
            *seen.lock().unwrap() = Some((waited, read, sent));
            session.closed().await;
            drop((sender, transport));
        });
        drop(started.expect("the peer starts"));
        assert_eq!(sim.run_for(Span::MINUTE), Ok(()));
        let (waited, read, sent) = out.lock().unwrap().take().expect("the peer reads");
        let code = Code(wire::header::REJECTED);
        assert_eq!(read, Err(transport::Error::Reset { code }));
        assert_eq!(sent, transport::Error::Stopped { code });
        let (header, bound) = (10 * Span::SECOND.nanos(), 11 * Span::SECOND.nanos());
        assert!(
            (header..bound).contains(&waited.nanos()),
            "reset after {waited:?}"
        );
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
    }

    /// What the peer and the probe of [`stop_with_a_peer`] see.
    #[derive(Clone, Debug, PartialEq)]
    enum Event {
        /// The peer's session closed with this error.
        Closed(transport::Error),
        /// The probe took the lock, then bound the node's port with this result.
        Locked(Result<(), env::net::Error>),
    }

    /// The node drains a peer's session before it frees the lock: the peer sees its
    /// close before a probe can take the lock, and the probe then binds the node's
    /// port at once.
    #[test]
    fn a_peer_sees_its_close_before_the_lock_is_free() {
        let closed = transport::Error::PeerClosed { code: Code(0) };
        let delay = Span::from_nanos(10 * Span::MILLISECOND.nanos());
        let (seen, _) =
            stop_with_a_peer(delay, Span::from_nanos(30 * Span::SECOND.nanos()));
        assert_eq!(seen, [Event::Closed(closed), Event::Locked(Ok(()))]);
    }

    /// The node waits at most 3 s for the drain, so a probe takes the lock before a
    /// peer with a one-way delay of 4 s sees its close. The close is not paced, so it
    /// leaves at the drop and arrives one delay later, also when the node stops 12.1 s
    /// in, when a paced close would wait about 0.57 s.
    #[test]
    fn a_peer_with_a_delay_over_3_s_sees_its_close_after_the_lock_is_free() {
        let closed = transport::Error::PeerClosed { code: Code(0) };
        let delay = Span::from_nanos(4 * Span::SECOND.nanos());
        let run = Span::from_nanos(12_100 * Span::MILLISECOND.nanos());
        let (seen, after) = stop_with_a_peer(delay, run);
        assert_eq!(seen, [Event::Locked(Ok(())), Event::Closed(closed)]);
        let late = after.nanos() - delay.nanos();
        assert!(
            (0..Span::MILLISECOND.nanos()).contains(&late),
            "the peer saw its close {after:?} after the stop, not {delay:?}"
        );
    }

    /// A peer whose one-way delay is under 3 s sees its close first, also when the
    /// node stops 14.7 s in, when the pacer of the connection would hold the close
    /// until after the lock is free.
    #[test]
    fn a_peer_with_a_delay_under_3_s_sees_its_close_first_after_the_handshake() {
        let closed = transport::Error::PeerClosed { code: Code(0) };
        let delay = Span::from_nanos(2_900 * Span::MILLISECOND.nanos());
        let run = Span::from_nanos(14_700 * Span::MILLISECOND.nanos());
        let (seen, _) = stop_with_a_peer(delay, run);
        assert_eq!(seen, [Event::Closed(closed), Event::Locked(Ok(()))]);
    }

    /// Stops a node `run` after its start, while a peer holds a session over links
    /// with a one-way `delay`. Gives, in order, what the peer and a probe that takes
    /// the lock as soon as it is free see, and the time from the stop to the peer's
    /// close.
    fn stop_with_a_peer(delay: Span, run: Span) -> (Vec<Event>, Span) {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        let peer = sim.node(sim::node::Config::default());
        let link = sim::link::Config {
            delay,
            ..sim::link::Config::default()
        };
        sim.link(&peer, &host, link);
        sim.link(&host, &peer, link);
        let listen = listen(&host);
        let events = Arc::new(Mutex::new(Vec::new()));
        let at = Arc::new(Mutex::new(None));
        let (seen, closed_at) = (Arc::clone(&events), Arc::clone(&at));
        let shard = env::shards::Config {
            name: "peer".into(),
            core: None,
        };
        let own = peer.clone();
        let started = peer.shards().start(shard, move |tasks| async move {
            let (transport, _pool) = transport(&own, tasks, CLIENT);
            let session = transport
                .dial(KEY.public(), &[Address::Udp(listen)])
                .await
                .expect("a session");
            let closed = session.closed().await;
            *closed_at.lock().unwrap() = Some(own.clock().now());
            seen.lock().unwrap().push(Event::Closed(closed));
            drop(transport);
        });
        drop(started.expect("the peer starts"));
        assert_eq!(sim.run_for(run), Ok(()));
        let stopped = peer.clock().now();
        node.stop();
        let seen = Arc::clone(&events);
        sim.run_on(&host, move |host, _| async move {
            let (lock, _) = take_lock(&host).await;
            let bound = transport::Port::bind(&host.net(), listen).map(drop);
            seen.lock().unwrap().push(Event::Locked(bound));
            drop(lock);
        })
        .expect("the probe ends");
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
        let closed = at.lock().unwrap().expect("the peer's session closed");
        (events.lock().unwrap().clone(), closed - stopped)
    }

    /// A program with no key starts a handshake that gets no answer, and sends its
    /// first packet again on each PTO, which keeps the handshake from the idle time.
    /// The stop holds the lock for the cut of 3 s, which ends the drain of that
    /// handshake before its 3 PTO of 1024 ms, and less than 1 ms for its other steps.
    #[test]
    fn a_handshake_in_flight_holds_the_lock_at_most_3_s() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
        let peer = sim.node(sim::node::Config::default());
        let cut = sim::link::Config {
            loss: 1.0,
            ..sim::link::Config::default()
        };
        sim.link(&host, &peer, cut);
        let listen = listen(&host);
        let shard = env::shards::Config {
            name: "peer".into(),
            core: None,
        };
        let own = peer.clone();
        let started = peer.shards().start(shard, move |tasks| async move {
            let (client, _pool) = program(&own, tasks);
            let dialed = client.dial(KEY.public(), &[Address::Udp(listen)]).await;
            let refused = dialed.expect_err("the dial fails");
            assert!(
                matches!(refused, transport::Error::Unreachable { .. }),
                "{refused:?}"
            );
        });
        drop(started.expect("the peer starts"));
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        node.stop();
        let waited = sim
            .run_on(&host, move |host, _| async move {
                let before = host.clock().now();
                drop(take_lock(&host).await);
                host.clock().now() - before
            })
            .expect("the probe ends");
        let bound = Span::from_nanos(3001 * Span::MILLISECOND.nanos());
        assert!(waited <= bound, "the stop held the lock {waited:?}");
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
    }

    mod key {
        use types::node::SealKey;

        use super::*;
        use crate::identity::{FILE, LEN};

        /// The file that `sector::publish` renames to [`FILE`].
        const NEW: &str = "node.key.new";

        /// Through the public fuzz entry, which panics when the decode is wrong.
        #[test]
        fn a_key_decodes_and_a_key_with_another_tag_does_not() {
            let mut body = [7; 96];
            body[..16].copy_from_slice(b"foundation/key/2");
            crate::fuzz::identity(&body);
            body[0] ^= 1;
            crate::fuzz::identity(&body);
        }

        /// The bytes of `node.key` on `host`.
        fn read(sim: &mut sim::Sim, host: &sim::node::Node) -> Vec<u8> {
            sim.run_on(host, |host, _| async move {
                let files = host.files();
                let mode = env::files::Mode::Read;
                let file = files.open(Path::new(FILE), mode).await.expect("opens");
                let len = usize::try_from(file.len()).unwrap();
                let pool = block::Pool::heap(
                    block::Config::new(4096).expect("the budget fits"),
                );
                let into = pool.alloc(len).expect("a block");
                file.read_at(0, into).await.expect("reads").to_vec()
            })
            .expect("the run ends")
        }

        /// The identity in `node.key` on `host`, or `None` when the file is not
        /// there. Opens it to read only, so it makes nothing. It checks the file
        /// between a crash and a start, where no public call reads it.
        ///
        /// # Panics
        ///
        /// When the file holds no key.
        fn found(sim: &mut sim::Sim, host: &sim::node::Node) -> Option<Vec<u8>> {
            let bytes = sim
                .run_on(host, |host, _| async move {
                    let mode = env::files::Mode::Read;
                    let file = match host.files().open(Path::new(FILE), mode).await {
                        Err(env::files::Error::NotFound { .. }) => return None,
                        file => file.expect("opens"),
                    };
                    assert_eq!(file.len(), LEN as u64, "the length of a key");
                    let pool = block::Pool::heap(
                        block::Config::new(4096).expect("the budget fits"),
                    );
                    let into = pool.alloc(LEN).expect("a block");
                    let read = file.read_at(0, into).await.expect("reads");
                    Some(<[u8; LEN]>::try_from(&*read).expect("a read fills it"))
                })
                .expect("the run ends")?;
            let tag = b"foundation/key/2";
            assert!(sector::written(&bytes, tag), "a key: {bytes:02x?}");
            Some(bytes.to_vec())
        }

        /// The bytes of the identity [`OWN`], [`KEY`].
        fn own() -> Vec<u8> {
            let identity = Identity {
                key: OWN,
                private_key: KEY,
                opener: Opener::from_bytes(SEAL),
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
            assert_eq!(&made[..16], b"foundation/key/2");
            let key = u128::from_be_bytes(made[16..32].try_into().unwrap());
            assert_eq!(key >> 76 & 0xf, 7, "version 7");
            let millis = i64::try_from(key >> 80).unwrap();
            let wall = super::super::hub::WALL / 1_000_000;
            assert!((wall..wall + 1_000).contains(&millis), "{millis} at {wall}");
            assert_ne!(made[64..96], made[32..64], "the seal key is its own");
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
            assert_ne!(made[0][64..96], made[1][64..96], "seal keys");
        }

        /// A node started on the `node.key` `bytes` proves the public half of
        /// `private_key` and keeps the bytes.
        fn proves(bytes: &[u8], private_key: &PrivateKey) {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            write_key(&mut sim, &host, bytes.to_vec());
            let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
            let public = private_key.public();
            assert_eq!(dial(&mut sim, &host, public), Ok(Peer::Node(public)));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            assert_eq!(read(&mut sim, &host), bytes);
        }

        #[test]
        fn a_node_proves_the_key_in_its_data_directory() {
            proves(&own(), &KEY);
        }

        /// Each input of the fuzz corpus that decoded in the form `foundation/key/1`
        /// has a copy in the new form: `valid-2`, which a node proves and keeps, and
        /// `valid-body-2`, which with its CRC32C is `valid-2`.
        #[test]
        fn a_node_proves_the_key_of_each_valid_input_of_the_corpus() {
            let valid: &[u8; LEN] =
                include_bytes!("../../../oracles/fuzz/node_identity/valid-2");
            let body =
                include_bytes!("../../../oracles/fuzz/node_identity/valid-body-2");
            assert_eq!(sector::summed(body).as_ref(), Some(valid));
            proves(valid, &PrivateKey([7; 32]));
        }

        /// The node never leaves a `node.key` with no bytes or with 100 zero bytes, so
        /// such a file stops the node, which keeps it.
        #[test]
        fn a_zero_or_empty_key_stops_the_node() {
            for bytes in [Vec::new(), vec![0; LEN]] {
                let mut sim = sim::Sim::new(sim::Config::default());
                let host = host(&mut sim, 2);
                write(&mut sim, &host, FILE, bytes.clone());
                assert_eq!(start_and_stop(&mut sim, &host), Err(Error::Key));
                assert_eq!(read(&mut sim, &host), bytes, "keeps the file");
            }
        }

        /// A start removes the file `node.key.new` that a crash left, of any length,
        /// and makes the key.
        #[test]
        fn a_start_removes_a_new_key_that_a_crash_left() {
            for bytes in [Vec::new(), vec![7; LEN], vec![7; 3]] {
                let mut sim = sim::Sim::new(sim::Config::default());
                let host = host(&mut sim, 2);
                write(&mut sim, &host, NEW, bytes);
                assert_eq!(start_and_stop(&mut sim, &host), Ok(()));
                assert!(found(&mut sim, &host).is_some(), "a key");
                assert!(!listed(&mut sim, &host, "").contains(&PathBuf::from(NEW)));
            }
        }

        /// A crash every 25 µs of the first start, on seeds 0 to 7, leaves no key or a
        /// whole one, and a start after a crash that left one proves it.
        #[test]
        fn a_crash_during_the_first_start_leaves_no_key_or_a_whole_key() {
            let crashes = [sim::Crash::Process, sim::Crash::Power];
            for (seed, crash) in
                (0..8).flat_map(|seed| crashes.map(|crash| (seed, crash)))
            {
                for after in (0..).step_by(25_000).map(Span::from_nanos) {
                    let at = format!("seed {seed}: {crash:?} at {after:?}");
                    let mut sim = sim::Sim::new(sim::Config {
                        seed,
                        ..sim::Config::default()
                    });
                    let host = host(&mut sim, 2);
                    let node =
                        Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
                    assert_eq!(sim.run_for(after), Ok(()), "{at}");
                    sim.crash(&host, crash);
                    drop(node);
                    let Some(made) = found(&mut sim, &host) else {
                        continue;
                    };
                    let public = PrivateKey(made[32..64].try_into().unwrap()).public();
                    let node =
                        Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
                    let dialed = dial(&mut sim, &host, public);
                    assert_eq!(dialed, Ok(Peer::Node(public)), "{at}");
                    node.stop();
                    assert_eq!(sim.run(), Ok(()), "{at}");
                    assert_eq!(node.join(), Ok(()), "{at}");
                    break;
                }
            }
        }

        /// A file of another length, or of another tag or checksum, stops the node,
        /// which keeps the file.
        #[test]
        fn a_key_that_is_not_valid_stops_the_node() {
            // Each tag byte with one bit changed, and the tag of the next form.
            let edits = (0..16).map(|i| (i, own()[i] ^ 1)).chain([(15, b'3')]);
            let tags = edits.map(|(i, byte)| {
                let mut tag = <[u8; LEN]>::try_from(own()).unwrap();
                tag[i] = byte;
                sector::checksum(&mut tag);
                tag.to_vec()
            });
            let mut changed = own();
            changed[40] ^= 1;
            let short = own()[..LEN - 1].to_vec();
            let long = [own(), vec![0]].concat();
            // The form before the seal key: the tag `foundation/key/1`, the node key,
            // and the private key.
            let old = [b"foundation/key/1".as_slice(), &own()[16..64]].concat();
            let old = sector::summed::<68>(&old).unwrap().to_vec();
            for bytes in [short, long, changed, old].into_iter().chain(tags) {
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
                "the file `node.key` in the data directory is not a node key; restore \
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
        ) -> Result<SealKey, Error> {
            sim.run_on(host, move |host, _| async move {
                crate::create_key(&host.files(), &host.entropy(), OWN, private_key)
                    .await
            })
            .expect("the run ends")
        }

        /// Checks that `made` holds [`OWN`], [`KEY`], and the private half of
        /// `seal_key`, with the tag and the checksum.
        fn assert_created(made: &[u8], seal_key: SealKey) {
            let made: &[u8; LEN] = made.try_into().expect("the length of a key");
            let opener = Opener::from_bytes(made[64..96].try_into().unwrap());
            assert_eq!(opener.public(), seal_key, "the seal key");
            let identity = Identity {
                key: OWN,
                private_key: KEY,
                opener,
            };
            assert_eq!(identity::encode(&identity), *made);
        }

        /// Each start proves the key that `create_key` writes, and keeps its seal key.
        #[test]
        fn a_node_proves_the_key_that_create_key_writes() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            let seal_key = create(&mut sim, &host, KEY).expect("creates");
            let made = read(&mut sim, &host);
            assert_created(&made, seal_key);
            let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
            let public = KEY.public();
            assert_eq!(dial(&mut sim, &host, public), Ok(Peer::Node(public)));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            assert_eq!(read(&mut sim, &host), made);
        }

        /// Two calls of `create_key` on two hosts make two seal keys.
        #[test]
        fn create_key_makes_a_seal_key_from_the_entropy() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [host(&mut sim, 1), host(&mut sim, 1)];
            let made = hosts.map(|host| create(&mut sim, &host, KEY).expect("creates"));
            assert_ne!(made[0], made[1]);
        }

        /// `create_key` writes nothing over a file that is there: a key, valid or
        /// not, a file of another length, and one with no bytes or 100 zero bytes. It
        /// leaves no `node.key.new`, which would hold its keys.
        #[test]
        fn create_key_writes_nothing_over_a_file() {
            let mut changed = own();
            changed[40] ^= 1;
            let short = own()[..LEN - 1].to_vec();
            let exists = env::files::Error::Exists {
                path: PathBuf::from("node.key"),
            };
            for bytes in [own(), changed, short, Vec::new(), vec![0; LEN]] {
                let mut sim = sim::Sim::new(sim::Config::default());
                let host = host(&mut sim, 2);
                write(&mut sim, &host, FILE, bytes.clone());
                let created = create(&mut sim, &host, PrivateKey([9; 32]));
                assert_eq!(created, Err(Error::Directory(exists.clone())));
                assert_eq!(read(&mut sim, &host), bytes, "keeps the file");
                let staged = PathBuf::from(NEW);
                assert!(
                    !listed(&mut sim, &host, "").contains(&staged),
                    "no staged key"
                );
            }
            assert_eq!(
                Error::Directory(exists).to_string(),
                "cannot use the data directory: path node.key is already there"
            );
        }

        /// A power cut after `create_key` finds a file leaves no `node.key.new` with
        /// its keys, also after a start.
        #[test]
        fn a_power_cut_after_create_key_finds_a_file_leaves_no_staged_key() {
            for seed in 0..32 {
                let mut sim = sim::Sim::new(sim::Config {
                    seed,
                    ..sim::Config::default()
                });
                let host = host(&mut sim, 2);
                write(&mut sim, &host, FILE, own());
                let created = create(&mut sim, &host, PrivateKey([9; 32]));
                let exists = env::files::Error::Exists {
                    path: PathBuf::from("node.key"),
                };
                assert_eq!(created, Err(Error::Directory(exists)), "seed {seed}");
                sim.crash(&host, sim::Crash::Power);
                assert_eq!(start_and_stop(&mut sim, &host), Ok(()), "seed {seed}");
                assert_eq!(bytes(&mut sim, &host, NEW), None, "seed {seed}");
                assert_eq!(bytes(&mut sim, &host, FILE), Some(own()), "seed {seed}");
            }
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
                let seal_key = create(&mut sim, &host, KEY).expect("creates");
                sim.crash(&host, sim::Crash::Power);
                assert_created(&read(&mut sim, &host), seal_key);
            }
        }

        /// A failed file call of `create_key` gives its error, and leaves no
        /// `node.key`.
        #[test]
        fn a_failed_file_call_of_create_key_gives_its_error() {
            use env::files::Operation::{Open, Remove, Rename, Sync, SyncDir, WriteAt};
            let calls = [Remove, Open, WriteAt, Sync, Rename].map(|call| (NEW, call));
            let others = [(FILE, Open), ("", SyncDir)];
            for (path, operation) in calls.into_iter().chain(others) {
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
                if operation != SyncDir {
                    assert_eq!(found(&mut sim, &host), None, "{operation:?}");
                }
            }
        }

        /// A failed file call on `node.key`, or on the `node.key.new` that makes it,
        /// stops the node, and the next start proves a key. A failed read keeps the
        /// key that the file holds.
        #[test]
        fn a_failed_file_call_on_the_key_stops_the_node() {
            use env::files::Operation::{Open, ReadAt, Remove, Rename, Sync, WriteAt};
            for (path, operation) in [
                (NEW, Remove),
                (NEW, Open),
                (NEW, WriteAt),
                (NEW, Sync),
                (NEW, Rename),
                (FILE, Open),
                (FILE, ReadAt),
            ] {
                let mut sim = sim::Sim::new(sim::Config::default());
                let host = host(&mut sim, 2);
                if path == FILE {
                    write_key(&mut sim, &host, own());
                }
                host.fail_file(Path::new(path), operation);
                let error = env::files::Error::Io {
                    path: PathBuf::from(path),
                    operation,
                    code: 5,
                };
                assert_eq!(
                    start_and_stop(&mut sim, &host),
                    Err(Error::Directory(error)),
                    "{operation:?} {path}"
                );
                assert_eq!(start_and_stop(&mut sim, &host), Ok(()), "{operation:?}");
                let made = found(&mut sim, &host).expect("a key");
                if path == FILE {
                    assert_eq!(made, own(), "{operation:?} {path}");
                }
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
                node.join() == error && found(&mut sim, &host).is_some()
            });
            assert!(stopped, "no start stopped at the sync of the directory");
        }

        /// A process crash can leave a key that a read sees but a power cut loses.
        /// The claim of the next start makes it durable, also when the start fails
        /// after it. Takes the first crash time, on seeds 0 to 7, at which a read sees
        /// the key, and checks that a power cut then can lose it.
        #[test]
        fn a_later_start_makes_the_key_durable() {
            let mut lost = 0;
            let crashed = |seed, after| {
                let mut sim = sim::Sim::new(sim::Config {
                    seed,
                    ..sim::Config::default()
                });
                let host = host(&mut sim, 2);
                let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
                assert_eq!(sim.run_for(after), Ok(()), "seed {seed} at {after:?}");
                sim.crash(&host, sim::Crash::Process);
                drop(node);
                let made = found(&mut sim, &host);
                (sim, host, made)
            };
            for seed in 0..8 {
                let (after, made) = (0..)
                    .step_by(5_000)
                    .map(Span::from_nanos)
                    .find_map(|after| crashed(seed, after).2.map(|made| (after, made)))
                    .expect("a crash leaves the key");
                let (mut sim, host, _) = crashed(seed, after);
                sim.crash(&host, sim::Crash::Power);
                if found(&mut sim, &host).is_none() {
                    lost += 1;
                }
                let (mut sim, host, _) = crashed(seed, after);
                let operation = env::files::Operation::Open;
                host.fail_file(Path::new(FILE), operation);
                let error = env::files::Error::Io {
                    path: PathBuf::from(FILE),
                    operation,
                    code: 5,
                };
                let started = start_and_stop(&mut sim, &host);
                let at = format!("seed {seed} at {after:?}");
                assert_eq!(started, Err(Error::Directory(error)), "{at}");
                sim.crash(&host, sim::Crash::Power);
                assert_eq!(found(&mut sim, &host), Some(made), "{at}");
            }
            assert!(lost > 0, "no power cut lost the key");
        }
    }

    pub(crate) mod mesh {
        use std::cell::{Cell, RefCell};
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
        use types::name::Name;
        use types::node::SealKey;

        use super::*;
        use crate::{Endpoint, Opening, route};

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
        /// A time by which the mesh of a node opens and its group leads, with no write
        /// of the log left.
        const OPEN: Span = Span::from_nanos(10_000_000);
        /// The time after [`OPEN`], in nanoseconds, at which the failed write of
        /// [`fail_write_at_open`] stops the group in the sim.
        const WRITE: i64 = 79_055;

        /// Why the group stops when a write of [`LOG`] fails.
        fn write_failed() -> ::mesh::Stopped {
            ::mesh::Stopped::Write(::mesh::log::Error::Files(env::files::Error::Io {
                path: PathBuf::from(LOG),
                operation: env::files::Operation::WriteAt,
                code: 5,
            }))
        }

        /// At [`OPEN`], fails the next write of [`LOG`] on `host`, then sets the home
        /// of a channel other than [`INDEX`] in the mesh of `node`, which makes that
        /// write.
        fn fail_write_at_open(node: &Node, host: &sim::node::Node) {
            let at = sim::node::Config::default().monotonic + OPEN;
            let own = host.clone();
            node.operate(move |ops| async move {
                own.clock().sleep_until(at).await;
                own.fail_file(Path::new(LOG), env::files::Operation::WriteAt);
                let set = ops.mesh().set_home(channel::Key::from_u128(8), OWN).await;
                assert_eq!(set, Err(::mesh::Error::Stopped(write_failed())));
            });
        }

        /// The member `key` with `private_key`, whose node listens on `host`.
        pub(crate) fn member(
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
        pub(crate) fn region(members: &[Member]) -> Founding {
            Founding {
                prefix: "plant".parse().unwrap(),
                members: members.to_vec(),
                voters: members.iter().map(|member| member.card.key()).collect(),
                definitions: BTreeMap::new(),
                homes: BTreeMap::new(),
            }
        }

        /// What [`open`] opens.
        pub(crate) struct Opened {
            pub(crate) hub: ::hub::Hub,
            pub(crate) mesh: ::mesh::Mesh,
            pub(crate) store: Rc<blob::Store>,
            pub(crate) transport: Rc<transport::Transport>,
            pub(crate) time: clock::Reader,
            pub(crate) entropy: env::entropy::Entropy,
        }

        /// Opens the endpoint of `identity` at [`listen`] on `host` in the region
        /// `founding`, and a hub on a new buffer of `host` with its mesh.
        pub(crate) async fn open(
            host: &sim::node::Node,
            tasks: &env::tasks::Tasks,
            identity: Identity,
            founding: Founding,
        ) -> Opened {
            let bound =
                transport::Port::bind(&host.net(), listen(host)).expect("a port");
            let (opening, clock) = (Opening::Given(founding), host.clock());
            let (entropy, settings) = (host.entropy(), crate::Settings::default());
            let endpoint =
                Endpoint::new(bound, opening, clock, entropy, host.net(), settings);
            let stop = crate::stop::Stop::default();
            let (open, buffer, given, time) =
                super::super::home::create_open(host, tasks, 0, stop);
            let opened = open.run(host.files(), Rc::new(buffer), tasks.clone()).await;
            let home = opened.expect("the buffer opens");
            let interner = given.await.expect("the open gives the interner");
            let (key, entropy) = (identity.key, endpoint.entropy.clone());
            let opened = endpoint.open(
                identity,
                host.files(),
                super::pool(),
                &time,
                tasks.clone(),
            );
            let (transport, mesh) = opened.await.expect("the transport opens");
            let joined = mesh.expect("the mesh opens").expect("a region");
            let (mesh, store) = (joined.mesh, joined.store);
            let hub = ::hub::Hub::new(::hub::Config {
                home,
                interner,
                tasks: tasks.clone(),
                node: key,
                time: time.clone(),
                entropy: entropy.clone(),
                region: Some(::hub::Region {
                    mesh: mesh.clone(),
                    transport: Rc::clone(&transport),
                }),
            });
            Opened {
                hub,
                mesh,
                store,
                transport,
                time,
                entropy,
            }
        }

        /// Starts the node on `host`, which [`keyed`] made, with `region`.
        fn start(host: &sim::node::Node, region: Founding) -> Node {
            Node::start(Config {
                region: Region::Given(region),
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
            node.spawn(move |hub| async move {
                let reader = hub
                    .reader(unnamed("plant.value", ::hub::reader::Mode::Complete))
                    .await;
                let mut reader = reader.expect("the reader opens");
                let mut writer = writer(&hub, &["plant.value"]).await;
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

        /// A founding with a data channel whose index the spec does not hold has
        /// problems, so the node runs and defines no channel.
        #[test]
        fn a_founding_with_a_dangling_index_defines_no_channel() {
            use super::super::hub::{I64, data};
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let mut founding = region(&[member(OWN, &KEY, &host)]);
            let value: Name = "plant.value".parse().unwrap();
            let channel = Definition::Channel(data(2, I64, 1));
            founding.definitions.insert(value.clone(), channel);
            let node = start(&host, founding);
            let empty = ::hub::reader::Error::Empty;
            assert_eq!(readers(&mut sim, &node, &["plant.value"]), [Some(empty)]);
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
        }

        /// The error of a reader that a task on `node` opens on each of `names`, or
        /// `None` for one that opens.
        fn readers(
            sim: &mut sim::Sim,
            node: &Node,
            names: &[&str],
        ) -> Vec<Option<::hub::reader::Error>> {
            let names: Vec<String> = names.iter().map(|name| (*name).into()).collect();
            let errors = Arc::new(Mutex::new(Vec::new()));
            let out = Arc::clone(&errors);
            node.spawn(move |hub| async move {
                for name in names {
                    let mode = ::hub::reader::Mode::Complete;
                    let reader = hub.reader(unnamed(&name, mode)).await;
                    out.lock().unwrap().push(reader.err());
                }
            });
            assert_eq!(sim.run_for(TEN), Ok(()));
            errors.lock().unwrap().drain(..).collect()
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
        /// index has its home at another node dials that home. The address of
        /// [`OTHER`] is this node's port, so the dial fails on its key.
        #[test]
        fn a_reader_of_a_channel_whose_home_is_another_node_dials_the_home() {
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
                let reader = hub
                    .reader(unnamed("plant.value", ::hub::reader::Mode::Complete))
                    .await;
                *out.lock().unwrap() = Some(reader.err());
            });
            assert_eq!(sim.run_for(TEN), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let peer = OTHER.1.public();
            let unreachable = transport::Error::Unreachable {
                peer,
                attempts: vec![(
                    Address::Udp(listen(&host)),
                    transport::Error::Authentication { expected: peer },
                )],
            };
            let error = ::hub::reader::Error::Transport(unreachable);
            assert_eq!(opened.lock().unwrap().take(), Some(Some(error)));
        }

        /// The node with key [`OWN`] on `host`, the one member and voter of its region.
        fn start_alone(host: &sim::node::Node) -> Node {
            start(host, region(&[member(OWN, &KEY, host)]))
        }

        /// The operations of the member `key` of `mesh` on `hub` and `tasks`, as a node
        /// makes them, with the clock, network, and secrets of `host`.
        fn operations(
            mesh: &::mesh::Mesh,
            key: types::node::Key,
            hub: &::hub::Hub,
            host: &sim::node::Node,
            tasks: &env::tasks::Tasks,
            time: &clock::Reader,
            entropy: &env::entropy::Entropy,
        ) -> Rc<ops::Node> {
            let member = mesh.member(key).expect("the peer is a member");
            let name = member.card.card().name.clone();
            let secrets = connector::testing::create_secrets(host.files());
            let supervisor =
                connector::supervisor::Supervisor::new(connector::supervisor::Config {
                    kinds: Arc::new(crate::kinds()),
                    clock: host.clock(),
                    entropy: entropy.clone(),
                    net: host.net(),
                    roots: connector::http::Roots::default(),
                    tasks: tasks.clone(),
                    hub: hub.clone(),
                    secrets,
                });
            Rc::new(ops::Node::new(ops::Config {
                mesh: mesh.clone(),
                keys: crate::channel_keys(time, entropy),
                front_ends: crate::front_ends(),
                supervisor,
                hub: hub.clone(),
                clock: host.clock(),
                name,
                roots: ops::roots::Failures::new(String::new(), Vec::new()),
            }))
        }

        /// As [`peer_hub`], with the region of `members`, and `act` gets only the mesh.
        fn peer<F: Future<Output = ()> + 'static>(
            host: &sim::node::Node,
            members: &[Member],
            act: impl FnOnce(::mesh::Mesh, sim::node::Node) -> F + Send + 'static,
        ) {
            peer_hub(host, region(members), |_, mesh, own| act(mesh, own));
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
            peer(&hosts[1], &members, move |mesh, _| async move {
                let set = mesh.set_home(INDEX, OTHER.0).await;
                out.lock().unwrap().push(set);
            });
            assert_eq!(sim.run_for(TEN), Ok(()));
            assert_eq!(*set.lock().unwrap(), [Ok(())]);
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
        }

        /// A node of the default message limit is a voter that takes each append of
        /// the leader: each of 50 homes, which the peer sets at once, commits within
        /// 10 s.
        #[test]
        fn homes_commit_with_a_voter_of_the_default_message_limit() {
            for seed in 0..4 {
                let mut sim = sim::Sim::new(sim::Config {
                    seed,
                    ..sim::Config::default()
                });
                let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
                let members = pair(&hosts);
                let node = start(&hosts[0], region(&members));
                let set = Arc::new(Mutex::new(0));
                let out = Arc::clone(&set);
                peer(&hosts[1], &members, move |mesh, own| async move {
                    own.clock().sleep(Span::SECOND).await;
                    let mut sets = Vec::new();
                    for key in 100..150 {
                        let (mesh, out) = (mesh.clone(), Arc::clone(&out));
                        sets.push(Box::pin(async move {
                            let index = channel::Key::from_u128(key);
                            if mesh.set_home(index, OTHER.0).await.is_ok() {
                                *out.lock().unwrap() += 1;
                            }
                        }));
                    }
                    let mut end = pin!(own.clock().sleep(TEN));
                    poll_fn(|cx| {
                        sets.retain_mut(|set| set.as_mut().poll(cx).is_pending());
                        if sets.is_empty() {
                            return Poll::Ready(());
                        }
                        end.as_mut().poll(cx)
                    })
                    .await;
                });
                assert_eq!(sim.run_for(TWENTY), Ok(()));
                node.stop();
                assert_eq!(sim.run(), Ok(()));
                assert_eq!(node.join(), Ok(()));
                assert_eq!(*set.lock().unwrap(), 50, "{seed}");
            }
        }

        /// The least pool budget that each buffer takes also holds an append of the
        /// largest record, so `Budget::pool` names no refusal of the mesh.
        #[test]
        fn the_least_pool_budget_of_the_buffers_opens_the_mesh() {
            assert_eq!(founds(least(), 0), Ok(()));
        }

        /// At the least pool budget, the store puts a founding chunk of a subject of
        /// 300 keys, about 9.6 KB, in a block of shard 0's pool.
        #[test]
        fn the_least_pool_budget_puts_a_founding_chunk_of_300_keys() {
            assert_eq!(founds(least(), 300), Ok(()));
        }

        /// At 1 MiB on two shards, the store puts a founding chunk of a subject of 1900
        /// keys, about 61 KB, in a block of shard 0's pool.
        #[test]
        fn a_mebibyte_on_two_shards_puts_a_founding_chunk_of_1900_keys() {
            assert_eq!(founds(Size::MEBIBYTE, 1900), Ok(()));
        }

        /// The least pool budget of a node on two shards, which [`Error::Small`]
        /// names.
        fn least() -> Size {
            let Err(Error::Small { min, .. }) = founds(Size::from_bytes(1), 0) else {
                panic!("a pool budget of 1 byte opens no buffer");
            };
            min
        }

        /// Starts a node of the region of [`pair`] on two shards with a pool budget of
        /// `budget`, and with a subject of `keys` keys in its founding spec when `keys`
        /// is not 0, then stops it, and gives its end.
        fn founds(budget: Size, keys: u16) -> Result<(), Error> {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let mut region = region(&pair(&hosts));
            if keys > 0 {
                region.definitions = subject(keys);
            }
            let node = Node::start(Config {
                region: Region::Given(region),
                ..config(&hosts[0], budget, Box::new(heap))
            });
            assert_eq!(sim.run_for(TEN), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            node.join()
        }

        /// The definitions of the subject `plant.operator` of `keys` keys.
        fn subject(keys: u16) -> BTreeMap<Name, Definition> {
            let keys = (0..keys).map(|n| {
                let mut bytes = [0; 32];
                bytes[..2].copy_from_slice(&n.to_le_bytes());
                PrivateKey(bytes).public()
            });
            let subject = Subject::new(keys.collect()).unwrap();
            let label = Kind::Subject.key("plant.operator").unwrap();
            [(label, Definition::Subject(subject))].into()
        }

        /// A node of the least message limit that its transport takes, 1472 bytes,
        /// cannot take an append of the largest record, so its mesh does not open.
        #[test]
        fn a_node_of_the_least_message_limit_does_not_start() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let least = crate::Settings {
                message_bytes_max: NonZeroUsize::new(1472).unwrap(),
                window_bytes: 2 * 1472,
                ..crate::Settings::default()
            };
            let node = Node::start(Config {
                region: Region::Given(region(&pair(&hosts))),
                settings: least,
                ..config(&hosts[0], Size::MEBIBYTE, Box::new(heap))
            });
            assert_eq!(sim.run(), Ok(()));
            let limit = ::mesh::Error::Limit {
                bytes_max: 1472,
                bytes_min: 50_355,
            };
            let refused = node.join().unwrap_err();
            assert_eq!(refused, Error::Mesh(limit));
            assert_eq!(
                refused.to_string(),
                "the node's mesh did not open: this node takes messages of at most 1472 \
                 bytes, below the 50355 bytes of an append of the largest record"
            );
        }

        /// A node with no region opens no mesh, so it starts at that limit.
        #[test]
        fn a_node_with_no_region_of_the_least_message_limit_starts() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2)];
            let least = crate::Settings {
                message_bytes_max: NonZeroUsize::new(1472).unwrap(),
                window_bytes: 2 * 1472,
                ..crate::Settings::default()
            };
            let node = Node::start(Config {
                region: Region::Absent,
                settings: least,
                ..config(&hosts[0], Size::MEBIBYTE, Box::new(heap))
            });
            assert_eq!(sim.run_for(TEN), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
        }

        /// A task's reader of a channel whose home is the peer holds a session to the
        /// peer when the node stops. The stop drops the hub and the reader, and each
        /// task of the hub then lets go of the transport (`hub::Hub::new`), so the
        /// port is free within the drain bound of 3 s.
        #[test]
        fn a_stop_frees_the_port_while_a_reader_holds_a_session_to_a_home() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let founding = paired(&hosts, OTHER.0);
            let node = start(&hosts[0], founding.clone());
            peer_hub(&hosts[1], founding, |_, _, host| async move {
                host.clock().sleep(Span::MINUTE).await;
            });
            let opened = Arc::new(Mutex::new(None));
            let out = Arc::clone(&opened);
            node.spawn(move |hub| async move {
                match hub
                    .reader(unnamed("plant.value", ::hub::reader::Mode::Complete))
                    .await
                {
                    Ok(mut reader) => {
                        *out.lock().unwrap() = Some(Ok(()));
                        let next = reader.next().await;
                        panic!("the reader ended before the stop: {next:?}");
                    }
                    Err(error) => *out.lock().unwrap() = Some(Err(error)),
                }
            });
            assert_eq!(sim.run_for(TEN), Ok(()));
            assert_eq!(*opened.lock().unwrap(), Some(Ok(())));
            let stopped = hosts[0].clock().now();
            node.stop();
            let (after, bound) = sim
                .run_on(&hosts[0], move |host, _| async move {
                    let (_lock, _) = take_lock(&host).await;
                    let after = host.clock().now() - stopped;
                    let bound = transport::Port::bind(&host.net(), listen(&host));
                    (after, bound.map(drop))
                })
                .expect("the probe ends");
            assert_eq!(bound, Ok(()));
            let drain = Span::from_nanos(3 * Span::SECOND.nanos());
            assert!(after <= drain, "the lock was free {after:?} after the stop");
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
            let first = super::open_unknown();
            let first: Vec<&[u8]> = first.iter().map(Vec::as_slice).collect();
            paired_sees(dialer, &first)
        }

        /// What `dialer` sees when it sends `first` to the node [`OWN`] of a region
        /// with [`OTHER`].
        fn paired_sees(dialer: Dialer, first: &[&[u8]]) -> Seen {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start(&hosts[0], region(&pair(&hosts)));
            let seen = dial(&mut sim, &hosts[0], dialer, first);
            watch(sim, node, &seen)
        }

        /// A member puts a chunk in the node's store over a blob stream, and gets it
        /// back on another stream into a store of its own.
        #[test]
        fn a_member_puts_a_chunk_in_the_store_and_gets_it_back() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start(&hosts[0], region(&pair(&hosts)));
            let peer = sim.node(sim::node::Config::default());
            let at = [Address::Udp(listen(&hosts[0]))];
            let digest = types::digest::Digest::of(b"chunk");
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            shard(&peer, move |own, tasks| async move {
                let (transport, pool) = transport(&own, tasks, OTHER.1);
                let session =
                    transport.dial(KEY.public(), &at).await.expect("a session");
                let open = |dir: &str| {
                    blob::Store::open(blob::Config {
                        files: own.files(),
                        dir: PathBuf::from(dir),
                        pool: Rc::clone(&pool),
                        floor_bytes: 0,
                    })
                };
                let (sent, got) =
                    (open("sent").await.unwrap(), open("got").await.unwrap());
                sent.put(digest, &pool.copy(b"chunk").unwrap())
                    .await
                    .unwrap();
                let put = blob::peer::put(&sent, &session, &[digest]).await;
                let misses = blob::peer::get(&got, &session, &[digest]).await;
                let chunk = got.get(digest).await.unwrap().map(|block| block.to_vec());
                *seen.lock().unwrap() = Some((put, misses, chunk));
            });
            assert_eq!(sim.run_for(Span::HOUR), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let seen = out.lock().unwrap().take();
            assert_eq!(seen, Some((Ok(()), Ok(vec![]), Some(b"chunk".to_vec()))));
        }

        /// A member that closes its session in a put body leaves a part in the node's
        /// store, and a later put of the member removes it.
        #[test]
        fn a_later_put_removes_the_part_of_a_put_ended_in_its_body() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start(&hosts[0], region(&pair(&hosts)));
            let peer = sim.node(sim::node::Config::default());
            let at = [Address::Udp(listen(&hosts[0]))];
            let digest = types::digest::Digest::of(&[7; 3000]);
            let later = types::digest::Digest::of(b"chunk");
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            shard(&peer, move |own, tasks| async move {
                let (transport, pool) = transport(&own, tasks, OTHER.1);
                let session =
                    transport.dial(KEY.public(), &at).await.expect("a session");
                let (mut sender, _receiver) =
                    session.open(Class::Complete).await.expect("a stream");
                let header = wire::header::encode(wire::Protocol::Blob);
                let mut head = [0; wire::blob::Put::LEN];
                wire::blob::Put { digest, len: 3000 }.encode(&mut head);
                for bytes in [&header[..], &head, &[7; 1000]] {
                    let block = pool.copy(bytes).unwrap();
                    sender.send(block).await.expect("the send");
                }
                own.clock().sleep(Span::SECOND).await;
                session.close(Code(0));
                own.clock().sleep(Span::MINUTE).await;
                let session =
                    transport.dial(KEY.public(), &at).await.expect("a session");
                let store = blob::Store::open(blob::Config {
                    files: own.files(),
                    dir: PathBuf::from("sent"),
                    pool: Rc::clone(&pool),
                    floor_bytes: 0,
                })
                .await
                .unwrap();
                store
                    .put(later, &pool.copy(b"chunk").unwrap())
                    .await
                    .unwrap();
                let put = blob::peer::put(&store, &session, &[later]).await;
                *seen.lock().unwrap() = Some(put);
            });
            assert_eq!(
                sim.run_for(Span::from_nanos(10 * Span::SECOND.nanos())),
                Ok(())
            );
            let names = blob_names(&mut sim, &hosts[0]);
            assert_eq!(parts(&names), [digest.to_string()], "{names:?}");
            assert_eq!(sim.run_for(Span::HOUR), Ok(()));
            assert_eq!(out.lock().unwrap().take(), Some(Ok(())));
            let names = blob_names(&mut sim, &hosts[0]);
            assert!(
                names.contains(&PathBuf::from(later.to_string())),
                "{names:?}"
            );
            assert_eq!(parts(&names), Vec::<String>::new());
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
        }

        /// The names in the blob directory of `host`, listed while its node runs.
        fn blob_names(sim: &mut sim::Sim, host: &sim::node::Node) -> Vec<PathBuf> {
            let shard = env::shards::Config {
                name: "list".into(),
                core: None,
            };
            let own = host.clone();
            let names = Arc::new(Mutex::new(None));
            let listed = Arc::clone(&names);
            let started = host.shards().start(shard, move |_| async move {
                let names = own.files().list(&crate::directory::blob()).await;
                *listed.lock().unwrap() = Some(names);
            });
            drop(started.expect("the list starts"));
            assert_eq!(sim.run_for(Span::SECOND), Ok(()));
            let names = names.lock().unwrap().take();
            names.expect("the list ends").expect("the directory lists")
        }

        /// The digest of each part file in `names`.
        fn parts(names: &[PathBuf]) -> Vec<String> {
            names
                .iter()
                .filter(|name| name.extension().is_some())
                .map(|name| name.file_stem().unwrap().to_string_lossy().into_owned())
                .collect()
        }

        /// A put of a member whose bytes do not hash to its digest ends its stream with
        /// `MISMATCH`, and the node logs the store's error as a warning.
        #[test]
        fn a_store_failure_on_a_blob_stream_of_a_member_logs_a_warning() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let lines = super::super::log::Lines::default();
            let node = Node::start(Config {
                region: Region::Given(region(&pair(&hosts))),
                log: super::super::log::to(&lines, false, tracing::Level::INFO),
                ..config(&hosts[0], Size::MEBIBYTE, Box::new(heap))
            });
            let peer = sim.node(sim::node::Config::default());
            let at = [Address::Udp(listen(&hosts[0]))];
            let digest = types::digest::Digest::of(b"chunk");
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            shard(&peer, move |own, tasks| async move {
                let (transport, pool) = transport(&own, tasks, OTHER.1);
                let session =
                    transport.dial(KEY.public(), &at).await.expect("a session");
                let (mut sender, mut receiver) =
                    session.open(Class::Complete).await.expect("a stream");
                let header = wire::header::encode(wire::Protocol::Blob);
                let mut head = [0; wire::blob::Put::LEN];
                wire::blob::Put { digest, len: 5 }.encode(&mut head);
                for bytes in [&header[..], &head, b"other"] {
                    drop(sender.send(pool.copy(bytes).unwrap()).await);
                }
                let read = receiver.recv().await.map(|m| m.map(|b| b.to_vec()));
                *seen.lock().unwrap() = Some(read);
                drop((sender, session, transport));
            });
            assert_eq!(sim.run_for(Span::HOUR), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            assert_eq!(sim.run(), Ok(()));
            let code = Code(wire::blob::MISMATCH);
            let read = out.lock().unwrap().take();
            assert_eq!(read, Some(Err(transport::Error::Reset { code })));
            let found = types::digest::Digest::of(b"other");
            let error = blob::Error::Mismatch { digest, found };
            let warned: Vec<_> = lines
                .read()
                .iter()
                .filter_map(|line| line.get(28..).map(str::to_owned))
                .filter(|line| line.starts_with("WARN "))
                .collect();
            let line = format!(
                "WARN node::route: the store ended a blob stream of a member \
                 error=\"{error}\""
            );
            assert_eq!(warned, [line]);
        }

        /// A member that sends put heads of 42 of the 47 block sizes of shard 0's pool,
        /// from the largest down, and no bodies, more than the 8 puts that the store
        /// holds open, still has its hub streams served, so a hub stream of the member
        /// still goes to the hub, which stops an open of a channel that the node does
        /// not know with `UNKNOWN`.
        #[test]
        fn put_heads_with_no_body_leave_a_hub_stream_served() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start(&hosts[0], region(&pair(&hosts)));
            let peer = sim.node(sim::node::Config::default());
            let at = [Address::Udp(listen(&hosts[0]))];
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            shard(&peer, move |own, tasks| async move {
                let (transport, pool) = transport(&own, tasks, OTHER.1);
                let session =
                    transport.dial(KEY.public(), &at).await.expect("a session");
                let message = |bytes: &[u8]| pool.copy(bytes).unwrap();
                let mut streams = Vec::new();
                let largest =
                    block::Pool::heap(block::Config::new(512 << 10).unwrap()).largest();
                let largest = u32::try_from(largest).unwrap();
                // A head of 42 of the 47 sizes of block, from the largest down, in fewer
                // streams than the 64 of a session.
                let lens =
                    std::iter::successors(Some(largest), |len| Some(len * 5 / 6));
                let lens = lens.take_while(|&len| len >= 64);
                for (byte, len) in (0_u16..).zip(lens) {
                    let (mut sender, receiver) =
                        session.open(Class::Complete).await.expect("a stream");
                    let header = wire::header::encode(wire::Protocol::Blob);
                    let put = wire::blob::Put {
                        digest: types::digest::Digest::of(&byte.to_le_bytes()),
                        len,
                    };
                    let mut head = [0; wire::blob::Put::LEN];
                    put.encode(&mut head);
                    for bytes in [&header[..], &head] {
                        drop(sender.send(message(bytes)).await);
                    }
                    streams.push((sender, receiver));
                }
                own.clock().sleep(Span::MINUTE).await;
                let (mut sender, mut receiver) =
                    session.open(Class::Complete).await.expect("a stream");
                for bytes in super::open_unknown() {
                    drop(sender.send(message(&bytes)).await);
                }
                let read = receiver.recv().await.map(|m| m.map(|b| b.to_vec()));
                *seen.lock().unwrap() = Some(read);
                drop((streams, sender, session, transport));
            });
            assert_eq!(sim.run_for(Span::HOUR), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let code = Code(wire::hub::UNKNOWN);
            let read = out.lock().unwrap().take();
            assert_eq!(read, Some(Err(transport::Error::Reset { code })));
        }

        /// The node stops a blob stream of a node that is not a member, and resets its
        /// reply half, with the code of a rejected header.
        #[test]
        fn rejects_a_blob_stream_of_a_node_that_is_not_a_member() {
            let header = wire::header::encode(wire::Protocol::Blob);
            let Seen { sent, read, .. } = paired_sees(Dialer::Node(CLIENT), &[&header]);
            let code = Code(wire::header::REJECTED);
            assert_eq!(sent, transport::Error::Stopped { code });
            assert_eq!(read, Err(transport::Error::Reset { code }));
        }

        /// The node stops a blob stream of a program, and resets its reply half, with
        /// the code of a rejected header.
        #[test]
        fn rejects_a_blob_stream_of_a_program() {
            let header = wire::header::encode(wire::Protocol::Blob);
            let Seen { sent, read, .. } = paired_sees(Dialer::Program, &[&header]);
            let code = Code(wire::header::REJECTED);
            assert_eq!(sent, transport::Error::Stopped { code });
            assert_eq!(read, Err(transport::Error::Reset { code }));
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

        /// The node gives a hub stream of a program to its hub, which takes it as the
        /// hello stream and closes the session at its first message, which is no
        /// hello, with the code of a malformed message.
        #[test]
        fn a_hub_stream_of_a_program_of_a_region_goes_to_the_hub() {
            let Seen { sent, read, .. } = opened(Dialer::Program);
            let code = Code(wire::header::MALFORMED);
            assert_eq!(sent, transport::Error::PeerClosed { code });
            assert_eq!(read, Err(transport::Error::PeerClosed { code }));
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

        /// Under a header setting of zero, a mesh stream whose header arrived when
        /// the node first reads it still goes to the mesh.
        #[test]
        fn a_header_that_arrived_routes_under_a_header_wait_of_zero() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = Node::start(Config {
                region: Region::Given(region(&[member(OWN, &KEY, &host)])),
                settings: crate::Settings {
                    header_wait: Span::ZERO,
                    ..crate::Settings::default()
                },
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            let header = wire::header::encode(wire::Protocol::Mesh);
            let seen = dial(&mut sim, &host, Dialer::Node(CLIENT), &[&header]);
            let Seen { sent, read, .. } = watch(sim, node, &seen);
            let code = Code(wire::header::MALFORMED);
            assert_eq!(sent, transport::Error::Stopped { code });
            assert_eq!(read, Err(transport::Error::Reset { code }));
        }

        /// The node stops a mesh stream of a program, and resets its reply half, with
        /// the code of a rejected header. The mesh gets no message of it: it stops a
        /// stream at its first message that is not a mesh message with `MALFORMED`.
        #[test]
        fn rejects_a_mesh_stream_of_a_program() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = start_alone(&host);
            let header = wire::header::encode(wire::Protocol::Mesh);
            let seen = dial(&mut sim, &host, Dialer::Program, &[&header, b"after"]);
            let Seen {
                peer, sent, read, ..
            } = watch(sim, node, &seen);
            let code = Code(wire::header::REJECTED);
            assert_eq!(peer, Peer::Node(KEY.public()));
            assert_eq!(sent, transport::Error::Stopped { code });
            assert_eq!(read, Err(transport::Error::Reset { code }));
        }

        /// `count` sessions that `program` dials to the node [`OWN`] at `at`, in order.
        async fn create_sessions(
            program: &transport::Client,
            at: &[Address],
            count: usize,
        ) -> Vec<transport::Session> {
            let mut held = Vec::new();
            for _ in 0..count {
                held.push(program.dial(KEY.public(), at).await.expect("a session"));
            }
            held
        }

        /// The index and the error of each of `sessions` that closes, in the order that
        /// they close.
        fn closings(
            sessions: &[transport::Session],
            tasks: &env::tasks::Tasks,
        ) -> Rc<RefCell<Vec<(usize, transport::Error)>>> {
            let closed = Rc::new(RefCell::new(Vec::new()));
            for (index, session) in sessions.iter().enumerate() {
                let (session, out) = (session.clone(), Rc::clone(&closed));
                tasks.spawn(async move {
                    let error = session.closed().await;
                    out.borrow_mut().push((index, error));
                });
            }
            closed
        }

        /// The error of a session that the node closed to free its place.
        fn refused() -> transport::Error {
            let code = Code(wire::session::REFUSED);
            transport::Error::PeerClosed { code }
        }

        /// The error of [`sent_clock`] on a session that the node admitted.
        fn admitted() -> transport::Error {
            let code = Code(wire::header::REJECTED);
            transport::Error::Stopped { code }
        }

        /// Runs `sim` for a minute, then stops `node` and runs `sim` to its end.
        fn run_and_stop(sim: &mut sim::Sim, node: &Node) {
            assert_eq!(sim.run_for(Span::MINUTE), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
        }

        /// 256 programs hold each place for peers outside the region. A new program
        /// gets a session, the session of the oldest closes with the code of a
        /// refused session, and the other 255 stay. The next program closes the
        /// second oldest.
        #[test]
        fn the_oldest_place_frees_for_a_new_peer_at_the_bound() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start(&hosts[0], region(&pair(&hosts)));
            let listen = listen(&hosts[0]);
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            shard(&hosts[1], move |own, tasks| async move {
                let at = [Address::Udp(listen)];
                let (program, pool) = program(&own, tasks.clone());
                let mut held = create_sessions(&program, &at, 256).await;
                let closed = closings(&held, &tasks);
                let (mut sent, mut after) = (Vec::new(), Vec::new());
                for _ in 0..2 {
                    let new = program.dial(KEY.public(), &at).await.expect("a session");
                    sent.push(sent_clock(&new, &pool, &own).await);
                    own.clock().sleep(Span::SECOND).await;
                    after.push(closed.borrow().clone());
                    held.push(new);
                }
                *seen.lock().unwrap() = Some((sent, after));
                drop((held, program));
            });
            run_and_stop(&mut sim, &node);
            assert_eq!(node.join(), Ok(()));
            let (sent, after) = out.lock().unwrap().take().expect("the peer ran");
            assert_eq!(sent, vec![admitted(), admitted()]);
            let first = vec![(0, refused())];
            let second = vec![(0, refused()), (1, refused())];
            assert_eq!(after, vec![first, second]);
        }

        /// Under a setting of 3 places, 3 programs hold each place. A new program
        /// gets a session, and the session of the oldest closes with the code of a
        /// refused session.
        #[test]
        fn the_oldest_place_frees_at_the_setting() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let settings = crate::Settings {
                places_max: NonZeroUsize::new(3).unwrap(),
                ..crate::Settings::default()
            };
            let node = Node::start(Config {
                region: Region::Given(region(&pair(&hosts))),
                settings,
                ..config(&hosts[0], Size::MEBIBYTE, Box::new(heap))
            });
            let listen = listen(&hosts[0]);
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            shard(&hosts[1], move |own, tasks| async move {
                let at = [Address::Udp(listen)];
                let (program, pool) = program(&own, tasks.clone());
                let held = create_sessions(&program, &at, 3).await;
                let closed = closings(&held, &tasks);
                let new = program.dial(KEY.public(), &at).await.expect("a session");
                let sent = sent_clock(&new, &pool, &own).await;
                own.clock().sleep(Span::SECOND).await;
                *seen.lock().unwrap() = Some((sent, closed.borrow().clone()));
                drop((held, new, program));
            });
            run_and_stop(&mut sim, &node);
            assert_eq!(node.join(), Ok(()));
            let (sent, closed) = out.lock().unwrap().take().expect("the peer ran");
            assert_eq!(sent, admitted());
            assert_eq!(closed, vec![(0, refused())]);
        }

        /// 256 programs fill the bound, then a member dials: it takes no place, so no
        /// session closes. The next program closes the oldest program, and the
        /// member's session stays.
        #[test]
        fn the_bound_closes_no_session_of_a_member() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start(&hosts[0], region(&pair(&hosts)));
            let listen = listen(&hosts[0]);
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            shard(&hosts[1], move |own, tasks| async move {
                let at = [Address::Udp(listen)];
                let (program, pool) = program(&own, tasks.clone());
                let held = create_sessions(&program, &at, 256).await;
                let closed = closings(&held, &tasks);
                let (node, node_pool) = transport(&own, tasks.clone(), OTHER.1);
                let member = node.dial(KEY.public(), &at).await.expect("a session");
                let of_member = closings(std::slice::from_ref(&member), &tasks);
                let sent_member = sent_clock(&member, &node_pool, &own).await;
                let at_member = closed.borrow().clone();
                let new = program.dial(KEY.public(), &at).await.expect("a session");
                let sent = sent_clock(&new, &pool, &own).await;
                own.clock().sleep(Span::SECOND).await;
                let closed = (
                    at_member,
                    of_member.borrow().clone(),
                    closed.borrow().clone(),
                );
                *seen.lock().unwrap() = Some((sent, sent_member, closed));
                drop((held, new, member, node, program));
            });
            run_and_stop(&mut sim, &node);
            assert_eq!(node.join(), Ok(()));
            let (sent, sent_member, closed) =
                out.lock().unwrap().take().expect("the peer ran");
            assert_eq!(sent, admitted());
            assert_eq!(sent_member, admitted());
            assert_eq!(closed, (vec![], vec![], vec![(0, refused())]));
        }

        /// A program that closes its session frees its place, so at the bound the
        /// next program closes no session.
        #[test]
        fn a_peer_that_closes_its_session_frees_its_place() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start(&hosts[0], region(&pair(&hosts)));
            let listen = listen(&hosts[0]);
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            shard(&hosts[1], move |own, tasks| async move {
                let at = [Address::Udp(listen)];
                let (program, pool) = program(&own, tasks.clone());
                let mut held = create_sessions(&program, &at, 256).await;
                held.remove(100).close(Code(0));
                let closed = closings(&held, &tasks);
                own.clock().sleep(Span::SECOND).await;
                let new = program.dial(KEY.public(), &at).await.expect("a session");
                let sent = sent_clock(&new, &pool, &own).await;
                own.clock().sleep(Span::SECOND).await;
                *seen.lock().unwrap() = Some((sent, closed.borrow().clone()));
                drop((held, new, program));
            });
            run_and_stop(&mut sim, &node);
            assert_eq!(node.join(), Ok(()));
            let (sent, closed) = out.lock().unwrap().take().expect("the peer ran");
            assert_eq!(sent, admitted());
            assert_eq!(closed, vec![]);
        }

        /// A node outside the region that holds a place, and that is a member now,
        /// frees its place and keeps its session: at the bound, no session closes. It
        /// calls `Places::take`, since no path makes a node a member at run time until
        /// the join of #336, so no session through `accept` can show it.
        #[test]
        fn a_peer_that_is_a_member_now_frees_its_place() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [
                sim.node(sim::node::Config::default()),
                sim.node(sim::node::Config::default()),
            ];
            let listen = listen(&hosts[0]);
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            shard(&hosts[0], move |own, tasks| async move {
                let (transport, _) = transport_at(&own, tasks.clone(), KEY, listen);
                let now = Peer::Node(CLIENT.public());
                let joined = Cell::new(false);
                let mut places =
                    route::Places::new(crate::Settings::default().places_max);
                let (mut sessions, mut holds) = (Vec::new(), Vec::new());
                for _ in 0..257 {
                    let session = transport.accept().await.expect("a session");
                    let hold =
                        places.take(&session, |peer| joined.get() && peer == now);
                    assert!(hold.is_some());
                    joined.set(joined.get() || session.peer() == now);
                    holds.push(hold);
                    sessions.push(session);
                }
                let closed = closings(&sessions, &tasks);
                own.clock().sleep(Span::SECOND).await;
                *seen.lock().unwrap() = Some(closed.borrow().clone());
                drop((holds, sessions, transport));
            });
            shard(&hosts[1], move |own, tasks| async move {
                let at = [Address::Udp(listen)];
                let (program, _) = program(&own, tasks.clone());
                let first = create_sessions(&program, &at, 1).await;
                let (node, _) = transport(&own, tasks, CLIENT);
                let member = node.dial(KEY.public(), &at).await.expect("a session");
                let held = create_sessions(&program, &at, 255).await;
                own.clock().sleep(Span::MINUTE).await;
                drop((first, member, held, program, node));
            });
            assert_eq!(sim.run_for(Span::MINUTE), Ok(()));
            assert_eq!(out.lock().unwrap().take(), Some(vec![]));
        }

        /// A session whose hold drops leaves its place: at the bound, the place closes
        /// only the sessions that still hold it. It calls `Places::take`, since through
        /// `accept` a hold drops only as its session ends, and a close of an ended
        /// session does nothing.
        #[test]
        fn a_session_whose_hold_drops_leaves_its_place() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [
                sim.node(sim::node::Config::default()),
                sim.node(sim::node::Config::default()),
            ];
            let listen = listen(&hosts[0]);
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            shard(&hosts[0], move |own, tasks| async move {
                let (transport, _) = transport_at(&own, tasks.clone(), KEY, listen);
                let mut places =
                    route::Places::new(crate::Settings::default().places_max);
                let left = transport.accept().await.expect("a session");
                let hold = places.take(&left, |_| false);
                let ended = transport.accept().await.expect("a session");
                let kept = places.take(&ended, |_| false);
                drop(hold);
                let (mut sessions, mut holds) = (Vec::new(), Vec::new());
                for _ in 0..256 {
                    let session = transport.accept().await.expect("a session");
                    holds.push(places.take(&session, |_| false));
                    sessions.push(session);
                }
                let closed = closings(std::slice::from_ref(&left), &tasks);
                own.clock().sleep(Span::SECOND).await;
                *seen.lock().unwrap() = Some(closed.borrow().clone());
                drop((kept, holds, sessions, left, ended, transport));
            });
            shard(&hosts[1], move |own, tasks| async move {
                let at = [Address::Udp(listen)];
                let (first, _) = transport(&own, tasks.clone(), CLIENT);
                let open = first.dial(KEY.public(), &at).await.expect("a session");
                let (second, _) = transport(&own, tasks.clone(), CLIENT);
                let ended = second.dial(KEY.public(), &at).await.expect("a session");
                ended.close(Code(0));
                own.clock().sleep(Span::SECOND).await;
                let (program, _) = program(&own, tasks);
                let held = create_sessions(&program, &at, 256).await;
                own.clock().sleep(Span::MINUTE).await;
                drop((open, ended, held, program, first, second));
            });
            assert_eq!(sim.run_for(Span::MINUTE), Ok(()));
            assert_eq!(out.lock().unwrap().take(), Some(vec![]));
        }

        /// A node with no region: 255 programs and the node [`CLIENT`] fill the bound,
        /// and the node of a second key closes the oldest program. [`CLIENT`] holds
        /// one place by its key: once it closes its session, the next program closes
        /// no session.
        #[test]
        fn a_node_outside_the_region_holds_one_place_by_its_key() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = Node::start(config(&hosts[0], Size::MEBIBYTE, Box::new(heap)));
            let listen = listen(&hosts[0]);
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            shard(&hosts[1], move |own, tasks| async move {
                let at = [Address::Udp(listen)];
                let (program, pool) = program(&own, tasks.clone());
                let held = create_sessions(&program, &at, 255).await;
                let closed = closings(&held, &tasks);
                let (first, _) = transport(&own, tasks.clone(), CLIENT);
                let placed = first.dial(KEY.public(), &at).await.expect("a session");
                let (second, second_pool) = transport(&own, tasks, OTHER.1);
                let other = second.dial(KEY.public(), &at).await.expect("a session");
                let of_other = sent_clock(&other, &second_pool, &own).await;
                placed.close(Code(0));
                own.clock().sleep(Span::SECOND).await;
                let new = program.dial(KEY.public(), &at).await.expect("a session");
                let of_new = sent_clock(&new, &pool, &own).await;
                own.clock().sleep(Span::SECOND).await;
                *seen.lock().unwrap() =
                    Some((of_other, of_new, closed.borrow().clone()));
                drop((held, other, new, first, second, program));
            });
            run_and_stop(&mut sim, &node);
            assert_eq!(node.join(), Ok(()));
            let (of_other, of_new, closed) =
                out.lock().unwrap().take().expect("the peer ran");
            assert_eq!(of_other, admitted());
            assert_eq!(of_new, admitted());
            assert_eq!(closed, vec![(0, refused())]);
        }

        /// A program on one host holds the oldest place, and 254 programs and one
        /// node outside the region on another host fill the bound. Once that host's
        /// links drop every packet, its sessions time out and free their places, so
        /// a later program closes no session: the oldest stays.
        #[test]
        fn places_free_when_the_peers_outside_the_region_vanish() {
            const LATE: Span = Span::from_nanos(150_000_000_000);
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start(&hosts[0], region(&pair(&hosts)));
            let listen = listen(&hosts[0]);
            let attacker = sim.node(sim::node::Config::default());
            let late = sim.node(sim::node::Config::default());
            let filled = Arc::new(AtomicBool::new(false));
            let out = Arc::new(Mutex::new(None));
            let done = Arc::clone(&filled);
            shard(&attacker, move |own, tasks| async move {
                let at = [Address::Udp(listen)];
                own.clock().sleep(Span::SECOND).await;
                let (program, _) = program(&own, tasks.clone());
                let (outsider, _) = transport(&own, tasks, CLIENT);
                let mut held = create_sessions(&program, &at, 254).await;
                held.push(outsider.dial(KEY.public(), &at).await.expect("a session"));
                done.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                drop((held, program, outsider));
            });
            let seen = Arc::clone(&out);
            shard(&late, move |own, tasks| async move {
                let at = [Address::Udp(listen)];
                let (program, pool) = program(&own, tasks.clone());
                let oldest = program.dial(KEY.public(), &at).await.expect("a session");
                let closed = closings(std::slice::from_ref(&oldest), &tasks);
                own.clock().sleep(LATE).await;
                let new = program.dial(KEY.public(), &at).await.expect("a session");
                let sent = sent_clock(&new, &pool, &own).await;
                own.clock().sleep(Span::SECOND).await;
                *seen.lock().unwrap() = Some((sent, closed.borrow().clone()));
                drop((oldest, new, program));
            });
            assert_eq!(sim.run_for(Span::MINUTE), Ok(()));
            assert!(
                filled.load(Ordering::SeqCst),
                "the attacker filled the bound"
            );
            let cut = sim::link::Config {
                loss: 1.0,
                ..sim::link::Config::default()
            };
            sim.link(&attacker, &hosts[0], cut);
            sim.link(&hosts[0], &attacker, cut);
            assert_eq!(sim.run_for(LATE), Ok(()));
            let seen = out.lock().unwrap().take();
            assert_eq!(seen, Some((admitted(), vec![])));
            drop(node);
        }

        /// 255 programs and the old session of a node outside the region fill the
        /// bound, and the node dials again while a program dials too. Both get a
        /// session, and the program closes the oldest program. A redial after the
        /// program's session keeps the node's place, so no other session closes.
        /// When both arrive at once, the redial closes at most the second oldest.
        #[test]
        fn a_node_that_dials_again_at_the_bound_races_a_program_for_its_place() {
            let first = vec![(0, refused())];
            let second = vec![(0, refused()), (1, refused())];
            for seed in 0..4 {
                let seen = redial_with_program(seed, true);
                let want = (Some(admitted()), Some(admitted()), first.clone());
                assert_eq!(seen, want, "seed {seed}");
            }
            for seed in 0..16 {
                let (of_node, of_program, closed) = redial_with_program(seed, false);
                assert_eq!(of_node, Some(admitted()), "seed {seed}");
                assert_eq!(of_program, Some(admitted()), "seed {seed}");
                let one = [first.clone(), second.clone()];
                assert!(one.contains(&closed), "seed {seed}: {closed:?}");
            }
        }

        /// What the new session of [`CLIENT`] and a program saw, and each program
        /// session that closed, when 255 programs and the old session of [`CLIENT`]
        /// fill the bound. [`CLIENT`] dials again once the program's session is done
        /// when `after`, else at the program's dial.
        fn redial_with_program(
            seed: u64,
            after: bool,
        ) -> (
            Option<transport::Error>,
            Option<transport::Error>,
            Vec<(usize, transport::Error)>,
        ) {
            let mut sim = sim::Sim::new(sim::Config {
                seed,
                ..sim::Config::default()
            });
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start(&hosts[0], region(&pair(&hosts)));
            let listen = listen(&hosts[0]);
            let attacker = sim.node(sim::node::Config::default());
            let (of_node, of_program, of_held) = (
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(Vec::new())),
            );
            let (seen_node, seen_program, seen_held) = (
                Arc::clone(&of_node),
                Arc::clone(&of_program),
                Arc::clone(&of_held),
            );
            shard(&attacker, move |own, tasks| async move {
                let at = [Address::Udp(listen)];
                let (programs, _) = program(&own, tasks.clone());
                let (late, late_pool) = program(&own, tasks.clone());
                let held = create_sessions(&programs, &at, 255).await;
                let closed = closings(&held, &tasks);
                let (first, _) = transport(&own, tasks.clone(), CLIENT);
                let old = first.dial(KEY.public(), &at).await.expect("a session");
                own.clock().sleep(Span::SECOND).await;
                let (second, pool) = transport(&own, tasks.clone(), CLIENT);
                let (other, out) = (own.clone(), Arc::clone(&seen_program));
                tasks.spawn(async move {
                    let session =
                        late.dial(KEY.public(), &at).await.expect("a session");
                    let sent = sent_clock(&session, &late_pool, &other).await;
                    *out.lock().unwrap() = Some(sent);
                    other.clock().sleep(Span::MINUTE).await;
                    drop((session, late));
                });
                while after && seen_program.lock().unwrap().is_none() {
                    own.clock().sleep(Span::MILLISECOND).await;
                }
                let new = second.dial(KEY.public(), &at).await.expect("a session");
                *seen_node.lock().unwrap() = Some(sent_clock(&new, &pool, &own).await);
                own.clock().sleep(Span::SECOND).await;
                *seen_held.lock().unwrap() = closed.borrow().clone();
                own.clock().sleep(Span::MINUTE).await;
                drop((held, programs, first, second, new, old));
            });
            assert_eq!(sim.run_for(Span::MINUTE), Ok(()));
            drop(node);
            let of_node = of_node.lock().unwrap().take();
            let of_held = of_held.lock().unwrap().clone();
            (of_node, of_program.lock().unwrap().take(), of_held)
        }

        /// A node outside the region holds the oldest place with its old
        /// session, then dials again from a new transport and closes the new session
        /// at once. Then 256 programs dial. The old session must not outlive the
        /// bound: it closes, as the newer session won, or as the oldest place.
        #[test]
        fn a_redial_that_ends_at_once_keeps_no_session_past_the_bound() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start(&hosts[0], region(&pair(&hosts)));
            let listen = listen(&hosts[0]);
            let attacker = sim.node(sim::node::Config::default());
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            shard(&attacker, move |own, tasks| async move {
                let at = [Address::Udp(listen)];
                let (first, first_pool) = transport(&own, tasks.clone(), CLIENT);
                let old = first.dial(KEY.public(), &at).await.expect("a session");
                let of_old = closings(std::slice::from_ref(&old), &tasks);
                own.clock().sleep(Span::SECOND).await;
                let (second, _) = transport(&own, tasks.clone(), CLIENT);
                let dead = second.dial(KEY.public(), &at).await.expect("a session");
                dead.close(Code(0));
                own.clock().sleep(Span::SECOND).await;
                let after_redial = of_old.borrow().clone();
                let (program, _) = program(&own, tasks.clone());
                let held = create_sessions(&program, &at, 256).await;
                let closed = closings(&held, &tasks);
                own.clock().sleep(Span::SECOND).await;
                let old_closed = of_old.borrow().clone();
                let sent_old = if old_closed.is_empty() {
                    Some(sent_clock(&old, &first_pool, &own).await)
                } else {
                    None
                };
                *seen.lock().unwrap() =
                    Some((after_redial, old_closed, closed.borrow().clone(), sent_old));
                drop((held, program, first, second, old, dead));
            });
            run_and_stop(&mut sim, &node);
            assert_eq!(node.join(), Ok(()));
            let (after_redial, old_closed, closed, sent_old) =
                out.lock().unwrap().take().expect("the peer ran");
            assert_eq!(
                (after_redial, old_closed, closed, sent_old),
                (vec![], vec![(0, refused())], vec![], None),
            );
        }

        /// 8 node keys outside the region each hold a session and send a
        /// dead redial, then 512 programs dial. At most 256 sessions of peers outside
        /// the region may stay open, so each old session of the 8 must close.
        #[test]
        fn redials_that_end_at_once_hold_no_session_over_the_bound() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start(&hosts[0], region(&pair(&hosts)));
            let listen = listen(&hosts[0]);
            let attacker = sim.node(sim::node::Config::default());
            let out = Arc::new(Mutex::new(None));
            let seen = Arc::clone(&out);
            shard(&attacker, move |own, tasks| async move {
                let at = [Address::Udp(listen)];
                let mut keep = Vec::new();
                let mut olds = Vec::new();
                for n in 0..8u8 {
                    let (first, _) =
                        transport(&own, tasks.clone(), PrivateKey([100 + n; 32]));
                    let old = first.dial(KEY.public(), &at).await.expect("a session");
                    let (second, _) =
                        transport(&own, tasks.clone(), PrivateKey([100 + n; 32]));
                    let dead = second.dial(KEY.public(), &at).await.expect("a session");
                    dead.close(Code(0));
                    olds.push(old);
                    keep.push((first, second, dead));
                }
                own.clock().sleep(Span::SECOND).await;
                let of_olds = closings(&olds, &tasks);
                let (program, _) = program(&own, tasks.clone());
                let held = create_sessions(&program, &at, 512).await;
                let closed = closings(&held, &tasks);
                own.clock().sleep(Span::SECOND).await;
                let open_programs = held.len() - closed.borrow().len();
                let open_olds = olds.len() - of_olds.borrow().len();
                *seen.lock().unwrap() = Some((open_olds, open_programs));
                drop((held, program, olds, keep));
            });
            run_and_stop(&mut sim, &node);
            assert_eq!(node.join(), Ok(()));
            let (open_olds, open_programs) =
                out.lock().unwrap().take().expect("the peer ran");
            assert!(
                open_olds + open_programs <= 256,
                "{open_olds} old node sessions and {open_programs} programs are open"
            );
        }

        /// A node outside the region holds the oldest place, 255 programs fill the
        /// bound, and the node dials again from a new transport. The transport of the
        /// started node closes the old session as the new one arrives. When the old
        /// session still holds the place at `accept`, the new one keeps its age, and
        /// the next program closes it; else the new one takes a new place, and the next
        /// program closes the oldest program. No other session closes, and each
        /// outcome occurs at some seed.
        #[test]
        fn a_node_that_dials_again_keeps_its_age_or_takes_a_new_place() {
            let kept = (vec![(0, refused())], vec![]);
            let fresh = (vec![], vec![(0, refused())]);
            let mut outcomes = Vec::new();
            for seed in 0..32 {
                let mut sim = sim::Sim::new(sim::Config {
                    seed,
                    ..sim::Config::default()
                });
                let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
                let node = start(&hosts[0], region(&pair(&hosts)));
                let listen = listen(&hosts[0]);
                let attacker = sim.node(sim::node::Config::default());
                let out = Arc::new(Mutex::new(None));
                let seen = Arc::clone(&out);
                shard(&attacker, move |own, tasks| async move {
                    let at = [Address::Udp(listen)];
                    let (first, _) = transport(&own, tasks.clone(), CLIENT);
                    let old = first.dial(KEY.public(), &at).await.expect("a session");
                    own.clock().sleep(Span::SECOND).await;
                    let (program, _) = program(&own, tasks.clone());
                    let held = create_sessions(&program, &at, 255).await;
                    let closed = closings(&held, &tasks);
                    let (second, _) = transport(&own, tasks.clone(), CLIENT);
                    let new = second.dial(KEY.public(), &at).await.expect("a session");
                    let of_old = old.closed().await;
                    own.clock().sleep(Span::SECOND).await;
                    let next =
                        program.dial(KEY.public(), &at).await.expect("a session");
                    let later = [new, next];
                    let of_later = closings(&later, &tasks);
                    own.clock().sleep(Span::SECOND).await;
                    let of_later = of_later.borrow().clone();
                    *seen.lock().unwrap() =
                        Some((of_old, (of_later, closed.borrow().clone())));
                    drop((held, program, first, second, later));
                });
                run_and_stop(&mut sim, &node);
                assert_eq!(node.join(), Ok(()));
                let (of_old, outcome) =
                    out.lock().unwrap().take().expect("the peer ran");
                assert_eq!(of_old, transport::Error::PeerClosed { code: Code(0) });
                assert!(
                    outcome == kept || outcome == fresh,
                    "seed {seed}: {outcome:?}"
                );
                outcomes.push(outcome);
            }
            assert!(
                outcomes.contains(&kept) && outcomes.contains(&fresh),
                "{outcomes:?}"
            );
        }

        /// The error of the first send that fails on a one-way stream of `session`
        /// whose header names the clock protocol, which the node rejects.
        async fn sent_clock(
            session: &transport::Session,
            pool: &block::Pool,
            host: &sim::node::Node,
        ) -> transport::Error {
            let sender = session.open_sender(Class::Complete).await;
            let mut sender = sender.expect("a stream");
            let header = wire::header::encode(wire::Protocol::Clock);
            loop {
                let block = pool.copy(&header).unwrap();
                if let Err(error) = sender.send(block).await {
                    return error;
                }
                host.clock().sleep(Span::MILLISECOND).await;
            }
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
                let block = |bytes: &[u8]| pool.copy(bytes).unwrap();
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
            fail_write_at_open(&node, &host);
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
            fail_write_at_open(&node, &host);
            node.spawn(|_| {
                let panics = Panics;
                async move {
                    std::future::pending::<()>().await;
                    drop(panics);
                }
            });
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
            fail_write_at_open(&node, &host);
            let at = sim::node::Config::default().monotonic
                + OPEN
                + Span::from_nanos(WRITE - 1);
            let own = host.clone();
            node.spawn(move |_| async move {
                own.clock().sleep_until(at).await;
                panic!("a task panics");
            });
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
        /// `join` gives the panic. At [`WRITE`], nine idle tasks that wake at that
        /// instant make the sim run the group's stop, then this task, before the node
        /// sees the stop. The task asserts that the group stopped before it panics.
        #[test]
        fn a_panic_before_the_node_sees_the_group_stop_gives_the_panic() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = start_alone(&host);
            fail_write_at_open(&node, &host);
            let at =
                sim::node::Config::default().monotonic + OPEN + Span::from_nanos(WRITE);
            for _ in 0..9 {
                let own = host.clone();
                node.spawn(move |_| async move {
                    own.clock().sleep_until(at).await;
                    std::future::pending::<()>().await;
                });
            }
            let own = host.clone();
            node.operate(move |ops| async move {
                let mut watch = ops.mesh().watch(INDEX);
                own.clock().sleep_until(at).await;
                let mut next = pin!(watch.next());
                let polled = poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx))).await;
                assert_eq!(polled, Poll::Ready(Err(write_failed())));
                panic!("a task panics");
            });
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
        /// the write of [`fail_write_at_open`] failing. `idle` more tasks on shard 0
        /// wake at that instant, which changes only the order of the sim's picks. Gives
        /// whether the task ran, and the node's `join`.
        fn udp_fault_at(idle: usize, after: Span) -> (bool, Result<(), Error>) {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = start_alone(&host);
            fail_write_at_open(&node, &host);
            let at = sim::node::Config::default().monotonic + OPEN + after;
            for _ in 0..idle {
                let own = host.clone();
                node.spawn(move |_| async move {
                    own.clock().sleep_until(at).await;
                    std::future::pending::<()>().await;
                });
            }
            let ran = Arc::new(AtomicBool::new(false));
            let (own, done) = (host.clone(), Arc::clone(&ran));
            node.spawn(move |_| async move {
                own.clock().sleep_until(at).await;
                own.fail_udp(listen(&own));
                done.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            });
            assert_eq!(sim.run(), Ok(()));
            (ran.load(Ordering::SeqCst), node.join())
        }

        /// Of a transport and a group that stop, `join` gives the one that the node
        /// sees first. At `WRITE`, the order of the picks decides which one that is,
        /// and whether the fault task runs before the node stops.
        #[test]
        fn join_gives_the_stop_that_the_node_sees_first() {
            let transport = || {
                Err(Error::Transport(transport::Error::Network {
                    error: env::net::Error::Io { code: 5 },
                }))
            };
            let group = || Err(Error::Group(write_failed()));
            for idle in 0..8 {
                assert_eq!(
                    udp_fault_at(idle, Span::from_nanos(WRITE - 1)),
                    (true, transport()),
                    "{idle} idle tasks"
                );
                let tie = udp_fault_at(idle, Span::from_nanos(WRITE));
                assert!(
                    [(true, transport()), (true, group()), (false, group())]
                        .contains(&tie),
                    "{idle} idle tasks: {tie:?}"
                );
                assert_eq!(
                    udp_fault_at(idle, Span::from_nanos(WRITE + 1)),
                    (false, group()),
                    "{idle} idle tasks"
                );
            }
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

        /// A lone node leads at open, so the set of a home ends within [`OPEN`] of
        /// the start, long before one election timeout of 1 s.
        #[test]
        fn a_lone_node_serves_a_set_of_a_home_at_open() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = start_alone(&host);
            let served = Arc::new(Mutex::new(None));
            let (own, seen) = (host.clone(), Arc::clone(&served));
            node.operate(move |ops| async move {
                let set = ops.mesh().set_home(channel::Key::from_u128(8), OWN).await;
                assert_eq!(set, Ok(()));
                *seen.lock().unwrap() = Some(own.clock().now());
            });
            assert_eq!(sim.run_for(OPEN), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let served = served.lock().unwrap().expect("the set ends before `OPEN`");
            assert!(served - sim::node::Config::default().monotonic < OPEN);
        }

        /// A restart with another region, after the log holds a record, stops the
        /// node, and `join` gives the region that the first start kept.
        #[test]
        fn a_restart_with_another_region_stops_the_node() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start_alone(&hosts[0]);
            // The lone voter votes for itself at open, and its write ends in this time.
            let elected = Span::from_nanos(3 * Span::SECOND.nanos());
            assert_eq!(sim.run_for(elected), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let mut members = pair(&hosts);
            let node = start(&hosts[0], region(&members));
            assert_eq!(sim.run(), Ok(()));
            members.sort_by_key(|member| member.card.key());
            let error = ::mesh::Error::Founding {
                stored: Box::new(region(&[member(OWN, &KEY, &hosts[0])])),
                given: Box::new(region(&members)),
            };
            assert_eq!(node.join(), Err(Error::Mesh(error)));
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

        /// The chunk store of a node leaves 1 GiB free: on a disk of 1 GiB, the put of
        /// the first founding chunk stops the node, and `join` gives the floor.
        #[test]
        fn a_founding_chunk_under_the_free_floor_stops_the_node() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed_on(
                &mut sim,
                sim::node::Config {
                    cores: NonZeroUsize::new(2).unwrap(),
                    disk_bytes: 1 << 30,
                    ..sim::node::Config::default()
                },
            );
            let mut region = region(&[member(OWN, &KEY, &host)]);
            let subject = Subject::new([KEY.public()].into()).unwrap();
            let label = Kind::Subject.key("plant.operator").unwrap();
            region.definitions = [(label, Definition::Subject(subject))].into();
            let mut chunks = Chunks::default();
            let tree = spec::region::tree(&mut chunks, &region.definitions);
            let len = chunks.get(tree.chunks[0]).expect("a chunk").len();
            let node = start(&host, region);
            assert_eq!(sim.run(), Ok(()));
            let free_bytes = sim
                .run_on(&host, |host, _| async move { host.files().free().await })
                .expect("the run ends")
                .expect("the free bytes");
            let error = ::mesh::Error::Blob(blob::Error::Floor {
                len,
                free_bytes,
                floor_bytes: 1 << 30,
            });
            assert_eq!(node.join(), Err(Error::Mesh(error)));
        }

        /// Under a floor setting of 64 GiB, on a disk of 64 GiB, the put of the first
        /// founding chunk stops the node, and `join` gives the floor of the setting.
        #[test]
        fn a_founding_chunk_under_the_floor_setting_stops_the_node() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let mut region = region(&[member(OWN, &KEY, &host)]);
            let subject = Subject::new([KEY.public()].into()).unwrap();
            let label = Kind::Subject.key("plant.operator").unwrap();
            region.definitions = [(label, Definition::Subject(subject))].into();
            let mut chunks = Chunks::default();
            let tree = spec::region::tree(&mut chunks, &region.definitions);
            let len = chunks.get(tree.chunks[0]).expect("a chunk").len();
            let settings = crate::Settings {
                floor_bytes: 64 << 30,
                ..crate::Settings::default()
            };
            let node = Node::start(Config {
                region: Region::Given(region),
                settings,
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            assert_eq!(sim.run(), Ok(()));
            let free_bytes = sim
                .run_on(&host, |host, _| async move { host.files().free().await })
                .expect("the run ends")
                .expect("the free bytes");
            let error = ::mesh::Error::Blob(blob::Error::Floor {
                len,
                free_bytes,
                floor_bytes: 64 << 30,
            });
            assert_eq!(node.join(), Err(Error::Mesh(error)));
        }

        /// A node whose chunk store holds each founding chunk starts again after
        /// another writer took the disk under 1 GiB free: the puts of its founding
        /// chunks take no new room.
        #[test]
        fn a_restart_on_a_disk_under_the_free_floor_opens_the_mesh() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let mut region = region(&[member(OWN, &KEY, &host)]);
            let subject = Subject::new([KEY.public()].into()).unwrap();
            let label = Kind::Subject.key("plant.operator").unwrap();
            region.definitions = [(label, Definition::Subject(subject))].into();
            let node = start(&host, region.clone());
            assert_eq!(sim.run_for(Span::SECOND), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let free_bytes = sim
                .run_on(&host, |host, _| async move {
                    let files = host.files();
                    let free_bytes = files.free().await.expect("the free bytes");
                    let len = free_bytes - (1 << 30) + 1;
                    let mode = env::files::Mode::Create { len };
                    let file = files.open(Path::new("other"), mode).await;
                    let file = file.expect("the file of the other writer opens");
                    file.sync().await.expect("syncs");
                    file.close().await;
                    files.sync_dir(Path::new("")).await.expect("syncs");
                    files.free().await.expect("the free bytes")
                })
                .expect("the run ends");
            assert_eq!(free_bytes, (1 << 30) - 1);
            let node = start(&host, region);
            assert_eq!(sim.run_for(Span::SECOND), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
        }

        /// Takes the lock of `host` as soon as it is free, then binds the node's port
        /// and opens the mesh's log to write. Gives whether the lock was held, the
        /// bind, the open of the log, and the file closes of `host` before the
        /// probe's own.
        fn probe(
            sim: &mut sim::Sim,
            host: &sim::node::Node,
        ) -> (
            bool,
            Result<(), env::net::Error>,
            Result<(), env::files::Error>,
            Vec<PathBuf>,
        ) {
            sim.run_on(host, |host, _| async move {
                let (lock, waited) = take_lock(&host).await;
                let files = host.files();
                let udp = env::net::udp::Config {
                    local: listen(&host),
                    send_buffer_bytes: 1 << 16,
                    recv_buffer_bytes: 1 << 16,
                };
                let port = host.net().udp(&udp).map(drop);
                let mode = env::files::Mode::Write;
                let log = files.open(Path::new(LOG), mode).await;
                let closes = host.file_closes();
                drop(lock);
                (waited, port, log.map(drop), closes)
            })
            .expect("the probe ends")
        }

        /// The peer runs no node, so a dial of the mesh to it is in flight at the stop.
        /// The stop closes that dial, and holds the lock for at most 3 s of its drain
        /// and less than 1 ms for its other steps.
        #[test]
        fn a_dial_to_a_peer_that_is_down_holds_the_lock_at_most_3_s() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let node = start(&hosts[0], region(&pair(&hosts)));
            assert_eq!(sim.run_for(TEN), Ok(()));
            node.stop();
            let waited = sim
                .run_on(&hosts[0], |host, _| async move {
                    let before = host.clock().now();
                    drop(take_lock(&host).await);
                    host.clock().now() - before
                })
                .expect("the probe ends");
            let bound = Span::from_nanos(3001 * Span::MILLISECOND.nanos());
            assert!(waited <= bound, "the stop held the lock {waited:?}");
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
        }

        /// Starts the peer [`OTHER`] of `members` on `host`, which sets a home in the
        /// mesh, one after another, until a set waits [`TEN`].
        fn set_homes(host: &sim::node::Node, members: &[Member]) {
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
        /// lock as soon as it is free: the node closes the mesh's log before the lock,
        /// the log is never busy then, and the port of a node that held the lock
        /// binds once the lock is free. A node stopped before its claim holds no
        /// lock, so its port can still be bound. The peer ends once a set waits
        /// [`TEN`], so the probe's run ends.
        #[test]
        fn the_log_of_the_mesh_is_free_once_the_lock_is() {
            let (mut held, mut logged, mut ordered) = (false, false, false);
            // The probe finds the lock free and the log made at about 4 ms only.
            let mut freed = false;
            // Three orders of the last closes of the log, `shard-1/ring`, and
            // `address` that the stops in a write give at seed 0: the log first, the
            // address first, and the ring first. They do not show which file call is
            // in flight (#2778), so a drift of the writes can still keep each order.
            let mut crossed = [false; 3];
            // A stop from about 1.35 ms comes after the node starts to open its
            // mesh, which opens its log at about 3.1 ms. From about 1.1 s the log
            // writes a home about each 0.7 ms, and the stops in the writes step from
            // before the write of one home, through its write and its sync, to after
            // them.
            let starts = (0..200).map(|step| step * 13_000);
            let opens = starts.chain((0..42).map(|step| 2_626_000 + step * 52_000));
            let opens = opens.map(|after| (after, false));
            let writes = (0..13).map(|step| (1_700_834_000 + step * 7_000, true));
            for (after, write) in opens.chain(writes) {
                let mut sim = sim::Sim::new(sim::Config::default());
                let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
                let members = pair(&hosts);
                let node = start(&hosts[0], region(&members));
                set_homes(&hosts[1], &members);
                let after = Span::from_nanos(after);
                assert_eq!(sim.run_for(after), Ok(()), "at {after:?}");
                node.stop();
                let (waited, port, log, closes) = probe(&mut sim, &hosts[0]);
                held |= waited;
                logged |= log.is_ok();
                freed |= !waited && log.is_ok();
                let busy = matches!(log, Err(env::files::Error::Busy { .. }));
                assert!(!busy, "at {after:?}: {log:?}");
                let last = |path| closes.iter().rposition(|c| c == Path::new(path));
                if let (Some(log), Some(lock)) = (last(LOG), last("lock")) {
                    assert!(log < lock, "at {after:?}: {closes:?}");
                    ordered = true;
                }
                let order = (last(LOG), last("shard-1/ring"), last("address"));
                if let (true, (Some(log), Some(ring), Some(address))) = (write, order) {
                    crossed[0] |= log < ring;
                    crossed[1] |= address < log;
                    crossed[2] |= ring < log && log < address;
                }
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
            assert!(held && logged && ordered && freed);
            assert_eq!(
                crossed, [true; 3],
                "the stops miss an order of the closes: find the write times again"
            );
        }

        /// When the mesh's group stops the node while a peer holds a session with it,
        /// the port binds once the lock is free.
        #[test]
        fn the_port_is_free_once_the_lock_is_after_the_group_stops() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let members = pair(&hosts);
            let node = start(&hosts[0], region(&members));
            set_homes(&hosts[1], &members);
            assert_eq!(sim.run_for(Span::from_nanos(1_700_000_000)), Ok(()));
            hosts[0].fail_file(Path::new(LOG), env::files::Operation::WriteAt);
            let (waited, port, _, _) = probe(&mut sim, &hosts[0]);
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
                let pool = block::Config::new(1 << 20).expect("the budget fits");
                let memory = block::Heap::new(pool.reservation());
                let pool = Rc::new(block::Pool::new(pool, memory));
                // A node gives no read of its chunk store, so the test opens the store
                // at its private path.
                let store = blob::Store::open(blob::Config {
                    files: own.files(),
                    dir: crate::directory::blob(),
                    pool,
                    floor_bytes: 0,
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
                peer(&hosts[1], &members, move |mesh, host| async move {
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

        /// The founding of [`pair`] with `plant.time` (key 1), homed at `home`, and
        /// `plant.value` (key 2).
        fn paired(hosts: &[sim::node::Node; 2], home: types::node::Key) -> Founding {
            use super::super::hub::{I64, data, index};
            let mut founding = region(&pair(hosts));
            let channels = [("plant.time", index(1)), ("plant.value", data(2, I64, 1))];
            for (name, channel) in channels {
                let name = name.parse().unwrap();
                founding
                    .definitions
                    .insert(name, Definition::Channel(channel));
            }
            founding
                .homes
                .insert(types::channel::Key::from_u128(1), home);
            founding
        }

        /// Starts the member [`OTHER`] of the region `founding` on `host` without a
        /// `Node`: an endpoint and a hub with its key, which serve the mesh as a node's
        /// do. Runs `act` with the hub and the mesh, then drops the mesh and its port.
        fn peer_hub<F: Future<Output = ()> + 'static>(
            host: &sim::node::Node,
            founding: Founding,
            act: impl FnOnce(::hub::Hub, ::mesh::Mesh, sim::node::Node) -> F
            + Send
            + 'static,
        ) {
            let shard = env::shards::Config {
                name: "peer".into(),
                core: None,
            };
            let own = host.clone();
            let started = host.shards().start(shard, move |tasks| async move {
                let identity = Identity {
                    key: OTHER.0,
                    private_key: OTHER.1,
                    opener: Opener::from_bytes(SEAL),
                };
                let key = identity.key;
                let Opened {
                    hub,
                    mesh,
                    store,
                    transport,
                    time,
                    entropy,
                } = open(&own, &tasks, identity, founding).await;
                let settings = crate::Settings::default();
                let clock = own.clock();
                let ops = operations(&mesh, key, &hub, &own, &tasks, &time, &entropy);
                let port = route::accept(
                    transport,
                    hub.clone(),
                    Some(route::Region { ops, store }),
                    clock,
                    tasks,
                    settings.header_wait,
                    settings.places_max,
                );
                let (mut port, mut act) =
                    (pin!(port), pin!(act(hub, mesh.clone(), own)));
                poll_fn(|cx| {
                    let stopped = port.as_mut().poll(cx);
                    assert!(stopped.is_pending(), "the peer's transport stopped");
                    act.as_mut().poll(cx)
                })
                .await;
                drop(mesh);
            });
            drop(started.expect("the peer starts"));
        }

        /// A task of the node reads the frames that the peer, the home, writes.
        #[test]
        fn a_node_reads_from_the_home_of_a_peer() {
            use super::super::hub::{samples, write, writer};
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let founding = paired(&hosts, OTHER.0);
            let node = start(&hosts[0], founding.clone());
            let read = Arc::new(Mutex::new(None));
            let out = Arc::clone(&read);
            node.spawn(move |hub| async move {
                let reader = hub
                    .reader(unnamed("plant.value", ::hub::reader::Mode::Complete))
                    .await;
                let mut reader = match reader {
                    Ok(reader) => reader,
                    Err(error) => {
                        *out.lock().unwrap() = Some(Err(error.to_string()));
                        return;
                    }
                };
                let received = reader.next().await;
                *out.lock().unwrap() = Some(match received {
                    Ok(received) => Ok(samples(&received, 2)),
                    Err(ended) => Err(ended.to_string()),
                });
            });
            peer_hub(&hosts[1], founding, move |hub, _, host| async move {
                let clock = host.clock();
                let mut writer = writer(&hub, &["plant.value"]).await;
                for at in 0..1000 {
                    write(&mut writer, super::super::hub::WALL + at, 7);
                    clock.sleep(Span::from_nanos(10_000_000)).await;
                }
                std::future::pending::<()>().await;
            });
            assert_eq!(sim.run_for(TWENTY), Ok(()));
            node.stop();
            assert_eq!(sim.run_for(TEN), Ok(()));
            assert_eq!(node.join(), Ok(()));
            assert_eq!(read.lock().unwrap().take(), Some(Ok(vec![7])));
        }

        /// A node stops while a task holds a reader at the peer's home, or waits in
        /// its open.
        #[test]
        fn a_node_stops_while_a_reader_at_a_peer_is_open() {
            use super::super::hub::{WALL, write, writer};
            for opened in [true, false] {
                let mut sim = sim::Sim::new(sim::Config::default());
                let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
                let founding = paired(&hosts, OTHER.0);
                let mut theirs = founding.clone();
                if !opened {
                    theirs.homes.clear();
                }
                let node = start(&hosts[0], founding);
                let read = Arc::new(Mutex::new(Vec::new()));
                let out = Arc::clone(&read);
                node.spawn(move |hub| async move {
                    let reader = hub
                        .reader(unnamed("plant.value", ::hub::reader::Mode::Complete))
                        .await;
                    out.lock().unwrap().push(reader.is_ok());
                    let mut reader = reader.expect("the reader opens");
                    let next = reader.next().await.is_ok();
                    out.lock().unwrap().push(next);
                    std::future::pending::<()>().await;
                });
                peer_hub(&hosts[1], theirs, move |hub, _, host| async move {
                    if opened {
                        let clock = host.clock();
                        let mut writer = writer(&hub, &["plant.value"]).await;
                        for at in 0..100 {
                            write(&mut writer, WALL + at, 7);
                            clock.sleep(Span::from_nanos(10_000_000)).await;
                        }
                    }
                    std::future::pending::<()>().await;
                });
                assert_eq!(sim.run_for(TEN), Ok(()), "opened {opened}");
                assert_eq!(
                    *read.lock().unwrap(),
                    if opened { vec![true, true] } else { vec![] },
                    "opened {opened}"
                );
                node.stop();
                assert_eq!(sim.run_for(TEN), Ok(()), "opened {opened}");
                assert_eq!(node.join(), Ok(()), "opened {opened}");
                let node = start(&hosts[0], paired(&hosts, OTHER.0));
                assert_eq!(sim.run_for(TEN), Ok(()), "opened {opened}");
                node.stop();
                assert_eq!(sim.run_for(TEN), Ok(()), "opened {opened}");
                assert_eq!(node.join(), Ok(()), "opened {opened}");
            }
        }

        /// A home that the peer sets, while it holds readers at the node's home,
        /// commits while the readers leave its mesh a stream to the node.
        #[test]
        fn readers_at_a_home_leave_the_mesh_a_stream() {
            for seed in 0..8 {
                assert!(set_with_readers(seed, 63), "seed {seed}");
            }
            // A known bug: 64 readers take each stream, so a peer that is a follower
            // cannot forward the change. #2019 asserts that each seed commits.
            let committed = (0..8).filter(|&seed| set_with_readers(seed, 64)).count();
            assert!(committed < 8, "64 readers: {committed} of 8 seeds commit");
        }

        /// Whether a home that the peer sets, while it holds `count` readers at the
        /// node's home, commits in a run of `seed`.
        fn set_with_readers(seed: u64, count: usize) -> bool {
            let case = (seed, count);
            let mut sim = sim::Sim::new(sim::Config {
                seed,
                ..sim::Config::default()
            });
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let founding = paired(&hosts, OWN);
            let node = start(&hosts[0], founding.clone());
            assert_eq!(sim.run_for(TEN), Ok(()), "case {case:?}");
            let results = Arc::new(Mutex::new((0, Vec::new())));
            let out = Arc::clone(&results);
            peer_hub(&hosts[1], founding, move |hub, mesh, host| async move {
                host.clock().sleep(Span::SECOND).await;
                let mut readers = Vec::new();
                for _ in 0..count {
                    let mode = ::hub::reader::Mode::Latest;
                    let reader = hub.reader(unnamed("plant.value", mode)).await;
                    readers.push(reader.expect("the reader opens"));
                    out.lock().unwrap().0 += 1;
                }
                let set = mesh.set_home(INDEX, OTHER.0).await;
                out.lock().unwrap().1.push(set);
                std::future::pending::<()>().await;
            });
            assert_eq!(sim.run_for(Span::MINUTE), Ok(()), "case {case:?}");
            let committed = {
                let (opened, set) = &*results.lock().unwrap();
                assert_eq!(*opened, count, "case {case:?}");
                match set.as_slice() {
                    [] => false,
                    [Ok(())] => true,
                    other => panic!("{case:?}: {other:?}"),
                }
            };
            node.stop();
            assert_eq!(sim.run_for(TEN), Ok(()), "case {case:?}");
            assert_eq!(node.join(), Ok(()), "case {case:?}");
            committed
        }

        /// The peer reads the frames that a task of the node, the home, writes.
        #[test]
        fn a_peer_reads_from_the_home_of_the_node() {
            use super::super::hub::{samples, write, writer};
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let founding = paired(&hosts, OWN);
            let node = start(&hosts[0], founding.clone());
            let clock = hosts[0].clock();
            node.spawn(move |hub| async move {
                let mut writer = writer(&hub, &["plant.value"]).await;
                for at in 0..1000 {
                    write(&mut writer, super::super::hub::WALL + at, 7);
                    clock.sleep(Span::from_nanos(10_000_000)).await;
                }
                std::future::pending::<()>().await;
            });
            let read = Arc::new(Mutex::new(None));
            let out = Arc::clone(&read);
            peer_hub(&hosts[1], founding, move |hub, _, _| async move {
                let reader = hub
                    .reader(unnamed("plant.value", ::hub::reader::Mode::Complete))
                    .await;
                let mut reader = match reader {
                    Ok(reader) => reader,
                    Err(error) => {
                        *out.lock().unwrap() = Some(Err(error.to_string()));
                        return;
                    }
                };
                let received = reader.next().await;
                *out.lock().unwrap() = Some(match received {
                    Ok(received) => Ok(samples(&received, 2)),
                    Err(ended) => Err(ended.to_string()),
                });
                std::future::pending::<()>().await;
            });
            assert_eq!(sim.run_for(TWENTY), Ok(()));
            assert_eq!(read.lock().unwrap().take(), Some(Ok(vec![7])));
            node.stop();
            assert_eq!(sim.run_for(TEN), Ok(()));
            assert_eq!(node.join(), Ok(()));
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
            let millis = i64::try_from(applied_key(&mut sim, &host).as_u128() >> 80);
            let start = sim::node::Config::default().wall.nanos() / 1_000_000;
            let millis = millis.unwrap();
            assert!(
                (start..start + 1_000).contains(&millis),
                "{millis} ms is not in the first second of {start} ms"
            );
        }

        /// A channel that an apply makes while mesh time is before 1970 has the time 0.
        #[test]
        fn an_applied_channel_before_1970_has_a_key_at_the_epoch() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = sim.node(sim::node::Config {
                cores: NonZeroUsize::new(2).unwrap(),
                wall: types::time::Stamp::EPOCH - Span::HOUR,
                ..sim::node::Config::default()
            });
            let identity = Identity {
                key: OWN,
                private_key: KEY,
                opener: Opener::from_bytes(SEAL),
            };
            write_key(&mut sim, &host, identity::encode(&identity).to_vec());
            assert_eq!(applied_key(&mut sim, &host).as_u128() >> 80, 0);
        }

        /// A key made after the error of the wall clock becomes known is no earlier
        /// than a key made while it was unknown, and both are near the wall.
        #[test]
        fn a_channel_key_after_the_wall_error_shrinks_is_no_earlier() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = sim.node(sim::node::Config {
                cores: NonZeroUsize::new(2).unwrap(),
                wall_error: None,
                ..sim::node::Config::default()
            });
            let identity = Identity {
                key: OWN,
                private_key: KEY,
                opener: Opener::from_bytes(SEAL),
            };
            write_key(&mut sim, &host, identity::encode(&identity).to_vec());
            let ten_ms = Span::from_nanos(10 * Span::MILLISECOND.nanos());
            let [first, second] = key_times(&mut sim, &host, move |host| {
                host.set_wall_error(Some(ten_ms));
            });
            let start = sim::node::Config::default().wall.nanos() / 1_000_000;
            assert!((start..start + 1_000).contains(&first), "{first} ms");
            let wall = first + 600_000..first + 605_000;
            assert!(wall.contains(&second), "{second} ms is not in {wall:?}");
        }

        /// A key made after the wall steps back from past 2162 is no earlier than a key
        /// made before the step, though the latest edge of mesh time stops at the end
        /// of the stamp range.
        #[test]
        fn a_channel_key_after_the_wall_steps_back_from_2171_is_no_earlier() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let jump = Span::from_nanos(145 * 365 * Span::DAY.nanos());
            host.step_wall(jump);
            let back = Span::from_nanos(-jump.nanos());
            let [first, second] =
                key_times(&mut sim, &host, move |host| host.step_wall(back));
            let start = (sim::node::Config::default().wall + jump).nanos() / 1_000_000;
            assert!((start..start + 1_000).contains(&first), "{first} ms");
            assert!(first <= second, "{second} ms is before {first} ms");
        }

        /// A key made while the clock holds over, after the wall error becomes
        /// unknown, has mesh time, not the time 0.
        #[test]
        fn a_channel_key_in_holdover_has_mesh_time() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let [first, second] =
                key_times(&mut sim, &host, |host| host.set_wall_error(None));
            let wall = first + 600_000..first + 605_000;
            assert!(wall.contains(&second), "{second} ms is not in {wall:?}");
        }

        /// The key times in ms of `plant.a` and `plant.b`, which a node of `host`
        /// applies 600 s apart, with `change` called on `host` between them.
        fn key_times(
            sim: &mut sim::Sim,
            host: &sim::node::Node,
            change: impl FnOnce(&sim::node::Node) + Send + 'static,
        ) -> [i64; 2] {
            let node = start(host, region(&[member(OWN, &KEY, host)]));
            let made = Arc::new(Mutex::new(Vec::new()));
            let out = Arc::clone(&made);
            let host = host.clone();
            node.operate(move |ops| async move {
                let first = apply_index(&ops, "a").await;
                change(&host);
                host.clock()
                    .sleep(Span::from_nanos(600 * Span::SECOND.nanos()))
                    .await;
                let second = apply_index(&ops, "b").await;
                out.lock().unwrap().extend([first, second]);
            });
            let eleven_minutes = Span::from_nanos(660 * Span::SECOND.nanos());
            assert_eq!(sim.run_for(eleven_minutes), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let millis: Vec<i64> = made
                .lock()
                .unwrap()
                .iter()
                .map(|key| i64::try_from(key.as_u128() >> 80).unwrap())
                .collect();
            millis.try_into().expect("two keys")
        }

        /// A channel key made before the clock has mesh time has the time 0. It calls
        /// `channel_key` directly: through `operate` the clock always has mesh time,
        /// since shard 0 runs it before the mesh opens and the source measures at once.
        #[test]
        fn a_channel_key_before_mesh_time_has_the_time_0() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 1);
            let key = sim
                .run_on(&host, |host, _| async move {
                    let (_driver, clock) = ::clock::Clock::new(host.clock());
                    crate::channel_key(&clock, &host.entropy())
                })
                .expect("the run ends");
            assert_eq!(key.as_u128() >> 80, 0);
        }

        /// The key of the index `plant.time` that an apply makes on a node of `host`,
        /// which [`keyed`] made, alone in its region.
        fn applied_key(sim: &mut sim::Sim, host: &sim::node::Node) -> channel::Key {
            let node = start(host, region(&[member(OWN, &KEY, host)]));
            let made = Arc::new(Mutex::new(None));
            let out = Arc::clone(&made);
            node.operate(move |ops| async move {
                *out.lock().unwrap() = Some(apply_index(&ops, "time").await);
            });
            assert_eq!(sim.run_for(HALF_MINUTE), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            made.lock().unwrap().take().expect("a channel")
        }

        const HALF_MINUTE: Span = Span::from_nanos(30 * Span::SECOND.nanos());

        /// Applies the index `plant.<name>` homed on [`OWN`] through `ops`, and gives
        /// its key.
        async fn apply_index(ops: &ops::Node, name: &str) -> channel::Key {
            key_of(&apply(ops, text(name, &[])).await, name)
        }

        /// The text of a spec with the index `plant.<index>` homed on [`OWN`] and an
        /// `i64` data channel `plant.<name>` on it for each of `data`.
        fn text(index: &str, data: &[&str]) -> String {
            use std::fmt::Write as _;
            let mut text = format!(
                "channel \"plant.{index}\" {{ kind = \"index\" }}\n\
                 placement \"plant\" {{\n  select = \"plant.*\"\n  home = \"plant.node{OWN}\"\n}}\n"
            );
            for name in data {
                writeln!(
                    text,
                    "channel \"plant.{name}\" {{\n  data_type = \"i64\"\n  index = \"plant.{index}\"\n}}"
                )
                .unwrap();
            }
            text
        }

        /// Applies `text` through `ops`, and gives the definitions of the spec in use
        /// after it.
        async fn apply(ops: &ops::Node, text: String) -> BTreeMap<Name, Definition> {
            let files = vec![(std::path::PathBuf::from("plant.hcl"), text)];
            let (plan, _) = ops.plan(files).await.expect("a plan");
            let path = std::path::Path::new("plant.plan");
            ops.apply(path, &plan).await.expect("an apply");
            Rc::unwrap_or_clone(ops.mesh().spec().await.expect("a spec").definitions)
        }

        /// The key of the channel `plant.<name>` of `definitions`.
        fn key_of(
            definitions: &BTreeMap<Name, Definition>,
            name: &str,
        ) -> channel::Key {
            let label = Kind::Channel.key(&format!("plant.{name}")).unwrap();
            let Some(Definition::Channel(channel)) = definitions.get(&label) else {
                panic!("a channel at plant.{name}");
            };
            channel.key
        }

        /// Starts the node [`OWN`] on `host` with `founding` and applies `text` through
        /// its operations, then gives the node and the definitions after the apply.
        fn applied(
            sim: &mut sim::Sim,
            host: &sim::node::Node,
            founding: Founding,
            text: String,
        ) -> (Node, BTreeMap<Name, Definition>) {
            applied_on(sim, start(host, founding), text)
        }

        /// Applies `text` through the operations of the started `node`, then gives
        /// the node and the definitions after the apply.
        fn applied_on(
            sim: &mut sim::Sim,
            node: Node,
            text: String,
        ) -> (Node, BTreeMap<Name, Definition>) {
            let applied = Arc::new(Mutex::new(None));
            let out = Arc::clone(&applied);
            node.operate(move |ops| async move {
                let definitions = apply(&ops, text).await;
                *out.lock().unwrap() = Some(definitions);
            });
            assert_eq!(sim.run_for(HALF_MINUTE), Ok(()));
            let definitions = applied.lock().unwrap().take().expect("an apply");
            (node, definitions)
        }

        /// The hub knows each channel that an apply adds, so a task opens a writer and
        /// a reader on them by name.
        #[test]
        fn a_task_opens_sessions_on_the_channels_that_an_apply_adds() {
            use super::super::hub::{WALL, samples, write_at, writer};
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let founding = region(&[member(OWN, &KEY, &host)]);
            let (node, definitions) =
                applied(&mut sim, &host, founding, text("time", &["value"]));
            let keys =
                ["time", "value"].map(|name| key_of(&definitions, name).as_u128());
            let read = Arc::new(Mutex::new(None));
            let out = Arc::clone(&read);
            node.spawn(move |hub| async move {
                let mode = ::hub::reader::Mode::Complete;
                let reader = hub.reader(unnamed("plant.value", mode)).await;
                let mut reader = reader.expect("the reader opens");
                let mut writer = writer(&hub, &["plant.value"]).await;
                write_at(&mut writer, keys, WALL, 7);
                let received = reader.next().await.expect("a frame");
                *out.lock().unwrap() =
                    Some((samples(&received, keys[0]), samples(&received, keys[1])));
            });
            assert_eq!(sim.run_for(TEN), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            assert_eq!(read.lock().unwrap().take(), Some((vec![WALL], vec![7])));
        }

        /// A task that a caller of `Mesh::spec` tells of a spec in use opens a reader on
        /// a channel of that spec, in each order of the tasks that the seed picks.
        #[test]
        fn a_task_told_of_a_spec_in_use_opens_a_reader_on_its_channel() {
            let mut failed = Vec::new();
            for seed in 0..32 {
                let mut sim = sim::Sim::new(sim::Config {
                    seed,
                    ..sim::Config::default()
                });
                let host = keyed(&mut sim, 2);
                let node = start(&host, region(&[member(OWN, &KEY, &host)]));
                let told = Arc::new(Mutex::new((false, None::<std::task::Waker>)));
                let opened = Arc::new(Mutex::new(None));
                let (wait, out) = (Arc::clone(&told), Arc::clone(&opened));
                node.spawn(move |hub| async move {
                    poll_fn(|cx| {
                        let mut told = wait.lock().unwrap();
                        if told.0 {
                            return Poll::Ready(());
                        }
                        told.1 = Some(cx.waker().clone());
                        Poll::Pending
                    })
                    .await;
                    let mode = ::hub::reader::Mode::Complete;
                    let reader = hub.reader(unnamed("plant.value", mode)).await;
                    *out.lock().unwrap() = Some(reader.err().map(|e| format!("{e:?}")));
                });
                let tell = Arc::clone(&told);
                node.operate(move |ops| async move {
                    apply(&ops, text("time", &["value"])).await;
                    let waker = {
                        let mut told = tell.lock().unwrap();
                        told.0 = true;
                        told.1.take()
                    };
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                });
                assert_eq!(sim.run_for(HALF_MINUTE), Ok(()));
                node.stop();
                assert_eq!(sim.run(), Ok(()));
                assert_eq!(node.join(), Ok(()));
                let opened = opened.lock().unwrap().take();
                if opened != Some(None) {
                    failed.push((seed, opened));
                }
            }
            assert_eq!(failed, []);
        }

        /// An apply that removes a channel ends each reader on it.
        #[test]
        fn an_apply_that_removes_a_channel_ends_each_reader_on_it() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let founding = region(&[member(OWN, &KEY, &host)]);
            let (node, definitions) =
                applied(&mut sim, &host, founding, text("time", &["value"]));
            let ended = Arc::new(Mutex::new(None));
            let out = Arc::clone(&ended);
            node.spawn(move |hub| async move {
                let mode = ::hub::reader::Mode::Complete;
                let reader = hub.reader(unnamed("plant.value", mode)).await;
                let mut reader = reader.expect("the reader opens");
                *out.lock().unwrap() = Some(reader.next().await.map(|_| ()));
            });
            assert_eq!(sim.run_for(TEN), Ok(()));
            assert_eq!(*ended.lock().unwrap(), None);
            node.operate(|ops| async move {
                apply(&ops, text("time", &[])).await;
            });
            assert_eq!(sim.run_for(HALF_MINUTE), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let removed = ::hub::reader::Ended::Removed(key_of(&definitions, "value"));
            assert_eq!(ended.lock().unwrap().take(), Some(Err(removed)));
        }

        /// A node that opens again after an apply defines the spec in use, not the
        /// founding.
        #[test]
        fn a_node_that_opens_again_after_an_apply_knows_the_applied_channels() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let (node, _) =
                applied(&mut sim, &host, founded(&host), text("time", &["flow"]));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let node = start(&host, founded(&host));
            let empty = ::hub::reader::Error::Empty;
            let readers = readers(&mut sim, &node, &["plant.value", "plant.flow"]);
            assert_eq!(readers, [Some(empty), None]);
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
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

        /// A node with a region whose disk budget holds no ring drops a task of
        /// `operate` uncalled, and `join` gives the disk error.
        #[test]
        fn operate_on_a_regional_node_with_too_little_disk_drops_the_task() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let smallest = ::buffer::Layout::fit(0, crate::BODY_MAX).unwrap_err().min;
            let disk = Size::from_bytes(smallest);
            let node = Node::start(Config {
                region: Region::Given(region(&[member(OWN, &KEY, &host)])),
                budget: Budget {
                    pool: Size::MEBIBYTE,
                    disk,
                },
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            node.operate(|_| async {});
            assert_eq!(sim.run(), Ok(()));
            let min = Size::from_bytes(8_626_176);
            assert_eq!(
                node.join(),
                Err(Error::Disk {
                    disk,
                    cores: 2,
                    min
                })
            );
        }

        /// A plan through `operate` takes each connector kind of the node, and gives
        /// `connector.unknown-kind` for another kind.
        #[test]
        fn a_plan_knows_the_connector_kinds_of_the_node() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let node = start(&host, region(&[member(OWN, &KEY, &host)]));
            let planned = Arc::new(Mutex::new(Vec::new()));
            let out = Arc::clone(&planned);
            node.operate(move |ops| async move {
                let modbus = format!("{CHANNELS}{}", modbus());
                let texts = [connector("influx"), modbus, connector("mqtt")];
                for text in texts {
                    let files = vec![(PathBuf::from("plant.hcl"), text)];
                    let plan = ops.plan(files).await;
                    let plan = plan.map(|_| ()).map_err(|error| error.to_string());
                    out.lock().unwrap().push(plan);
                }
            });
            assert_eq!(sim.run_for(HALF_MINUTE), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let unknown = concat!(
                r#"{"errors":[{"code":"connector.unknown-kind","#,
                r#""fix":"Use one of [\"influx\", \"modbus\", \"opcua\"]","#,
                r#""message":"this build has no connector kind \"mqtt\"","#,
                r#""notes":[],"place":{"column":10,"file":"plant.hcl","line":2}}]}"#,
            );
            let planned = planned.lock().unwrap();
            assert_eq!(*planned, [Ok(()), Ok(()), Err(unknown.to_owned())]);
        }

        /// The node runs a connector that an apply places on it, and runs it again when
        /// an apply changes its config. Its status gives how the run ended: `influx`
        /// ends with a config error, since its `select` matches no channel, and does
        /// not restart. The reader opens after the first run, because the status
        /// channels come with the connector.
        #[test]
        fn a_node_runs_a_connector_that_an_apply_places_on_it() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let founding = region(&[member(OWN, &KEY, &host)]);
            let text = connector("influx");
            let (node, definitions) = applied(&mut sim, &host, founding, text.clone());
            let read = read_status(&node, &definitions);
            assert_eq!(sim.run_for(TEN), Ok(()));
            node.operate(|ops| async move {
                apply(&ops, text.replace("influx:8086", "influx:8087")).await;
            });
            assert_eq!(sim.run_for(HALF_MINUTE), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            // `state` running, ending, then stopped; `class` none, then config.
            assert_eq!(*read.lock().unwrap(), [(3, 0, 0), (4, 3, 1), (5, 2, 1)]);
        }

        /// `status` through `operate` gives the connector that an apply places on the
        /// node, read through its hub as a subject that may read its status.
        #[test]
        fn status_gives_the_connector_on_the_node() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let founding = region(&[member(OWN, &KEY, &host)]);
            let watch = format!(
                "subject \"plant.operator\" {{ keys = [\"{OPERATOR}\"] }}\n\
                 access \"plant.watch\" {{\n  subjects = \"plant.operator\"\n  \
                 select = \"plant.influx.**\"\n  allow = [read]\n}}\n"
            );
            let text = format!("{}{watch}", connector("influx"));
            let (node, _) = applied(&mut sim, &host, founding, text);
            assert_eq!(sim.run_for(TEN), Ok(()));
            let replied = Arc::new(Mutex::new(None));
            let out = Arc::clone(&replied);
            node.operate(move |ops| async move {
                let subject = "plant.operator".parse().expect("a name");
                let reply = ops::serve(&ops, &subject, b"{\"status\":{}}\n").await;
                *out.lock().unwrap() = Some(String::from_utf8(reply).expect("UTF-8"));
            });
            assert_eq!(sim.run_for(HALF_MINUTE), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let reply = replied.lock().unwrap().take().expect("a reply");
            let connectors = concat!(
                r#"{"output":{"connectors":[{"name":"plant.influx","kind":"influx","#,
                r#""state":"stopped","address":"http://influx:8086","restarts":0,"#,
                r#""counts":{"confirmed":0,"lost":0,"oversized":0},"#,
                r#""error":{"class":"config","text":"#,
                r#""the selector matches no channel. Select a channel that the spec "#,
                r#"defines"},"note":null}],"roots":null}}"#,
                "\n",
            );
            assert_eq!(reply, connectors);
        }

        /// `status` through `operate` gives each failure of the read of the OS roots
        /// of the node, in full, past the 16 that the log lists.
        #[test]
        fn status_gives_each_failure_of_the_roots_in_full() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let founding = region(&[member(OWN, &KEY, &host)]);
            let mut failures: Vec<String> =
                (0..17).map(|n| format!("failure {n}")).collect();
            failures.push(format!("head {} tail", "x".repeat(3000)));
            let node = Node::start(Config {
                region: Region::Given(founding),
                roots: os::roots::Roots {
                    failures: failures.clone(),
                    ..os::roots::Roots::default()
                },
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            let watch = format!(
                "subject \"plant.operator\" {{ keys = [\"{OPERATOR}\"] }}\n\
                 access \"plant.watch\" {{\n  subjects = \"plant.operator\"\n  \
                 select = \"plant.node{OWN}\"\n  allow = [read]\n}}\n"
            );
            let (node, _) = applied_on(&mut sim, node, watch);
            let replied = Arc::new(Mutex::new(None));
            let out = Arc::clone(&replied);
            node.operate(move |ops| async move {
                let subject = "plant.operator".parse().expect("a name");
                let reply = ops::serve(&ops, &subject, b"{\"status\":{}}\n").await;
                *out.lock().unwrap() = Some(reply);
            });
            assert_eq!(sim.run_for(HALF_MINUTE), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let reply = replied.lock().unwrap().take().expect("a reply");
            let reply: serde_json::Value =
                serde_json::from_slice(&reply).expect("JSON");
            let roots = serde_json::json!({
                "source": "the OS store",
                "failures": failures,
                "unlisted": 0,
            });
            let expected = serde_json::json!({
                "output": { "connectors": [], "roots": roots },
            });
            assert_eq!(reply, expected);
        }

        /// The node runs a `modbus` connector that an apply places on it. With no
        /// device at its address, `status` gives the device error of its lookup.
        #[test]
        fn status_gives_the_lookup_error_of_a_modbus_connector_with_no_device() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let founding = region(&[member(OWN, &KEY, &host)]);
            let watch = format!(
                "subject \"plant.operator\" {{ keys = [\"{OPERATOR}\"] }}\n\
                 access \"plant.watch\" {{\n  subjects = \"plant.operator\"\n  \
                 select = \"plant.plc.**\"\n  allow = [read]\n}}\n"
            );
            let text = format!("{CHANNELS}{}{watch}", modbus());
            let (node, _) = applied(&mut sim, &host, founding, text);
            assert_eq!(sim.run_for(TEN), Ok(()));
            let replied = Arc::new(Mutex::new(None));
            let out = Arc::clone(&replied);
            node.operate(move |ops| async move {
                let subject = "plant.operator".parse().expect("a name");
                let reply = ops::serve(&ops, &subject, b"{\"status\":{}}\n").await;
                *out.lock().unwrap() = Some(String::from_utf8(reply).expect("UTF-8"));
            });
            assert_eq!(sim.run_for(HALF_MINUTE), Ok(()));
            node.stop();
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let reply = replied.lock().unwrap().take().expect("a reply");
            let connectors = concat!(
                r#"{"output":{"connectors":[{"name":"plant.plc","kind":"modbus","#,
                r#""state":"restarting","address":"plc:502","restarts":6,"#,
                r#""counts":{"in":0,"lost":0,"missed":0},"error":{"class":"device","#,
                r#""text":"the lookup of plc failed: name plc has no address"},"#,
                r#""note":null}],"roots":null}}"#,
                "\n",
            );
            assert_eq!(reply, connectors);
        }

        /// A node that starts on a spec that places a connector on it applies that
        /// spec to its runs: it runs the connector at the start, before any apply.
        /// `influx` ends with a config error, since its `select` matches no channel, so
        /// each later apply runs it again, also one that keeps it. That apply adds
        /// `plant.other`, so the run reads it until the next apply changes the config.
        /// The reader opens after the run of the start, which no reader sees: its open
        /// and the open of that run wait for the same home. The seq of the first frame
        /// that it reads counts the three frames of that run.
        #[test]
        fn a_node_applies_the_spec_it_starts_on_to_its_runs() {
            for seed in 0..16 {
                let mut sim = sim::Sim::new(sim::Config {
                    seed,
                    ..sim::Config::default()
                });
                let host = keyed(&mut sim, 2);
                let founding = region(&[member(OWN, &KEY, &host)]);
                let placed = connector("influx");
                let (node, definitions) =
                    applied(&mut sim, &host, founding.clone(), placed.clone());
                node.stop();
                assert_eq!(sim.run(), Ok(()));
                assert_eq!(node.join(), Ok(()));
                let node = start(&host, founding);
                assert_eq!(sim.run_for(TEN), Ok(()));
                let read = read_status(&node, &definitions);
                assert_eq!(sim.run_for(TEN), Ok(()));
                let other = text("other", &[]);
                let kept = format!("{placed}{other}");
                let changed = kept.replace("influx:8086", "influx:8087");
                for text in [kept, changed] {
                    node.operate(|ops| async move {
                        apply(&ops, text).await;
                    });
                    assert_eq!(sim.run_for(HALF_MINUTE), Ok(()));
                }
                node.stop();
                assert_eq!(sim.run(), Ok(()));
                assert_eq!(node.join(), Ok(()));
                let read = read.lock().unwrap();
                let runs = [(6, 0, 0), (7, 3, 0), (8, 2, 0), (9, 0, 0)];
                assert_eq!(*read, runs, "seed {seed}");
            }
        }

        /// A connector that an apply moves from another member to this node keeps its
        /// status index and that index's home on the other member, so its run panics
        /// shard 0 (#2337).
        #[test]
        fn a_connector_that_an_apply_moves_to_the_node_panics_its_shard() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let hosts = [keyed(&mut sim, 2), keyed(&mut sim, 2)];
            let mut founding = region(&pair(&hosts));
            founding.voters = [OWN].into_iter().collect();
            let own = connector("influx");
            let away = own.replace(
                &format!("node = \"plant.node{OWN}\""),
                &format!("node = \"plant.node{}\"", OTHER.0),
            );
            let (node, _) = applied(&mut sim, &hosts[0], founding, away);
            node.operate(|ops| async move {
                apply(&ops, own).await;
            });
            let message = format!(
                "the status channels of the connector plant.influx do not open: the \
                 home of an index of the writer is node {}, and a writer writes only \
                 at this node",
                OTHER.0,
            );
            assert_eq!(
                sim.run_for(HALF_MINUTE),
                Err(sim::Error::Panicked {
                    thread: "shard-0".into(),
                    message,
                    seed: 0,
                })
            );
            assert_eq!(sim.run(), Ok(()));
            let error = node.join().unwrap_err();
            assert_eq!(
                error,
                crate::Error::Panicked(env::thread::Panicked {
                    name: "shard-0".into()
                })
            );
            assert_eq!(error.to_string(), "thread shard-0 panicked");
        }

        /// Reads each seq, `state`, and `class` of `plant.influx` that `node` writes,
        /// from a task on `node`, by the keys of `definitions`.
        fn read_status(
            node: &Node,
            definitions: &BTreeMap<Name, Definition>,
        ) -> Arc<Mutex<Vec<(u64, u8, u8)>>> {
            use super::super::hub::{bytes, seq};
            let [state, class] = ["state", "class"]
                .map(|name| key_of(definitions, &format!("influx.status.{name}")));
            let read = Arc::new(Mutex::new(Vec::new()));
            let out = Arc::clone(&read);
            node.spawn(move |hub| async move {
                let select = ["plant.influx.status.state", "plant.influx.status.class"];
                let config = ::hub::reader::Config {
                    select: types::name::Selector::new(select).expect("a selector"),
                    ..unnamed(
                        "plant.influx.status.state",
                        ::hub::reader::Mode::Complete,
                    )
                };
                let mut reader = hub.reader(config).await.expect("the reader opens");
                while let Ok(received) = reader.next().await {
                    let first = seq(&received, state.as_u128());
                    let [state, class] =
                        [state, class].map(|key| bytes(&received, key.as_u128()));
                    let seqs = (first..).zip(state).zip(class);
                    out.lock()
                        .unwrap()
                        .extend(seqs.map(|((seq, s), c)| (seq, s, c)));
                }
            });
            read
        }

        /// The config of the connector `plant.influx`: its body without `kind` and
        /// `node`.
        const INFLUX: &str = "address = \"http://influx:8086\"\nselect = \"plant.*\"\n\
                              database = \"plant\"\nmeasurement = \"telemetry\"\n\
                              reader {\n  mode = \"complete\"\n}\n";

        /// The text of a spec with the connector `plant.influx` of `kind` on [`OWN`].
        fn connector(kind: &str) -> String {
            format!(
                "connector \"plant.influx\" {{\n  kind = \"{kind}\"\n  \
                 node = \"plant.node{OWN}\"\n{INFLUX}}}\n"
            )
        }

        /// The channels that [`modbus`] reads into.
        const CHANNELS: &str = "channel \"plant.time\" { kind = \"index\" }\n\
                                channel \"plant.u16\" {\n  data_type = \"u16\"\n  \
                                index = \"plant.time\"\n}\n";

        /// The text of a spec with the connector `plant.plc` of kind `modbus` on
        /// [`OWN`], which reads `plant.u16`.
        fn modbus() -> String {
            format!(
                "connector \"plant.plc\" {{\n  kind = \"modbus\"\n  \
                 node = \"plant.node{OWN}\"\n  address = \"plc:502\"\n  \
                 rate = \"10Hz\"\n  read \"plant.u16\" {{\n    \
                 table = \"holding\"\n    address = 0\n  }}\n}}\n"
            )
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

        /// Starts an InfluxDB sim named `server` that serves `https` on port 8086 with
        /// the valid certificate of the test CA, and gives its store and its shard.
        fn influx_tls(
            sim: &mut sim::Sim,
        ) -> (
            Arc<Mutex<connector_influx::sim::Store>>,
            env::thread::Handle,
        ) {
            let influx = sim.node(sim::node::Config::default());
            let answer = sim::name::Answer::Addresses(vec![influx.addresses()[0]]);
            let name = sim::name::Config {
                answer,
                ..sim::name::Config::default()
            };
            sim.name("server", name);
            let listen = env::net::tcp::Listen {
                local: SocketAddr::new(influx.addresses()[0], 8086),
                backlog: 4,
                options: env::net::tcp::Options {
                    send_buffer_bytes: 1 << 16,
                    recv_buffer_bytes: 1 << 16,
                    unsent_bytes_max: NonZeroUsize::new(1 << 14).unwrap(),
                    delayed: false,
                },
            };
            let listener = influx.net().listen(&listen).expect("the port is free");
            let store = Arc::new(Mutex::new(connector_influx::sim::Store::default()));
            let into = Arc::clone(&store);
            let shard = env::shards::Config {
                name: "influx".into(),
                core: Some(0),
            };
            let server = influx
                .shards()
                .start(shard, move |tasks| async move {
                    let certificate = connector::http::sim::Certificate::Valid;
                    let served = connector_influx::sim::serve(
                        listener,
                        tasks,
                        connector::cancel::Token::new(),
                        connector::http::sim::Scheme::Tls(certificate),
                        into,
                        "plant".into(),
                        None,
                    );
                    panic!("the listener failed: {:?}", served.await);
                })
                .expect("the shard starts");
            (store, server)
        }

        /// The roots of `Config::roots` reach the supervisor: a node whose roots hold
        /// a root that does not parse and the test CA writes a sample to an InfluxDB
        /// that serves `https` with a certificate that this CA signs, and the connector
        /// has no `ca`.
        #[test]
        fn a_node_writes_over_https_with_the_ca_of_its_roots() {
            use super::super::hub::{WALL, write_at, writer};
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = keyed(&mut sim, 2);
            let (store, _server) = influx_tls(&mut sim);
            let text = format!(
                "{}channel \"plant.value\" {{\n  data_type = \"f64\"\n  \
                 index = \"plant.time\"\n}}\n{}",
                text("time", &[]),
                connector("influx")
                    .replace("http://influx:8086", "https://server:8086")
                    .replace("plant.*", "plant.value"),
            );
            let node = Node::start(Config {
                region: Region::Given(region(&[member(OWN, &KEY, &host)])),
                roots: os::roots::Roots {
                    certificates: vec![b"not DER".to_vec(), CA.to_vec()],
                    ..os::roots::Roots::default()
                },
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            let (node, definitions) = applied_on(&mut sim, node, text);
            let keys =
                ["time", "value"].map(|name| key_of(&definitions, name).as_u128());
            node.spawn(move |hub| async move {
                let mut writer = writer(&hub, &["plant.value"]).await;
                let value = i64::from_le_bytes(7.0_f64.to_le_bytes());
                write_at(&mut writer, keys, WALL, value);
                std::future::pending::<()>().await;
            });
            assert_eq!(sim.run_for(HALF_MINUTE), Ok(()));
            node.stop();
            assert_eq!(sim.run_for(HALF_MINUTE), Ok(()));
            assert_eq!(node.join(), Ok(()));
            let store = store.lock().unwrap();
            let times: Vec<_> = store
                .points("telemetry", &[])
                .map(|point| point.time.nanos())
                .collect();
            assert_eq!(times, [WALL]);
        }
    }
}

mod name {
    use types::name::Name;

    use super::*;
    use crate::name::{FILE, LEN};

    const NEW: &str = "name.new";

    fn name(text: &str) -> Name {
        text.parse().expect("a name")
    }

    /// What [`crate::name`] gives on `host` for `given`.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a test helper panics on a run that does not end"
    )]
    fn resolve(
        sim: &mut sim::Sim,
        host: &sim::node::Node,
        given: Option<&str>,
    ) -> Result<Name, Error> {
        let given = given.map(name);
        sim.run_on(host, move |host, _| async move {
            crate::name(&host.files(), given).await
        })
        .expect("the run ends")
    }

    /// The bytes of the file `name` on `host`, or `None` when it is not there.
    fn read(sim: &mut sim::Sim, host: &sim::node::Node) -> Option<Vec<u8>> {
        bytes(sim, host, FILE)
    }

    /// Starts a node named `text` on `host`, runs `sim` for a second, then stops it
    /// and gives the error of its join.
    fn start_and_stop(
        sim: &mut sim::Sim,
        host: &sim::node::Node,
        text: &str,
    ) -> Result<(), Error> {
        let node = Node::start(Config {
            name: name(text),
            ..config(host, Size::MEBIBYTE, Box::new(heap))
        });
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        node.join()
    }

    #[test]
    fn a_data_directory_with_no_name_gives_the_given_one_and_writes_nothing() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        assert_eq!(resolve(&mut sim, &host, None), Err(Error::Unnamed));
        assert_eq!(resolve(&mut sim, &host, Some("edge")), Ok(name("edge")));
        assert_eq!(read(&mut sim, &host), None);
        assert_eq!(
            Error::Unnamed.to_string(),
            "the data directory holds no node name; give the node a name"
        );
    }

    #[test]
    fn the_text_of_no_card_states_the_two_ways_out() {
        assert_eq!(
            Error::NoCard.to_string(),
            "the data directory has the directory of a mesh, and the file `node.key` \
             is not there or holds a key that the founding of its region does not \
             hold; restore `node.key` from a backup of this node, or give another \
             data directory"
        );
    }

    #[test]
    fn the_first_start_keeps_its_name_for_each_later_start() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        assert_eq!(start_and_stop(&mut sim, &host, "site_a.edge"), Ok(()));
        let kept = read(&mut sim, &host).expect("the file");
        assert_eq!(kept.len(), 277);
        assert_eq!(&kept[..17], b"foundation/name/1");
        assert_eq!(kept[17], 11);
        assert_eq!(&kept[18..29], b"site_a.edge");
        assert_eq!(kept[29..273], [0; 244]);
        let crc = crc32c::crc32c(&kept[..273]).to_le_bytes();
        assert_eq!(kept[273..], crc, "little-endian");
        assert_eq!(resolve(&mut sim, &host, None), Ok(name("site_a.edge")));
        let given = resolve(&mut sim, &host, Some("site_a.edge"));
        assert_eq!(given, Ok(name("site_a.edge")));
        assert_eq!(start_and_stop(&mut sim, &host, "site_a.edge"), Ok(()));
        assert_eq!(read(&mut sim, &host), Some(kept));
    }

    #[test]
    fn a_name_of_the_most_bytes_is_kept() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let long = "a".repeat(191);
        assert_eq!(start_and_stop(&mut sim, &host, &long), Ok(()));
        let kept = read(&mut sim, &host).expect("the file");
        assert_eq!((kept.len(), kept[17]), (277, 191));
        assert_eq!(resolve(&mut sim, &host, None), Ok(name(&long)));
        assert_eq!(start_and_stop(&mut sim, &host, &long), Ok(()));
    }

    /// Each file call on the name that fails stops the node with its error, at the
    /// first start and at a later one. A failed first start leaves no name, and the
    /// next start makes it.
    #[test]
    fn a_failed_file_call_on_the_name_stops_the_node() {
        use env::files::Operation::{Open, ReadAt, Remove, Rename, Sync, WriteAt};
        for (path, operation, text) in [
            (NEW, Remove, "remove of name.new failed with OS error 5"),
            (NEW, Open, "open of name.new failed with OS error 5"),
            (NEW, WriteAt, "write_at of name.new failed with OS error 5"),
            (NEW, Sync, "sync of name.new failed with OS error 5"),
            (NEW, Rename, "rename of name.new failed with OS error 5"),
            (FILE, Open, "open of name failed with OS error 5"),
            (FILE, ReadAt, "read_at of name failed with OS error 5"),
        ] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            if path == FILE {
                assert_eq!(start_and_stop(&mut sim, &host, "edge"), Ok(()));
            }
            host.fail_file(Path::new(path), operation);
            let error = Error::Directory(env::files::Error::Io {
                path: PathBuf::from(path),
                operation,
                code: 5,
            });
            let started = start_and_stop(&mut sim, &host, "edge");
            assert_eq!(started, Err(error.clone()), "{operation:?} {path}");
            assert_eq!(
                error.to_string(),
                format!("cannot use the data directory: {text}")
            );
            if path == NEW {
                assert_eq!(resolve(&mut sim, &host, None), Err(Error::Unnamed));
            }
            assert_eq!(start_and_stop(&mut sim, &host, "edge"), Ok(()));
            assert_eq!(resolve(&mut sim, &host, None), Ok(name("edge")));
        }
    }

    /// A read of the name that fails gives its error, not a name or no name.
    #[test]
    fn a_failed_read_of_the_name_gives_its_error() {
        use env::files::Operation::{Open, ReadAt};
        for operation in [Open, ReadAt] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            assert_eq!(start_and_stop(&mut sim, &host, "edge"), Ok(()));
            host.fail_file(Path::new(FILE), operation);
            let error = Error::Directory(env::files::Error::Io {
                path: PathBuf::from(FILE),
                operation,
                code: 5,
            });
            let found = resolve(&mut sim, &host, None);
            assert_eq!(found, Err(error), "{operation:?}");
            host.fail_file(Path::new(FILE), operation);
            let given = resolve(&mut sim, &host, Some("cloud"));
            assert_eq!(given, Ok(name("cloud")), "reads nothing: {operation:?}");
        }
    }

    /// `node::name` gives a given name and reads nothing, so only the start, under
    /// the lock, refuses another name.
    #[test]
    fn a_start_with_another_name_stops_and_keeps_the_file() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        assert_eq!(start_and_stop(&mut sim, &host, "edge"), Ok(()));
        let kept = read(&mut sim, &host);
        let renamed = Error::Renamed {
            stored: name("edge"),
            given: name("cloud"),
        };
        let given = resolve(&mut sim, &host, Some("cloud"));
        assert_eq!(given, Ok(name("cloud")));
        let started = start_and_stop(&mut sim, &host, "cloud");
        assert_eq!(started, Err(renamed.clone()));
        assert_eq!(read(&mut sim, &host), kept, "keeps the file");
        assert_eq!(
            renamed.to_string(),
            "the data directory holds the node edge, not cloud; give edge, or another \
             data directory"
        );
    }

    /// A region refuses a reserved name, so a start keeps none, and a later start
    /// with another name runs.
    #[test]
    fn a_reserved_name_stops_the_start_and_keeps_no_name() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        for given in ["@x", "a.@b"] {
            let reserved = Error::Reserved { name: name(given) };
            assert_eq!(start_and_stop(&mut sim, &host, given), Err(reserved));
            assert_eq!(read(&mut sim, &host), None, "keeps no name: {given}");
        }
        assert_eq!(start_and_stop(&mut sim, &host, "edge"), Ok(()));
        assert_eq!(
            Error::Reserved { name: name("a.@b") }.to_string(),
            "the name a.@b has a segment that starts with `@`, which is reserved for \
             Foundation; give another name"
        );
    }

    /// A reserved name that is not the stored name stops the start as reserved, not
    /// as renamed, and the node keeps the stored name. `node::name` does not check it.
    #[test]
    fn a_reserved_name_comes_before_the_stored_name() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        assert_eq!(start_and_stop(&mut sim, &host, "edge"), Ok(()));
        let kept = read(&mut sim, &host);
        assert_eq!(resolve(&mut sim, &host, Some("@x")), Ok(name("@x")));
        let started = start_and_stop(&mut sim, &host, "@x");
        assert_eq!(started, Err(Error::Reserved { name: name("@x") }));
        assert_eq!(read(&mut sim, &host), kept, "keeps the file");
    }

    /// The claim checks the shard count before the name.
    #[test]
    fn another_shard_count_comes_before_another_name() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        assert_eq!(start_and_stop(&mut sim, &host, "edge"), Ok(()));
        let kept = read(&mut sim, &host);
        write(&mut sim, &host, "shards-3", Vec::new());
        let started = start_and_stop(&mut sim, &host, "cloud");
        assert_eq!(
            started,
            Err(Error::Shards {
                stored: 3,
                cores: 2
            })
        );
        assert_eq!(read(&mut sim, &host), kept, "keeps the file");
    }

    /// A name that fails the claim stops the start before any ring opens.
    #[test]
    fn a_name_that_fails_the_claim_opens_no_ring() {
        enum Stored {
            Zeros,
            Kept(Name),
        }
        let renamed = Error::Renamed {
            stored: name("edge"),
            given: name("cloud"),
        };
        let cases = [
            (Stored::Zeros, Error::Name),
            (Stored::Kept(name("edge")), renamed),
        ];
        for (stored, error) in cases {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            match stored {
                Stored::Kept(stored) => sim
                    .run_on(&host, move |host, _| async move {
                        crate::name::keep(&host.files(), &stored).await
                    })
                    .expect("the run ends")
                    .expect("keeps the name"),
                Stored::Zeros => write(&mut sim, &host, FILE, vec![0; LEN]),
            }
            let started = start_and_stop(&mut sim, &host, "cloud");
            assert_eq!(started, Err(error.clone()));
            let made = ["lock", "name", "shards-2"].map(PathBuf::from);
            assert_eq!(listed(&mut sim, &host, ""), made, "{error}");
        }
    }

    /// A reserved name is the first failure, also before a disk budget that holds no
    /// ring.
    #[test]
    fn a_reserved_name_starts_no_shard_and_comes_before_a_small_disk() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let node = Node::start(Config {
            name: name("@x"),
            ..config(&host, Size::MEBIBYTE, Box::new(heap))
        });
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Err(Error::Reserved { name: name("@x") }));
        assert_eq!(host.shard_starts().len(), 0);
        let disk = Size::from_bytes(1);
        let node = Node::start(Config {
            name: name("@x"),
            budget: Budget {
                pool: Size::MEBIBYTE,
                disk,
            },
            ..config(&host, Size::MEBIBYTE, Box::new(heap))
        });
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Err(Error::Reserved { name: name("@x") }));
    }

    /// A name longer than 191 bytes stops a start with no region before the claim,
    /// and a later start with another name runs.
    #[test]
    fn a_long_name_stops_the_start_and_keeps_no_name() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let long = "a".repeat(192);
        let error = Error::Long { name: name(&long) };
        assert_eq!(start_and_stop(&mut sim, &host, &long), Err(error));
        assert_eq!(host.shard_starts().len(), 0);
        assert_eq!(read(&mut sim, &host), None, "keeps no name");
        assert_eq!(start_and_stop(&mut sim, &host, "edge"), Ok(()));
    }

    /// A long name fails after a reserved name, and before a listen address that no
    /// peer reaches and a disk budget that holds no ring.
    #[test]
    fn a_long_name_comes_after_a_reserved_name_and_before_the_other_checks() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let long = name(&"a".repeat(192));
        let reserved = name(&format!("@{}", "a".repeat(191)));
        let cases = [
            (long.clone(), Error::Long { name: long }),
            (reserved.clone(), Error::Reserved { name: reserved }),
        ];
        for (given, error) in cases {
            let node = Node::start(Config {
                name: given,
                listen: "224.0.0.1:0".parse().expect("a socket address"),
                budget: Budget {
                    pool: Size::MEBIBYTE,
                    disk: Size::from_bytes(1),
                },
                ..config(&host, Size::MEBIBYTE, Box::new(heap))
            });
            assert_eq!(sim.run(), Ok(()));
            assert_eq!(node.join(), Err(error));
        }
        assert_eq!(host.shard_starts().len(), 0);
    }

    /// A file of another length that is not 0, of another version, or that `encode`
    /// does not give, is not a name: also one whose padding is not zero, under a
    /// checksum that holds.
    #[test]
    fn a_file_that_no_node_wrote_stops_the_node_which_keeps_it() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        assert_eq!(start_and_stop(&mut sim, &host, "edge"), Ok(()));
        let kept = read(&mut sim, &host).expect("the file");
        let mut changed = kept.clone();
        changed[20] ^= 1;
        let mut padded = kept.clone();
        padded[22] = b'x';
        let checksum = crc32c::crc32c(&padded[..LEN - 4]).to_le_bytes();
        padded[LEN - 4..].copy_from_slice(&checksum);
        let mut version = kept.clone();
        version[16] = b'2';
        let checksum = crc32c::crc32c(&version[..LEN - 4]).to_le_bytes();
        version[LEN - 4..].copy_from_slice(&checksum);
        let version_long = [version.clone(), vec![0; 10]].concat();
        let short = kept[..LEN - 1].to_vec();
        let long = [kept, vec![0]].concat();
        for bytes in [short, long, changed, padded, version, version_long] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = super::host(&mut sim, 2);
            write(&mut sim, &host, FILE, bytes.clone());
            assert_eq!(resolve(&mut sim, &host, None), Err(Error::Name));
            let given = resolve(&mut sim, &host, Some("edge"));
            assert_eq!(given, Ok(name("edge")));
            assert_eq!(start_and_stop(&mut sim, &host, "edge"), Err(Error::Name));
            assert_eq!(read(&mut sim, &host), Some(bytes), "keeps the file");
        }
        assert_eq!(
            Error::Name.to_string(),
            "the file `name` in the data directory is not a node name; remove it, and \
             start the node with its name"
        );
    }

    /// The node never leaves a file `name` with no bytes or with zeros, so such a
    /// file is not a name.
    #[test]
    fn an_empty_or_zero_file_is_not_a_name() {
        for bytes in [Vec::new(), vec![0; LEN]] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            write(&mut sim, &host, FILE, bytes.clone());
            assert_eq!(resolve(&mut sim, &host, None), Err(Error::Name));
            assert_eq!(start_and_stop(&mut sim, &host, "edge"), Err(Error::Name));
            assert_eq!(read(&mut sim, &host), Some(bytes), "keeps the file");
        }
    }

    /// A start removes the file `name.new` that a crash left, of any length, and
    /// makes the name.
    #[test]
    fn a_start_removes_a_new_name_that_a_crash_left() {
        for bytes in [Vec::new(), vec![7; LEN], vec![7; 3]] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            write(&mut sim, &host, NEW, bytes);
            assert_eq!(start_and_stop(&mut sim, &host, "edge"), Ok(()));
            assert_eq!(resolve(&mut sim, &host, None), Ok(name("edge")));
            assert!(!listed(&mut sim, &host, "").contains(&PathBuf::from(NEW)));
        }
    }

    /// The error of a start whose shard 0 cannot make its directory.
    fn shard_dir_failed() -> Error {
        Error::Buffer {
            core: 0,
            error: ::buffer::Error::Files(env::files::Error::Io {
                path: PathBuf::from("shard-0"),
                operation: env::files::Operation::CreateDir,
                code: 5,
            }),
        }
    }

    /// A first start that fails after the claim, before any other sync of the data
    /// directory, has made the name durable.
    #[test]
    fn a_first_start_makes_the_name_durable() {
        for seed in 0..8 {
            let mut sim = sim::Sim::new(sim::Config {
                seed,
                ..sim::Config::default()
            });
            let host = host(&mut sim, 2);
            host.fail_file(Path::new("shard-0"), env::files::Operation::CreateDir);
            let started = start_and_stop(&mut sim, &host, "edge");
            assert_eq!(started, Err(shard_dir_failed()), "seed {seed}");
            sim.crash(&host, sim::Crash::Power);
            assert_eq!(resolve(&mut sim, &host, None), Ok(name("edge")), "{seed}");
        }
    }

    /// A process crash can leave a name that a read sees but a power cut loses. The
    /// claim of the next start makes it durable, also when the start fails after it.
    /// Takes the first crash time, on seeds 0 to 7, at which a read sees the name,
    /// and checks that a power cut then can lose it.
    #[test]
    fn a_later_start_makes_the_name_durable() {
        let mut lost = 0;
        let crashed = |seed, after| {
            let mut sim = sim::Sim::new(sim::Config {
                seed,
                ..sim::Config::default()
            });
            let host = host(&mut sim, 2);
            let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
            assert_eq!(sim.run_for(after), Ok(()), "seed {seed} at {after:?}");
            sim.crash(&host, sim::Crash::Process);
            drop(node);
            let found = resolve(&mut sim, &host, None);
            (sim, host, found)
        };
        for seed in 0..8 {
            let after = (0..10_000_000)
                .step_by(5_000)
                .map(Span::from_nanos)
                .find(|&after| crashed(seed, after).2 == Ok(name("edge")))
                .expect("a crash leaves the name");
            let (mut sim, host, _) = crashed(seed, after);
            sim.crash(&host, sim::Crash::Power);
            if resolve(&mut sim, &host, None) == Err(Error::Unnamed) {
                lost += 1;
            }
            let (mut sim, host, _) = crashed(seed, after);
            host.fail_file(Path::new("shard-0"), env::files::Operation::CreateDir);
            let started = start_and_stop(&mut sim, &host, "edge");
            assert_eq!(started, Err(shard_dir_failed()), "seed {seed} at {after:?}");
            sim.crash(&host, sim::Crash::Power);
            let found = resolve(&mut sim, &host, None);
            assert_eq!(found, Ok(name("edge")), "seed {seed} at {after:?}");
        }
        assert!(lost > 0, "no power cut lost the name");
    }

    /// Reads the name on `host` `count` times, on a shard of its own, while the run
    /// goes on. Gives each result.
    fn reads(
        host: &sim::node::Node,
        count: usize,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<Result<Name, Error>>>> {
        let found = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (slot, own) = (std::sync::Arc::clone(&found), host.clone());
        let config = env::shards::Config {
            name: "reader".into(),
            core: None,
        };
        let start = host.shards().start(config, move |_| async move {
            for _ in 0..count {
                let read = crate::name(&own.files(), None).await;
                slot.lock().unwrap().push(read);
            }
        });
        drop(start.expect("starts"));
        found
    }

    /// A read while a node starts on the data directory, for the first time or a
    /// later time, gives no name or the whole name.
    #[test]
    fn a_read_during_a_start_gives_no_name_or_the_name() {
        reads_during_a_start(&Region::Absent, 0..64);
    }

    /// As [`a_read_during_a_start_gives_no_name_or_the_name`], with a start that founds
    /// a region.
    #[test]
    fn a_read_during_a_start_that_founds_a_region_gives_no_name_or_the_name() {
        reads_during_a_start(&Region::Stored, 0..16);
    }

    /// Reads the name during a first and a later start with `region`, on each of
    /// `seeds`, and checks that each read gives no name or the whole name.
    fn reads_during_a_start(region: &Region, seeds: std::ops::Range<u64>) {
        for (seed, later) in seeds.flat_map(|seed| [(seed, false), (seed, true)]) {
            for offset in (0..200_000).step_by(20_000) {
                let mut sim = sim::Sim::new(sim::Config {
                    seed,
                    ..sim::Config::default()
                });
                let host = host(&mut sim, 2);
                let start = || {
                    Node::start(Config {
                        region: region.clone(),
                        ..config(&host, Size::MEBIBYTE, Box::new(heap))
                    })
                };
                if later {
                    let node = start();
                    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
                    node.stop();
                    assert_eq!(sim.run(), Ok(()));
                    assert_eq!(node.join(), Ok(()));
                }
                let node = start();
                assert_eq!(sim.run_for(Span::from_nanos(offset)), Ok(()));
                let found = reads(&host, 400);
                assert_eq!(sim.run_for(Span::SECOND), Ok(()));
                node.stop();
                assert_eq!(sim.run(), Ok(()));
                assert_eq!(node.join(), Ok(()));
                let kept: &[_] = if later {
                    &[Ok(name("edge"))]
                } else {
                    &[Ok(name("edge")), Err(Error::Unnamed)]
                };
                for read in found.lock().unwrap().iter() {
                    let at = format!("seed {seed}, later {later}, at {offset}");
                    assert!(kept.contains(read), "{read:?}: {at}");
                }
            }
        }
    }

    /// A crash every 25 µs of the first start, on seeds 0 to 7, leaves no name or the
    /// whole name, and the whole name once each ring has opened.
    #[test]
    fn a_crash_during_the_first_start_leaves_no_name_or_the_whole_name() {
        crashes_during_the_first_start(&Region::Absent, 0..8);
    }

    /// As [`a_crash_during_the_first_start_leaves_no_name_or_the_whole_name`], with a
    /// start that founds a region.
    #[test]
    fn a_crash_during_a_first_start_that_founds_a_region_leaves_no_name_or_the_name() {
        crashes_during_the_first_start(&Region::Stored, 0..4);
    }

    /// Crashes the first start with `region` every 25 µs, on each of `seeds`, and checks
    /// the name that a read then gives.
    fn crashes_during_the_first_start(region: &Region, seeds: std::ops::Range<u64>) {
        let crashes = [sim::Crash::Process, sim::Crash::Power];
        for (seed, crash) in seeds.flat_map(|seed| crashes.map(|crash| (seed, crash))) {
            for after in (0..).step_by(25_000).map(Span::from_nanos) {
                let mut sim = sim::Sim::new(sim::Config {
                    seed,
                    ..sim::Config::default()
                });
                let host = host(&mut sim, 2);
                let node = Node::start(Config {
                    region: region.clone(),
                    ..config(&host, Size::MEBIBYTE, Box::new(heap))
                });
                let probe = probe(&node);
                let run = sim.run_for(after);
                assert_eq!(run, Ok(()), "seed {seed}: {crash:?} at {after:?}");
                let opened = super::buffer::all_opened(&probe, after);
                sim.crash(&host, crash);
                drop(node);
                let found = resolve(&mut sim, &host, None);
                if opened {
                    let at = format!("seed {seed}: {crash:?} at {after:?}");
                    assert_eq!(found, Ok(name("edge")), "{at}");
                    break;
                }
                let kept = [Ok(name("edge")), Err(Error::Unnamed)];
                assert!(
                    kept.contains(&found),
                    "{found:?}: seed {seed}: {crash:?} at {after:?}"
                );
            }
        }
    }
}

mod budget {
    use super::*;
    use crate::budget::{FILE, LEN};

    const NEW: &str = "budget.new";

    /// What [`crate::budget`] gives on `host`.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a test helper panics on a run that does not end"
    )]
    fn kept(
        sim: &mut sim::Sim,
        host: &sim::node::Node,
    ) -> Result<Option<Budget>, Error> {
        sim.run_on(
            host,
            |host, _| async move { crate::budget(&host.files()).await },
        )
        .expect("the run ends")
    }

    fn budget(pool: u64, disk: u64) -> Budget {
        Budget {
            pool: Size::from_bytes(pool),
            disk: Size::from_bytes(disk),
        }
    }

    /// Starts a node with `budget` on `host`, runs `sim` for a second, then stops it
    /// and gives the error of its join.
    fn start_and_stop(
        sim: &mut sim::Sim,
        host: &sim::node::Node,
        budget: Budget,
    ) -> Result<(), Error> {
        let node = Node::start(Config {
            budget,
            ..config(host, Size::MEBIBYTE, Box::new(heap))
        });
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        node.join()
    }

    #[test]
    fn a_data_directory_with_no_budgets_keeps_none() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        assert_eq!(kept(&mut sim, &host), Ok(None));
        assert_eq!(bytes(&mut sim, &host, FILE), None);
    }

    /// A later start with other budgets runs on them, and leaves the kept ones.
    #[test]
    fn the_first_start_keeps_its_budgets_and_a_later_one_leaves_them() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let first = budget(1 << 20, DISK.bytes());
        assert_eq!(start_and_stop(&mut sim, &host, first), Ok(()));
        let file = bytes(&mut sim, &host, FILE).expect("the file");
        assert_eq!(file.len(), LEN);
        assert_eq!(&file[..19], b"foundation/budget/1");
        assert_eq!(file[19..27], (1u64 << 20).to_le_bytes());
        assert_eq!(file[27..35], DISK.bytes().to_le_bytes());
        assert_eq!(kept(&mut sim, &host), Ok(Some(first)));
        let later = budget(2 << 20, 2 * DISK.bytes());
        assert_eq!(start_and_stop(&mut sim, &host, later), Ok(()));
        assert_eq!(bytes(&mut sim, &host, FILE), Some(file));
        assert_eq!(kept(&mut sim, &host), Ok(Some(first)));
    }

    /// A start that fails before each shard has opened its buffer keeps nothing, so
    /// the next start can give other budgets.
    #[test]
    fn a_start_that_fails_before_each_buffer_opens_keeps_no_budgets() {
        let buffer = Error::Small {
            pool: Size::from_bytes(512 << 10),
            cores: 16,
            min: Size::from_bytes(996_352),
        };
        let disk = Error::Disk {
            disk: Size::from_bytes(1),
            cores: 2,
            min: Size::from_bytes(8_626_176),
        };
        let small = [
            (budget(512 << 10, DISK.bytes()), 16, buffer),
            (budget(1 << 20, 1), 2, disk),
        ];
        for (small, cores, error) in small {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, cores);
            let started = start_and_stop(&mut sim, &host, small);
            assert_eq!(started, Err(error));
            assert_eq!(kept(&mut sim, &host), Ok(None), "{small:?}");
            let first = budget(16 << 20, DISK.bytes() * 8);
            assert_eq!(start_and_stop(&mut sim, &host, first), Ok(()));
            assert_eq!(kept(&mut sim, &host), Ok(Some(first)));
        }
    }

    /// A file of another length, of another version, or that `encode` does not give,
    /// is not budgets.
    #[test]
    fn a_file_that_no_node_wrote_stops_the_node_which_keeps_it() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = host(&mut sim, 2);
        let first = budget(1 << 20, DISK.bytes());
        assert_eq!(start_and_stop(&mut sim, &host, first), Ok(()));
        let file = bytes(&mut sim, &host, FILE).expect("the file");
        let mut changed = file.clone();
        changed[20] ^= 1;
        let mut version = file.clone();
        version[18] = b'2';
        let checksum = crc32c::crc32c(&version[..LEN - 4]).to_le_bytes();
        version[LEN - 4..].copy_from_slice(&checksum);
        let version_long = [version.clone(), vec![0; 10]].concat();
        let short = file[..LEN - 1].to_vec();
        let long = [file, vec![0]].concat();
        let zeros = vec![0; LEN];
        for bytes in [
            short,
            long,
            changed,
            version,
            version_long,
            Vec::new(),
            zeros,
        ] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = super::host(&mut sim, 2);
            write(&mut sim, &host, FILE, bytes.clone());
            assert_eq!(kept(&mut sim, &host), Err(Error::Budget));
            let started = start_and_stop(&mut sim, &host, first);
            assert_eq!(started, Err(Error::Budget));
            assert_eq!(super::bytes(&mut sim, &host, FILE), Some(bytes));
        }
        assert_eq!(
            Error::Budget.to_string(),
            "the file `budget` in the data directory does not hold budgets that a \
             node wrote; remove it, and the next start writes it again"
        );
    }

    /// Each file call on the budgets that fails stops the node with its error. A
    /// failed keep leaves no budgets, and the next start keeps them.
    #[test]
    fn a_failed_file_call_on_the_budgets_stops_the_node() {
        use env::files::Operation::{Open, ReadAt, Remove, Rename, Sync, WriteAt};
        let first = budget(1 << 20, DISK.bytes());
        for (path, operation) in [
            (NEW, Remove),
            (NEW, Open),
            (NEW, WriteAt),
            (NEW, Sync),
            (NEW, Rename),
            (FILE, Open),
            (FILE, ReadAt),
        ] {
            let mut sim = sim::Sim::new(sim::Config::default());
            let host = host(&mut sim, 2);
            if path == FILE {
                assert_eq!(start_and_stop(&mut sim, &host, first), Ok(()));
            }
            host.fail_file(Path::new(path), operation);
            let error = Error::Directory(env::files::Error::Io {
                path: PathBuf::from(path),
                operation,
                code: 5,
            });
            let started = start_and_stop(&mut sim, &host, first);
            assert_eq!(started, Err(error), "{operation:?} {path}");
            if path == NEW {
                assert_eq!(kept(&mut sim, &host), Ok(None));
            }
            assert_eq!(start_and_stop(&mut sim, &host, first), Ok(()));
            assert_eq!(kept(&mut sim, &host), Ok(Some(first)));
        }
    }
}

/// The `Cargo.toml` of the crate of each kind of [`crate::kinds`], by kind name.
const MANIFESTS: [(&str, &str); 3] = [
    ("influx", include_str!("../../connector-influx/Cargo.toml")),
    ("modbus", include_str!("../../connector-modbus/Cargo.toml")),
    ("opcua", include_str!("../../connector-opcua/Cargo.toml")),
];

#[test]
fn each_kind_runs_its_conformance_suite() {
    let names: Vec<_> = MANIFESTS.iter().map(|&(name, _)| name).collect();
    assert_eq!(crate::kinds().names().collect::<Vec<_>>(), names);
    for (name, manifest) in MANIFESTS {
        let entry = format!(
            "[[test]]\nname = \"conformance\"\n\
             path = \"../../oracles/conformance/connector/{name}/main.rs\"\n"
        );
        let Some((_, rest)) = manifest.split_once(&entry) else {
            panic!("{name} runs no suite");
        };
        let keys = rest.lines().take_while(|line| !line.is_empty());
        // Only the features that CI turns on, never `test = false`.
        for key in keys {
            assert!(key.starts_with("required-features = "), "{name}: {key}");
        }
    }
}

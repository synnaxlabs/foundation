use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use env::thread;
use sim::shard::Fault;

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
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let host = sim.node(sim::node::Config {
        cores: NonZeroUsize::new(cores).unwrap(),
        ..sim::node::Config::default()
    });
    for &(core, fault) in faults {
        host.fail_shard(core, fault);
    }
    let node = Node::start(Config {
        shards: host.shards(),
        budget,
        memory,
    });
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
    assert_eq!(
        run.sim.run(),
        Err(sim::Error::Stuck {
            threads: vec!["shard-0".into(), "shard-1".into(), "shard-2".into()],
            seed: 7,
        })
    );
    run.node.stop();
    assert_eq!(run.sim.run(), Ok(()));
    assert_eq!(run.node.join(), Ok(()));
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
fn a_config_shows_its_budget_but_not_its_memory() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = sim.node(sim::node::Config::default());
    let config = Config {
        shards: host.shards(),
        budget: 4096,
        memory: Box::new(heap),
    };
    assert_eq!(
        format!("{config:?}"),
        "Config { shards: Shards { .. }, budget: 4096, .. }"
    );
}

#[test]
#[should_panic(expected = "pool budget 18446744073709551615 is too large")]
fn a_budget_past_the_address_space_panics_at_start() {
    drop(start_with(7, 1, &[], usize::MAX, Box::new(heap)));
}

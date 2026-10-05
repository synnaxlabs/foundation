use std::num::NonZeroUsize;

use env::thread;
use sim::shard::Fault;

use crate::{Config, Error, Node};

struct Run {
    sim: sim::Sim,
    host: sim::node::Node,
    node: Node,
}

/// Starts a node on `cores` cores of a `sim` host, after `faults` aim at its shards.
fn start(seed: u64, cores: usize, faults: &[(usize, Fault)]) -> Run {
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
    });
    Run { sim, host, node }
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
            Error::Thread(thread::Error::Panicked {
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
        Error::Thread(thread::Error::Start {
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
        Error::Thread(thread::Error::Pin {
            name: "shard-0".into(),
            core: 0
        })
    );
    assert_eq!(e.to_string(), "cannot pin thread shard-0 to core 0");
}

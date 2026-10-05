use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, PoisonError};

use env::shards::{Main, Shards};
use env::thread::{self, Handle};

use crate::{Config, Error, Node};

#[derive(Clone, Copy, Debug)]
enum Fault {
    Start,
    Pin,
    Panic,
}

/// Starts shards on a `sim` node, records each request, and injects one fault. Goes
/// when `sim` injects shard faults (#245).
struct Driver {
    inner: Shards,
    fault: Option<(usize, Fault)>,
    requests: Arc<Mutex<Vec<env::shards::Config>>>,
}

impl env::shards::Driver for Driver {
    fn cores(&self) -> NonZeroUsize {
        self.inner.cores()
    }

    fn start(
        &self,
        config: env::shards::Config,
        main: Main,
    ) -> Result<Handle, thread::Error> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(config.clone());
        let name = config.name.clone();
        match self.fault {
            Some((core, Fault::Start)) if config.core == Some(core) => {
                Err(thread::Error::Start {
                    name,
                    reason: "injected".into(),
                })
            }
            Some((core, Fault::Pin)) if config.core == Some(core) => {
                Err(thread::Error::Pin { name, core })
            }
            Some((core, Fault::Panic)) if config.core == Some(core) => {
                self.inner.start(config, move |tasks| {
                    tasks.spawn(async { panic!("injected") });
                    main(tasks)
                })
            }
            _ => self.inner.start(config, main),
        }
    }
}

struct Run {
    sim: sim::Sim,
    node: Node,
    requests: Arc<Mutex<Vec<env::shards::Config>>>,
}

fn start(cores: usize, fault: Option<(usize, Fault)>) -> Run {
    let mut sim = sim::Sim::new(sim::Config {
        seed: 7,
        ..sim::Config::default()
    });
    let cores = NonZeroUsize::new(cores).unwrap();
    let host = sim.node(sim::node::Config {
        cores,
        ..sim::node::Config::default()
    });
    let requests = Arc::default();
    let driver = Driver {
        inner: host.shards(),
        fault,
        requests: Arc::clone(&requests),
    };
    let node = Node::start(Config {
        shards: Shards::new(driver),
    });
    Run {
        sim,
        node,
        requests,
    }
}

fn requests(run: &Run) -> Vec<(String, Option<usize>)> {
    let requests = run.requests.lock().unwrap();
    requests.iter().map(|c| (c.name.clone(), c.core)).collect()
}

fn named(cores: &[usize]) -> Vec<(String, Option<usize>)> {
    cores
        .iter()
        .map(|&c| (format!("shard-{c}"), Some(c)))
        .collect()
}

#[test]
fn starts_one_pinned_shard_per_core_and_runs_until_stopped() {
    let mut run = start(3, None);
    assert_eq!(requests(&run), named(&[0, 1, 2]));
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
    let mut run = start(2, None);
    run.node.stop();
    run.node.stop();
    assert_eq!(run.sim.run(), Ok(()));
    assert_eq!(run.node.join(), Ok(()));
}

#[test]
fn a_panic_in_one_shard_stops_the_others() {
    let mut run = start(3, Some((1, Fault::Panic)));
    assert_eq!(
        run.sim.run(),
        Err(sim::Error::Panicked {
            thread: "shard-1".into(),
            message: "injected".into(),
            seed: 7,
        })
    );
    assert_eq!(run.sim.run(), Ok(()));
    let e = run.node.join().unwrap_err();
    assert_eq!(
        e,
        Error::Thread(thread::Error::Panicked {
            name: "shard-1".into()
        })
    );
    assert_eq!(e.to_string(), "thread shard-1 panicked");
}

#[test]
fn a_shard_that_cannot_start_stops_the_started_shards() {
    let mut run = start(4, Some((2, Fault::Start)));
    assert_eq!(requests(&run), named(&[0, 1, 2]));
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
    let mut run = start(2, Some((0, Fault::Pin)));
    assert_eq!(requests(&run), named(&[0]));
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

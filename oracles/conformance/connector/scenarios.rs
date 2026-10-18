//! The scenarios that each connector kind passes, and [`suite!`], which runs each of
//! them on the [`Suite`] of one kind.
//!
//! Each scenario runs two sim nodes: the plant, with a rig and the connector, and the
//! device, with the kind's simulator.

use std::cell::RefCell;
use std::future::poll_fn;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::task::Poll;

use ::sim::node::Node;
use ::sim::{Crash, Sim, link};
use connector::cancel::Token;
use connector::kind::Error;
use connector::status::{Class, Reading, State};
use connector::testing::{Rig, Run};
use document::Document;
use document::diagnostic::{Code, Diagnostic};
use env::clock::Clock;
use env::tasks::Tasks;
use types::name::Name;
use types::time::Span;

pub(crate) use inputs::{Backoff, Plant, Serve, Suite};

/// The inputs that the suite of one kind gives the scenarios.
#[expect(
    dead_code,
    reason = "a kind names one backoff form, and reads only the inputs it needs"
)]
mod inputs {
    use std::net::SocketAddr;
    use std::pin::Pin;

    use ::sim::node::Node;
    use connector::cancel::Token;
    use connector::kind::Table;
    use connector::testing::Rig;
    use document::Document;
    use document::diagnostic::Diagnostic;
    use env::tasks::Tasks;

    /// What the suite of one kind gives the scenarios.
    pub(crate) struct Suite {
        /// The table that holds the kind, and no other kind.
        pub kinds: fn() -> Table,
        /// Defines on the rig what a connector of the kind needs, starts each writer
        /// that feeds it until `until`, and gives the config of one connector that
        /// reaches the device at `address`.
        pub setup:
            for<'a> fn(Plant<'a>) -> Pin<Box<dyn Future<Output = Document> + 'a>>,
        /// Runs the kind's simulator on `address` of the device node until `until`,
        /// then closes each connection and returns.
        pub serve: fn(Serve) -> Pin<Box<dyn Future<Output = ()>>>,
        /// The count that grows while data moves.
        pub progress: &'static str,
        /// Where the kind waits between two tries of a device that fails.
        pub backoff: Backoff,
        /// Configs that the kind's check refuses, each with its exact diagnostics.
        pub refused: fn() -> Vec<(Document, Vec<Diagnostic>)>,
    }

    impl Suite {
        /// The name of the one kind of [`Suite::kinds`].
        ///
        /// # Panics
        ///
        /// When the table holds no kind or more than one.
        pub(crate) fn kind(&self) -> &'static str {
            let names: Vec<_> = (self.kinds)().names().collect();
            let [kind] = names[..] else {
                panic!("the table holds one kind, not {names:?}");
            };
            kind
        }
    }

    /// Where a kind waits between two tries of a device that fails.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Backoff {
        /// The run ends with the error, and the supervisor starts it again:
        /// Restarting, with restarts that grow.
        Restart,
        /// The run goes on and tries again after its own backoff: Running, with class
        /// Retry, an error text that names the address of the device, no restart, and
        /// no progress.
        Run,
    }

    /// The plant that [`Suite::setup`] defines on.
    pub(crate) struct Plant<'a> {
        /// The rig of the connector.
        pub rig: &'a Rig,
        /// The plant node, which runs the rig.
        pub node: &'a Node,
        /// The tasks of the shard of the rig.
        pub tasks: &'a Tasks,
        /// The address of the device.
        pub address: SocketAddr,
        /// Cancelled once the scenario ends.
        pub until: &'a Token,
    }

    /// The inputs of [`Suite::serve`].
    pub(crate) struct Serve {
        /// The device node.
        pub node: Node,
        /// The tasks of the shard of the simulator.
        pub tasks: Tasks,
        /// The address to bind.
        pub address: SocketAddr,
        /// Cancelled when the device stops.
        pub until: Token,
    }
}

/// Expands to one test for each scenario, on `$suite`, a [`Suite`].
macro_rules! suite {
    ($suite:expr) => {
        #[test]
        fn starts() {
            $crate::scenarios::starts(&$suite);
        }

        #[test]
        fn heals_once_an_absent_device_serves() {
            $crate::scenarios::heals_once_an_absent_device_serves(&$suite);
        }

        #[test]
        fn backs_off_while_the_device_does_not_answer() {
            $crate::scenarios::backs_off_while_the_device_does_not_answer(&$suite);
        }

        #[test]
        fn heals_after_the_device_stops() {
            $crate::scenarios::heals_after_the_device_stops(&$suite);
        }

        #[test]
        fn heals_after_the_device_crashes() {
            $crate::scenarios::heals_after_the_device_crashes(&$suite);
        }

        #[test]
        fn stops_while_it_runs() {
            $crate::scenarios::stops_while_it_runs(&$suite);
        }

        #[test]
        fn stops_in_a_backoff() {
            $crate::scenarios::stops_in_a_backoff(&$suite);
        }

        #[test]
        fn stops_in_a_connect() {
            $crate::scenarios::stops_in_a_connect(&$suite);
        }

        #[test]
        fn refuses_each_config_with_its_diagnostics() {
            $crate::scenarios::refuses_each_config_with_its_diagnostics(&$suite);
        }

        #[test]
        fn stops_at_a_write_channel_that_no_spec_defines() {
            $crate::scenarios::stops_at_a_write_channel_that_no_spec_defines(&$suite);
        }

        #[test]
        fn shows_a_loss_after_two_minutes() {
            $crate::scenarios::shows_a_loss_after_two_minutes(&$suite);
        }

        #[test]
        fn heals_after_an_absence_of_90_s() {
            $crate::scenarios::heals_after_an_absence_of_90_s(&$suite);
        }
    };
}

pub(crate) use suite;

const SEED: u64 = 1;
/// The port of the device.
const PORT: u16 = 9000;
/// The span of each run of the sim between two asks of the plant.
const STEP: Span = Span::from_nanos(100_000_000);
/// How soon a connector of a device that serves is Running with progress.
const START: Span = Span::from_nanos(10_000_000_000);
/// How soon a connector of an absent device shows its backoff form.
const ABSENT: Span = Span::MINUTE;
/// How long a connector of [`Backoff::Run`] holds its backoff status.
const HOLD: Span = Span::from_nanos(10_000_000_000);
/// How soon a connector shows an error once its device is lost.
const LOST: Span = Span::from_nanos(30_000_000_000);
/// How soon a connector heals: the backoff cap of 1 min, plus 10 s.
const HEAL: Span = Span::from_nanos(70_000_000_000);
/// How soon [`Run::stop`] returns.
const STOP: Span = Span::from_nanos(5_000_000_000);
/// The name of each connector of the scenarios.
const CONNECTOR: &str = "plant.device";

/// The plant side of a scenario.
struct Scene {
    suite: &'static Suite,
    rig: Rig,
    node: Node,
    tasks: Tasks,
    /// The device node.
    device: Node,
    address: SocketAddr,
    /// Cancelled once the script returns, for [`Plant::until`].
    until: Token,
    /// Cancelled to ask for a crash of the device node.
    crash: Token,
    /// Cancelled once the device node crashed.
    crashed: Token,
}

/// A simulator that [`Scene::serve`] started.
struct Served {
    until: Token,
    /// Cancelled once the simulator returned.
    ended: Token,
}

impl Scene {
    /// Defines what the kind needs, and gives the config of its connector.
    async fn setup(&self) -> Document {
        let plant = Plant {
            rig: &self.rig,
            node: &self.node,
            tasks: &self.tasks,
            address: self.address,
            until: &self.until,
        };
        (self.suite.setup)(plant).await
    }

    /// Starts the kind's simulator on a new shard of the device node.
    fn serve(&self) -> Served {
        let (until, ended) = (Token::new(), Token::new());
        let (node, address) = (self.device.clone(), self.address);
        let (stop, end, serve) = (until.clone(), ended.clone(), self.suite.serve);
        let started =
            self.device
                .shards()
                .start(shard("device"), move |tasks| async move {
                    let until = stop;
                    serve(Serve {
                        node,
                        tasks,
                        address,
                        until,
                    })
                    .await;
                    end.cancel();
                });
        drop(started.expect("the device shard starts"));
        Served { until, ended }
    }

    /// Stops the simulator, and waits until it returned.
    async fn stop(&self, served: Served) {
        served.until.cancel();
        served.ended.wait().await;
    }

    /// Crashes the device node with [`Crash::Power`], so no segment of its streams
    /// reaches the plant.
    async fn crash(&self) {
        self.crash.cancel();
        self.crashed.wait().await;
    }

    async fn supervise(&self, config: Document) -> Run {
        let name = CONNECTOR.parse().expect("a valid name");
        self.rig.supervise(self.suite.kind(), name, config).await
    }

    /// The progress count of `reading`.
    fn progress(&self, reading: &Reading) -> u64 {
        let progress: Name = self.suite.progress.parse().expect("a valid name");
        let Some(&count) = reading.counts.get(&progress) else {
            panic!("the status has no count {progress}: {reading:?}")
        };
        count
    }

    /// Waits until the connector is Running with no error and progress above `from`.
    async fn running(&self, run: &mut Run, within: Span, from: u64) -> Reading {
        let done = |reading: &Reading| {
            reading.state == State::Running
                && reading.error.is_none()
                && self.progress(reading) > from
        };
        run.until(within, done).await
    }

    /// Panics unless the error text of `reading` holds the device address.
    fn named(&self, reading: &Reading) {
        let address = self.address.to_string();
        let text = reading.error.as_ref().map(|(_, text)| text.as_str());
        let named = text.is_some_and(|text| text.contains(&address));
        assert!(named, "the error names the device: {reading:?}");
    }

    /// Waits until the connector is in the backoff of [`Suite::backoff`] with no
    /// device, and gives the status. For [`Backoff::Run`], no status within [`HOLD`]
    /// after it leaves that backoff. A status writes only on a change, so this checks
    /// each change, and none when nothing changes.
    async fn backed_off(&self, run: &mut Run) -> Reading {
        match self.suite.backoff {
            Backoff::Restart => {
                let done = |reading: &Reading| {
                    let config = matches!(reading.error, Some((Class::Config, _)));
                    config
                        || (reading.state == State::Restarting && reading.restarts >= 2)
                };
                let reading = run.until(ABSENT, done).await;
                let class = reading.error.as_ref().map(|(class, _)| *class);
                assert!(
                    matches!(class, Some(Class::Device | Class::Retry)),
                    "{reading:?}"
                );
                self.named(&reading);
                reading
            }
            Backoff::Run => {
                let reading =
                    run.until(ABSENT, |reading| reading.error.is_some()).await;
                let held = |reading: &Reading| {
                    let class = reading.error.as_ref().map(|(class, _)| *class);
                    let given = (reading.state, reading.restarts, class);
                    assert_eq!(
                        given,
                        (State::Running, 0, Some(Class::Retry)),
                        "{reading:?}"
                    );
                    assert_eq!(self.progress(reading), 0, "{reading:?}");
                    self.named(reading);
                };
                held(&reading);
                let clock: Clock = self.node.clock();
                let mut late = pin!(clock.sleep(HOLD));
                while let Some(next) = before(late.as_mut(), run.next()).await {
                    held(&next);
                }
                reading
            }
        }
    }

    /// Gives what `future` gives, and panics when it takes longer than `within`.
    async fn within<T>(
        &self,
        within: Span,
        what: &str,
        future: impl Future<Output = T>,
    ) -> T {
        let clock: Clock = self.node.clock();
        let given = before(pin!(clock.sleep(within)), future).await;
        given.unwrap_or_else(|| panic!("{what} takes more than {within}"))
    }
}

/// Gives what `future` gives, or `None` once `late` ends first.
async fn before<T>(
    mut late: Pin<&mut impl Future<Output = ()>>,
    future: impl Future<Output = T>,
) -> Option<T> {
    let mut future = pin!(future);
    poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Ready(given) => Poll::Ready(Some(given)),
        Poll::Pending => late.as_mut().poll(cx).map(|()| None),
    })
    .await
}

fn shard(name: &str) -> env::shards::Config {
    env::shards::Config {
        name: name.into(),
        core: None,
    }
}

/// Runs `script` on the plant node of a new sim, after `prepare` sets the links of
/// the sim. Crashes the device node at each ask of the script, between two runs. Once
/// the script returns, the run of the sim must end: a task or a stream that the
/// script left keeps it from its end.
fn run<S, F>(suite: &'static Suite, prepare: fn(&mut Sim, &Node, &Node), script: S)
where
    S: FnOnce(Rc<Scene>) -> F + Send + 'static,
    F: Future<Output = ()> + 'static,
{
    let mut sim = Sim::new(::sim::Config {
        seed: SEED,
        ..::sim::Config::default()
    });
    let (plant, host) = (
        sim.node(::sim::node::Config::default()),
        sim.node(::sim::node::Config::default()),
    );
    prepare(&mut sim, &plant, &host);
    let (crash, crashed, done) = (Token::new(), Token::new(), Token::new());
    let (node, inputs) = (
        plant.clone(),
        (host.clone(), crash.clone(), crashed.clone()),
    );
    let end = done.clone();
    let started = plant
        .shards()
        .start(shard("plant"), move |tasks| async move {
            let env = hub::testing::Env {
                files: node.files(),
                clock: node.clock(),
                wall: node.wall(),
                entropy: node.entropy(),
                tasks: tasks.clone(),
            };
            let rig = Rig::open(env, node.net(), (suite.kinds)()).await;
            let (host, crash, crashed) = inputs;
            let address = SocketAddr::new(host.addresses()[0], PORT);
            let scene = Rc::new(Scene {
                suite,
                rig,
                node,
                tasks,
                device: host,
                address,
                until: Token::new(),
                crash,
                crashed,
            });
            script(Rc::clone(&scene)).await;
            scene.until.cancel();
            end.cancel();
        });
    drop(started.expect("the plant shard starts"));
    while !done.cancelled() {
        sim.run_for(STEP).expect("the run goes on");
        if crash.cancelled() && !crashed.cancelled() {
            sim.crash(&host, Crash::Power);
            crashed.cancel();
        }
    }
    sim.run().expect("each task of the run ends");
}

/// No link changes.
fn direct(_: &mut Sim, _: &Node, _: &Node) {}

/// The link from the plant to the device delays each packet by 1 h.
fn unanswered(sim: &mut Sim, plant: &Node, device: &Node) {
    let delay = link::Config {
        delay: Span::HOUR,
        ..link::Config::default()
    };
    sim.link(plant, device, delay);
}

/// Within 10 s: Running, no error, no restart, and progress above 0.
pub(crate) fn starts(suite: &'static Suite) {
    run(suite, direct, move |scene| async move {
        let config = scene.setup().await;
        let served = scene.serve();
        let mut run = scene.supervise(config).await;
        let reading = scene.running(&mut run, START, 0).await;
        assert_eq!(reading.restarts, 0, "{reading:?}");
        run.stop().await.expect("the run ends with no error");
        scene.stop(served).await;
    });
}

/// With no simulator, within 1 min: the backoff of [`Suite::backoff`], an error text
/// that names the device address, and never class Config. For [`Backoff::Restart`]:
/// Restarting, class Device or Retry, and restarts that grow. For [`Backoff::Run`]:
/// Running, class Retry, no restart, and no progress, and no status in the next
/// [`HOLD`] that leaves it. Then the simulator starts, and the connector heals.
pub(crate) fn heals_once_an_absent_device_serves(suite: &'static Suite) {
    run(suite, direct, move |scene| async move {
        let config = scene.setup().await;
        let mut run = scene.supervise(config).await;
        let reading = scene.backed_off(&mut run).await;
        let from = scene.progress(&reading);
        let served = scene.serve();
        scene.running(&mut run, HEAL, from).await;
        run.stop().await.expect("the run ends with no error");
        scene.stop(served).await;
    });
}

/// The simulator serves behind a link that delays each packet by 1 h: as in
/// [`heals_once_an_absent_device_serves`], within 1 min the backoff of
/// [`Suite::backoff`], with no status in the next [`HOLD`] that leaves it.
pub(crate) fn backs_off_while_the_device_does_not_answer(suite: &'static Suite) {
    run(suite, unanswered, |scene| async move {
        let config = scene.setup().await;
        let served = scene.serve();
        let mut run = scene.supervise(config).await;
        scene.backed_off(&mut run).await;
        run.stop().await.expect("the run ends with no error");
        scene.stop(served).await;
    });
}

/// The simulator stops, each connection closed. The connector heals once it serves
/// again.
pub(crate) fn heals_after_the_device_stops(suite: &'static Suite) {
    run(suite, direct, move |scene| async move {
        lost(&scene, |scene, served| {
            Box::pin(async move { scene.stop(served).await })
        })
        .await;
    });
}

/// The device node crashes, so no close reaches the plant. The connector heals once
/// the device serves again.
pub(crate) fn heals_after_the_device_crashes(suite: &'static Suite) {
    run(suite, direct, move |scene| async move {
        lost(&scene, |scene, served| {
            drop(served);
            Box::pin(scene.crash())
        })
        .await;
    });
}

/// The device is lost to `lose` while the connector runs. Within 30 s: an error of
/// class Device or Retry, whose text names the device address. Then the simulator
/// starts again: the connector heals, and no count goes down. Each status holds the
/// form of [`Suite::backoff`]: for [`Backoff::Run`], Running with no restart and no
/// class but Retry; for [`Backoff::Restart`], restarts that grow by the heal.
async fn lost<'a>(
    scene: &'a Scene,
    lose: impl FnOnce(&'a Scene, Served) -> Pin<Box<dyn Future<Output = ()> + 'a>>,
) {
    let config = scene.setup().await;
    let served = scene.serve();
    let mut run = scene.supervise(config).await;
    let last = RefCell::new(scene.running(&mut run, START, 0).await);
    // Each reading of the run, as `Run::until` gives none but the last.
    let seen = |reading: &Reading| {
        let mut last = last.borrow_mut();
        let down = |(name, count): (&Name, &u64)| {
            reading.counts.get(name).is_none_or(|now| now < count)
        };
        let went = last.counts.iter().find(|&count| down(count));
        assert!(went.is_none(), "{went:?} went down: {reading:?}");
        assert!(reading.restarts >= last.restarts, "{reading:?}");
        if let Backoff::Run = scene.suite.backoff {
            let class = reading.error.as_ref().map(|(class, _)| *class);
            let given = (reading.state, reading.restarts);
            assert_eq!(given, (State::Running, 0), "the run goes on: {reading:?}");
            assert!(matches!(class, None | Some(Class::Retry)), "{reading:?}");
        }
        *last = reading.clone();
    };
    let restarts = last.borrow().restarts;
    lose(scene, served).await;
    let error = |reading: &Reading| {
        seen(reading);
        reading.error.is_some()
    };
    let reading = run.until(LOST, error).await;
    let class = reading.error.as_ref().map(|(class, _)| *class);
    assert!(
        matches!(class, Some(Class::Device | Class::Retry)),
        "{reading:?}"
    );
    scene.named(&reading);
    let from = scene.progress(&reading);
    let served = scene.serve();
    let healed = |reading: &Reading| {
        seen(reading);
        reading.state == State::Running
            && reading.error.is_none()
            && scene.progress(reading) > from
    };
    let reading = run.until(HEAL, healed).await;
    if let Backoff::Restart = scene.suite.backoff {
        assert!(reading.restarts > restarts, "the run restarts: {reading:?}");
    }
    run.stop().await.expect("the run ends with no error");
    scene.stop(served).await;
}

/// `Run::stop` of `run` gives `Ok` within 5 s, and the plant then holds no stream.
async fn stopped(scene: &Scene, run: Run) {
    let end = scene.within(STOP, "the stop", run.stop()).await;
    end.expect("the run ends with no error");
    assert_eq!(scene.node.streams(), 0, "the plant holds no stream");
}

/// Stops the connector while it runs.
pub(crate) fn stops_while_it_runs(suite: &'static Suite) {
    run(suite, direct, move |scene| async move {
        let config = scene.setup().await;
        let served = scene.serve();
        let mut run = scene.supervise(config).await;
        scene.running(&mut run, START, 0).await;
        stopped(&scene, run).await;
        scene.stop(served).await;
    });
}

/// Stops the connector in the backoff of [`Suite::backoff`] after failed connects.
pub(crate) fn stops_in_a_backoff(suite: &'static Suite) {
    run(suite, direct, move |scene| async move {
        let config = scene.setup().await;
        let mut run = scene.supervise(config).await;
        scene.backed_off(&mut run).await;
        stopped(&scene, run).await;
    });
}

/// Stops the connector while it connects to a device whose link delays each packet
/// by 1 h. Waits on #3040 for the form of a link that loses each packet.
pub(crate) fn stops_in_a_connect(suite: &'static Suite) {
    run(suite, unanswered, |scene| async move {
        let config = scene.setup().await;
        let served = scene.serve();
        let run = scene.supervise(config).await;
        scene.node.clock().sleep(Span::SECOND).await;
        assert_eq!(scene.node.streams(), 1, "the plant connects");
        stopped(&scene, run).await;
        scene.stop(served).await;
    });
}

/// Each config of [`Suite::refused`] gives exactly its diagnostics from the check,
/// and the same [`Error::Config`] from a supervised run.
pub(crate) fn refuses_each_config_with_its_diagnostics(suite: &'static Suite) {
    run(suite, direct, move |scene| async move {
        let kinds = (suite.kinds)();
        let refused = (suite.refused)();
        assert!(!refused.is_empty(), "the kind refuses a config");
        for (config, diagnostics) in refused {
            let checked = kinds.check(suite.kind(), None, &config);
            assert_eq!(checked.map(drop), Err(diagnostics.clone()), "{config:?}");
            let run = scene.supervise(config).await;
            match run.stop().await {
                Err(Error::Config(given)) => assert_eq!(given, diagnostics, "the run"),
                other => panic!("a config error, not {other:?}"),
            }
        }
    });
}

/// For a kind whose check gives writes, a write channel that no spec defines gives
/// Stopped, class Config, no restart, and the text of `connector.unknown-channel`. A
/// kind whose check gives no writes passes with no check, until the read form of
/// #2642.
pub(crate) fn stops_at_a_write_channel_that_no_spec_defines(suite: &'static Suite) {
    run(suite, direct, move |scene| async move {
        let config = scene.setup().await;
        let checked = (suite.kinds)().check(suite.kind(), None, &config);
        let writes = checked.expect("the config of the setup checks").writes;
        let Some(missing) = writes.first() else {
            return;
        };
        scene.rig.remove(std::slice::from_ref(missing));
        let served = scene.serve();
        let mut run = scene.supervise(config).await;
        let stopped = |reading: &Reading| reading.state == State::Stopped;
        let reading = run.until(START, stopped).await;
        let unknown = Diagnostic::new(
            Code::new("connector.unknown-channel"),
            None,
            format!("no channel is named {missing}"),
            "Define the channel in the spec".into(),
        );
        let text = Error::Config(vec![unknown.clone()]).to_string();
        let given = (reading.restarts, reading.error.clone());
        assert_eq!(given, (0, Some((Class::Config, text))), "{reading:?}");
        match run.stop().await {
            Err(Error::Config(given)) => assert_eq!(given, [unknown], "the run"),
            other => panic!("a config error, not {other:?}"),
        }
        scene.stop(served).await;
    });
}

/// Runs 2 min, then the simulator stops: an error of class Device or Retry within
/// [`LOST`].
pub(crate) fn shows_a_loss_after_two_minutes(suite: &'static Suite) {
    run(suite, direct, move |scene| async move {
        let config = scene.setup().await;
        let served = scene.serve();
        let mut run = scene.supervise(config).await;
        scene.running(&mut run, START, 0).await;
        scene
            .node
            .clock()
            .sleep(Span::from_nanos(120_000_000_000))
            .await;
        scene.stop(served).await;
        let reading = run.until(LOST, |reading| reading.error.is_some()).await;
        let class = reading.error.as_ref().map(|(class, _)| *class);
        assert!(
            matches!(class, Some(Class::Device | Class::Retry)),
            "{reading:?}"
        );
        run.stop().await.expect("the run ends with no error");
    });
}

/// With no simulator for 90 s after the check of the backoff form ends (for `Run`,
/// 100 s after the backoff form shows), then the simulator starts: the connector
/// heals.
pub(crate) fn heals_after_an_absence_of_90_s(suite: &'static Suite) {
    run(suite, direct, move |scene| async move {
        let config = scene.setup().await;
        let mut run = scene.supervise(config).await;
        let reading = scene.backed_off(&mut run).await;
        let from = scene.progress(&reading);
        scene
            .node
            .clock()
            .sleep(Span::from_nanos(90_000_000_000))
            .await;
        let served = scene.serve();
        scene.running(&mut run, HEAL, from).await;
        run.stop().await.expect("the run ends with no error");
        scene.stop(served).await;
    });
}

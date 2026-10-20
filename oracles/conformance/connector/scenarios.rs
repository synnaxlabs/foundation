//! The scenarios that each connector kind passes, and [`suite!`], which runs each of
//! them on the [`Suite`] of one kind.
//!
//! Each scenario runs two sim nodes: the plant, with a rig and the connector, and the
//! device, with the kind's simulator.

use std::cell::RefCell;
use std::env::VarError;
use std::fmt::Debug;
use std::future::poll_fn;
use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use ::sim::node::Node;
use ::sim::{Crash, Sim, link};
use connector::cancel::Token;
use connector::kind::Error;
use connector::reader::SELECT;
use connector::status::{Class, Reading, State};
use connector::testing::{Rig, Run, create_secret};
use document::diagnostic::{Code, Diagnostic};
use document::value::{self, Value};
use document::{Attribute, Document, Map};
use env::clock::Clock;
use env::rng::Rng;
use env::tasks::Tasks;
use types::name::Name;
use types::time::Span;

pub(crate) use inputs::{Backoff, Plant, Serve, Simulator, Suite};

/// The inputs that the suite of one kind gives the scenarios.
#[expect(
    dead_code,
    reason = "a kind names one backoff form, and reads only the inputs it needs"
)]
pub(crate) mod inputs {
    use std::net::SocketAddr;
    use std::pin::Pin;

    use ::sim::node::Node;
    use connector::cancel::Token;
    use connector::kind::Table;
    use connector::testing::Rig;
    use document::Document;
    use document::diagnostic::Diagnostic;
    use document::value::{self, Call, Value};
    use env::tasks::Tasks;
    use types::name::Name;

    /// What the suite of one kind gives the scenarios.
    pub(crate) struct Suite {
        /// The table that holds the kind, and no other kind.
        pub kinds: fn() -> Table,
        /// Defines on the rig what a connector of the kind needs, starts each writer
        /// that feeds it until `until`, and gives the config of one connector that
        /// reaches the device at `address`.
        pub setup:
            for<'a> fn(Plant<'a>) -> Pin<Box<dyn Future<Output = Document> + 'a>>,
        /// Runs the kind's simulator. The simulator of a kind that subscribes changes
        /// its data at least every 100 ms while it serves, so the progress count grows.
        pub serve: Simulator,
        /// The count that grows while data moves.
        pub progress: &'static str,
        /// Where the kind waits between two tries of a device that fails.
        pub backoff: Backoff,
        /// Configs that the kind's check refuses, each with its exact diagnostics.
        pub refused: fn() -> Vec<(Document, Vec<Diagnostic>)>,
        /// Marks `config`, the config of [`Suite::setup`]: each attribute that takes a
        /// secret reads one, and the value of each holds `marker`. Gives the marked
        /// config and each secret that it reads, by name and value. `None` for a kind
        /// that takes no secret, and for each kind that CONNECTOR CONFORMANCE names as
        /// not yet in scenario 7.
        pub secrets: Option<fn(config: Document, marker: &str) -> Marked>,
        /// A simulator that answers each request with an error whose text holds each
        /// header and value of the request, as a gateway in debug mode does. `None`
        /// for a kind whose device answers with a code and no text of its own.
        pub echo: Option<Simulator>,
    }

    /// The call `secret("<source>")`, for [`Suite::secrets`].
    pub(crate) fn secret(source: &str) -> value::Kind {
        let source = Value {
            kind: value::Kind::String(source.into()),
            span: None,
        };
        value::Kind::Call(Call {
            function: "secret".into(),
            function_span: None,
            arguments: vec![source],
        })
    }

    /// A marked config, and each secret that it reads, by name and value.
    pub(crate) type Marked = (Document, Vec<(Name, Vec<u8>)>);

    /// A simulator: it serves on `address` of the device node until `until`, then
    /// closes each connection and returns.
    pub(crate) type Simulator = fn(Serve) -> Pin<Box<dyn Future<Output = ()>>>;

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
        fn stops_at_a_read_channel_that_no_spec_defines() {
            $crate::scenarios::stops_at_a_read_channel_that_no_spec_defines(&$suite);
        }

        #[test]
        fn heals_after_random_faults() {
            $crate::scenarios::heals_after_random_faults(&$suite);
        }

        #[test]
        fn keeps_each_secret_from_the_status() {
            $crate::scenarios::keeps_each_secret_from_the_status(&$suite);
        }

        #[test]
        #[should_panic(
            expected = "the scenario ran past 90s of plant time, where the ring of the \
                        rig's home can lose a status (#160)"
        )]
        fn fails_past_the_ring() {
            $crate::scenarios::fails_past_the_ring(&$suite);
        }

        #[test]
        #[should_panic(
            expected = "the scenario ran past 90s of plant time, where the ring of the \
                        rig's home can lose a status (#160)"
        )]
        fn fails_50_ms_past_the_ring() {
            $crate::scenarios::fails_50_ms_past_the_ring(&$suite);
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
pub(crate) const START: Span = Span::from_nanos(10_000_000_000);
/// How soon a connector of an absent device shows its backoff form.
const ABSENT: Span = Span::MINUTE;
/// How long a connector of [`Backoff::Run`] holds its backoff status.
const HOLD: Span = Span::from_nanos(10_000_000_000);
/// How soon a connector shows an error once its device is lost.
pub(crate) const LOST: Span = Span::from_nanos(30_000_000_000);
/// How soon a connector heals: the backoff cap of 1 min, plus 10 s.
const HEAL: Span = Span::from_nanos(70_000_000_000);
/// The longest plant time of a scenario. The ring of the rig's home fills after about
/// 100 s for each kind with a suite, and then can lose a status, until #3050 trims it.
const RING: Span = Span::from_nanos(90_000_000_000);
/// How soon [`Run::stop`] returns.
const STOP: Span = Span::from_nanos(5_000_000_000);
/// The name of each connector of the scenarios.
const CONNECTOR: &str = "plant.device";
/// The runs of [`heals_after_random_faults`], each on its own seed, on each merge.
const RUNS: u64 = 1000;
/// The variable that gives the seeds of [`heals_after_random_faults`] in place of
/// [`RUNS`]: one seed in hex, such as `0x2a`, or a range `<start>..<end>` in hex, the
/// end excluded, such as `0x0..0xf4240`.
const REPLAY: &str = "CONFORMANCE_SEED";
/// The span in which [`heals_after_random_faults`] puts its faults.
const FAULTS: Span = Span::from_nanos(15_000_000_000);
/// The most faults of one run of [`heals_after_random_faults`].
const FAULTS_MAX: u64 = 8;
/// The longest error text of a status.
const TEXT_MAX: usize = 1024;
/// A name that no spec of the scenarios defines.
const UNDEFINED: &str = "conformance.undefined";
/// The text that each secret of [`Suite::secrets`] holds, which no status shows.
const MARKER: &str = "c0nformance-marker-5e7c41";
/// How long [`keeps_each_secret_from_the_status`] watches after each error.
const GLANCE: Span = Span::from_nanos(5_000_000_000);

/// The plant side of a scenario.
pub(crate) struct Scene {
    suite: &'static Suite,
    rig: Rig,
    node: Node,
    tasks: Tasks,
    /// The device node.
    device: Node,
    address: SocketAddr,
    /// Cancelled once the script returns, for [`Plant::until`].
    until: Token,
    /// What the script asks of the sim, each with the token that is cancelled once
    /// the sim did it.
    asks: Asks,
}

type Asks = Arc<Mutex<Vec<(Ask, Token)>>>;

/// What a script asks of the sim, which [`run`] does between two steps.
#[derive(Clone, Copy, Debug)]
enum Ask {
    /// Sets the link from the plant to the device, and the link back.
    Links(link::Config, link::Config),
    /// Crashes the device node with [`Crash::Power`].
    Crash,
}

/// A simulator that [`Scene::serve`] started.
pub(crate) struct Served {
    until: Token,
    /// Cancelled once the simulator returned.
    ended: Token,
}

impl Scene {
    /// Defines what the kind needs, and gives the config of its connector.
    pub(crate) async fn setup(&self) -> Document {
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
    pub(crate) fn serve(&self) -> Served {
        self.start(self.suite.serve)
    }

    /// Starts `serve` on a new shard of the device node.
    fn start(&self, serve: Simulator) -> Served {
        let (until, ended) = (Token::new(), Token::new());
        let (node, address) = (self.device.clone(), self.address);
        let (stop, end) = (until.clone(), ended.clone());
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
    pub(crate) async fn stop(&self, served: Served) {
        served.until.cancel();
        served.ended.wait().await;
    }

    /// Crashes the device node with [`Crash::Power`], so no segment of its streams
    /// reaches the plant.
    async fn crash(&self) {
        self.ask(Ask::Crash).await;
    }

    /// Sets the link from the plant to the device to `to`, and the link back to
    /// `back`.
    async fn links(&self, to: link::Config, back: link::Config) {
        self.ask(Ask::Links(to, back)).await;
    }

    /// Waits until the sim did `ask`, at the end of the current step.
    async fn ask(&self, ask: Ask) {
        let done = Token::new();
        let asks = self.asks.lock();
        asks.expect("no panic holds the lock")
            .push((ask, done.clone()));
        done.wait().await;
    }

    pub(crate) async fn supervise(&self, config: Document) -> Run {
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
    pub(crate) fn named(&self, reading: &Reading) {
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

    /// Watches the status of `run` for [`GLANCE`], then stops it, and panics at a
    /// status or an end that shows [`MARKER`].
    async fn close(&self, mut run: Run) {
        let clock: Clock = self.node.clock();
        let mut late = pin!(clock.sleep(GLANCE));
        while let Some(reading) = before(late.as_mut(), run.next()).await {
            unmarked(&reading);
        }
        let end = run.stop().await;
        unmarked(&end);
        if let Err(error) = end {
            unmarked(&error.to_string());
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
/// the sim. Does each ask of the script between two steps. Once the script returns,
/// the run of the sim must end: a task or a stream that the script left keeps it
/// from its end.
pub(crate) fn run<S, F>(
    suite: &'static Suite,
    prepare: fn(&mut Sim, &Node, &Node),
    script: S,
) where
    S: FnOnce(Rc<Scene>) -> F + Send + 'static,
    F: Future<Output = ()> + 'static,
{
    run_seeded(SEED, suite, prepare, script);
}

/// Runs `script` as [`run`] does, on a sim of `seed`.
fn run_seeded<S, F>(
    seed: u64,
    suite: &'static Suite,
    prepare: fn(&mut Sim, &Node, &Node),
    script: S,
) where
    S: FnOnce(Rc<Scene>) -> F + Send + 'static,
    F: Future<Output = ()> + 'static,
{
    let mut sim = Sim::new(::sim::Config {
        seed,
        ..::sim::Config::default()
    });
    let (plant, host) = (
        sim.node(::sim::node::Config::default()),
        sim.node(::sim::node::Config::default()),
    );
    prepare(&mut sim, &plant, &host);
    let (asks, done) = (Asks::default(), Token::new());
    let (node, inputs) = (plant.clone(), (host.clone(), Arc::clone(&asks)));
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
            let (host, asks) = inputs;
            let address = SocketAddr::new(host.addresses()[0], PORT);
            let scene = Rc::new(Scene {
                suite,
                rig,
                node,
                tasks,
                device: host,
                address,
                until: Token::new(),
                asks,
            });
            script(Rc::clone(&scene)).await;
            scene.until.cancel();
            end.cancel();
        });
    drop(started.expect("the plant shard starts"));
    let mut ran = Span::ZERO;
    while !done.cancelled() {
        assert!(
            ran < RING,
            "the scenario ran past {RING} of plant time, where the ring of the rig's \
             home can lose a status (#160)"
        );
        sim.run_for(STEP).expect("the run goes on");
        ran = Span::from_nanos(ran.nanos() + STEP.nanos());
        let asked = std::mem::take(&mut *asks.lock().expect("no panic holds the lock"));
        for (ask, done) in asked {
            match ask {
                Ask::Links(to, back) => {
                    sim.link(&plant, &host, to);
                    sim.link(&host, &plant, back);
                }
                Ask::Crash => sim.crash(&host, Crash::Power),
            }
            done.cancel();
        }
    }
    sim.run().expect("each task of the run ends");
}

/// No link changes.
pub(crate) fn direct(_: &mut Sim, _: &Node, _: &Node) {}

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
        no_count_down(&last, reading);
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

/// Panics when a count or the restarts of `reading` went down from `last`.
fn no_count_down(last: &Reading, reading: &Reading) {
    let down = |(name, count): (&Name, &u64)| {
        reading.counts.get(name).is_none_or(|now| now < count)
    };
    let went = last.counts.iter().find(|&count| down(count));
    assert!(went.is_none(), "{went:?} went down: {reading:?}");
    assert!(reading.restarts >= last.restarts, "{reading:?}");
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
/// by 1 h, at the first millisecond that the plant holds a stream, within 1 s. Waits
/// on #3040 for the form of a link that loses each packet.
pub(crate) fn stops_in_a_connect(suite: &'static Suite) {
    run(suite, unanswered, |scene| async move {
        let config = scene.setup().await;
        let served = scene.serve();
        let run = scene.supervise(config).await;
        let clock: Clock = scene.node.clock();
        let connects = async {
            while scene.node.streams() == 0 {
                clock.sleep(Span::MILLISECOND).await;
            }
        };
        scene.within(Span::SECOND, "the connect", connects).await;
        assert_eq!(scene.node.streams(), 1, "the plant connects once");
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
/// kind whose check gives no writes passes with no check.
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
        let unknown = Diagnostic::new(
            Code::new("connector.unknown-channel"),
            None,
            format!("no channel is named {missing}"),
            "Define the channel in the spec".into(),
        );
        stops_with(&scene, config, unknown).await;
        scene.stop(served).await;
    });
}

/// For a kind whose check gives a reader, a reader that selects only a name that no
/// spec defines gives Stopped, class Config, no restart, and the text of
/// `connector.no-channel`. A kind whose check gives no reader passes with no check.
pub(crate) fn stops_at_a_read_channel_that_no_spec_defines(suite: &'static Suite) {
    run(suite, direct, move |scene| async move {
        let config = scene.setup().await;
        let checked = (suite.kinds)().check(suite.kind(), None, &config);
        if checked
            .expect("the config of the setup checks")
            .reader
            .is_none()
        {
            return;
        }
        let served = scene.serve();
        let none = Diagnostic::new(
            Code::new("connector.no-channel"),
            None,
            "the selector matches no channel".into(),
            "Select a channel that the spec defines".into(),
        );
        stops_with(&scene, undefined(config), none).await;
        scene.stop(served).await;
    });
}

/// A run of `config` gives Stopped, class Config, no restart, and the text of
/// `diagnostic`, and ends with it.
async fn stops_with(scene: &Scene, config: Document, diagnostic: Diagnostic) {
    let mut run = scene.supervise(config).await;
    let stopped = |reading: &Reading| reading.state == State::Stopped;
    let reading = run.until(START, stopped).await;
    let text = Error::Config(vec![diagnostic.clone()]).to_string();
    let given = (reading.restarts, reading.error.clone());
    assert_eq!(given, (0, Some((Class::Config, text))), "{reading:?}");
    match run.stop().await {
        Err(Error::Config(given)) => assert_eq!(given, [diagnostic], "the run"),
        other => panic!("a config error, not {other:?}"),
    }
}

/// `config` with a reader that selects only [`UNDEFINED`].
fn undefined(config: Document) -> Document {
    let select = value::Kind::String(UNDEFINED.into());
    with(config, SELECT.name, select)
}

/// `config` with the attribute `key` set to `kind`, in place of one that it had.
pub(crate) fn with(config: Document, key: &str, kind: value::Kind) -> Document {
    let others = config.attributes.iter().filter(|other| &*other.key != key);
    let mut attributes: Vec<_> = others.cloned().collect();
    attributes.push(Attribute {
        key: key.into(),
        key_span: None,
        value: Value { kind, span: None },
    });
    Document {
        attributes: Map::new(attributes).expect("unique keys"),
        blocks: config.blocks,
    }
}

/// One fault of [`heals_after_random_faults`].
#[derive(Clone, Copy, Debug)]
enum Fault {
    /// Sets the link from the plant to the device, and the link back.
    Links(link::Config, link::Config),
    /// Stops the simulator, each connection closed, when it serves.
    Stop,
    /// Crashes the device node.
    Crash,
    /// Starts the simulator when it does not serve.
    Serve,
}

/// The faults of the run of `seed`, 1 to [`FAULTS_MAX`], each at its time from the
/// start of [`FAULTS`], in time order.
fn faults(seed: u64) -> Vec<(Span, Fault)> {
    let mut rng = Rng::from_seed(seed);
    let count = 1 + rng.below(FAULTS_MAX);
    let mut faults: Vec<_> = (0..count)
        .map(|_| {
            let at = below(&mut rng, FAULTS);
            let fault = match rng.below(4) {
                0 => Fault::Links(faulty(&mut rng), faulty(&mut rng)),
                1 => Fault::Stop,
                2 => Fault::Crash,
                _ => Fault::Serve,
            };
            (at, fault)
        })
        .collect();
    faults.sort_by_key(|&(at, _)| at);
    faults
}

/// A link with a delay below 2 s, a jitter below 500 ms, and a loss of 0, 0.25, 0.5,
/// 0.75, or 1. No duplication: the sim never duplicates a TCP segment.
fn faulty(rng: &mut Rng) -> link::Config {
    let quarters = u32::try_from(rng.below(5)).expect("at most 4");
    link::Config {
        delay: below(rng, Span::from_nanos(2_000_000_000)),
        jitter: below(rng, Span::from_nanos(500_000_000)),
        loss: f64::from(quarters) / 4.0,
        ..link::Config::default()
    }
}

/// A span from 0 to below `span`.
fn below(rng: &mut Rng, span: Span) -> Span {
    let nanos = u64::try_from(span.nanos()).expect("a span above 0");
    let drawn = i64::try_from(rng.below(nanos)).expect("below the span");
    Span::from_nanos(drawn)
}

/// For each seed of [`seeds`]: the connector runs, then meets the faults of
/// [`faults`] of that seed. While they come: never class Config, each error text at
/// most [`TEXT_MAX`] bytes, and no count and no restart count goes down. After the
/// last fault, both links are as at the start and the simulator serves: the
/// connector heals. A failed run panics as [`replayed`] says.
#[expect(
    clippy::disallowed_methods,
    reason = "the seeds to run are an input of the test run, not of a node"
)]
pub(crate) fn heals_after_random_faults(suite: &'static Suite) {
    replayed(seeds(std::env::var(REPLAY)), |seed| {
        run_seeded(seed, suite, direct, move |scene| faulted(scene, seed));
    });
}

/// The seeds that `value`, the value of [`REPLAY`], gives, or the [`RUNS`] seeds
/// from 0 when it is not set.
///
/// # Panics
///
/// When `value` is not UTF-8, or is not a seed or a nonempty range of seeds in hex.
fn seeds(value: Result<String, VarError>) -> RangeInclusive<u64> {
    let value = match value {
        Ok(value) => value,
        Err(VarError::NotPresent) => return 0..=RUNS - 1,
        Err(VarError::NotUnicode(value)) => panic!("{REPLAY} is not UTF-8: {value:?}"),
    };
    let hex = |text: &str| {
        let digits = text.strip_prefix("0x").unwrap_or(text);
        u64::from_str_radix(digits, 16).ok()
    };
    let range = match value.split_once("..") {
        Some((start, end)) => hex(start).zip(hex(end)).and_then(|(start, end)| {
            let last = end.checked_sub(1).filter(|&last| start <= last);
            last.map(|last| start..=last)
        }),
        None => hex(&value).map(|seed| seed..=seed),
    };
    range.unwrap_or_else(|| {
        panic!("{REPLAY} is a seed or a nonempty range of seeds in hex, not {value:?}")
    })
}

/// Calls `run` with each seed of `seeds` in order. When a call panics, panics with
/// its seed, its text, and the command that runs that seed alone.
fn replayed(seeds: RangeInclusive<u64>, run: impl Fn(u64)) {
    for seed in seeds {
        let Err(panic) = catch_unwind(AssertUnwindSafe(|| run(seed))) else {
            continue;
        };
        let text = panic.downcast_ref::<String>().map(String::as_str);
        let text = text.or_else(|| panic.downcast_ref::<&str>().copied());
        panic!(
            "the run of seed {seed:#x} fails: {}\nreplay: {REPLAY}={seed:#x} cargo \
             test -p {} --all-features --test conformance \
             heals_after_random_faults",
            text.unwrap_or("a panic with no text"),
            env!("CARGO_PKG_NAME"),
        );
    }
}

/// The script of one run of [`heals_after_random_faults`].
async fn faulted(scene: Rc<Scene>, seed: u64) {
    let config = scene.setup().await;
    let mut served = Some(scene.serve());
    let mut run = scene.supervise(config).await;
    let last = RefCell::new(scene.running(&mut run, START, 0).await);
    let check = |reading: &Reading| {
        let mut last = last.borrow_mut();
        no_count_down(&last, reading);
        let error = reading.error.as_ref();
        assert!(!matches!(error, Some((Class::Config, _))), "{reading:?}");
        let long = error.is_some_and(|(_, text)| text.len() > TEXT_MAX);
        assert!(!long, "an error text over {TEXT_MAX} bytes: {reading:?}");
        *last = reading.clone();
    };
    let clock: Clock = scene.node.clock();
    let start = clock.now();
    let slot = &mut served;
    let faults = async {
        for (at, fault) in faults(seed) {
            clock.sleep_until(start + at).await;
            match fault {
                Fault::Links(to, back) => scene.links(to, back).await,
                Fault::Stop => {
                    if let Some(served) = slot.take() {
                        scene.stop(served).await;
                    }
                }
                Fault::Crash => {
                    *slot = None;
                    scene.crash().await;
                }
                Fault::Serve => {
                    *slot = slot.take().or_else(|| Some(scene.serve()));
                }
            }
        }
        let healthy = link::Config::default();
        scene.links(healthy, healthy).await;
        *slot = slot.take().or_else(|| Some(scene.serve()));
    };
    let watch = async {
        loop {
            check(&run.next().await);
        }
    };
    // Checks each reading until the faults end; `watch` never ends.
    before(pin!(faults), watch).await;
    let from = scene.progress(&last.borrow());
    let healed = |reading: &Reading| {
        check(reading);
        reading.state == State::Running
            && reading.error.is_none()
            && scene.progress(reading) > from
    };
    run.until(HEAL, healed).await;
    run.stop().await.expect("the run ends with no error");
    if let Some(served) = served {
        scene.stop(served).await;
    }
}

/// For a kind with [`Suite::secrets`], whose config reads secrets that each hold
/// [`MARKER`], no status and no end of a run shows it. Two runs meet the simulator,
/// and are Running with progress within [`START`]. Then the simulator stops for the
/// first, and the device node crashes for the second, and each gives an error within
/// [`LOST`] and 5 s more. A third run meets no device: an error within [`START`], and
/// 5 s more. A fourth meets the echo of [`Suite::echo`]: an error within [`START`],
/// and 5 s more. Then the write form of scenario 5 for 5 s. A kind with no secret
/// passes with no check.
pub(crate) fn keeps_each_secret_from_the_status(suite: &'static Suite) {
    let Some(marked) = suite.secrets else {
        return;
    };
    run(suite, direct, move |scene| async move {
        let setup = scene.setup().await;
        let (config, secrets) = marked(setup.clone(), MARKER);
        assert_ne!(config, setup, "the kind reads a secret");
        assert!(
            !secrets.is_empty(),
            "the kind gives each secret that it reads"
        );
        for (name, value) in &secrets {
            let held = value
                .windows(MARKER.len())
                .any(|part| part == MARKER.as_bytes());
            assert!(held, "the secret {name} holds the marker");
            create_secret(&scene.node.files(), name, value).await;
        }
        let kinds = (suite.kinds)();
        let checked = kinds.check(suite.kind(), None, &config);
        let writes = checked.expect("the config with secrets checks").writes;
        let failed = |reading: &Reading| {
            unmarked(reading);
            reading.error.is_some()
        };
        let started = |reading: &Reading| {
            unmarked(reading);
            reading.state == State::Running
                && reading.error.is_none()
                && scene.progress(reading) > 0
        };
        for crashed in [false, true] {
            let served = scene.serve();
            let mut run = scene.supervise(config.clone()).await;
            run.until(START, started).await;
            if crashed {
                drop(served);
                scene.crash().await;
            } else {
                scene.stop(served).await;
            }
            run.until(LOST, failed).await;
            scene.close(run).await;
        }
        let mut run = scene.supervise(config.clone()).await;
        run.until(START, failed).await;
        scene.close(run).await;
        if let Some(echo) = suite.echo {
            let served = scene.start(echo);
            let mut run = scene.supervise(config.clone()).await;
            run.until(START, failed).await;
            scene.close(run).await;
            scene.stop(served).await;
        }
        if let Some(missing) = writes.first() {
            scene.rig.remove(std::slice::from_ref(missing));
            scene.close(scene.supervise(config).await).await;
        }
    });
}

/// Panics when the Debug text of `shown` holds [`MARKER`]. The Debug text holds each
/// field, also one that a later change adds, so a secret in any field fails.
fn unmarked(shown: &impl Debug) {
    let text = format!("{shown:?}");
    assert!(!text.contains(MARKER), "the secret shows: {text}");
}

/// A scenario that runs 1 s past [`RING`] fails.
pub(crate) fn fails_past_the_ring(suite: &'static Suite) {
    run(suite, direct, |scene| async move {
        let past = Span::from_nanos(RING.nanos() + Span::SECOND.nanos());
        scene.node.clock().sleep(past).await;
    });
}

/// A scenario that runs 50 ms past [`RING`] fails.
pub(crate) fn fails_50_ms_past_the_ring(suite: &'static Suite) {
    run(suite, direct, |scene| async move {
        let past = Span::from_nanos(RING.nanos() + 50_000_000);
        scene.node.clock().sleep(past).await;
    });
}

#[cfg(test)]
mod tests {
    //! These call the private `seeds` and `replayed`: their caller reads
    //! `CONFORMANCE_SEED`, which a test cannot set with no `unsafe`, and no kind fails
    //! a seed with no hook.

    use std::cell::RefCell;
    use std::env::VarError;
    use std::ffi::OsString;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::{replayed, seeds};

    const PACKAGE: &str = env!("CARGO_PKG_NAME");

    #[test]
    fn seeds_are_runs_or_the_hex_of_replay() {
        assert_eq!(seeds(Err(VarError::NotPresent)), 0..=999);
        assert_eq!(seeds(Ok("0x2a".into())), 42..=42);
        assert_eq!(seeds(Ok("2a".into())), 42..=42);
        assert_eq!(seeds(Ok("0xffffffffffffffff".into())), u64::MAX..=u64::MAX);
        assert_eq!(seeds(Ok("0x0..0xf4240".into())), 0..=999_999);
        assert_eq!(seeds(Ok("0x5..0x6".into())), 5..=5);
    }

    #[cfg(unix)]
    fn not_utf_8() -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0xff])
    }

    #[cfg(windows)]
    fn not_utf_8() -> OsString {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0xd800])
    }

    #[test]
    #[cfg_attr(
        unix,
        should_panic(expected = "CONFORMANCE_SEED is not UTF-8: \"\\xFF\"")
    )]
    #[cfg_attr(
        windows,
        should_panic(expected = "CONFORMANCE_SEED is not UTF-8: \"\\u{d800}\"")
    )]
    fn seeds_panic_on_a_value_that_is_not_utf_8() {
        seeds(Err(VarError::NotUnicode(not_utf_8())));
    }

    #[test]
    #[should_panic(
        expected = "CONFORMANCE_SEED is a seed or a nonempty range of seeds in hex, \
                    not \"0x2g\""
    )]
    fn seeds_panic_on_a_value_that_is_not_hex() {
        seeds(Ok("0x2g".into()));
    }

    #[test]
    #[should_panic(
        expected = "CONFORMANCE_SEED is a seed or a nonempty range of seeds in hex, \
                    not \"0x3e8..0x0\""
    )]
    fn seeds_panic_on_a_reversed_range() {
        seeds(Ok("0x3e8..0x0".into()));
    }

    #[test]
    #[should_panic(
        expected = "CONFORMANCE_SEED is a seed or a nonempty range of seeds in hex, \
                    not \"0x5..0x5\""
    )]
    fn seeds_panic_on_an_empty_range() {
        seeds(Ok("0x5..0x5".into()));
    }

    /// The seeds that `replayed` runs over 0x29 to 0x2b with `run`, and its panic text.
    fn replay(run: impl Fn(u64)) -> (Vec<u64>, String) {
        let ran = RefCell::new(Vec::new());
        let failed = catch_unwind(AssertUnwindSafe(|| {
            replayed(0x29..=0x2b, |seed| {
                ran.borrow_mut().push(seed);
                run(seed);
            });
        }));
        let panic = failed.expect_err("a run fails");
        let text = panic.downcast_ref::<String>().expect("a text");
        (ran.into_inner(), text.clone())
    }

    #[test]
    fn a_failed_run_panics_with_its_replay() {
        let (ran, text) = replay(|seed| assert!(seed != 0x2a, "a fault at {seed:#x}"));
        let expected = format!(
            "the run of seed 0x2a fails: a fault at 0x2a\nreplay: \
             CONFORMANCE_SEED=0x2a cargo test -p {PACKAGE} --all-features --test \
             conformance heals_after_random_faults",
        );
        assert_eq!(text, expected);
        assert_eq!(ran, [0x29, 0x2a], "no run after the failed one");
    }

    #[test]
    fn a_failed_run_with_a_static_text_panics_with_it() {
        let (_, text) = replay(|seed| assert!(seed != 0x29, "a fault"));
        let expected = format!(
            "the run of seed 0x29 fails: a fault\nreplay: CONFORMANCE_SEED=0x29 cargo \
             test -p {PACKAGE} --all-features --test conformance \
             heals_after_random_faults",
        );
        assert_eq!(text, expected);
    }
}

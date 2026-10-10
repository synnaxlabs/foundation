//! Tests of `Runs` on a supervisor with test kinds. `Node` runs only the kinds of
//! `node::kinds`, so a test of a spec change through `Node` cannot hold a run or
//! end one at a chosen time.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use connector::cancel;
use connector::kind::{Channels, Context, Error, Kind, Table};
use connector::testing;
use document::diagnostic::Diagnostic;
use document::encoding::Checked;
use document::value::{self, Value};
use document::{Attribute, Document, Map};
use spec::connector::Connector;
use spec::definition::Definition;
use types::name::Name;
use types::time::{Monotonic, Span};

use super::Runs;

/// The node that the runs are on.
const NODE: &str = "plant.cloud";

/// Each connector name a test uses, in the order of the keys of its status
/// channels.
const NAMES: [&str; 3] = ["plant.a", "plant.b", "plant.c"];

/// Each start of a run, or each cancel: its connector, its time, and the version of
/// its config.
type Starts = Arc<Mutex<Vec<(String, Monotonic, usize)>>>;

/// A kind whose run records its start, spawns a task that records the cancel and
/// ends `linger` after it, or never with `None`, and returns at the cancel, or at
/// once when `brief`. Its config is a version: the count of the attributes of the
/// document.
struct Hold {
    linger: Option<Span>,
    brief: bool,
    starts: Starts,
    cancels: Starts,
}

impl Kind for Hold {
    type Config = usize;

    fn parse(&self, config: &Document) -> Result<usize, Vec<Diagnostic>> {
        Ok(config.attributes.iter().len())
    }

    fn check(&self, _: &usize) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    fn discover(
        &self,
        _: &cancel::Token,
    ) -> impl Future<Output = Result<Vec<Document>, Error>> {
        std::future::ready(Ok(Vec::new()))
    }

    async fn run(&self, ctx: Context<usize>) -> Result<(), Error> {
        let start = (ctx.name().to_string(), ctx.clock().now(), *ctx.config());
        self.starts.lock().expect("no panic").push(start.clone());
        if self.brief {
            return Ok(());
        }
        let (cancel, clock, linger) =
            (ctx.cancel().clone(), ctx.clock().clone(), self.linger);
        let (cancels, mut at) = (Arc::clone(&self.cancels), start);
        ctx.tasks().spawn(async move {
            cancel.wait().await;
            at.1 = clock.now();
            cancels.lock().expect("no panic").push(at);
            match linger {
                Some(linger) => clock.sleep(linger).await,
                None => std::future::pending().await,
            }
        });
        ctx.cancel().wait().await;
        Ok(())
    }
}

/// Spawns on `tasks` and counts the tasks that have not ended.
struct Counted {
    tasks: env::tasks::Tasks,
    live: Rc<Cell<usize>>,
}

/// Counts one task down when it drops.
struct Live(Rc<Cell<usize>>);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

impl env::tasks::Driver for Counted {
    fn spawn(&self, task: env::tasks::Task) {
        self.live.set(self.live.get() + 1);
        let live = Live(Rc::clone(&self.live));
        self.tasks.spawn(async move {
            let _live = live;
            task.await;
        });
    }
}

/// One connector of a spec: its name, its kind, its node, and the version of its
/// config.
type Placed = (&'static str, &'static str, &'static str, usize);

/// One spec at its time.
type Step = (Span, Vec<Placed>);

/// The step of `spec` at `at`.
fn on(at: i64, spec: Vec<Placed>) -> Step {
    (ms(at), spec)
}

/// Each start of a run, or each cancel, with its time from the time the hub opened.
type Events = Vec<(String, Span, usize)>;

/// Each start of a run until 10 s, as [`record`] gives it.
fn starts(linger: Option<Span>, steps: Vec<Step>) -> Events {
    record(linger, steps, None).starts
}

/// What [`record`] gives.
struct Recorded {
    starts: Events,
    cancels: Events,
    /// The names that the runs hold after each step.
    held: Vec<Vec<String>>,
    /// The tasks of the supervisor that have not ended at each step, before its apply.
    live: Vec<usize>,
}

/// Applies each step at its time, with the kind `hold`, whose task ends `linger` after
/// the cancel, the kind `quick`, whose task ends at the cancel, and the kind `brief`,
/// whose run returns at once, and drops the runs at `dropped`, if given. Gives each
/// start of a run and each cancel until 10 s, from the time the hub opened.
fn record(linger: Option<Span>, steps: Vec<Step>, dropped: Option<i64>) -> Recorded {
    let mut sim = ::sim::Sim::new(::sim::Config::default());
    let node = sim.node(::sim::node::Config::default());
    let run = sim.run_on(&node, move |node, tasks| async move {
        let (starts, cancels) = (Starts::default(), Starts::default());
        let hold = |linger, brief| Hold {
            linger,
            brief,
            starts: Arc::clone(&starts),
            cancels: Arc::clone(&cancels),
        };
        let kinds = Table::new()
            .with("hold", hold(linger, false))
            .with("quick", hold(Some(Span::ZERO), false))
            .with("brief", hold(None, true));
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks: tasks.clone(),
        };
        let mut config = testing::create_config(env, node.net(), kinds).await;
        let counted = Rc::new(Cell::new(0));
        config.tasks = env::tasks::Tasks::new(Counted {
            tasks: config.tasks.clone(),
            live: Rc::clone(&counted),
        });
        let hub = config.hub.clone();
        let clock = node.clock();
        let start = clock.now();
        let mut runs = Runs::new(config, name(NODE));
        let (mut names, mut live) = (Vec::new(), Vec::new());
        for (at, spec) in steps {
            clock.sleep_until(start + at).await;
            live.push(counted.get());
            let definitions = definitions(&spec);
            hub.set_definitions(&definitions);
            runs.apply(&definitions);
            names.push(runs.loops.keys().map(ToString::to_string).collect());
        }
        if let Some(dropped) = dropped {
            clock.sleep_until(start + ms(dropped)).await;
            drop(runs);
        }
        clock.sleep_until(start + ms(10_000)).await;
        let since = |(connector, at, version): &(String, Monotonic, usize)| {
            (connector.clone(), *at - start, *version)
        };
        let since = |events: &Starts| {
            let events = events.lock().expect("no panic");
            events.iter().map(since).collect()
        };
        Recorded {
            starts: since(&starts),
            cancels: since(&cancels),
            held: names,
            live,
        }
    });
    run.expect("the run ends")
}

/// The definitions of `spec`: each connector and its status channels.
fn definitions(spec: &[Placed]) -> BTreeMap<Name, Definition> {
    let mut definitions = BTreeMap::new();
    for &(connector, kind, node, version) in spec {
        let position = NAMES.iter().position(|n| *n == connector);
        let position = position.expect("a name of `NAMES`");
        let first = types::channel::Key::from_u128(1 + 100 * position as u128);
        let connector = name(connector);
        definitions.extend(testing::create_status(&connector, &[], first));
        let config = Checked::new(config(version)).expect("a shallow document");
        let placed = Connector::new(name(kind), name(node), config);
        definitions.insert(connector, Definition::Connector(placed));
    }
    definitions
}

/// A config of the version `version`: a document with that many attributes.
fn config(version: usize) -> Document {
    let attributes = (0..version)
        .map(|i| Attribute {
            key: format!("v{i}").into(),
            key_span: None,
            value: Value {
                kind: value::Kind::Bool(true),
                span: None,
            },
        })
        .collect();
    Document {
        attributes: Map::new(attributes).expect("distinct keys"),
        blocks: Vec::new(),
    }
}

fn name(text: &str) -> Name {
    text.parse().expect("a valid name")
}

fn ms(n: i64) -> Span {
    Span::from_nanos(n * 1_000_000)
}

fn start(connector: &str, at: i64, version: usize) -> (String, Span, usize) {
    (connector.to_owned(), ms(at), version)
}

#[test]
fn a_change_of_two_waits_only_for_the_run_that_holds() {
    let first = vec![("plant.a", "hold", NODE, 0), ("plant.b", "quick", NODE, 0)];
    let second = vec![("plant.a", "hold", NODE, 1), ("plant.b", "quick", NODE, 1)];
    let starts = starts(None, vec![on(0, first), on(1_000, second)]);
    assert_eq!(
        starts,
        [
            start("plant.a", 0, 0),
            start("plant.b", 0, 0),
            start("plant.b", 1_000, 1),
        ]
    );
}

#[test]
fn a_change_of_one_starts_after_its_old_run_ended() {
    let first = vec![("plant.a", "hold", NODE, 0), ("plant.b", "quick", NODE, 0)];
    let second = vec![("plant.a", "hold", NODE, 1), ("plant.b", "quick", NODE, 0)];
    let starts = starts(Some(ms(2_000)), vec![on(0, first), on(1_000, second)]);
    assert_eq!(
        starts,
        [
            start("plant.a", 0, 0),
            start("plant.b", 0, 0),
            start("plant.a", 3_000, 1),
        ]
    );
}

#[test]
fn a_second_change_before_the_old_run_ended_runs_the_last_config() {
    let spec = |version| vec![("plant.a", "hold", NODE, version)];
    let specs = vec![on(0, spec(0)), on(1_000, spec(1)), on(1_500, spec(2))];
    let starts = starts(Some(ms(2_000)), specs);
    assert_eq!(starts, [start("plant.a", 0, 0), start("plant.a", 3_000, 2)]);
}

#[test]
fn a_rename_back_starts_after_the_old_run_of_the_name_ended() {
    let spec = |connector| vec![(connector, "hold", NODE, 0)];
    let specs = vec![
        on(0, spec("plant.a")),
        on(1_000, spec("plant.c")),
        on(2_000, spec("plant.a")),
    ];
    let starts = starts(Some(ms(2_000)), specs);
    assert_eq!(
        starts,
        [
            start("plant.a", 0, 0),
            start("plant.c", 1_000, 0),
            start("plant.a", 3_000, 0),
        ]
    );
}

#[test]
fn a_connector_on_another_node_starts_no_run() {
    let spec = vec![
        ("plant.a", "quick", "plant.edge", 0),
        ("plant.b", "quick", NODE, 0),
    ];
    let starts = starts(None, vec![on(0, spec)]);
    assert_eq!(starts, [start("plant.b", 0, 0)]);
}

#[test]
fn a_change_before_the_old_run_ended_then_a_removal_starts_no_run() {
    let spec = |version| vec![("plant.a", "hold", NODE, version)];
    let specs = vec![on(0, spec(0)), on(1_000, spec(1)), on(1_500, Vec::new())];
    assert_eq!(starts(Some(ms(2_000)), specs), [start("plant.a", 0, 0)]);
}

/// A name that a removal ended before its next run starts again at once when it comes
/// back.
#[test]
fn an_addition_after_a_removal_that_ended_the_loop_starts_at_once() {
    let spec = |version| vec![("plant.a", "hold", NODE, version)];
    let specs = vec![
        on(0, spec(0)),
        on(1_000, spec(1)),
        on(1_500, Vec::new()),
        on(4_000, spec(3)),
    ];
    let starts = starts(Some(ms(2_000)), specs);
    assert_eq!(starts, [start("plant.a", 0, 0), start("plant.a", 4_000, 3)]);
}

#[test]
fn a_drop_cancels_each_run() {
    let spec = vec![("plant.a", "hold", NODE, 0), ("plant.b", "quick", NODE, 0)];
    let cancels = record(None, vec![on(0, spec)], Some(1_000)).cancels;
    assert_eq!(
        cancels,
        [start("plant.a", 1_000, 0), start("plant.b", 1_000, 0)]
    );
}

/// A removed run that holds stays until an apply after its loop ended. An entry that
/// ended holds no future and changes no start, so no caller sees it: the test reads
/// `loops`.
#[test]
fn an_apply_drops_each_removed_run_that_ended() {
    let spec = vec![("plant.a", "hold", NODE, 0)];
    let steps = vec![
        on(0, spec),
        on(1_000, vec![]),
        on(2_000, vec![]),
        on(5_000, vec![]),
    ];
    let held = record(Some(ms(2_000)), steps, None).held;
    let held_a = vec!["plant.a"];
    assert_eq!(held, [held_a.clone(), held_a.clone(), held_a, vec![]]);
}

/// A change after the second run started waits for that run, not the first.
#[test]
fn a_third_change_starts_after_the_second_run_ended() {
    let spec = |version| vec![("plant.a", "hold", NODE, version)];
    let steps = vec![on(0, spec(0)), on(1_000, spec(1)), on(4_000, spec(2))];
    let starts = starts(Some(ms(2_000)), steps);
    let second = start("plant.a", 3_000, 1);
    assert_eq!(
        starts,
        [start("plant.a", 0, 0), second, start("plant.a", 6_000, 2)]
    );
}

/// Each change while the old run holds changes what the loop of the name runs next.
#[test]
fn many_changes_while_the_old_run_holds_keep_one_future() {
    let steps = (0..100_usize)
        .zip((0..).step_by(50))
        .map(|(version, at)| on(at, vec![("plant.a", "hold", NODE, version)]))
        .collect();
    let recorded = record(None, steps, None);
    // The loop of the name and the task of its kind.
    let mut live = vec![0];
    live.resize(100, 2);
    assert_eq!(recorded.live, live);
    assert_eq!(recorded.starts, [start("plant.a", 0, 0)]);
}

/// Changes with no poll of the runs between them.
#[test]
fn changes_at_one_instant_keep_one_future() {
    let steps = (0..10_usize)
        .map(|version| {
            let at = if version == 0 { 0 } else { 1_000 };
            on(at, vec![("plant.a", "hold", NODE, version)])
        })
        .collect();
    let recorded = record(None, steps, None);
    // The loop of the name and the task of its kind.
    let mut live = vec![0];
    live.resize(10, 2);
    assert_eq!(recorded.live, live);
    assert_eq!(recorded.starts, [start("plant.a", 0, 0)]);
}

#[test]
fn a_connector_whose_run_returned_starts_no_run_until_it_changes() {
    let spec = |version| vec![("plant.a", "brief", NODE, version)];
    let steps = vec![on(0, spec(0)), on(1_000, spec(0)), on(2_000, spec(1))];
    let starts = starts(None, steps);
    assert_eq!(starts, [start("plant.a", 0, 0), start("plant.a", 2_000, 1)]);
}

/// A node gets its first mesh time 1 s after its hub opened, and the spec that the
/// mesh uses at that time removes the connector whose run waits to open its status.
#[test]
fn a_removal_as_the_mesh_time_arrives_ends_the_run_that_waits_for_it() {
    let mut sim = ::sim::Sim::new(::sim::Config::default());
    let node = sim.node(::sim::node::Config::default());
    let run = sim.run_on(&node, move |node, tasks| async move {
        let starts = Starts::default();
        let kinds = Table::new().with(
            "quick",
            Hold {
                linger: Some(Span::ZERO),
                brief: false,
                starts: Arc::clone(&starts),
                cancels: Starts::default(),
            },
        );
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks: tasks.clone(),
        };
        let clock = node.clock();
        let start = clock.now();
        let hub = hub::testing::open_unsynced(env, ms(1_000)).await;
        let config = connector::supervisor::Config {
            kinds: Arc::new(kinds),
            clock: clock.clone(),
            entropy: node.entropy(),
            net: node.net(),
            tasks: tasks.clone(),
            hub: hub.clone(),
        };
        let runs =
            std::rc::Rc::new(std::cell::RefCell::new(Runs::new(config, name(NODE))));
        let mut placed = definitions(&[("plant.a", "quick", NODE, 0)]);
        // The channels of `plant.b` let a probe learn when the mesh time arrives.
        let probe = definitions(&[("plant.b", "quick", "plant.edge", 0)]);
        placed.extend(probe);
        hub.set_definitions(&placed);
        // The probe waits for the mesh time before the run does, so it removes the
        // connector in the instant that the time arrives, before the run polls again.
        let (probe_hub, probe_runs) = (hub.clone(), std::rc::Rc::clone(&runs));
        tasks.spawn(async move {
            let config = hub::writer::Config {
                subject: name("plant.probe"),
                authority: types::authority::Authority::ABSOLUTE,
                lease: None,
                channels: vec![name("plant.b.status.state")],
            };
            let writer = probe_hub.writer(config).await.expect("the probe opens");
            drop(writer);
            let empty = definitions(&[]);
            probe_hub.set_definitions(&empty);
            probe_runs.borrow_mut().apply(&empty);
        });
        runs.borrow_mut().apply(&placed);
        clock.sleep_until(start + ms(10_000)).await;
        let started = starts.lock().expect("no panic").len();
        drop(runs);
        started
    });
    assert_eq!(run, Ok(0));
}

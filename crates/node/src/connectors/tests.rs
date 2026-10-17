//! Tests of `Runs` on a supervisor with test kinds. `Node` runs only the kinds of
//! `node::kinds`, so a test of a spec change through `Node` cannot hold a run or
//! end one at a chosen time.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use connector::kind::{Channels, Context, Error, Kind, Table};
use connector::testing;
use document::diagnostic::Diagnostic;
use document::encoding::Checked;
use document::value::{self, Value};
use document::{Attribute, Document, Map};
use spec::connector::Connector;
use spec::definition::Definition;
use types::authority::Authority;
use types::frame::{Form, Label, Path};
use types::name::Name;
use types::time::{Monotonic, Span};

use super::Runs;
use crate::tests::port::mesh::{self, member, region};

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

/// A kind with the count `frames`, whose run records its start, sets the count to 1,
/// and holds until the cancel.
struct Tally {
    starts: Starts,
}

impl Kind for Tally {
    type Config = usize;

    fn parse(&self, config: &Document) -> Result<usize, Vec<Diagnostic>> {
        Ok(config.attributes.iter().len())
    }

    fn check(&self, _: &usize) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            counts: vec![name("frames")],
            ..Channels::default()
        })
    }

    async fn run(&self, ctx: Context<usize>) -> Result<(), Error> {
        let start = (ctx.name().to_string(), ctx.clock().now(), *ctx.config());
        self.starts.lock().expect("no panic").push(start);
        ctx.count("frames").set(1);
        ctx.cancel().wait().await;
        Ok(())
    }
}

/// A kind whose run records its start, opens a writer of `plant.data.time` and
/// `plant.data.value`, writes one frame at once, and holds the writer until the cancel.
struct Source {
    starts: Starts,
}

impl Kind for Source {
    type Config = ();

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
        let start = (ctx.name().to_string(), ctx.clock().now(), 0);
        self.starts.lock().expect("no panic").push(start);
        let channels = vec![name("plant.data.time"), name("plant.data.value")];
        let mut writer = ctx.writer(channels, Authority(1), None).await?;
        let entries = writer.set().entries();
        let entry = |key| {
            let key = types::channel::Key::from_u128(key);
            entries.iter().position(|entry| entry.key == key)
        };
        let (time, value) = (entry(900).expect("time"), entry(901).expect("value"));
        let group = entries[time].group;
        let mut series = [(time, 8), (value, 8)];
        series.sort_unstable();
        let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
        let stamp = writer.now().nanos();
        let time = draft.series_mut(time).expect("the index series");
        time.copy_from_slice(&stamp.to_le_bytes());
        let value = draft.series_mut(value).expect("the value series");
        value.copy_from_slice(&1.0_f64.to_le_bytes());
        draft.set_count(group, 1);
        let outcomes = writer.write(Label::Path(Path::Live), draft);
        let outcomes = outcomes.expect("the home takes it");
        assert!(
            matches!(outcomes, [hub::home::Outcome::Applied { .. }]),
            "{outcomes:?}"
        );
        ctx.cancel().wait().await;
        Ok(())
    }
}

/// A kind that reads with `settings`, and records its start and each frame that its run
/// gets, with the time.
struct Sink {
    settings: connector::reader::Settings,
    starts: Starts,
    got: Starts,
}

impl Kind for Sink {
    type Config = ();

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            reader: Some(self.settings.clone()),
            ..Channels::default()
        })
    }

    async fn run(&self, mut ctx: Context<()>) -> Result<(), Error> {
        let start = (ctx.name().to_string(), ctx.clock().now(), 0);
        self.starts.lock().expect("no panic").push(start);
        let mut reader = ctx.reader().expect("the reader of `check`");
        let cancel = ctx.cancel().clone();
        while let Some(Ok(_)) = cancel.race(reader.next()).await {
            let got = (ctx.name().to_string(), ctx.clock().now(), 0);
            self.got.lock().expect("no panic").push(got);
        }
        Ok(())
    }
}

/// The runs of the node `NODE` on a supervisor of `config`, on its tasks.
fn runs(config: connector::supervisor::Config) -> Runs {
    let tasks = config.tasks.clone();
    Runs::new(
        connector::supervisor::Supervisor::new(config),
        tasks,
        name(NODE),
    )
}

/// `config` with its tasks counted, and the count of its tasks that have not ended.
fn count(
    mut config: connector::supervisor::Config,
) -> (connector::supervisor::Config, Rc<Cell<usize>>) {
    let live = Rc::new(Cell::new(0));
    config.tasks = env::tasks::Tasks::new(Counted {
        tasks: config.tasks.clone(),
        live: Rc::clone(&live),
    });
    (config, live)
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

/// Each start of a run, or each cancel, with its time from the time the hub opened, in
/// order of time, then name.
type Events = Vec<(String, Span, usize)>;

/// Each start of a run until 10 s, as [`record`] gives it.
fn starts(linger: Option<Span>, steps: Vec<Step>) -> Events {
    record(
        steps,
        Scene {
            linger,
            ..Scene::default()
        },
    )
    .starts
}

/// What [`record`] gives.
struct Recorded {
    starts: Events,
    cancels: Events,
    /// Each frame that a run of `sink` got.
    got: Events,
    /// The names that the runs hold after each step.
    held: Vec<Vec<String>>,
    /// The tasks of the supervisor that have not ended at each step, before its apply.
    live: Vec<usize>,
}

/// When the hub of [`create`] gets its first mesh time.
#[derive(Clone, Copy, Default)]
enum Mesh {
    /// At once.
    #[default]
    Synced,
    /// This long after it opened.
    Late(Span),
}

/// The options of [`record`].
#[derive(Clone, Copy, Default)]
struct Scene {
    /// The seed of the simulation.
    seed: u64,
    /// The time that the task of `hold` ends after the cancel.
    linger: Option<Span>,
    mesh: Mesh,
    /// The time to drop the runs at, in ms from the time the hub opened, if given.
    dropped: Option<i64>,
}

/// Applies each step at its time, in `scene`, with the kind `hold`, the kind `quick`,
/// whose task ends at the cancel, the kind `brief`, whose run returns at once, and the
/// kinds `source`, a [`Source`], `sink`, a [`Sink`] of `plant.data.*`, and `void`, a
/// [`Sink`] of a selector that matches no channel. Each spec also defines [`data`].
/// Gives each start of a run, each cancel, and each frame got until 10 s, from the time
/// the hub opened, in order of time, then of name: the sim polls the tasks that are
/// ready at one instant in a random order.
fn record(steps: Vec<Step>, scene: Scene) -> Recorded {
    let Scene {
        seed,
        linger,
        mesh,
        dropped,
    } = scene;
    let config = ::sim::Config {
        seed,
        ..::sim::Config::default()
    };
    let mut sim = ::sim::Sim::new(config);
    let node = sim.node(::sim::node::Config::default());
    let run = sim.run_on(&node, move |node, tasks| async move {
        let (starts, cancels, got) =
            (Starts::default(), Starts::default(), Starts::default());
        let kinds = kinds(linger, &starts, &cancels, &got);
        let (config, counted) = create(&node, &tasks, kinds, mesh).await;
        let hub = config.hub.clone();
        let clock = node.clock();
        let start = clock.now();
        let mut runs = runs(config);
        let (mut names, mut live) = (Vec::new(), Vec::new());
        for (at, spec) in steps {
            clock.sleep_until(start + at).await;
            live.push(counted.get());
            let mut definitions = definitions(&spec);
            definitions.extend(data());
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
            let mut events: Events = events.iter().map(since).collect();
            events.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
            events
        };
        Recorded {
            starts: since(&starts),
            cancels: since(&cancels),
            got: since(&got),
            held: names,
            live,
        }
    });
    run.expect("the run ends")
}

/// The kinds of [`record`].
fn kinds(
    linger: Option<Span>,
    starts: &Starts,
    cancels: &Starts,
    got: &Starts,
) -> Table {
    let hold = |linger, brief| Hold {
        linger,
        brief,
        starts: Arc::clone(starts),
        cancels: Arc::clone(cancels),
    };
    let sink = |select| Sink {
        settings: settings(select),
        starts: Arc::clone(starts),
        got: Arc::clone(got),
    };
    let source = Source {
        starts: Arc::clone(starts),
    };
    Table::new()
        .with("hold", hold(linger, false))
        .with("quick", hold(Some(Span::ZERO), false))
        .with("brief", hold(None, true))
        .with("source", source)
        .with("sink", sink("plant.data.*"))
        .with("void", sink("plant.none.*"))
        .with(
            "tally",
            Tally {
                starts: Arc::clone(starts),
            },
        )
}

/// The settings of a complete reader of the channels that `select` matches.
fn settings(select: &str) -> connector::reader::Settings {
    let select = Attribute {
        key: "select".into(),
        key_span: None,
        value: Value {
            kind: value::Kind::String(select.into()),
            span: None,
        },
    };
    let config = Document {
        attributes: Map::new(vec![select]).expect("one key"),
        blocks: Vec::new(),
    };
    connector::reader::read(&config, &[], &[]).expect("the reader settings")
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

/// The definitions of `plant.data.time` (key 900), the index, and `plant.data.value`
/// (key 901, `f64`).
fn data() -> [(Name, Definition); 2] {
    let index = types::channel::Key::from_u128(900);
    let time = spec::channel::Channel {
        key: index,
        kind: spec::channel::Kind::Index {
            error: None,
            control: None,
        },
    };
    let f64 = types::sample::Type::Scalar(types::sample::Scalar::F64);
    let data = spec::channel::Data::new(
        index,
        None,
        spec::data_type::DataType::Sample(f64),
        None,
    );
    let value = spec::channel::Channel {
        key: types::channel::Key::from_u128(901),
        kind: spec::channel::Kind::Data(data.expect("no unit")),
    };
    [
        (name("plant.data.time"), Definition::Channel(time)),
        (name("plant.data.value"), Definition::Channel(value)),
    ]
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
    let cancels = record(
        vec![on(0, spec)],
        Scene {
            dropped: Some(1_000),
            ..Scene::default()
        },
    )
    .cancels;
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
    let held = record(
        steps,
        Scene {
            linger: Some(ms(2_000)),
            ..Scene::default()
        },
    )
    .held;
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
    let recorded = record(steps, Scene::default());
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
    let recorded = record(steps, Scene::default());
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

/// A kind whose run records its start, spawns a task that ends `flush` after the
/// cancel, opens a writer of `plant.data.time` and `plant.data.value`, and holds the
/// writer until the cancel.
struct Write {
    starts: Starts,
    flush: Span,
}

impl Kind for Write {
    type Config = ();

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
        let start = (ctx.name().to_string(), ctx.clock().now(), 0);
        self.starts.lock().expect("no panic").push(start);
        let (cancel, clock, flush) =
            (ctx.cancel().clone(), ctx.clock().clone(), self.flush);
        ctx.tasks().spawn(async move {
            cancel.wait().await;
            clock.sleep(flush).await;
        });
        let channels = vec![name("plant.data.time"), name("plant.data.value")];
        let writer = ctx.writer(channels, Authority(1), None).await?;
        ctx.cancel().wait().await;
        drop(writer);
        Ok(())
    }
}

/// The counts of the runs of a connector of [`Write`] at 4 s and 10 s. Its first run
/// gets `Config`, since no channel has its names. At 1 s, a spec change adds the
/// channels, with the connector unchanged. At 5 s, an apply keeps the connector
/// and its run.
fn starts_of_write(flush: Span) -> Result<(usize, usize), ::sim::Error> {
    let mut sim = ::sim::Sim::new(::sim::Config::default());
    let node = sim.node(::sim::node::Config::default());
    sim.run_on(&node, move |node, tasks| async move {
        let starts = Starts::default();
        let write = Write {
            starts: Arc::clone(&starts),
            flush,
        };
        let kinds = Table::new().with("write", write);
        let (config, _) = create(&node, &tasks, kinds, Mesh::Synced).await;
        let hub = config.hub.clone();
        let clock = node.clock();
        let start = clock.now();
        let mut runs = runs(config);
        let first = definitions(&[("plant.a", "write", NODE, 0)]);
        hub.set_definitions(&first);
        runs.apply(&first);
        clock.sleep_until(start + ms(1_000)).await;
        let mut second = first.clone();
        second.extend(data());
        hub.set_definitions(&second);
        runs.apply(&second);
        clock.sleep_until(start + ms(4_000)).await;
        let early = starts.lock().expect("no panic").len();
        clock.sleep_until(start + ms(5_000)).await;
        runs.apply(&second);
        clock.sleep_until(start + ms(10_000)).await;
        let started = starts.lock().expect("no panic").len();
        drop(runs);
        (early, started)
    })
}

/// The spec change that adds the written channels is the fix that `Config` names, so
/// the connector runs again.
#[test]
fn a_spec_change_that_adds_the_written_channels_runs_the_connector_again() {
    assert_eq!(starts_of_write(Span::ZERO), Ok((2, 2)));
}

/// The task of the refused run ends at 2 s, so the spec change at 1 s comes before
/// `run` returned `Config`. It still runs the connector again.
#[test]
fn a_spec_change_while_a_refused_run_ends_runs_the_connector_again() {
    assert_eq!(starts_of_write(ms(2_000)), Ok((2, 2)));
}

/// A node gets its first mesh time 1 s after its hub opened, and the spec that the
/// mesh uses at that time removes the connector whose run waits to open its status.
/// The sim polls the run and the removal in a random order, so each seed checks that
/// no run starts after the removal, and some seed removes the connector first.
#[test]
fn a_removal_as_the_mesh_time_arrives_ends_the_run_that_waits_for_it() {
    let runs: Vec<(usize, usize)> =
        (0..16).map(removal_as_the_mesh_time_arrives).collect();
    for (seed, &(removed, started)) in runs.iter().enumerate() {
        assert_eq!(started, removed, "seed {seed}");
    }
    assert!(runs.contains(&(0, 0)), "{runs:?}");
}

/// Runs the scene of the test above with `seed`. Gives the count of starts at the
/// removal and at 10 s.
fn removal_as_the_mesh_time_arrives(seed: u64) -> (usize, usize) {
    let mut sim = ::sim::Sim::new(::sim::Config {
        seed,
        ..::sim::Config::default()
    });
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
        let clock = node.clock();
        let start = clock.now();
        let (config, _) = create(&node, &tasks, kinds, Mesh::Late(ms(1_000))).await;
        let hub = config.hub.clone();
        let runs = std::rc::Rc::new(std::cell::RefCell::new(runs(config)));
        let mut placed = definitions(&[("plant.a", "quick", NODE, 0)]);
        // The channels of `plant.b` let a probe learn when the mesh time arrives.
        let probe = definitions(&[("plant.b", "quick", "plant.edge", 0)]);
        placed.extend(probe);
        hub.set_definitions(&placed);
        let (probe_hub, probe_runs) = (hub.clone(), std::rc::Rc::clone(&runs));
        let removed = std::rc::Rc::new(std::cell::Cell::new(None));
        let (probe_starts, probe_removed) =
            (Arc::clone(&starts), std::rc::Rc::clone(&removed));
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
            probe_removed.set(Some(probe_starts.lock().expect("no panic").len()));
        });
        runs.borrow_mut().apply(&placed);
        clock.sleep_until(start + ms(10_000)).await;
        let started = starts.lock().expect("no panic").len();
        drop(runs);
        (removed.get().expect("the probe removed it"), started)
    });
    run.expect("the run ends")
}

/// The config of a supervisor on `node` with `kinds`, whose hub gets its first mesh
/// time at `mesh`, and the count of its tasks that have not ended.
async fn create(
    node: &::sim::node::Node,
    tasks: &env::tasks::Tasks,
    kinds: Table,
    mesh: Mesh,
) -> (connector::supervisor::Config, Rc<Cell<usize>>) {
    let env = hub::testing::Env {
        files: node.files(),
        clock: node.clock(),
        wall: node.wall(),
        entropy: node.entropy(),
        tasks: tasks.clone(),
    };
    let hub = match mesh {
        Mesh::Synced => hub::testing::open(env).await.0,
        Mesh::Late(synced) => hub::testing::open_unsynced(env, synced).await,
    };
    let secrets = secret::Resolver::new(
        Arc::default(),
        node.files(),
        &crate::directory::secrets(),
    );
    count(connector::supervisor::Config {
        kinds: Arc::new(kinds),
        clock: node.clock(),
        entropy: node.entropy(),
        net: node.net(),
        roots: connector::http::Roots::default(),
        tasks: tasks.clone(),
        hub,
        secrets,
    })
}

/// The sorted names of the runs that got a frame, on each of 32 seeds.
fn got_on_each_seed(steps: impl Fn() -> Vec<Step>) -> Vec<Vec<String>> {
    (0..32)
        .map(|seed| {
            let got = record(
                steps(),
                Scene {
                    seed,
                    ..Scene::default()
                },
            )
            .got;
            let mut got: Vec<_> = got.into_iter().map(|(name, _, _)| name).collect();
            got.sort_unstable();
            got
        })
        .collect()
}

/// The out connector sorts before the in connector, so its loop is spawned first.
#[test]
fn an_out_connector_gets_the_first_frame_of_an_in_connector_of_its_apply() {
    let spec = vec![("plant.a", "sink", NODE, 0), ("plant.b", "source", NODE, 0)];
    let got = got_on_each_seed(|| vec![on(0, spec.clone())]);
    assert_eq!(got, vec![vec!["plant.a".to_owned()]; 32]);
}

/// The out connector sorts after the in connector, so its loop is spawned last.
#[test]
fn a_later_out_connector_gets_the_first_frame_of_an_in_connector_of_its_apply() {
    let spec = vec![("plant.a", "source", NODE, 0), ("plant.c", "sink", NODE, 0)];
    let got = got_on_each_seed(|| vec![on(0, spec.clone())]);
    assert_eq!(got, vec![vec!["plant.c".to_owned()]; 32]);
}

/// A later apply adds an in and an out connector while an out connector runs.
#[test]
fn each_out_connector_gets_the_first_frame_of_an_in_connector_of_a_later_apply() {
    let first = vec![("plant.a", "sink", NODE, 0)];
    let mut second = first.clone();
    second.extend([("plant.b", "source", NODE, 0), ("plant.c", "sink", NODE, 0)]);
    let got =
        got_on_each_seed(|| vec![on(0, first.clone()), on(1_000, second.clone())]);
    let both = vec!["plant.a".to_owned(), "plant.c".to_owned()];
    assert_eq!(got, vec![both; 32]);
}

/// Two applies with no await between them: the first adds the out connector, the
/// second the in connector.
#[test]
fn an_out_connector_gets_the_first_frame_of_an_in_connector_of_the_next_apply() {
    let first = vec![("plant.a", "sink", NODE, 0)];
    let mut second = first.clone();
    second.push(("plant.b", "source", NODE, 0));
    let got = got_on_each_seed(|| vec![on(0, first.clone()), on(0, second.clone())]);
    assert_eq!(got, vec![vec!["plant.a".to_owned()]; 32]);
}

/// Two out connectors and an in connector, so one open resolves while two are pending.
#[test]
fn each_out_connector_of_an_apply_gets_the_first_frame_of_its_in_connector() {
    let spec = vec![
        ("plant.a", "sink", NODE, 0),
        ("plant.b", "source", NODE, 0),
        ("plant.c", "sink", NODE, 0),
    ];
    let got = got_on_each_seed(|| vec![on(0, spec.clone())]);
    let both = vec!["plant.a".to_owned(), "plant.c".to_owned()];
    assert_eq!(got, vec![both; 32]);
}

/// The reader open of `void` fails, since its selector matches no channel.
#[test]
fn an_out_connector_whose_reader_open_failed_holds_no_run() {
    let spec = vec![("plant.a", "void", NODE, 0), ("plant.b", "source", NODE, 0)];
    let recorded = record(vec![on(0, spec)], Scene::default());
    assert_eq!(recorded.starts, [start("plant.b", 0, 0)]);
    assert_eq!(recorded.got, []);
}

/// The open of `plant.a`, of no kind, resolves at once, and the first open of
/// `plant.b` waits for the mesh time at 5 s. A removal of `plant.a` at 1 s ends its
/// wait for that open, and its loop.
#[test]
fn a_removal_ends_the_wait_of_its_connector_for_the_other_first_opens() {
    let mut sim = ::sim::Sim::new(::sim::Config::default());
    let node = sim.node(::sim::node::Config::default());
    let run = sim.run_on(&node, move |node, tasks| async move {
        let kinds = Table::new().with(
            "quick",
            Hold {
                linger: Some(Span::ZERO),
                brief: false,
                starts: Starts::default(),
                cancels: Starts::default(),
            },
        );
        let clock = node.clock();
        let start = clock.now();
        let (config, live) = create(&node, &tasks, kinds, Mesh::Late(ms(5_000))).await;
        let hub = config.hub.clone();
        let mut runs = runs(config);
        let placed =
            definitions(&[("plant.a", "none", NODE, 0), ("plant.b", "quick", NODE, 0)]);
        hub.set_definitions(&placed);
        runs.apply(&placed);
        clock.sleep_until(start + ms(1_000)).await;
        let before = live.get();
        let kept = definitions(&[("plant.b", "quick", NODE, 0)]);
        hub.set_definitions(&kept);
        runs.apply(&kept);
        clock.sleep_until(start + ms(2_000)).await;
        let after = live.get();
        drop(runs);
        (before, after)
    });
    assert_eq!(run, Ok((2, 1)));
}

/// [`record`] on `seed`, on a hub whose first mesh time comes at 5 s.
fn record_unsynced(seed: u64, steps: Vec<Step>) -> Recorded {
    let mesh = Mesh::Late(ms(5_000));
    record(
        steps,
        Scene {
            seed,
            mesh,
            ..Scene::default()
        },
    )
}

/// The seeds of 0 to 31 on which no `sink` got a frame, with [`record_unsynced`].
fn missed_unsynced(steps: impl Fn() -> Vec<Step>) -> Vec<u64> {
    (0..32)
        .filter(|&seed| record_unsynced(seed, steps()).got.is_empty())
        .collect()
}

/// Both first opens wait for the mesh time at 5 s. At 1 s, a change of the in
/// connector, whose first run has not started, keeps its share.
#[test]
fn a_change_before_the_first_run_keeps_its_share() {
    let first = vec![("plant.a", "sink", NODE, 0), ("plant.b", "source", NODE, 0)];
    let second = vec![("plant.a", "sink", NODE, 0), ("plant.b", "source", NODE, 1)];
    assert_eq!(
        missed_unsynced(|| vec![on(0, first.clone())]),
        Vec::<u64>::new()
    );
    let missed =
        missed_unsynced(|| vec![on(0, first.clone()), on(1_000, second.clone())]);
    assert_eq!(missed, Vec::<u64>::new());
}

/// As above, with a change of the out connector while its first open waits: the next
/// open keeps the share, and holds the first run of the in connector.
#[test]
fn a_change_while_the_first_open_waits_keeps_its_share() {
    let first = vec![("plant.a", "sink", NODE, 0), ("plant.b", "source", NODE, 0)];
    let second = vec![("plant.a", "sink", NODE, 1), ("plant.b", "source", NODE, 0)];
    let missed =
        missed_unsynced(|| vec![on(0, first.clone()), on(1_000, second.clone())]);
    assert_eq!(missed, Vec::<u64>::new());
}

/// At 1 s, a removal of the out connector, whose first open waits for the mesh time at
/// 5 s, frees its share, so the in connector runs.
#[test]
fn a_removal_before_the_first_run_frees_its_share() {
    let first = vec![("plant.a", "sink", NODE, 0), ("plant.b", "source", NODE, 0)];
    let second = vec![("plant.b", "source", NODE, 0)];
    let starts: Vec<usize> = (0..32)
        .map(|seed| {
            let steps = vec![on(0, first.clone()), on(1_000, second.clone())];
            record_unsynced(seed, steps).starts.len()
        })
        .collect();
    assert_eq!(starts, vec![1; 32]);
}

/// At 0, a connector of no kind, whose open resolves at once, and a `sink`, whose first
/// open waits for the mesh time at 5 s. At 1 s, the first becomes `to`, and `with`
/// stays.
fn missed_after_kind(to: &'static str, with: &'static str) -> Vec<u64> {
    let first = vec![("plant.a", "none", NODE, 0), ("plant.b", with, NODE, 0)];
    let second = vec![("plant.a", to, NODE, 0), ("plant.b", with, NODE, 0)];
    missed_unsynced(|| vec![on(0, first.clone()), on(1_000, second.clone())])
}

/// The first open of the connector of no kind resolved before the change, and its loop
/// waits for the open of the `sink`. The `source` that the change gives still waits.
#[test]
fn a_change_after_the_first_open_resolved_keeps_the_wait() {
    assert_eq!(missed_after_kind("source", "sink"), Vec::<u64>::new());
}

/// The `sink` that the change gives holds the first run of the `source`.
#[test]
fn a_change_after_the_first_open_resolved_holds_the_other_first_runs() {
    assert_eq!(missed_after_kind("sink", "source"), Vec::<u64>::new());
}

/// A change of a connector whose first run started takes no share: the old task of
/// `hold` lingers 2 s after the cancel, and the first run of an addition of that apply
/// does not wait for it.
#[test]
fn a_change_after_the_first_run_takes_no_share() {
    let first = vec![("plant.a", "hold", NODE, 0)];
    let second = vec![("plant.a", "hold", NODE, 1), ("plant.b", "quick", NODE, 0)];
    let steps = vec![on(0, first), on(1_000, second)];
    assert_eq!(
        starts(Some(ms(2_000)), steps),
        [
            start("plant.a", 0, 0),
            start("plant.b", 1_000, 0),
            start("plant.a", 3_000, 1),
        ]
    );
}

/// A name added again while the task of its first run lingers takes no share: the first
/// run of an addition of that apply does not wait for it.
#[test]
fn an_addition_again_after_the_first_run_takes_no_share() {
    let first = vec![("plant.a", "hold", NODE, 0)];
    let third = vec![("plant.a", "hold", NODE, 0), ("plant.b", "quick", NODE, 0)];
    let steps = vec![on(0, first), on(1_000, vec![]), on(1_500, third)];
    assert_eq!(
        starts(Some(ms(2_000)), steps),
        [
            start("plant.a", 0, 0),
            start("plant.b", 1_500, 0),
            start("plant.a", 3_000, 0),
        ]
    );
}

/// A connector of no kind runs alone at 0, so its first run starts. At 1 s, it becomes
/// a `source`, and the apply adds a `sink`. The `source` does not wait for the first
/// open of the `sink`, so on some seeds the `sink` misses the frame.
#[test]
fn a_change_after_the_first_run_waits_for_no_first_open() {
    let first = vec![("plant.a", "none", NODE, 0)];
    let second = vec![("plant.a", "source", NODE, 0), ("plant.b", "sink", NODE, 0)];
    let missed =
        missed_unsynced(|| vec![on(0, first.clone()), on(1_000, second.clone())]);
    assert_ne!(missed, Vec::<u64>::new());
}

/// On `seed`, the sinks `plant.a` and `plant.c` and the source `plant.b` at 0, and at
/// 5 s, the instant of the mesh time, after `yields` polls of other tasks, an apply that
/// changes `plant.a`. Gives `None` when a run of `plant.a` or `plant.b` started before
/// that apply returned, else whether `plant.a` got the frame of `plant.b`.
fn change_at_the_mesh_time(seed: u64, yields: usize) -> Option<bool> {
    let config = ::sim::Config {
        seed,
        ..::sim::Config::default()
    };
    let mut sim = ::sim::Sim::new(config);
    let node = sim.node(::sim::node::Config::default());
    let run = sim.run_on(&node, move |node, tasks| async move {
        let (starts, got) = (Starts::default(), Starts::default());
        let kinds = kinds(None, &starts, &Starts::default(), &got);
        let (config, _) = create(&node, &tasks, kinds, Mesh::Late(ms(5_000))).await;
        let clock = node.clock();
        let start = clock.now();
        let hub = config.hub.clone();
        let mut runs = runs(config);
        let mut apply = |version| {
            let spec = [
                ("plant.a", "sink", NODE, version),
                ("plant.b", "source", NODE, 0),
                ("plant.c", "sink", NODE, 0),
            ];
            let mut definitions = definitions(&spec);
            definitions.extend(data());
            hub.set_definitions(&definitions);
            runs.apply(&definitions);
        };
        apply(0);
        clock.sleep_until(start + ms(5_000)).await;
        for _ in 0..yields {
            let mut yielded = false;
            std::future::poll_fn(|cx| {
                if yielded {
                    return std::task::Poll::Ready(());
                }
                yielded = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            })
            .await;
        }
        apply(1);
        let early = (starts.lock().expect("no panic").iter())
            .any(|(name, _, _)| name != "plant.c");
        clock.sleep_until(start + ms(10_000)).await;
        drop(runs);
        let got = got.lock().expect("no panic");
        let got = got.iter().any(|(name, _, _)| name == "plant.a");
        (!early).then_some(got)
    });
    run.expect("the run ends")
}

/// Out `plant.a` and in `plant.b` can wait for the first open of `plant.c` when an
/// apply changes `plant.a`. The share of the change counts from that apply.
#[test]
fn a_change_before_the_first_run_counts_from_its_apply() {
    let mut runs = Vec::new();
    for seed in 0..32 {
        for yields in 0..8 {
            runs.push(((seed, yields), change_at_the_mesh_time(seed, yields)));
        }
    }
    let missed: Vec<_> = runs.iter().filter(|run| run.1 == Some(false)).collect();
    assert_eq!(missed, Vec::<&_>::new());
    assert!(runs.iter().any(|run| run.1 == Some(true)), "{runs:?}");
}

/// The definitions of `plant.a` of `kind` on [`NODE`] at `version`, with the counts
/// `counts`, whose status channels have keys from `first` on.
fn rekeyed(
    kind: &str,
    version: usize,
    first: u128,
    counts: &[Name],
) -> BTreeMap<Name, Definition> {
    let connector = name("plant.a");
    let first = types::channel::Key::from_u128(first);
    let mut definitions: BTreeMap<Name, Definition> =
        testing::create_status(&connector, counts, first)
            .into_iter()
            .collect();
    let config = Checked::new(config(version)).expect("a shallow document");
    let placed = Connector::new(name(kind), name(NODE), config);
    definitions.insert(connector, Definition::Connector(placed));
    definitions
}

/// What [`rekey`] gives.
struct Rekeyed {
    /// The frames on the new channel that the test reads.
    frames: usize,
    starts: Events,
}

/// Applies `plant.a` of `kind` with status keys from 1, then at 1 s the spec in use
/// after an apply that removed it and one that placed it again at `version`, which
/// `mesh::used::Watch::next` gives as one change. The status channels then have new
/// keys from 1001. The task of `hold` ends `linger` after the cancel. Gives the frames
/// of `plant.a.status.state` on the new keys and each start until 10 s, as [`record`]
/// does.
fn rekey(kind: &'static str, version: usize, linger: Option<Span>) -> Rekeyed {
    let first = rekeyed(kind, 0, 1, &[]);
    let again = rekeyed(kind, version, 1_001, &[]);
    respec(first, again, "plant.a.status.state", linger)
}

/// Applies `first`, then at 1 s `again`, as one change. Gives the frames of `select`
/// in `again` and each start until 10 s, as [`record`] does.
fn respec(
    first: BTreeMap<Name, Definition>,
    again: BTreeMap<Name, Definition>,
    select: &'static str,
    linger: Option<Span>,
) -> Rekeyed {
    let mut sim = ::sim::Sim::new(::sim::Config::default());
    let node = sim.node(::sim::node::Config::default());
    let run = sim.run_on(&node, move |node, tasks| async move {
        let (starts, cancels, got) =
            (Starts::default(), Starts::default(), Starts::default());
        let kinds = kinds(linger, &starts, &cancels, &got);
        let (config, _) = create(&node, &tasks, kinds, Mesh::Synced).await;
        let hub = config.hub.clone();
        let clock = node.clock();
        let start = clock.now();
        let mut runs = runs(config);
        hub.set_definitions(&first);
        runs.apply(&first);
        clock.sleep_until(start + ms(1_000)).await;
        hub.set_definitions(&again);
        let open = ::hub::reader::Config {
            select: types::name::Selector::new([select]).expect("a selector"),
            mode: ::hub::reader::Mode::Complete,
            subject: name("plant.test"),
            name: None,
            hold: Span::ZERO,
        };
        let mut reader = hub.reader(open).await.expect("the reader opens");
        let frames = Rc::new(Cell::new(0));
        let counted = Rc::clone(&frames);
        tasks.spawn(async move {
            while reader.next().await.is_ok() {
                counted.set(counted.get() + 1);
            }
        });
        runs.apply(&again);
        clock.sleep_until(start + ms(10_000)).await;
        drop(runs);
        let starts = starts.lock().expect("no panic");
        let since = |(connector, at, version): &(String, Monotonic, usize)| {
            (connector.clone(), *at - start, *version)
        };
        Rekeyed {
            frames: frames.get(),
            starts: starts.iter().map(since).collect(),
        }
    });
    run.expect("the run ends")
}

/// The control: a change of the config with the new keys starts a run that writes
/// them.
#[test]
fn a_change_with_new_status_keys_writes_the_new_status() {
    let rekeyed = rekey("quick", 1, None);
    assert_eq!(rekeyed.frames, 1, "`state` running");
}

/// A removal and a placement again of the same connector, which the node sees as one
/// spec in use, start a run that writes the status channels of the new keys.
#[test]
fn a_removal_and_a_placement_in_one_spec_in_use_writes_the_new_status() {
    let rekeyed = rekey("quick", 0, None);
    assert_eq!(rekeyed.frames, 1, "`state` running");
    let starts = [start("plant.a", 0, 0), start("plant.a", 1_000, 0)];
    assert_eq!(rekeyed.starts, starts);
}

/// The run of the new keys starts only after the old run returned, so no two runs of
/// one name write its status.
#[test]
fn a_placement_again_starts_after_the_old_run_ended() {
    let rekeyed = rekey("hold", 0, Some(ms(2_000)));
    assert_eq!(rekeyed.frames, 1, "`state` running");
    let starts = [start("plant.a", 0, 0), start("plant.a", 3_000, 0)];
    assert_eq!(rekeyed.starts, starts);
}

/// A spec change that keeps the status index and gives the count channel a new key
/// starts the run again, and the new count channel gets its value.
#[test]
fn a_new_key_of_a_count_alone_writes_the_new_count() {
    let counts = [name("frames")];
    let first = rekeyed("tally", 0, 1, &counts);
    let mut again = first.clone();
    let frames = name("plant.a.status.frames");
    let Some(Definition::Channel(channel)) = again.get_mut(&frames) else {
        panic!("the count channel is defined");
    };
    channel.key = types::channel::Key::from_u128(1_001);
    let rekeyed = respec(first, again, "plant.a.status.frames", None);
    assert_eq!(rekeyed.frames, 2, "the frames of the count on its new key");
    let starts = [start("plant.a", 0, 0), start("plant.a", 1_000, 0)];
    assert_eq!(rekeyed.starts, starts);
}

/// A spec change that gives only a data channel a new key keeps the run of each
/// connector.
#[test]
fn a_new_key_of_a_data_channel_changes_no_connector() {
    let mut first = rekeyed("quick", 0, 1, &[]);
    first.extend(data());
    let mut again = first.clone();
    let Some(Definition::Channel(channel)) = again.get_mut(&name("plant.data.value"))
    else {
        panic!("the data channel is defined");
    };
    channel.key = types::channel::Key::from_u128(1_901);
    let rekeyed = respec(first, again, "plant.a.status.state", None);
    assert_eq!(rekeyed.starts, [start("plant.a", 0, 0)]);
}

/// A hub on `node` in a region whose one member and voter is the node, with the home
/// of each index of `homes` at the node, and the mesh.
async fn regional(
    node: &::sim::node::Node,
    tasks: &env::tasks::Tasks,
    homes: &[u128],
) -> (hub::Hub, ::mesh::Mesh) {
    let key = types::node::Key::from_u128(1);
    let private_key = types::ed25519::PrivateKey([2; 32]);
    let mut founding = region(&[member(key, &private_key, node)]);
    founding.homes = homes
        .iter()
        .map(|&index| (types::channel::Key::from_u128(index), key))
        .collect();
    let identity = crate::identity::Identity {
        key,
        private_key,
        opener: secret::seal::Opener::from_bytes([3; 32]),
    };
    let opened = mesh::open(node, tasks, identity, founding).await;
    (opened.hub, opened.mesh)
}

/// Applies `first`, then `second` 1 s after `first` is in use, each with its channels,
/// through the mesh of [`regional`], where the status index of each connector has its
/// home at the node and `plant.data.time` has none. Gives each start of a run until
/// 10 s after `second` is in use, with its time from that instant.
fn in_region(
    seed: u64,
    first: (Vec<Placed>, Vec<(Name, Definition)>),
    second: (Vec<Placed>, Vec<(Name, Definition)>),
) -> Events {
    let mut sim = ::sim::Sim::new(::sim::Config {
        seed,
        ..::sim::Config::default()
    });
    let node = sim.node(::sim::node::Config::default());
    let run = sim.run_on(&node, move |node, tasks| async move {
        let (starts, cancels, got) =
            (Starts::default(), Starts::default(), Starts::default());
        let kinds = kinds(None, &starts, &cancels, &got);
        let (hub, mesh) = regional(&node, &tasks, &[1, 101, 201, 902]).await;
        let secrets = secret::Resolver::new(
            Arc::default(),
            node.files(),
            &crate::directory::secrets(),
        );
        let config = connector::supervisor::Config {
            kinds: Arc::new(kinds),
            clock: node.clock(),
            entropy: node.entropy(),
            net: node.net(),
            roots: connector::http::Roots::default(),
            tasks: tasks.clone(),
            hub,
            secrets,
        };
        let clock = node.clock();
        let mut runs = runs(config);
        let mut apply = async |(spec, channels): (Vec<Placed>, Vec<_>)| {
            let mut definitions = definitions(&spec);
            definitions.extend(channels);
            let applied =
                mesh.apply(mesh.pointer(), definitions.clone(), BTreeMap::new());
            let pointer = applied.await.expect("applies the spec");
            let used = mesh.spec().await.expect("the mesh runs");
            assert_eq!(used.pointer, Some(pointer));
            runs.apply(&definitions);
        };
        apply(first).await;
        clock.sleep(ms(1_000)).await;
        apply(second).await;
        let start = clock.now();
        clock.sleep(ms(10_000)).await;
        drop(runs);
        let starts = starts.lock().expect("no panic");
        let mut starts: Events = starts
            .iter()
            .map(|(connector, at, version)| (connector.clone(), *at - start, *version))
            .collect();
        starts.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
        starts
    });
    run.expect("the run ends")
}

/// `plant.data.value` (key 901) on the index `plant.other` (key 902), whose home is the
/// node, and no `plant.data.time`.
fn moved() -> Vec<(Name, Definition)> {
    let index = types::channel::Key::from_u128(902);
    let other = spec::channel::Channel {
        key: index,
        kind: spec::channel::Kind::Index {
            error: None,
            control: None,
        },
    };
    let f64 = types::sample::Type::Scalar(types::sample::Scalar::F64);
    let data = spec::channel::Data::new(
        index,
        None,
        spec::data_type::DataType::Sample(f64),
        None,
    );
    let value = spec::channel::Channel {
        key: types::channel::Key::from_u128(901),
        kind: spec::channel::Kind::Data(data.expect("no unit")),
    };
    vec![
        (name("plant.other"), Definition::Channel(other)),
        (name("plant.data.value"), Definition::Channel(value)),
    ]
}

/// At 0, a `sink` whose first open waits for the home of `plant.data.time`, which the
/// mesh never names. At 1 s, the spec moves `plant.data.value`, the one channel that
/// the selector then matches, to an index whose home is the node, and adds a `quick`.
/// The open checks its names again and opens on the new index, so both start.
#[test]
fn a_move_of_the_channel_of_an_open_that_waits_for_a_home_frees_the_wait() {
    let held: Vec<(u64, Events)> = (0..32)
        .map(|seed| {
            let first = (vec![("plant.a", "sink", NODE, 0)], data().to_vec());
            let second = (
                vec![("plant.a", "sink", NODE, 0), ("plant.b", "quick", NODE, 0)],
                moved(),
            );
            (seed, in_region(seed, first, second))
        })
        .filter(|(_, starts)| {
            *starts != [start("plant.a", 0, 0), start("plant.b", 0, 0)]
        })
        .collect();
    assert_eq!(held, []);
}

/// As above, but the spec at 1 s removes the channels of `plant.data`, so the selector
/// of the `sink` matches no channel. Its open fails with `Empty`, as a new open does,
/// so the `quick` starts.
#[test]
fn a_removal_of_the_channels_of_an_open_that_waits_for_a_home_frees_the_wait() {
    let held: Vec<(u64, Events)> = (0..32)
        .map(|seed| {
            let first = (vec![("plant.a", "sink", NODE, 0)], data().to_vec());
            let second = (
                vec![("plant.a", "sink", NODE, 0), ("plant.b", "quick", NODE, 0)],
                Vec::new(),
            );
            (seed, in_region(seed, first, second))
        })
        .filter(|(_, starts)| *starts != [start("plant.b", 0, 0)])
        .collect();
    assert_eq!(held, []);
}

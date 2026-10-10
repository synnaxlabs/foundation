use std::collections::BTreeMap;
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

/// Each connector name a test uses, in the order of the keys of its status channels.
const NAMES: [&str; 3] = ["plant.a", "plant.b", "plant.c"];

/// Each start of a run: its connector, its time, and the version of its config.
type Starts = Arc<Mutex<Vec<(String, Monotonic, usize)>>>;

/// A kind whose run records its start, spawns a task that ends `linger` after the
/// cancel, or never with `None`, and returns at the cancel, or at once when `brief`.
/// Its config is a version: the count of the attributes of the document.
struct Hold {
    linger: Option<Span>,
    brief: bool,
    starts: Starts,
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
        self.starts.lock().expect("no panic").push(start);
        if self.brief {
            return Ok(());
        }
        let (cancel, clock, linger) =
            (ctx.cancel().clone(), ctx.clock().clone(), self.linger);
        ctx.tasks().spawn(async move {
            cancel.wait().await;
            match linger {
                Some(linger) => clock.sleep(linger).await,
                None => std::future::pending().await,
            }
        });
        ctx.cancel().wait().await;
        Ok(())
    }
}

/// One connector of a spec: its name, its kind, its node, and the version of its
/// config.
type Placed = (&'static str, &'static str, &'static str, usize);

/// One spec at its time: the name of this node in the region, or `None` when it is no
/// member, and the connectors.
type Step = (Span, Option<&'static str>, Vec<Placed>);

/// The step of `spec` at `at` on [`NODE`].
fn on(at: i64, spec: Vec<Placed>) -> Step {
    (ms(at), Some(NODE), spec)
}

/// Applies each step at its time, with the kind `hold`, whose task ends `linger` after
/// the cancel, the kind `quick`, whose task ends at the cancel, and the kind `brief`,
/// whose run returns at once. Gives each start of a run until 10 s, from the time the
/// hub opened.
fn starts(linger: Option<Span>, steps: Vec<Step>) -> Vec<(String, Span, usize)> {
    let mut sim = ::sim::Sim::new(::sim::Config::default());
    let node = sim.node(::sim::node::Config::default());
    let run = sim.run_on(&node, move |node, tasks| async move {
        let starts = Starts::default();
        let hold = |linger, brief| Hold {
            linger,
            brief,
            starts: Arc::clone(&starts),
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
        let config = testing::create_config(env, node.net(), kinds).await;
        let hub = config.hub.clone();
        let clock = node.clock();
        let start = clock.now();
        let mut runs = Runs::new(config);
        for (at, node, spec) in steps {
            clock.sleep_until(start + at).await;
            let definitions = definitions(&spec);
            hub.set_definitions(&definitions);
            runs.apply(node.map(name).as_ref(), &definitions);
        }
        clock.sleep_until(start + ms(10_000)).await;
        let starts = starts.lock().expect("no panic");
        let since = |(connector, at, version): &(String, Monotonic, usize)| {
            (connector.clone(), *at - start, *version)
        };
        starts.iter().map(since).collect()
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
fn a_node_that_is_no_member_runs_no_connector() {
    let spec = || vec![("plant.a", "quick", NODE, 0)];
    let steps = vec![on(0, spec()), (ms(1_000), None, spec()), on(2_000, spec())];
    let starts = starts(None, steps);
    assert_eq!(starts, [start("plant.a", 0, 0), start("plant.a", 2_000, 0)]);
}

#[test]
fn a_connector_whose_run_returned_starts_no_run_until_it_changes() {
    let spec = |version| vec![("plant.a", "brief", NODE, version)];
    let steps = vec![on(0, spec(0)), on(1_000, spec(0)), on(2_000, spec(1))];
    let starts = starts(None, steps);
    assert_eq!(starts, [start("plant.a", 0, 0), start("plant.a", 2_000, 1)]);
}

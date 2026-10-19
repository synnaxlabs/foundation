//! Fixtures that the tests of `plan`, `apply`, `status`, `browse`, and `Node` share.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use connector::kind::{self, Channels, Context, Table};
use connector::supervisor::{self, Supervisor};
use document::Document;
use document::diagnostic::Diagnostic;
use env::files::{self, Operation};
use env::paths::Paths;
use env::tasks::Tasks;
use hub::Hub;
use mesh::Mesh;
use serde_json::{Value, json};
use sim::Sim;
use spec::definition::Definition;
use types::channel::Key;
use types::name::Name;
use wire::hub::client::BODY_BYTES_MAX;

use crate::error::{Error, Sources};
use crate::front_end::{File, FrontEnd, FrontEnds};
use crate::{Config, Failure, Node, Run, plan};

#[path = "../tests/common/founding.rs"]
mod founding;

#[path = "../tests/common/quickstart.rs"]
mod quickstart;

mod tree;

pub(crate) use founding::{ADMIN, NODE, create_mesh};
pub(crate) use quickstart::{plant, shown};
pub(crate) use tree::{ADDRESS, Tree};

pub(crate) const SITE: &str =
    include_str!("../../acceptance/tests/it/fixtures/site.hcl");

/// Lines that `ssh-keygen -t ed25519` wrote.
pub(crate) const BOB: &str = concat!(
    "ssh-ed25519 ",
    "AAAAC3NzaC1lZDI1NTE5AAAAIP0QMDFGOHfS9XR71aVyCvs+QnNQ4BXrHs9dGDDz7KY6",
    " bob@site",
);
pub(crate) const EVE: &str = concat!(
    "ssh-ed25519 ",
    "AAAAC3NzaC1lZDI1NTE5AAAAIGVVuOR8JKYpAcWLMUveadmJ1wUAmYGgIDtqlhFe7Yhg",
    " eve@laptop",
);
pub(crate) const SAM: &str = concat!(
    "ssh-ed25519 ",
    "AAAAC3NzaC1lZDI1NTE5AAAAIBwW/hh0ZkgH9jcODHef/ulXUH2Hx66uLYTxi9hWfS+o",
    " sam@site",
);

/// A kind whose channels are the labels of its `read` blocks, which it writes. It takes
/// each attribute, so it stands in for each kind of the fixtures.
pub(crate) struct Reader {
    /// The counts of the kind.
    counts: &'static [&'static str],
}

/// The stand-in of the `opcua` kind, with the counts that #435 and #2622 give it.
pub(crate) const OPCUA: Reader = Reader {
    counts: &["in", "lost", "missed", "bad", "uncertain", "mistyped"],
};

/// The stand-in of the `influx` kind, with the count `confirmed` that #1734 gives it,
/// the count `lost` that #2725 gives it, and the count `oversized` that #2555 gives it.
pub(crate) const INFLUX: Reader = Reader {
    counts: &["confirmed", "lost", "oversized"],
};

impl kind::Kind for Reader {
    type Config = Vec<Name>;
    const SCHEMA: document::schema::Body = connector::testing::READS;

    fn parse(&self, config: &Document) -> Result<Vec<Name>, Vec<Diagnostic>> {
        let reads = config
            .blocks
            .iter()
            .filter(|block| &*block.keyword == "read");
        Ok(reads
            .map(|block| block.labels[0].text.parse().expect("a name"))
            .collect())
    }

    fn check(&self, writes: &Vec<Name>) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            reader: None,
            writes: writes.clone(),
            counts: self.counts.iter().map(|count| name(count)).collect(),
        })
    }

    fn run(
        &self,
        _: Context<Vec<Name>>,
    ) -> impl Future<Output = Result<(), kind::Error>> {
        std::future::ready(Ok(()))
    }
}

pub(crate) fn front_ends() -> FrontEnds {
    FrontEnds::new("hcl", hcl())
}

/// The front end of HCL.
pub(crate) fn hcl() -> FrontEnd {
    FrontEnd {
        read: config_hcl::read,
        update: |source, text, document| {
            config_hcl::update(source, text, document)
                .map_err(|refusal| Vec::from(&refusal))
        },
    }
}

/// The kinds of the connectors of the fixtures.
pub(crate) fn create_kinds() -> Table {
    Table::new().with("influx", INFLUX).with("opcua", OPCUA)
}

/// The sim node of a run, and a hub on it.
pub(crate) struct Fixture {
    pub(crate) sim: sim::node::Node,
    pub(crate) hub: Hub,
    /// The name of the member of the mesh on `sim`.
    pub(crate) name: Name,
    /// The tasks of the run.
    pub(crate) tasks: Tasks,
}

/// The node of `mesh` on `fixture`, whose channel keys count from 1.
pub(crate) fn create_node(fixture: &Fixture, mesh: Mesh) -> Node {
    create_node_reading(fixture, mesh, front_ends())
}

/// The node of `mesh` on `fixture` that reads with `front_ends`, whose channel keys
/// count from 1.
pub(crate) fn create_node_reading(
    fixture: &Fixture,
    mesh: Mesh,
    front_ends: FrontEnds,
) -> Node {
    let secrets = connector::testing::create_secrets(fixture.sim.files());
    create_node_with(fixture, mesh, front_ends, secrets)
}

/// The node of `mesh` on `fixture` that reads with `front_ends` and reads secrets
/// through `secrets`, whose channel keys count from 1.
pub(crate) fn create_node_with(
    fixture: &Fixture,
    mesh: Mesh,
    front_ends: FrontEnds,
    secrets: secret::Resolver,
) -> Node {
    Node::new(create_config(fixture, mesh, front_ends, secrets))
}

/// The config of a node of `mesh` on `fixture`, as [`create_node_with`] makes it.
pub(crate) fn create_config(
    fixture: &Fixture,
    mesh: Mesh,
    front_ends: FrontEnds,
    secrets: secret::Resolver,
) -> Config {
    let made = Cell::new(0);
    let key = move || {
        made.set(made.get() + 1);
        Key::from_u128(made.get())
    };
    Config {
        mesh,
        keys: Box::new(key),
        front_ends,
        supervisor: create_supervisor(fixture, create_kinds(), secrets),
        hub: fixture.hub.clone(),
        clock: fixture.sim.clock(),
        name: fixture.name.clone(),
        roots: crate::roots::Failures::new(String::new(), Vec::new()),
    }
}

/// The node of `mesh` on `fixture`, as [`create_node`] makes it, with the kinds `tree`,
/// `broken`, `failing`, and `slow` of [`Tree`] in place of the kinds of the fixtures.
pub(crate) fn create_browsing_node(fixture: &Fixture, mesh: Mesh) -> Node {
    let secrets = connector::testing::create_secrets(fixture.sim.files());
    let kinds = Table::new()
        .with("tree", Tree::create())
        .with("broken", Tree::create_broken())
        .with("failing", Tree::create_failing())
        .with("slow", Tree::create_slow());
    Node::new(Config {
        supervisor: create_supervisor(fixture, kinds, secrets.clone()),
        ..create_config(fixture, mesh, front_ends(), secrets)
    })
}

/// A supervisor of `kinds` on `fixture`, which reads secrets through `secrets`.
pub(crate) fn create_supervisor(
    fixture: &Fixture,
    kinds: Table,
    secrets: secret::Resolver,
) -> Supervisor {
    Supervisor::new(supervisor::Config {
        kinds: Arc::new(kinds),
        clock: fixture.sim.clock(),
        entropy: fixture.sim.entropy(),
        net: fixture.sim.net(),
        roots: connector::http::Roots::default(),
        tasks: fixture.tasks.clone(),
        hub: fixture.hub.clone(),
        secrets,
    })
}

/// The plan of `files` on `applied` at `base`, as `Node::plan` makes it with no read
/// of a secret.
pub(crate) fn plan(
    files: &[File],
    base: spec::Pointer,
    applied: &BTreeMap<Name, Definition>,
    members: &BTreeSet<Name>,
    front_ends: &FrontEnds,
    kinds: &Table,
) -> Result<(plan::Output, config::plan::Plan), Error> {
    plan::Documents::read(files, front_ends)?
        .changes(base, applied, members, kinds)
        .map_err(|diagnostics| Error::config(diagnostics, &Sources::of(files)))
}

/// Asserts that `reply` is the error reply of `status` to a request whose `count`
/// problems do not fit in one reply: at most the bound, the first problems, each of
/// `code`, then `ops.more-problems` with the count of the rest. Gives it as JSON.
pub(crate) fn more_problems(
    reply: &[u8],
    code: &str,
    count: usize,
    status: u8,
) -> Value {
    assert!(
        u64::try_from(reply.len()).is_ok_and(|len| len <= BODY_BYTES_MAX),
        "{}",
        reply.len()
    );
    let reply: Value = serde_json::from_slice(reply).expect("JSON");
    let errors = reply["errors"].as_array().expect("errors");
    let (more, kept) = errors.split_last().expect("a problem");
    assert!(!kept.is_empty());
    for problem in kept {
        assert_eq!(problem["code"], code);
    }
    let message = format!(
        "{} more problems do not fit in one reply",
        count - kept.len()
    );
    let expected = json!({
        "code": "ops.more-problems",
        "message": message,
        "fix": "Fix the problems above, then run it again",
        "notes": [],
    });
    assert_eq!(*more, expected);
    assert_eq!(reply["status"], status);
    reply
}

pub(crate) fn name(text: &str) -> Name {
    text.parse().expect("a name")
}

pub(crate) fn files(files: &[(&str, &str)]) -> Vec<File> {
    files
        .iter()
        .map(|(path, text)| File {
            path: PathBuf::from(path),
            text: (*text).to_owned(),
        })
        .collect()
}

/// The run, output, and errors of `foundation <args>`, whose requests go through
/// `reach`.
pub(crate) async fn cli<C>(
    paths: &Paths,
    args: &[&str],
    reach: impl AsyncFnMut(&Path) -> Result<C, Failure>,
) -> (Run, String, String)
where
    C: AsyncFn(&[u8]) -> Result<Vec<u8>, Failure>,
{
    let args = ["foundation"].iter().chain(args).map(Into::into);
    let (mut output, mut errors) = (Vec::new(), Vec::new());
    let input = async || crate::line(&mut std::io::empty());
    let front_ends = front_ends();
    let kinds = create_kinds();
    let run = crate::cli(
        args,
        input,
        &mut output,
        &mut errors,
        paths,
        &front_ends,
        &kinds,
        reach,
    );
    let run = run.await;
    let text = |bytes| String::from_utf8(bytes).expect("UTF-8");
    (run, text(output), text(errors))
}

/// `site.hcl` with a placement that homes its index on `edge`.
pub(crate) fn placed_site() -> String {
    format!("{SITE}placement \"p\" {{\n  select = \"site.*\"\n  home = \"edge\"\n}}\n")
}

/// Runs `body` with the one node of a run and the mesh of [`create_mesh`] on it, whose
/// one member is `edge`, with the definitions of a first start.
pub(crate) fn solo<F: Future<Output = ()> + 'static>(
    body: impl FnOnce(Fixture, Mesh) -> F + Send + 'static,
) {
    founded(&["edge"], spec::founding::create(ADMIN.public()), body);
}

/// Runs `body` with the one node of a run and the mesh of [`create_mesh`] on it, with
/// the members `names` and the founding `definitions`.
pub(crate) fn founded<F: Future<Output = ()> + 'static>(
    names: &'static [&'static str],
    definitions: BTreeMap<Name, Definition>,
    body: impl FnOnce(Fixture, Mesh) -> F + Send + 'static,
) {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let ran = sim.run_on(&node, move |node, tasks| async move {
        let mesh = create_mesh(&node, &tasks, names, definitions).await;
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks: tasks.clone(),
        };
        let (hub, _) = hub::testing::open(env).await;
        let fixture = Fixture {
            sim: node,
            hub,
            name: name(names[0]),
            tasks,
        };
        body(fixture, mesh).await;
    });
    assert_eq!(ran, Ok(()));
}

/// Makes each sync of the log of the mesh of [`create_mesh`] on `node` fail, and gives
/// the stop of its group at its next write of the log.
pub(crate) fn fail_sync(node: &sim::node::Node) -> mesh::Stopped {
    let path = Path::new("log").join("log-0");
    node.fail_file(&path, Operation::Sync);
    let cause = files::Error::Io {
        path,
        operation: Operation::Sync,
        code: 5,
    };
    mesh::Stopped::Write(mesh::log::Error::Files(cause))
}

/// Gives `Key::from_u128(n)` for each `n` from `from` up.
pub(crate) fn keys(from: u128) -> impl FnMut() -> Key {
    let mut next = from;
    move || {
        next += 1;
        Key::from_u128(next)
    }
}

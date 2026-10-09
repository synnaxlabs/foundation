use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::future::poll_fn;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::task::Poll;

use connector::cancel;
use connector::kind::{self, Channels, Context, Table};
use document::Document;
use document::diagnostic::{Code, Diagnostic};
use mesh::Mesh;
use mesh::used::{Behind, Cause};
use sim::Sim;
use spec::Pointer;
use spec::channel::{Edge, Problem};
use spec::definition::{Definition, Kind};
use spec::subject::Subject;
use types::digest::Digest;
use types::ed25519::PrivateKey;
use types::name::{Name, Prefix};

use super::{Applied, apply};
use crate::common::{
    ADMIN, NODE, PLANT, Reader, files, founded, front_ends, keys, name, open,
    placed_site, solo,
};
use crate::error::Error;
use crate::plan::{self, Counts, Output, plan};

/// The plan of `texts` on the spec that `mesh` uses.
async fn plan_on(mesh: &Mesh, texts: &[(&str, &str)]) -> (Output, config::plan::Plan) {
    plan_among(mesh, texts, &mesh.names()).await
}

/// The plan of `texts` on the spec that `mesh` uses, with the members `members`.
async fn plan_among(
    mesh: &Mesh,
    texts: &[(&str, &str)],
    members: &BTreeSet<Name>,
) -> (Output, config::plan::Plan) {
    let spec = mesh.spec().await.expect("a spec");
    plan(
        &files(texts),
        spec.pointer.expect("a spec in use"),
        &spec.definitions,
        members,
        &front_ends(),
        &kinds(),
    )
    .expect("a plan")
}

/// The kinds of the connectors of the fixtures.
fn kinds() -> Table {
    Table::new().with("influx", Reader).with("opcua", Reader)
}

fn path() -> &'static Path {
    Path::new("site.plan")
}

#[test]
fn applies_a_plan_and_then_plans_no_change() {
    solo(|mesh| async move {
        let (_, planned) = plan_on(&mesh, &[("site.hcl", &placed_site())]).await;
        let applied = apply(path(), &planned.encode(), &mesh, &kinds(), keys(0)).await;
        let pointer = mesh.pointer();
        assert_eq!(pointer.version, 1);
        let applied = applied.expect("an apply");
        assert_eq!(
            applied,
            Applied {
                file: "site.plan".to_owned(),
                pointer: plan::Pointer::from(pointer),
                counts: Counts {
                    added: 3,
                    changed: 0,
                    removed: 0,
                },
                homes: 1,
            }
        );
        assert_eq!(
            applied.text(),
            "Applied site.plan: 3 added, 1 home listed.\n"
        );
        let (output, _) = plan_on(&mesh, &[("site.hcl", &placed_site())]).await;
        assert_eq!(output.text(), "0 to add, 0 to change, 0 to remove.\n");
        let time = &mesh.spec().await.expect("a spec").definitions[&name("site.time")];
        let Definition::Channel(time) = time else {
            panic!("a channel at site.time");
        };
        assert_eq!(mesh.watch(time.key).next().await, Ok(Some(NODE)));
    });
}

#[test]
fn refuses_a_plan_of_a_spec_that_changed_and_proposes_nothing() {
    solo(|mesh| async move {
        let base = mesh.pointer();
        let (_, site) = plan_on(&mesh, &[("site.hcl", &placed_site())]).await;
        let (_, plant) = plan_on(&mesh, &[("plant.hcl", PLANT)]).await;
        apply(path(), &site.encode(), &mesh, &kinds(), keys(0))
            .await
            .expect("an apply");
        let pointer = mesh.pointer();
        let error = refuses(
            &mesh,
            &plant.encode(),
            &kinds(),
            Error::Stale { base, pointer },
        )
        .await;
        assert_eq!(
            error.text(),
            format!(
                "error[ops.stale-plan]: the spec changed: the spec is at version 1, \
                 root {}, not at the base version 0, root {} of the plan\nfix: Plan \
                 again\n",
                pointer.root, base.root
            )
        );
    });
}

#[test]
fn counts_each_change_and_removal_of_an_apply() {
    solo(|mesh| async move {
        let channel = |name| {
            format!(
                "channel \"site.{name}\" {{\n  data_type = \"f64\"\n  \
                 index = \"site.time\"\n}}\n"
            )
        };
        let wide = format!("{}{}{}", placed_site(), channel("extra"), channel("more"));
        let (_, planned) = plan_on(&mesh, &[("site.hcl", &wide)]).await;
        apply(path(), &planned.encode(), &mesh, &kinds(), keys(0))
            .await
            .expect("an apply");
        let narrow = placed_site().replace("f64", "f32");
        let (_, planned) = plan_on(&mesh, &[("site.hcl", &narrow)]).await;
        let applied = apply(path(), &planned.encode(), &mesh, &kinds(), keys(10))
            .await
            .expect("an apply");
        assert_eq!(
            applied.counts,
            Counts {
                added: 0,
                changed: 1,
                removed: 2,
            }
        );
    });
}

#[test]
fn refuses_a_stale_plan_with_a_founding_change_as_stale() {
    solo(|mesh| async move {
        let base = mesh.pointer();
        let founding = spec::founding::create(ADMIN.public());
        let other =
            Subject::new(vec![PrivateKey([8; 32]).public()]).expect("a subject");
        let (_, planned) = plan_on(&mesh, &[("plant.hcl", PLANT)]).await;
        let founding = one(
            &planned,
            "@admin.@subject",
            Some(&founding[&name("@admin.@subject")]),
            Some(Definition::Subject(other)),
        );
        let (_, site) = plan_on(&mesh, &[("site.hcl", &placed_site())]).await;
        apply(path(), &site.encode(), &mesh, &kinds(), keys(0))
            .await
            .expect("an apply");
        let pointer = mesh.pointer();
        refuses(
            &mesh,
            &founding.encode(),
            &kinds(),
            Error::Stale { base, pointer },
        )
        .await;
    });
}

#[test]
fn gives_a_stale_plan_when_another_apply_commits_first() {
    solo(|mesh| async move {
        let base = mesh.pointer();
        let (_, site) = plan_on(&mesh, &[("site.hcl", &placed_site())]).await;
        let (_, plant) = plan_on(&mesh, &[("plant.hcl", PLANT)]).await;
        let (site, plant) = (site.encode(), plant.encode());
        let kinds = kinds();
        let mut first = pin!(apply(path(), &site, &mesh, &kinds, keys(0)));
        let mut second = pin!(apply(path(), &plant, &mesh, &kinds, keys(10)));
        let (mut a, mut b) = (None, None);
        poll_fn(|cx| {
            if a.is_none() {
                a = match first.as_mut().poll(cx) {
                    Poll::Ready(applied) => Some(applied),
                    Poll::Pending => None,
                };
            }
            if b.is_none() {
                b = match second.as_mut().poll(cx) {
                    Poll::Ready(applied) => Some(applied),
                    Poll::Pending => None,
                };
            }
            if a.is_some() && b.is_some() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        let pointer = mesh.pointer();
        assert_eq!(pointer.version, 1);
        let stale = Error::Stale { base, pointer };
        let (won, lost) = match (a.expect("an end"), b.expect("an end")) {
            (Ok(won), lost) | (lost, Ok(won)) => (won, lost),
            ends => panic!("no apply in {ends:?}"),
        };
        assert_eq!(won.pointer, plan::Pointer::from(pointer));
        assert_eq!(lost.as_ref().map_err(Error::status), Err(1));
        assert_eq!(lost, Err(stale));
    });
}

/// The plan of `planned` with one change, at `at`: from the stored definition `old`
/// to `new`.
fn one(
    planned: &config::plan::Plan,
    at: &str,
    old: Option<&Definition>,
    new: Option<Definition>,
) -> config::plan::Plan {
    let mut planned = planned.clone();
    let (_, mut change) = planned.changes.pop_first().expect("a change");
    change.old = old.map(|old| Digest::of(&old.encode()));
    let entry = change.new.take().expect("an add");
    change.new = new.map(|definition| {
        let mut entry = entry;
        entry.definition = config::Definition::Spec(definition);
        entry
    });
    planned.changes = BTreeMap::from([(name(at), change)]);
    planned.homes.clear();
    planned
}

#[test]
fn refuses_a_change_of_a_founding_definition_and_proposes_nothing() {
    solo(|mesh| async move {
        let founding = spec::founding::create(ADMIN.public());
        let (subject, access) = (name("@admin.@subject"), name("@admin.@access"));
        let other =
            Subject::new(vec![PrivateKey([8; 32]).public()]).expect("a subject");
        let (_, planned) = plan_on(&mesh, &[("plant.hcl", PLANT)]).await;
        let cases = [
            (
                one(
                    &planned,
                    "@admin.@subject",
                    Some(&founding[&subject]),
                    Some(Definition::Subject(other)),
                ),
                "@admin.@subject",
            ),
            (
                one(&planned, "@admin.@access", Some(&founding[&access]), None),
                "@admin.@access",
            ),
        ];
        for (planned, at) in cases {
            let mismatch = config::plan::Error::Mismatch { name: name(at) };
            let error =
                refuses(&mesh, &planned.encode(), &kinds(), Error::Plan(mismatch))
                    .await;
            assert_eq!(
                error.text(),
                format!(
                    "error[ops.bad-plan]: the plan holds a change at {at} that a plan \
                     of the applied spec cannot make: plan again\nfix: Make a plan \
                     with `foundation plan`, and apply it with no edits\n"
                )
            );
        }
    });
}

/// The plan of `planned` with the connector `plc` of `kind` on `node`.
fn rogue(planned: &config::plan::Plan, kind: &str, node: &str) -> config::plan::Plan {
    let mut planned = planned.clone();
    let change = planned
        .changes
        .get_mut(&name("plc"))
        .expect("a change at plc");
    let entry = change.new.as_mut().expect("an add");
    let config::Definition::Spec(Definition::Connector(plc)) = &entry.definition else {
        panic!("plc is a connector");
    };
    let config = plc.config().clone();
    let plc = spec::connector::Connector::new(name(kind), name(node), config);
    entry.definition = config::Definition::Spec(Definition::Connector(plc));
    planned
}

#[test]
fn refuses_a_plan_that_plan_refuses_and_proposes_nothing() {
    solo(|mesh| async move {
        let (_, planned) = plan_on(&mesh, &[("plant.hcl", PLANT)]).await;
        let cases = [
            (
                rogue(&planned, "nothing", "edge"),
                problem(
                    "connector.unknown-kind",
                    "this build has no connector kind \"nothing\"",
                    "Use one of [\"influx\", \"opcua\"]",
                ),
            ),
            (
                rogue(&planned, "opcua", "nowhere"),
                problem(
                    "config.unknown-node",
                    "no node of the mesh is named `nowhere`",
                    "Name a node of the mesh",
                ),
            ),
        ];
        for (planned, problem) in cases {
            refuses(
                &mesh,
                &planned.encode(),
                &kinds(),
                Error::Config(vec![problem]),
            )
            .await;
        }
    });
}

/// `planned` with an add of `definition` at `at`, made from its add at `plc`.
fn added(
    planned: &config::plan::Plan,
    at: Name,
    definition: Definition,
) -> config::plan::Plan {
    let mut planned = planned.clone();
    let mut change = planned.changes[&name("plc")].clone();
    let entry = change.new.as_mut().expect("an add");
    entry.definition = config::Definition::Spec(definition);
    planned.changes.insert(at, change);
    planned
}

#[test]
fn refuses_a_plan_that_breaks_a_rule_on_names_or_private_keys() {
    solo(|mesh| async move {
        let (_, planned) = plan_on(&mesh, &[("plant.hcl", PLANT)]).await;
        let entry = planned.changes[&name("plc")].new.as_ref().expect("an add");
        let config::Definition::Spec(plc) = entry.definition.clone() else {
            panic!("plc is in the spec");
        };
        let holders = vec![PrivateKey([8; 32]).public()];
        let subject = Definition::Subject(Subject::new(holders).expect("a subject"));
        let at = Kind::Subject.key("plc").expect("a tree key");
        let cases = [
            (
                rogue(&planned, "opcua", "b3BlbnNzaC1rZXktdjEA"),
                problem(
                    "config.private-key",
                    "the value is a private key, which must never be in a file",
                    "Remove the private key from this file now, and use the one line \
                     of its `.pub` file",
                ),
            ),
            (
                added(&planned, name("PLC"), plc),
                problem(
                    "config.duplicate-name",
                    "the name \"plc\" repeats the earlier `connector` name \"PLC\"",
                    "Give each `connector` block a name that differs by more than case",
                ),
            ),
            (
                added(&planned, at, subject),
                problem(
                    "config.subject-is-connector",
                    "the subject \"plc\" has the name of a connector",
                    "Rename the subject or the connector",
                ),
            ),
        ];
        for (planned, problem) in cases {
            refuses(
                &mesh,
                &planned.encode(),
                &kinds(),
                Error::Config(vec![problem]),
            )
            .await;
        }
    });
}

/// A kind that refuses each config.
struct Refuser;

impl kind::Kind for Refuser {
    type Config = ();

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        let code = Code::new("test.refused");
        Err(vec![Diagnostic::new(
            code,
            None,
            "refused".into(),
            "Fix it".into(),
        )])
    }

    fn discover(
        &self,
        _: &cancel::Token,
    ) -> impl Future<Output = Result<Vec<Document>, kind::Error>> {
        std::future::ready(Ok(Vec::new()))
    }

    fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), kind::Error>> {
        std::future::ready(Ok(()))
    }
}

/// The plan with each change and home of `plans`, which share a base.
fn merged(plans: Vec<config::plan::Plan>) -> config::plan::Plan {
    let mut plans = plans.into_iter();
    let mut merged = plans.next().expect("a plan");
    for plan in plans {
        assert_eq!(plan.base, merged.base);
        merged.changes.extend(plan.changes);
        merged.homes.extend(plan.homes);
    }
    merged
}

/// A connector `at` of the kind `opcua` on `node`, which writes each of `writes`.
fn connector(at: &str, node: &str, writes: &[&str]) -> String {
    let reads = writes
        .iter()
        .map(|write| format!("  read \"{write}\" {{}}\n"))
        .collect::<Vec<_>>()
        .concat();
    format!(
        "connector \"{at}\" {{\n  kind = \"opcua\"\n  node = \"{node}\"\n{reads}}}\n"
    )
}

/// A placement `at` that selects `select` with the home `home`.
fn placement(at: &str, select: &str, home: &str) -> String {
    format!("placement \"{at}\" {{\n  select = {select}\n  home = \"{home}\"\n}}\n")
}

/// A `Config` problem with no place.
fn problem(code: &str, message: &str, fix: &str) -> crate::error::Problem {
    crate::error::Problem {
        code: code.into(),
        message: message.into(),
        fix: fix.into(),
        place: None,
        notes: Vec::new(),
    }
}

/// Applies `bytes` with `kinds`, asserts that the apply gives `expected` and proposes
/// nothing, and returns the error.
async fn refuses(mesh: &Mesh, bytes: &[u8], kinds: &Table, expected: Error) -> Error {
    let base = mesh.pointer();
    let error = apply(path(), bytes, mesh, kinds, keys(0))
        .await
        .expect_err("a refused plan");
    assert_eq!(error, expected);
    assert_eq!(mesh.pointer(), base);
    error
}

const A_TIME: &str = "channel \"a.time\" { kind = \"index\" }\n";

/// The plan of the placement `t`, which wins for the connector `a`, and the placement
/// `r`, which wins for its index `a.time`.
async fn split(mesh: &Mesh) -> config::plan::Plan {
    let t = format!(
        "{A_TIME}{}{}",
        connector("a", "edge", &["a.time"]),
        placement("t", "[\"a\", \"a.*\"]", "edge")
    );
    let (_, t) = plan_on(mesh, &[("t.hcl", &t)]).await;
    let r = placement("r", "\"a.time\"", "edge");
    let (_, r) = plan_on(mesh, &[("r.hcl", &r)]).await;
    merged(vec![t, r])
}

#[test]
fn refuses_each_plan_that_check_refuses_and_proposes_nothing() {
    let definitions = spec::founding::create(ADMIN.public());
    founded(&["edge", "other"], definitions, |mesh| async move {
        let (_, placed) = plan_on(&mesh, &[("site.hcl", &placed_site())]).await;
        let mut unplaced = placed.clone();
        unplaced.changes.remove(&name("p.@placement"));
        unplaced.homes.clear();
        let writer = |at, node| format!("{A_TIME}{}", connector(at, node, &["a.time"]));
        let (_, on_edge) = plan_on(&mesh, &[("w.hcl", &writer("w", "edge"))]).await;
        let other = writer("v", "other");
        let (_, on_other) = plan_on(&mesh, &[("v.hcl", &other)]).await;
        let stray = connector("site.c", "other", &[]);
        let (_, stray) = plan_on(&mesh, &[("c.hcl", &stray)]).await;
        let writer_nodes = problem(
            "config.writer-nodes",
            "connectors on the nodes `other` and `edge` write the index `a.time`, so \
             it has no one home",
            "Run each connector that writes `a.time` on one node",
        );
        let connector_home = problem(
            "config.connector-home",
            "the placement `p` names the home `edge`, but the connector `site.c` runs \
             on the node `other`",
            "Name `other` as the `home`, and keep `other` out of `standby` and \
             `copies`",
        );
        let cases = [
            (
                unplaced,
                vec![problem(
                    "config.unplaced",
                    "no placement selects the index, and no connector writes it",
                    "Select the index with a placement that names a `home`, or write \
                     it with a connector",
                )],
            ),
            (
                merged(vec![on_edge.clone(), on_other.clone()]),
                vec![writer_nodes.clone()],
            ),
            (
                merged(vec![placed.clone(), stray.clone()]),
                vec![connector_home.clone()],
            ),
            (
                split(&mesh).await,
                vec![problem(
                    "config.split-placement",
                    "the placement `r` wins for the index `a.time`, but the placement \
                     `t` wins for the connector `a`",
                    "Make the placement `t` win for the connector `a` and its indexes",
                )],
            ),
            (
                merged(vec![on_edge, on_other, placed, stray]),
                vec![writer_nodes, connector_home],
            ),
        ];
        for (planned, problems) in cases {
            refuses(&mesh, &planned.encode(), &kinds(), Error::Config(problems)).await;
        }
    });
}

#[test]
fn refuses_a_config_that_its_kind_refuses_at_the_apply() {
    solo(|mesh| async move {
        let (_, plant) = plan_on(&mesh, &[("plant.hcl", PLANT)]).await;
        let refusing = Table::new().with("influx", Reader).with("opcua", Refuser);
        let refused = problem("test.refused", "refused", "Fix it");
        refuses(
            &mesh,
            &plant.encode(),
            &refusing,
            Error::Config(vec![refused]),
        )
        .await;
    });
}

#[test]
fn refuses_a_plan_with_only_a_home_on_a_node_that_is_not_a_member() {
    let founding = spec::founding::create(ADMIN.public());
    let site = placed_site().replace("home = \"edge\"", "home = \"other\"");
    let members = BTreeSet::from([name("edge"), name("other")]);
    let base = Pointer {
        version: 0,
        root: Digest::of(b"root"),
    };
    let texts = files(&[("site.hcl", &site)]);
    let (_, planned) = plan(&texts, base, &founding, &members, &front_ends(), &kinds())
        .expect("a plan");
    let definitions = planned
        .definitions(&founding, keys(0))
        .expect("definitions");
    founded(&["edge"], definitions, move |mesh| async move {
        let (_, site) = plan_among(&mesh, &[("site.hcl", &site)], &members).await;
        assert!(site.changes.is_empty());
        assert_eq!(site.homes.len(), 1);
        let problem = problem(
            "config.unknown-node",
            "no node of the mesh is named `other`",
            "Name a node of the mesh",
        );
        refuses(
            &mesh,
            &site.encode(),
            &kinds(),
            Error::Config(vec![problem]),
        )
        .await;
    });
}

#[test]
fn refuses_bytes_that_are_not_a_plan_and_proposes_nothing() {
    solo(|mesh| async move {
        let version = config::plan::Error::Version { found: b'p' };
        let error = refuses(&mesh, b"plan", &kinds(), Error::Plan(version)).await;
        assert_eq!(error.status(), 2);
        assert_eq!(
            error.text(),
            "error[ops.bad-plan]: the plan has format version 112, and this build \
             reads only version 1; plan again with this build\nfix: Make a plan with \
             `foundation plan`, and apply it with no edits\n"
        );
        let (_, planned) = plan_on(&mesh, &[("plant.hcl", PLANT)]).await;
        let mut bytes = planned.encode();
        bytes.truncate(2);
        let malformed = config::plan::Error::Malformed { at: 1 };
        let error = refuses(&mesh, &bytes, &kinds(), Error::Plan(malformed)).await;
        assert_eq!(error.status(), 2);
    });
}

#[test]
fn refuses_a_path_that_is_not_utf8_before_it_reads_the_plan() {
    solo(|mesh| async move {
        let path = Path::new(OsStr::from_bytes(b"a\xff.plan"));
        let error = apply(path, b"plan", &mesh, &kinds(), keys(0))
            .await
            .expect_err("a path that is not UTF-8");
        assert_eq!(
            error.text(),
            "\
error[ops.path-not-utf8]: the path \"a\\xFF.plan\" is not UTF-8
fix: Rename the file to a UTF-8 name
"
        );
    });
}

#[test]
fn gives_each_other_error_of_the_mesh_as_an_apply_error() {
    solo(|mesh| async move {
        let (_, site) = plan_on(&mesh, &[("site.hcl", &placed_site())]).await;
        apply(path(), &site.encode(), &mesh, &kinds(), keys(0))
            .await
            .expect("an apply");
        let pointer = mesh.pointer();
        let spec = mesh.spec().await.expect("a spec");
        let time = name("site.time");
        let stored = &spec.definitions[&time];
        let Definition::Channel(stored_time) = stored else {
            panic!("a channel at site.time");
        };
        let mut removal = one(&site, "site.time", Some(stored), None);
        removal.base = pointer;
        let error = apply(path(), &removal.encode(), &mesh, &kinds(), keys(10))
            .await
            .expect_err("a dangling edge");
        let dangling = Problem::Dangling {
            from: name("site.temp"),
            edge: Edge::Index,
            to: stored_time.key,
        };
        let problems = vec![spec::region::Problem::Channel(dangling)];
        let cause = mesh::Error::Problems(problems);
        assert_eq!(
            error.text(),
            format!(
                "error[ops.apply]: {cause}\nfix: Fix the cause in the message, then \
                 plan and apply again\n"
            )
        );
        assert_eq!(error, Error::Apply(cause));
        assert_eq!(error.status(), 1);
        assert_eq!(mesh.pointer(), pointer);
    });
}

#[test]
fn leaves_out_each_count_of_zero() {
    let applied = |added, changed, removed, homes| {
        let pointer = Pointer {
            version: 1,
            root: spec::tree::empty(),
        };
        let applied = Applied {
            file: "a\n.plan".to_owned(),
            pointer: plan::Pointer::from(pointer),
            counts: Counts {
                added,
                changed,
                removed,
            },
            homes,
        };
        applied.text()
    };
    assert_eq!(applied(0, 0, 0, 0), "Applied a\\n.plan: no change.\n");
    assert_eq!(
        applied(1, 2, 3, 2),
        "Applied a\\n.plan: 1 added, 2 changed, 3 removed, 2 homes listed.\n"
    );
    assert_eq!(applied(0, 2, 0, 0), "Applied a\\n.plan: 2 changed.\n");
    assert_eq!(applied(0, 0, 3, 0), "Applied a\\n.plan: 3 removed.\n");
    assert_eq!(applied(0, 0, 0, 1), "Applied a\\n.plan: 1 home listed.\n");
}

#[test]
fn gives_the_json_of_an_apply() {
    let pointer = plan::Pointer::from(Pointer {
        version: 1,
        root: spec::tree::empty(),
    });
    let root = pointer.root.clone();
    let applied = Applied {
        file: "site.plan".to_owned(),
        pointer,
        counts: Counts {
            added: 1,
            changed: 2,
            removed: 3,
        },
        homes: 4,
    };
    let value = serde_json::to_value(&applied).expect("JSON");
    assert_eq!(
        value,
        serde_json::json!({
            "file": "site.plan",
            "pointer": {"version": 1, "root": root},
            "added": 1,
            "changed": 2,
            "removed": 3,
            "homes": 4,
        })
    );
    assert_eq!(
        serde_json::from_value::<Applied>(value).expect("an apply"),
        applied
    );
}

#[test]
fn refuses_a_plan_on_a_node_that_uses_no_spec_as_behind() {
    let mut definitions = spec::founding::create(ADMIN.public());
    let subject = Subject::new(vec![PrivateKey([8; 32]).public()]).expect("a subject");
    let misplaced = name("plant.@x.@subject");
    definitions.insert(misplaced.clone(), Definition::Subject(subject));
    founded(&["edge"], definitions, |mesh| async move {
        let spec = mesh.spec().await.expect("a spec");
        assert_eq!(spec.pointer, None);
        let behind = spec.behind.expect("a node behind");
        let problem = spec::region::Problem::Misplaced {
            name: misplaced,
            kind: Kind::Subject,
        };
        assert_eq!(behind.cause, Cause::Problems(vec![problem.clone()]));
        let (_, planned) = plan(
            &files(&[]),
            behind.pointer,
            &BTreeMap::new(),
            &mesh.names(),
            &front_ends(),
            &kinds(),
        )
        .expect("a plan");
        let error = refuses(
            &mesh,
            &planned.encode(),
            &kinds(),
            Error::Behind(Box::new(behind.clone())),
        )
        .await;
        assert_eq!(error.status(), 1);
        assert_eq!(
            error.text(),
            format!(
                "error[ops.behind]: the node does not use the newest spec, at version \
                 0, root {}: it has problems at this build: {problem}\nfix: Fix the \
                 cause, then plan again\n",
                behind.pointer.root
            )
        );
        let version = config::plan::Error::Version { found: b'p' };
        refuses(&mesh, b"plan", &kinds(), Error::Plan(version)).await;
        assert_eq!(mesh.pointer(), behind.pointer);
    });
}

#[test]
fn writes_the_cause_of_a_node_behind() {
    let pointer = Pointer {
        version: 2,
        root: Digest::of(b"root"),
    };
    let path = PathBuf::from("spec/2");
    let io = env::files::Error::Io {
        path,
        operation: env::files::Operation::Sync,
        code: 5,
    };
    let misplaced = spec::region::Problem::Misplaced {
        name: name("plant.@x.@subject"),
        kind: Kind::Subject,
    };
    let ungoverned = spec::region::Problem::Ungoverned {
        name: name("@region"),
        region: Prefix::ROOT,
    };
    let missing = spec::tree::Error::Missing(Digest::of(b"chunk"));
    let cases = [
        (
            Cause::Read(spec::region::Error::Tree(missing)),
            format!("its tree does not read: {missing}"),
        ),
        (
            Cause::Problems(vec![misplaced.clone(), ungoverned.clone()]),
            format!("it has problems at this build: {misplaced}; {ungoverned}"),
        ),
        (
            Cause::Blob(blob::Error::Files(io.clone())),
            format!(
                "a call of the store failed: {}",
                blob::Error::Files(io.clone())
            ),
        ),
        (
            Cause::Files(io.clone()),
            format!("the file of the pointer in use was not made durable: {io}"),
        ),
    ];
    for (cause, line) in cases {
        let error = Error::Behind(Box::new(Behind { pointer, cause }));
        assert_eq!(
            error.to_string(),
            format!("the node does not use the newest spec, at {pointer}: {line}")
        );
    }
}

#[test]
fn proposes_nothing_for_a_plan_with_no_change() {
    // A site with an index lists its home, so this site has none.
    let people = concat!(
        "subject \"carol\" {\n  keys = [\"ssh-ed25519 ",
        "AAAAC3NzaC1lZDI1NTE5AAAAIP0QMDFGOHfS9XR71aVyCvs+QnNQ4BXrHs9dGDDz7KY6",
        " bob@site\"]\n}\n",
    );
    solo(move |mesh| async move {
        let (_, site) = plan_on(&mesh, &[("people.hcl", people)]).await;
        apply(path(), &site.encode(), &mesh, &kinds(), keys(0))
            .await
            .expect("an apply");
        let pointer = mesh.pointer();
        assert_eq!(pointer.version, 1);
        let (_, planned) = plan_on(&mesh, &[("people.hcl", people)]).await;
        assert!(planned.changes.is_empty() && planned.homes.is_empty());
        let applied = apply(path(), &planned.encode(), &mesh, &kinds(), keys(10))
            .await
            .expect("an apply");
        assert_eq!(
            applied,
            Applied {
                file: "site.plan".to_owned(),
                pointer: plan::Pointer::from(pointer),
                counts: Counts {
                    added: 0,
                    changed: 0,
                    removed: 0,
                },
                homes: 0,
            }
        );
        assert_eq!(applied.text(), "Applied site.plan: no change.\n");
        assert_eq!(mesh.pointer(), pointer);
    });
}

#[test]
fn refuses_a_plan_with_no_change_at_an_old_base_as_stale() {
    solo(|mesh| async move {
        let base = mesh.pointer();
        let (_, empty) = plan_on(&mesh, &[]).await;
        assert!(empty.changes.is_empty() && empty.homes.is_empty());
        let (_, site) = plan_on(&mesh, &[("site.hcl", &placed_site())]).await;
        apply(path(), &site.encode(), &mesh, &kinds(), keys(0))
            .await
            .expect("an apply");
        let pointer = mesh.pointer();
        refuses(
            &mesh,
            &empty.encode(),
            &kinds(),
            Error::Stale { base, pointer },
        )
        .await;
    });
}

#[test]
fn applies_a_plan_with_no_home_and_then_a_plan_with_only_a_home() {
    solo(|mesh| async move {
        let (_, mut site) = plan_on(&mesh, &[("site.hcl", &placed_site())]).await;
        let homes = std::mem::take(&mut site.homes);
        assert!(!homes.is_empty());
        apply(path(), &site.encode(), &mesh, &kinds(), keys(0))
            .await
            .expect("a plan with no home applies");
        let pointer = mesh.pointer();
        assert_eq!(pointer.version, 1);
        let spec = mesh.spec().await.expect("a spec");
        let Definition::Channel(time) = &spec.definitions[&name("site.time")] else {
            panic!("a channel at site.time");
        };
        let key = time.key;
        assert_eq!(mesh.watch(key).next().await, Ok(None));
        let mut only = site.clone();
        only.base = pointer;
        only.changes.clear();
        only.homes = homes;
        let applied = apply(path(), &only.encode(), &mesh, &kinds(), keys(10))
            .await
            .expect("a plan with only a home applies");
        assert_eq!(applied.pointer, plan::Pointer::from(mesh.pointer()));
        assert_eq!(applied.text(), "Applied site.plan: 1 home listed.\n");
        assert_eq!(mesh.pointer().version, 2);
        assert_eq!(mesh.watch(key).next().await, Ok(Some(NODE)));
    });
}

#[test]
fn refuses_a_plan_on_a_node_that_uses_an_old_spec_as_behind() {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let ran = sim.run_on(&node, |node, tasks| async move {
        let definitions = spec::founding::create(ADMIN.public());
        let mesh = open(&node, &tasks, &["edge"], definitions).await;
        let (_, site) = plan_on(&mesh, &[("site.hcl", &placed_site())]).await;
        apply(path(), &site.encode(), &mesh, &kinds(), keys(0))
            .await
            .expect("an apply");
        let first = mesh.pointer();
        let site = placed_site();
        let (_, both) =
            plan_on(&mesh, &[("site.hcl", &site), ("plant.hcl", PLANT)]).await;
        node.fail_file(Path::new("spec"), env::files::Operation::SyncDir);
        apply(path(), &both.encode(), &mesh, &kinds(), keys(10))
            .await
            .expect("a second apply");
        let spec = mesh.spec().await.expect("a spec");
        assert_eq!(spec.pointer, Some(first));
        let behind = spec.behind.expect("a node behind");
        refuses(
            &mesh,
            &both.encode(),
            &kinds(),
            Error::Behind(Box::new(behind)),
        )
        .await;
    });
    assert_eq!(ran, Ok(()));
}

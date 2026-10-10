use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use connector::kind::Table;
use document::diagnostic::{Code, Diagnostic};
use mesh::Mesh;
use serde_json::{Value, json};
use types::channel::Key;

use super::Node;
use crate::apply::Applied;
use crate::common::{
    NODE, PLANT, Reader, fail_sync, front_ends, hcl, placed_site, solo,
};
use crate::error::Error;
use crate::front_end::{File, FrontEnd, FrontEnds};
use crate::plan::{self, Counts};
use crate::used;

/// The node of `mesh`, whose channel keys count from 1.
fn create_node(mesh: Mesh) -> Node {
    create_node_reading(mesh, front_ends())
}

/// The node of `mesh` that reads with `front_ends`, whose channel keys count from 1.
fn create_node_reading(mesh: Mesh, front_ends: FrontEnds) -> Node {
    let made = Cell::new(0);
    let key = move || {
        made.set(made.get() + 1);
        Key::from_u128(made.get())
    };
    Node::new(mesh, key, front_ends, Arc::new(kinds()))
}

fn kinds() -> Table {
    Table::new().with("influx", Reader).with("opcua", Reader)
}

/// The JSON output of `plan::plan` of `files` on `mesh`.
async fn planned(mesh: &Mesh, files: Vec<(PathBuf, String)>) -> Value {
    let spec = mesh.spec().await.expect("a spec");
    let files: Vec<File> = files
        .into_iter()
        .map(|(path, text)| File { path, text })
        .collect();
    let base = spec.pointer.expect("a spec in use");
    let names = mesh.names();
    plan::plan(
        &files,
        base,
        &spec.definitions,
        &names,
        &front_ends(),
        &kinds(),
    )
    .expect("a plan")
    .0
    .json()
}

fn site() -> Vec<(PathBuf, String)> {
    vec![(PathBuf::from("site.hcl"), placed_site())]
}

#[test]
fn applies_with_each_connector_kind_of_the_node() {
    solo(|_, mesh| async move {
        let node = create_node(mesh.clone());
        let files = vec![(PathBuf::from("plant.hcl"), PLANT.to_owned())];
        let (plan, _) = node.plan(files).await.expect("a plan");
        let applied = node.apply(Path::new("plant.plan"), &plan).await;
        let expected = Applied {
            file: "plant.plan".to_owned(),
            pointer: plan::Pointer::from(mesh.pointer()),
            counts: Counts {
                added: 6,
                changed: 0,
                removed: 0,
            },
            homes: 1,
        };
        assert_eq!(applied, Ok(serde_json::to_value(expected).expect("JSON")));
    });
}

#[test]
fn plans_and_applies_in_the_json_of_the_cli() {
    solo(|_, mesh| async move {
        let node = create_node(mesh.clone());
        let expected = planned(&mesh, site()).await;
        let (plan, output) = node.plan(site()).await.expect("a plan");
        assert_eq!(output["added"], 3, "{output}");
        assert_eq!(output, expected);
        let applied = node.apply(Path::new("site.plan"), &plan).await;
        let pointer = mesh.pointer();
        let expected = Applied {
            file: "site.plan".to_owned(),
            pointer: plan::Pointer::from(pointer),
            counts: Counts {
                added: 3,
                changed: 0,
                removed: 0,
            },
            homes: 1,
        };
        assert_eq!(applied, Ok(serde_json::to_value(expected).expect("JSON")));
        let (_, output) = node.plan(site()).await.expect("a second plan");
        assert_eq!(output["changes"], json!([]), "{output}");
        let definitions = mesh.spec().await.expect("a spec").definitions;
        let Some(spec::definition::Definition::Channel(time)) =
            definitions.get(&"site.time".parse().expect("a name"))
        else {
            panic!("a channel at site.time");
        };
        assert_eq!(mesh.watch(time.key).next().await, Ok(Some(NODE)));
    });
}

#[test]
fn plans_with_each_connector_kind_of_the_node() {
    solo(|_, mesh| async move {
        let node = create_node(mesh.clone());
        let files = vec![(PathBuf::from("plant.hcl"), PLANT.to_owned())];
        let expected = planned(&mesh, files.clone()).await;
        let (_, output) = node.plan(files).await.expect("a plan");
        assert_eq!(output, expected);
    });
}

#[test]
fn gives_the_json_error_of_a_plan() {
    solo(|_, mesh| async move {
        assert_unknown(mesh, front_ends(), "`.hcl`").await;
    });
}

#[test]
fn gives_the_json_error_of_an_apply() {
    solo(|_, mesh| async move {
        let node = create_node(mesh.clone());
        let base = mesh.pointer();
        let (plan, _) = node.plan(site()).await.expect("a plan");
        let path = Path::new("site.plan");
        node.apply(path, &plan).await.expect("an apply");
        let error = node.apply(path, &plan).await.expect_err("a stale plan");
        let pointer = mesh.pointer();
        assert_eq!(error, Error::Stale { base, pointer }.json());
    });
}

#[test]
fn debug_names_each_front_end() {
    solo(|_, mesh| async move {
        let node = create_node(mesh);
        let expected = r#"Node { front_ends: FrontEnds(["hcl"]), .. }"#;
        assert_eq!(format!("{node:?}"), expected);
        assert_eq!(format!("{:?}", FrontEnd { read: hcl }), "FrontEnd { .. }");
    });
}

/// Plans a `.txt` file on a node of `mesh` that reads with `front_ends`, and asserts
/// that its error names `extensions` in the fix.
async fn assert_unknown(mesh: Mesh, front_ends: FrontEnds, extensions: &str) {
    let node = create_node_reading(mesh, front_ends);
    let files = vec![(PathBuf::from("site.txt"), placed_site())];
    let error = node.plan(files).await.expect_err("an unknown extension");
    let expected = json!({ "errors": [{
        "code": "ops.unknown-extension",
        "fix": format!("Use a file that ends in {extensions}"),
        "message": "no config syntax reads this file",
        "notes": [],
        "place": { "column": 1, "file": "site.txt", "line": 1 },
    }]});
    assert_eq!(error, expected);
}

#[test]
fn names_each_extension_of_the_table_in_the_fix() {
    solo(|_, mesh| async move {
        let two = front_ends().with("toml", FrontEnd { read: hcl });
        assert_unknown(mesh.clone(), two.clone(), "`.hcl` or `.toml`").await;
        let three = two.with("yaml", FrontEnd { read: hcl });
        assert_unknown(mesh, three, "`.hcl`, `.toml`, or `.yaml`").await;
    });
}

#[test]
fn reads_with_the_last_front_end_of_an_extension() {
    solo(|_, mesh| async move {
        let refuse = FrontEnd {
            read: |_, _| {
                let code = Code::new("test.refused");
                Err(vec![Diagnostic::new(
                    code,
                    None,
                    "refused".into(),
                    "Fix it".into(),
                )])
            },
        };
        let front_ends =
            FrontEnds::new("hcl", refuse).with("hcl", FrontEnd { read: hcl });
        let node = create_node_reading(mesh.clone(), front_ends);
        let expected = planned(&mesh, site()).await;
        let (_, output) = node.plan(site()).await.expect("a plan");
        assert_eq!(output, expected);
    });
}

/// The output of `ops.stopped` for `stopped`.
fn stopped_errors(stopped: &mesh::Stopped) -> Value {
    json!({ "errors": [{
        "code": "ops.stopped",
        "message": stopped.to_string(),
        "fix": "Fix the cause in the message, then start the node and plan again",
        "notes": [],
    }] })
}

#[test]
fn gives_a_stop_of_the_group_as_stopped_in_plan() {
    solo(|sim, mesh| async move {
        let node = create_node(mesh.clone());
        let (plan, _) = node.plan(site()).await.expect("a plan");
        let stopped = fail_sync(&sim);
        let applied = node.apply(Path::new("site.plan"), &plan).await;
        assert_eq!(applied, Err(stopped_errors(&stopped)));
        let planned = node.plan(site()).await.map(|(_, output)| output);
        assert_eq!(planned, Err(stopped_errors(&stopped)));
        let error = used::spec(&mesh).await.expect_err("a stop");
        assert_eq!(error, Error::Stopped(stopped));
        assert_eq!(error.status(), 1);
    });
}

#[test]
fn gives_a_stop_of_the_group_as_stopped_in_apply_of_a_plan_with_no_change() {
    solo(|sim, mesh| async move {
        let node = create_node(mesh);
        let (plan, _) = node.plan(site()).await.expect("a plan");
        let (empty, output) = node.plan(Vec::new()).await.expect("a plan");
        assert_eq!(output["added"], json!(0));
        assert_eq!(output["removed"], json!(0));
        let stopped = fail_sync(&sim);
        let applied = node.apply(Path::new("site.plan"), &plan).await;
        assert_eq!(applied, Err(stopped_errors(&stopped)));
        let applied = node.apply(Path::new("empty.plan"), &empty).await;
        assert_eq!(applied, Err(stopped_errors(&stopped)));
    });
}

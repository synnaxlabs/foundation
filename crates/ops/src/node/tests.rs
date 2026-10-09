use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use connector::kind::Table;
use document::Source;
use mesh::Mesh;
use serde_json::json;
use types::channel::Key;

use super::Node;
use crate::apply::Applied;
use crate::common::{NODE, Reader, front_ends, placed_site, solo};
use crate::error::{Error, Problem};
use crate::front_end;
use crate::plan::{self, Counts};

/// The node of `mesh`, whose channel keys count from 1.
fn create_node(mesh: Mesh) -> Node {
    let made = Cell::new(0);
    let key = move || {
        made.set(made.get() + 1);
        Key::from_u128(made.get())
    };
    let kinds = Table::new().with("influx", Reader).with("opcua", Reader);
    Node::new(mesh, key, front_ends(), kinds)
}

fn site() -> Vec<(PathBuf, String)> {
    vec![(PathBuf::from("site.hcl"), placed_site())]
}

#[test]
#[should_panic(expected = "`ops::Node` needs a front end")]
fn refuses_an_empty_table_of_front_ends() {
    solo(|mesh| async move {
        drop(Node::new(mesh, || Key::from_u128(1), BTreeMap::new(), Table::new()));
    });
}

#[test]
fn plans_and_applies_in_the_json_of_the_cli() {
    solo(|mesh| async move {
        let node = create_node(mesh.clone());
        let (plan, output) = node.plan(site()).await.expect("a plan");
        assert_eq!(output["added"], 3, "{output}");
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
fn gives_the_json_error_of_a_plan() {
    solo(|mesh| async move {
        let node = create_node(mesh);
        let files = vec![(PathBuf::from("site.txt"), placed_site())];
        let error = node.plan(files).await.expect_err("an unknown extension");
        let problem = front_end::unknown(Source(0), &front_ends());
        let paths = [PathBuf::from("site.txt")];
        let expected = Error::Config(vec![Problem::of(problem, &paths)]);
        assert_eq!(error, expected.json());
    });
}

#[test]
fn gives_the_json_error_of_an_apply() {
    solo(|mesh| async move {
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

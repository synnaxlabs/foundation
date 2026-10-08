use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use connector::cancel;
use connector::kind::{self, Channels, Context, Table};
use document::diagnostic::Diagnostic;
use document::{Document, Source};
use spec::channel::{self, Channel, Data};
use spec::data_type::DataType;
use spec::definition::Definition;
use types::channel::Key;
use types::name::Name;
use types::sample;

use super::{Planned, Problems, plan};
use crate::front_end::{self, File, FrontEnd};

const PLANT: &str = include_str!("../../../acceptance/tests/it/fixtures/plant.hcl");
const SITE: &str = include_str!("../../../acceptance/tests/it/fixtures/site.hcl");

/// A kind whose channels are the labels of its `read` blocks, which it writes. It takes
/// each attribute, so it stands in for each kind of the fixtures.
struct Reader;

impl kind::Kind for Reader {
    type Config = Vec<Name>;

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
            reads: Vec::new(),
            writes: writes.clone(),
        })
    }

    fn discover(
        &self,
        _: &cancel::Token,
    ) -> impl Future<Output = Result<Vec<Document>, kind::Error>> {
        std::future::ready(Ok(Vec::new()))
    }

    fn run(
        &self,
        _: Context<Vec<Name>>,
    ) -> impl Future<Output = Result<(), kind::Error>> {
        std::future::ready(Ok(()))
    }
}

fn hcl(source: Source, text: &str) -> Result<Document, Vec<Diagnostic>> {
    config_hcl::read(source, text)
        .map_err(|errors| errors.iter().map(Diagnostic::from).collect())
}

fn front_ends() -> BTreeMap<&'static str, FrontEnd> {
    BTreeMap::from([("hcl", FrontEnd { read: hcl })])
}

fn name(text: &str) -> Name {
    text.parse().expect("a name")
}

fn files(files: &[(&str, &str)]) -> Vec<File> {
    files
        .iter()
        .map(|(path, text)| File {
            path: PathBuf::from(path),
            text: (*text).to_owned(),
        })
        .collect()
}

fn empty() -> spec::Pointer {
    spec::Pointer {
        version: 0,
        root: spec::tree::empty(),
    }
}

fn run(
    texts: &[(&str, &str)],
    applied: &BTreeMap<Name, Definition>,
) -> Result<Planned, Problems> {
    let kinds = Table::new().with("influx", Reader).with("opcua", Reader);
    let members = BTreeSet::from([name("edge")]);
    plan(
        &files(texts),
        empty(),
        applied,
        &members,
        &front_ends(),
        &kinds,
    )
}

fn problems(texts: &[(&str, &str)]) -> Problems {
    run(texts, &BTreeMap::new()).expect_err("problems")
}

fn channel(key: u128, kind: channel::Kind) -> Definition {
    Definition::Channel(Channel {
        key: Key::from_u128(key),
        kind,
    })
}

/// `site.hcl` with a placement that homes its index on `edge`.
fn placed_site() -> String {
    format!("{SITE}placement \"p\" {{\n  select = \"site.*\"\n  home = \"edge\"\n}}\n")
}

const INDEX: channel::Kind = channel::Kind::Index {
    error: None,
    control: None,
};

#[test]
fn shows_each_definition_in_file_order_then_the_counts() {
    let planned = run(&[("plant.hcl", PLANT)], &BTreeMap::new()).expect("a plan");
    assert_eq!(
        planned.to_string(),
        "\
+ channel plant.time
+ channel plant.spike
+ channel plant.dip
+ channel plant.trend
+ connector plc
+ connector influx
6 to add, 0 to change, 0 to remove.
"
    );
}

#[test]
fn follows_the_order_of_the_files() {
    let a = "channel \"a.time\" { kind = \"index\" }\n";
    let b = "\
channel \"b.time\" { kind = \"index\" }
placement \"p\" {
  select = \"*.time\"
  home = \"edge\"
}
";
    let planned = run(&[("b.hcl", b), ("a.hcl", a)], &BTreeMap::new()).expect("a plan");
    assert_eq!(
        planned.to_string(),
        "\
+ channel b.time
+ placement p
+ channel a.time
3 to add, 0 to change, 0 to remove.
"
    );
}

#[test]
fn shows_a_change_then_a_removal() {
    let i64 = Data::new(
        Key::from_u128(1),
        None,
        DataType::Sample(sample::Type::Scalar(sample::Scalar::I64)),
        None,
    )
    .expect("a data channel");
    let applied = BTreeMap::from([
        (name("site.time"), channel(1, INDEX)),
        (name("site.temp"), channel(2, channel::Kind::Data(i64))),
        (name("gone.time"), channel(3, INDEX)),
    ]);
    let placed = placed_site();
    let planned = run(&[("site.hcl", &placed)], &applied).expect("a plan");
    assert_eq!(
        planned.to_string(),
        "\
~ channel site.temp
+ placement p
- channel gone.time
1 to add, 1 to change, 1 to remove.
"
    );
}

#[test]
fn gives_the_json_of_the_site_plan() {
    let placed = placed_site();
    let planned = run(&[("site.hcl", &placed)], &BTreeMap::new()).expect("a plan");
    let json = serde_json::to_string_pretty(&planned.json()).expect("JSON");
    assert_eq!(format!("{json}\n"), include_str!("site.golden.json"));
}

#[test]
fn gives_each_problem_with_its_place_and_fix() {
    let wrong = PLANT.replacen("data_type", "datatype", 1);
    assert_eq!(
        problems(&[("plant.hcl", &wrong)]).to_string(),
        "\
error[document.missing-attribute]: the `channel` block has no `data_type`
  --> plant.hcl:5:1
fix: Add a `data_type` attribute such as \"f64\"

error[document.unknown-attribute]: `datatype` is not an attribute of the `channel` block
  --> plant.hcl:6:3
fix: Use `kind`, `data_type`, `index`, `quality`, or `unit`, or remove it
"
    );
}

#[test]
fn refuses_a_file_that_no_front_end_reads() {
    let problems = problems(&[("plant.yaml", ""), ("site.hcl", SITE), ("plant", "")]);
    assert_eq!(
        problems.to_string(),
        "\
error[ops.unknown-extension]: no config syntax reads `plant.yaml`
fix: Use a file that ends in `.hcl`

error[ops.unknown-extension]: no config syntax reads `plant`
fix: Use a file that ends in `.hcl`
"
    );
    assert_eq!(
        problems.json(),
        serde_json::json!({ "errors": [
            {
                "code": "ops.unknown-extension",
                "message": "no config syntax reads `plant.yaml`",
                "fix": "Use a file that ends in `.hcl`",
                "notes": [],
            },
            {
                "code": "ops.unknown-extension",
                "message": "no config syntax reads `plant`",
                "fix": "Use a file that ends in `.hcl`",
                "notes": [],
            },
        ]})
    );
}

#[test]
fn names_each_extension_of_the_table_in_the_fix() {
    let mut front_ends = front_ends();
    front_ends.insert("toml", FrontEnd { read: hcl });
    let two = front_end::unknown(&PathBuf::from("plant.json"), &front_ends);
    assert_eq!(two.fix, "Use a file that ends in `.hcl` or `.toml`");
    front_ends.insert("yaml", FrontEnd { read: hcl });
    let diagnostic = front_end::unknown(&PathBuf::from("plant.json"), &front_ends);
    assert_eq!(
        diagnostic.fix,
        "Use a file that ends in `.hcl`, `.toml`, or `.yaml`"
    );
}

#[test]
fn gives_each_note_with_its_place() {
    let time = "channel \"a.time\" { kind = \"index\" }\n";
    let problems = problems(&[("one.hcl", time), ("two.hcl", time)]);
    assert_eq!(
        problems.to_string(),
        "\
error[config.duplicate-name]: the name \"a.time\" repeats the earlier `channel` name \
 \"a.time\"
  --> two.hcl:1:9
fix: Give each `channel` block a name that differs by more than case
note: the earlier name
  --> one.hcl:1:9
"
    );
    assert_eq!(
        problems.json(),
        serde_json::json!({ "errors": [{
            "code": "config.duplicate-name",
            "message": "the name \"a.time\" repeats the earlier `channel` name \
                \"a.time\"",
            "fix": "Give each `channel` block a name that differs by more than case",
            "file": "two.hcl",
            "line": 1,
            "column": 9,
            "notes": [{
                "text": "the earlier name",
                "file": "one.hcl",
                "line": 1,
                "column": 9,
            }],
        }]})
    );
}

#[test]
fn reads_a_file_named_only_by_its_extension() {
    let placed = placed_site();
    let planned = run(&[("configs/.hcl", &placed)], &BTreeMap::new()).expect("a plan");
    assert_eq!(
        planned.to_string(),
        "\
+ channel site.time
+ channel site.temp
+ placement p
3 to add, 0 to change, 0 to remove.
"
    );
}

#[test]
#[should_panic(expected = "invariant: a front end gives a problem with each error")]
fn refuses_a_front_end_error_with_no_problem() {
    let front_ends = BTreeMap::from([(
        "hcl",
        FrontEnd {
            read: |_, _| Err(Vec::new()),
        },
    )]);
    let applied = BTreeMap::from([(name("a.time"), channel(1, INDEX))]);
    drop(plan(
        &files(&[("a.hcl", "")]),
        empty(),
        &applied,
        &BTreeSet::new(),
        &front_ends,
        &Table::new(),
    ));
}

#[test]
fn keeps_the_quote_of_a_producer_as_it_is() {
    let text = "\
channel \"a.time\" { kind = \"index\" }
channel \"a.v\" {
  index     = \"a.time\"
  data_type = \"f\\t64\"
}
";
    let message = "cannot read the data type \"f\\t64\": expected a data type such as \
                   f64, f32[3], list<u8, 16>, string, bytes, or quality";
    let fix = "Use one of the forms that the message names, with exact case and a \
               space only after the comma of a list";
    let problems = problems(&[("a.hcl", text)]);
    assert_eq!(
        problems.to_string(),
        format!(
            "error[config.bad-data-type]: {message}\n  --> a.hcl:4:15\nfix: {fix}\n"
        )
    );
    assert_eq!(
        problems.json(),
        serde_json::json!({ "errors": [{
            "code": "config.bad-data-type",
            "message": message,
            "fix": fix,
            "file": "a.hcl",
            "line": 4,
            "column": 15,
            "notes": [],
        }]})
    );
}

#[test]
fn gives_the_problems_of_a_front_end_and_of_each_extension_in_file_order() {
    let time = "channel \"a.time\" { kind = \"index\" }\n";
    let problems = problems(&[
        ("good.hcl", time),
        ("bad.hcl", "channel {\n"),
        ("x.yaml", ""),
    ]);
    assert_eq!(
        problems.to_string(),
        "\
error[hcl.syntax]: the file needs a key, a block, or the end of the body here
  --> bad.hcl:2:1
fix: Write it here, or correct the text here or before it

error[ops.unknown-extension]: no config syntax reads `x.yaml`
fix: Use a file that ends in `.hcl`
"
    );
}

#[test]
fn gives_the_json_of_a_change_and_a_removal() {
    let i64 = Data::new(
        Key::from_u128(1),
        None,
        DataType::Sample(sample::Type::Scalar(sample::Scalar::I64)),
        None,
    )
    .expect("a data channel");
    let applied = BTreeMap::from([
        (name("site.time"), channel(1, INDEX)),
        (name("site.temp"), channel(2, channel::Kind::Data(i64))),
        (name("gone.time"), channel(3, INDEX)),
        (name("old.time"), channel(4, INDEX)),
    ]);
    let planned = run(&[("site.hcl", &placed_site())], &applied).expect("a plan");
    let changes = &planned.json()["changes"];
    assert_eq!(
        *changes,
        serde_json::json!([
            {
                "action": "change",
                "kind": "channel",
                "name": "site.temp",
                "file": "site.hcl",
                "line": 2,
                "column": 9,
            },
            {
                "action": "add",
                "kind": "placement",
                "name": "p",
                "file": "site.hcl",
                "line": 6,
                "column": 11,
            },
            { "action": "remove", "kind": "channel", "name": "gone.time" },
            { "action": "remove", "kind": "channel", "name": "old.time" },
        ])
    );
    let json = planned.json();
    let counts = ["added", "changed", "removed"].map(|count| json[count].clone());
    assert_eq!(counts, [1, 1, 2].map(serde_json::Value::from));
}

#[test]
#[should_panic(expected = "invariant: `node` gives a front end")]
fn refuses_an_empty_table_of_front_ends() {
    drop(plan(
        &[],
        empty(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &Table::new(),
    ));
}

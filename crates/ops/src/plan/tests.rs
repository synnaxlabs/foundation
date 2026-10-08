use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use connector::cancel;
use connector::kind::{self, Channels, Context, Table};
use document::diagnostic::{Code, Diagnostic, Note};
use document::{Document, Position, Source, Span};
use spec::channel::{self, Channel, Data};
use spec::data_type::DataType;
use spec::definition::Definition;
use spec::subject::Subject;
use types::channel::Key;
use types::name::Name;
use types::sample;

use super::{Output, plan};
use crate::error::Error;
use crate::front_end::{self, File, FrontEnd};

pub(crate) const PLANT: &str =
    include_str!("../../../acceptance/tests/it/fixtures/plant.hcl");
pub(crate) const SITE: &str =
    include_str!("../../../acceptance/tests/it/fixtures/site.hcl");

/// A kind whose channels are the labels of its `read` blocks, which it writes. It takes
/// each attribute, so it stands in for each kind of the fixtures.
pub(crate) struct Reader;

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

pub(crate) fn front_ends() -> BTreeMap<&'static str, FrontEnd> {
    BTreeMap::from([("hcl", FrontEnd { read: hcl })])
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

fn empty() -> spec::Pointer {
    spec::Pointer {
        version: 0,
        root: spec::tree::empty(),
    }
}

fn run(
    texts: &[(&str, &str)],
    applied: &BTreeMap<Name, Definition>,
) -> Result<Output, Error> {
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
    .map(|(output, _)| output)
}

fn problems(texts: &[(&str, &str)]) -> Error {
    run(texts, &BTreeMap::new()).expect_err("problems")
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::to_value(output).expect("JSON")
}

fn channel(key: u128, kind: channel::Kind) -> Definition {
    Definition::Channel(Channel {
        key: Key::from_u128(key),
        kind,
    })
}

/// `site.hcl` with a placement that homes its index on `edge`.
pub(crate) fn placed_site() -> String {
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
        planned.text(),
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
        planned.text(),
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
        planned.text(),
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
    let value = json(&planned);
    let text = serde_json::to_string_pretty(&value).expect("JSON");
    assert_eq!(format!("{text}\n"), include_str!("site.golden.json"));
    assert_eq!(
        serde_json::from_value::<Output>(value).expect("an output"),
        planned
    );
}

#[test]
fn gives_each_problem_with_its_place_and_fix() {
    let wrong = PLANT.replacen("data_type", "datatype", 1);
    assert_eq!(
        problems(&[("plant.hcl", &wrong)]).text(),
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
        problems.text(),
        "\
error[ops.unknown-extension]: no config syntax reads this file
  --> plant.yaml:1:1
fix: Use a file that ends in `.hcl`

error[ops.unknown-extension]: no config syntax reads this file
  --> plant:1:1
fix: Use a file that ends in `.hcl`
"
    );
    assert_eq!(
        problems.json(),
        serde_json::json!({ "errors": [
            {
                "code": "ops.unknown-extension",
                "message": "no config syntax reads this file",
                "fix": "Use a file that ends in `.hcl`",
                "place": { "file": "plant.yaml", "line": 1, "column": 1 },
                "notes": [],
            },
            {
                "code": "ops.unknown-extension",
                "message": "no config syntax reads this file",
                "fix": "Use a file that ends in `.hcl`",
                "place": { "file": "plant", "line": 1, "column": 1 },
                "notes": [],
            },
        ]})
    );
}

#[test]
fn names_each_extension_of_the_table_in_the_fix() {
    let mut front_ends = front_ends();
    front_ends.insert("toml", FrontEnd { read: hcl });
    let two = front_end::unknown(Source(0), &front_ends);
    assert_eq!(two.fix, "Use a file that ends in `.hcl` or `.toml`");
    front_ends.insert("yaml", FrontEnd { read: hcl });
    let diagnostic = front_end::unknown(Source(0), &front_ends);
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
        problems.text(),
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
            "place": { "file": "two.hcl", "line": 1, "column": 9 },
            "notes": [{
                "text": "the earlier name",
                "place": { "file": "one.hcl", "line": 1, "column": 9 },
            }],
        }]})
    );
}

#[test]
fn reads_a_file_named_only_by_its_extension() {
    let placed = placed_site();
    let planned = run(&[("configs/.hcl", &placed)], &BTreeMap::new()).expect("a plan");
    assert_eq!(
        planned.text(),
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
        problems.text(),
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
            "place": { "file": "a.hcl", "line": 4, "column": 15 },
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
        problems.text(),
        "\
error[hcl.syntax]: the file needs a key, a block, or the end of the body here
  --> bad.hcl:2:1
fix: Write it here, or correct the text here or before it

error[ops.unknown-extension]: no config syntax reads this file
  --> x.yaml:1:1
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
    let json = json(&planned);
    let changes = &json["changes"];
    assert_eq!(
        *changes,
        serde_json::json!([
            {
                "action": "change",
                "kind": "channel",
                "name": "site.temp",
                "place": { "file": "site.hcl", "line": 2, "column": 9 },
            },
            {
                "action": "add",
                "kind": "placement",
                "name": "p",
                "place": { "file": "site.hcl", "line": 6, "column": 11 },
            },
            { "action": "remove", "kind": "channel", "name": "gone.time" },
            { "action": "remove", "kind": "channel", "name": "old.time" },
        ])
    );
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

#[test]
fn names_one_problem_or_the_count_and_exits_with_2() {
    let wrong = PLANT.replacen("data_type", "datatype", 1);
    let two = problems(&[("plant.hcl", &wrong)]);
    assert_eq!(
        (two.to_string(), two.status()),
        ("the config files have 2 problems".to_owned(), 2)
    );
    assert_eq!(
        problems(&[("x.yaml", "")]).to_string(),
        "no config syntax reads this file"
    );
}

#[test]
fn escapes_a_control_character_in_the_text_of_a_problem() {
    let front_ends = BTreeMap::from([(
        "hcl",
        FrontEnd {
            read: |source, _| {
                let start = Position {
                    offset: 0,
                    line: 0,
                    column: 0,
                };
                let mut diagnostic = Diagnostic::new(
                    Code::new("test.raw"),
                    None,
                    "a\u{1b}[2J\nb\\n".to_owned(),
                    "c\rd".to_owned(),
                );
                diagnostic.notes.push(Note {
                    span: Span::new(source, start, start).expect("a span"),
                    text: "e\u{7}f".to_owned(),
                });
                Err(vec![diagnostic])
            },
        },
    )]);
    let error = plan(
        &files(&[("a\u{1b}\\.hcl", "")]),
        empty(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        &front_ends,
        &Table::new(),
    )
    .expect_err("problems");
    assert_eq!(
        error.text(),
        "error[test.raw]: a\\u{1b}[2J\\nb\\n\n\
         fix: c\\rd\n\
         note: e\\u{7}f\n  \
         --> a\\u{1b}\\\\.hcl:1:1\n"
    );
}

#[test]
fn escapes_the_place_of_a_file_that_no_front_end_reads() {
    assert_eq!(
        problems(&[("a\\\u{202e}\u{2028}b.yaml", "")]).text(),
        "error[ops.unknown-extension]: no config syntax reads this file\n  \
         --> a\\\\\\u{202e}\\u{2028}b.yaml:1:1\n\
         fix: Use a file that ends in `.hcl`\n"
    );
}

#[test]
fn gives_the_exact_path_in_the_json_of_a_place() {
    let path = "a\u{1b}\\\u{202e}.hcl";
    let problems = problems(&[(path, "channel {\n")]);
    assert_eq!(problems.json()["errors"][0]["place"]["file"], path);
}

#[test]
fn picks_the_front_end_by_the_text_after_the_last_dot() {
    let placed = placed_site();
    let planned = run(&[("site.v2.hcl", &placed)], &BTreeMap::new()).expect("a plan");
    assert_eq!(planned.counts.added, 3);
}

#[test]
fn gives_two_paths_that_differ_two_texts() {
    let problems = problems(&[("a\\n.yaml", ""), ("a\n.yaml", "")]);
    assert_eq!(
        problems.text(),
        "error[ops.unknown-extension]: no config syntax reads this file\n  \
         --> a\\\\n.yaml:1:1\n\
         fix: Use a file that ends in `.hcl`\n\
         \n\
         error[ops.unknown-extension]: no config syntax reads this file\n  \
         --> a\\n.yaml:1:1\n\
         fix: Use a file that ends in `.hcl`\n"
    );
}

fn bytes(paths: &[&[u8]], text: &str) -> Vec<File> {
    paths
        .iter()
        .map(|path| File {
            path: PathBuf::from(OsStr::from_bytes(path)),
            text: text.to_owned(),
        })
        .collect()
}

#[test]
fn refuses_a_path_that_is_not_utf8_before_its_extension() {
    let error = plan(
        &bytes(&[b"a\xff.hcl", b"a\xfe.hcl", b"a\xff.yaml"], &placed_site()),
        empty(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        &front_ends(),
        &Table::new(),
    )
    .expect_err("problems");
    assert_eq!(
        error.text(),
        "\
error[ops.path-not-utf8]: the path \"a\\xFF.hcl\" is not UTF-8
fix: Rename the file to a UTF-8 name

error[ops.path-not-utf8]: the path \"a\\xFE.hcl\" is not UTF-8
fix: Rename the file to a UTF-8 name

error[ops.path-not-utf8]: the path \"a\\xFF.yaml\" is not UTF-8
fix: Rename the file to a UTF-8 name
"
    );
}

#[test]
fn gives_a_path_that_is_not_utf8_in_file_order_with_the_other_problems() {
    let mut files = files(&[("a.hcl", "channel {\n"), ("a.txt", "")]);
    files.insert(1, bytes(&[b"a\xff.hcl"], "").remove(0));
    let error = plan(
        &files,
        empty(),
        &BTreeMap::new(),
        &BTreeSet::new(),
        &front_ends(),
        &Table::new(),
    )
    .expect_err("problems");
    assert_eq!(
        error.text(),
        "\
error[hcl.syntax]: the file needs a key, a block, or the end of the body here
  --> a.hcl:2:1
fix: Write it here, or correct the text here or before it

error[ops.path-not-utf8]: the path \"a\\xFF.hcl\" is not UTF-8
fix: Rename the file to a UTF-8 name

error[ops.unknown-extension]: no config syntax reads this file
  --> a.txt:1:1
fix: Use a file that ends in `.hcl`
"
    );
}

/// Lines that `ssh-keygen -t ed25519` wrote, and the fingerprint that
/// `ssh-keygen -lf` gives for each.
const ALICE: &str = concat!(
    "ssh-ed25519 ",
    "AAAAC3NzaC1lZDI1NTE5AAAAIGVVuOR8JKYpAcWLMUveadmJ1wUAmYGgIDtqlhFe7Yhg",
    " alice@laptop",
);
const ALICE_FINGERPRINT: &str = "SHA256:AaHjcjahcS7PIOJwyahzFqtJH7PJ8NKy89OZdEKcurc";
const BOB: &str = concat!(
    "ssh-ed25519 ",
    "AAAAC3NzaC1lZDI1NTE5AAAAIP0QMDFGOHfS9XR71aVyCvs+QnNQ4BXrHs9dGDDz7KY6",
    " bob@site",
);
const BOB_FINGERPRINT: &str = "SHA256:yrQ4K597Aogzr4Zp1m1So77Lh8tM3HApOLoy0kzzo3k";
/// The 32 bytes of the key of `BOB`.
const BOB_KEY: [u8; 32] = [
    253, 16, 48, 49, 70, 56, 119, 210, 245, 116, 123, 213, 165, 114, 10, 251, 62, 66,
    115, 80, 224, 21, 235, 30, 207, 93, 24, 48, 243, 236, 166, 58,
];

/// A subject labeled `carol` with Bob's key, then Alice's.
fn carol() -> String {
    format!("subject \"carol\" {{\n  keys = [\"{BOB}\", \"{ALICE}\"]\n}}\n")
}

/// The applied subject labeled `alice`, with the key `bytes`.
fn applied_subject(bytes: [u8; 32]) -> BTreeMap<Name, Definition> {
    let key = types::ed25519::PublicKey::new(bytes).expect("a key");
    let subject = Subject::new(vec![key]).expect("a subject");
    BTreeMap::from([(name("alice.@subject"), Definition::Subject(subject))])
}

#[test]
fn shows_the_fingerprint_of_each_key_of_a_subject_by_its_bytes() {
    let planned =
        run(&[("people.hcl", &carol())], &applied_subject(BOB_KEY)).expect("a plan");
    assert_eq!(
        planned.text(),
        format!(
            "\
+ subject carol
    key {ALICE_FINGERPRINT}
    key {BOB_FINGERPRINT}
- subject alice
    key {BOB_FINGERPRINT}
1 to add, 0 to change, 1 to remove.
"
        )
    );
}

#[test]
fn gives_the_fingerprints_of_a_subject_after_the_apply_or_before_a_removal() {
    let planned =
        run(&[("people.hcl", &carol())], &applied_subject(BOB_KEY)).expect("a plan");
    assert_eq!(
        json(&planned)["changes"],
        serde_json::json!([
            {
                "action": "add",
                "kind": "subject",
                "name": "carol",
                "place": { "file": "people.hcl", "line": 1, "column": 9 },
                "fingerprints": [ALICE_FINGERPRINT, BOB_FINGERPRINT],
            },
            {
                "action": "remove",
                "kind": "subject",
                "name": "alice",
                "fingerprints": [BOB_FINGERPRINT],
            },
        ])
    );
}

#[test]
fn gives_the_fingerprints_of_a_change_after_the_apply() {
    let alice = format!("subject \"alice\" {{\n  keys = [\"{ALICE}\"]\n}}\n");
    let planned =
        run(&[("people.hcl", &alice)], &applied_subject(BOB_KEY)).expect("a plan");
    assert_eq!(
        planned.text(),
        format!(
            "\
~ subject alice
    key {ALICE_FINGERPRINT}
0 to add, 1 to change, 0 to remove.
"
        )
    );
}

#[test]
fn gives_no_fingerprints_for_another_kind() {
    let planned =
        run(&[("site.hcl", &placed_site())], &BTreeMap::new()).expect("a plan");
    let json = json(&planned);
    let changes = json["changes"].as_array().expect("changes");
    assert!(
        changes
            .iter()
            .all(|change| change.get("fingerprints").is_none())
    );
}

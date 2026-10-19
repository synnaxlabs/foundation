//! Adds the blocks of an import to HCL files through `config::import::add`, with a
//! kind of this file.

use config::import::{Added, Outcome, Pick, add};
use connector::browse::{Facts, Path};
use connector::import::fill;
use connector::kind::{Channels, Context, Error, Kind, Table};
use document::diagnostic::{Code, Diagnostic};
use document::value::{self, Value};
use document::{Attribute, Block, Document, Label, Map, Source};
use proptest::prelude::*;
use types::name::Name;
use types::sample::{Scalar, Type};

/// A kind whose block of a point is `read "<channel>" { tag = "<tag>" }`, with `scale =
/// fill("the scale")` for a tag that starts with `scaled`, and `scale = fill()` for a
/// tag that starts with `unfilled`. It refuses a tag that starts with `bad`. The tag of
/// a `read` block is its `tag` without a leading `ns=0;`, so two texts can name one
/// point. It writes the channel of each `read` block, and its check refuses a channel
/// whose last segment is `refused`.
struct Points;

impl Kind for Points {
    type Config = Vec<Name>;
    const SCHEMA: document::schema::Body = connector::testing::READS;

    fn parse(&self, config: &Document) -> Result<Vec<Name>, Vec<Diagnostic>> {
        let reads = config
            .blocks
            .iter()
            .filter(|block| &*block.keyword == "read");
        Ok(reads
            .map(|block| block.labels[0].text.parse().expect("a channel name"))
            .collect())
    }

    fn check(&self, reads: &Vec<Name>) -> Result<Channels, Vec<Diagnostic>> {
        if let Some(read) = reads
            .iter()
            .find(|read| read.as_str().ends_with(".refused"))
        {
            return Err(vec![Diagnostic::new(
                Code::new("points.refused"),
                None,
                format!("the check refuses {read}"),
                "Read another".into(),
            )]);
        }
        Ok(Channels {
            writes: reads.clone(),
            ..Channels::default()
        })
    }

    fn run(&self, _: Context<Vec<Name>>) -> impl Future<Output = Result<(), Error>> {
        std::future::ready(Ok(()))
    }

    fn block(
        &self,
        tag: &str,
        _: &Facts,
        channel: &Name,
    ) -> Result<Vec<Block>, Vec<Diagnostic>> {
        if tag.starts_with("bad") {
            return Err(vec![Diagnostic::new(
                Code::new("points.bad"),
                None,
                format!("no block for {channel}"),
                "Pick another".into(),
            )]);
        }
        let mut attributes = vec![attribute("tag", string(tag))];
        if tag.starts_with("scaled") {
            attributes.push(attribute("scale", fill("the scale")));
        }
        if tag.starts_with("unfilled") {
            let call = value::Call {
                function: "fill".into(),
                function_span: None,
                arguments: Vec::new(),
            };
            let value = Value {
                kind: value::Kind::Call(call),
                span: None,
            };
            attributes.push(attribute("scale", value));
        }
        Ok(vec![read_block(channel, attributes)])
    }

    fn tag(&self, block: &Block) -> Option<String> {
        if &*block.keyword != "read" {
            return None;
        }
        match &block.body.attributes.get("tag")?.value.kind {
            value::Kind::String(tag) => {
                Some(tag.strip_prefix("ns=0;").unwrap_or(tag).to_owned())
            }
            _ => None,
        }
    }
}

/// A kind with the count `in` and no blocks.
struct Counting;

impl Kind for Counting {
    type Config = ();
    const SCHEMA: document::schema::Body = connector::testing::READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            counts: vec![name("in")],
            ..Channels::default()
        })
    }

    fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
        std::future::ready(Ok(()))
    }
}

fn read_block(channel: &Name, attributes: Vec<Attribute>) -> Block {
    Block {
        keyword: "read".into(),
        keyword_span: None,
        labels: vec![Label {
            text: channel.as_str().into(),
            span: None,
        }],
        body: Document {
            attributes: Map::new(attributes).expect("unique keys"),
            blocks: Vec::new(),
        },
        span: None,
    }
}

fn attribute(key: &str, value: Value) -> Attribute {
    Attribute {
        key: key.into(),
        key_span: None,
        value,
    }
}

fn string(text: &str) -> Value {
    Value {
        kind: value::Kind::String(text.into()),
        span: None,
    }
}

fn kinds() -> Table {
    Table::new()
        .with("points", Points)
        .with("counting", Counting)
}

fn read(source: u32, text: &str) -> Document {
    config_hcl::read(Source(source), text).expect("the text is HCL")
}

fn name(text: &str) -> Name {
    text.parse().expect("a name")
}

fn path(text: &str) -> Path {
    text.parse().expect("a path")
}

fn float() -> Facts {
    Facts {
        data_type: Some(Type::Scalar(Scalar::F64)),
        ..Facts::default()
    }
}

const PLANT: &str = r#"# The plant.
connector "plc" {
  kind = "points"
  node = "edge"
}
"#;

/// A pick of `tag` at `path` with the facts of `float`.
struct Want<'a> {
    path: Path,
    tag: &'a str,
    facts: Facts,
    name: Option<Name>,
}

fn want<'a>(at: &str, tag: &'a str) -> Want<'a> {
    Want {
        path: path(at),
        tag,
        facts: float(),
        name: None,
    }
}

fn named<'a>(at: &str, tag: &'a str, given: &str) -> Want<'a> {
    Want {
        name: Some(name(given)),
        ..want(at, tag)
    }
}

/// What `add` gives for the connector `plc` of `texts`, from the root.
fn added(texts: &[&str], wants: &[Want<'_>]) -> Result<Added, Vec<Diagnostic>> {
    added_from(texts, &Path::root(), wants)
}

/// What `add` gives for the connector `plc` of `texts`, from `start`.
fn added_from(
    texts: &[&str],
    start: &Path,
    wants: &[Want<'_>],
) -> Result<Added, Vec<Diagnostic>> {
    let documents: Vec<Document> = (0..)
        .zip(texts)
        .map(|(source, text)| read(source, text))
        .collect();
    let picks: Vec<Pick<'_>> = wants
        .iter()
        .map(|want| Pick {
            path: &want.path,
            tag: want.tag,
            facts: &want.facts,
            name: want.name.as_ref(),
        })
        .collect();
    add(&documents, &name("plc"), start, &picks, &kinds())
}

/// The text of `text`, the file of source 0, with the blocks of `added` written in.
fn updated(text: &str, added: &Added) -> String {
    config_hcl::update(Source(0), text, &added.document).expect("the update writes")
}

fn outcome(text: &str) -> Outcome {
    Outcome::Added {
        name: name(text),
        fills: Vec::new(),
    }
}

/// Each problem as its code, message, and fix.
fn problems(diagnostics: Vec<Diagnostic>) -> Vec<(&'static str, String, String)> {
    diagnostics
        .into_iter()
        .map(|problem| (problem.code.as_str(), problem.message, problem.fix))
        .collect()
}

fn problem(
    code: &'static str,
    message: &str,
    fix: &str,
) -> (&'static str, String, String) {
    (code, message.into(), fix.into())
}

#[test]
fn writes_the_reads_into_the_connector_and_the_channels_with_a_new_index_at_the_end() {
    let wants = [want("Line1/Temp", "t1"), want("Line1/Mode", "t2")];
    let added = added(&[PLANT], &wants).expect("it adds");
    assert_eq!(added.source, Source(0));
    assert_eq!(added.index, Some(name("plc.time")));
    assert_eq!(
        added.picks,
        vec![outcome("plc.Line1.Temp"), outcome("plc.Line1.Mode")]
    );
    assert_eq!(
        updated(PLANT, &added),
        r#"# The plant.
connector "plc" {
  kind = "points"
  node = "edge"

  read "plc.Line1.Temp" {
    tag = "t1"
  }

  read "plc.Line1.Mode" {
    tag = "t2"
  }
}

channel "plc.time" {
  kind = "index"
}

channel "plc.Line1.Temp" {
  data_type = "f64"
  index = "plc.time"
}

channel "plc.Line1.Mode" {
  data_type = "f64"
  index = "plc.time"
}
"#
    );
}

const READING: &str = r#"connector "plc" {
  kind = "points"
  node = "edge"

  # The oven.
  read "plc.oven" {
    tag = "ns=0;i=2258"
  }
}

channel "plc.clock" {
  kind = "index"
}

channel "plc.oven" {
  data_type = "f64"
  index     = "plc.clock"
}
"#;

#[test]
fn takes_the_index_of_the_channels_that_the_connector_reads_and_keeps_each_byte() {
    let added = added(&[READING], &[want("Line1/Temp", "t1")]).expect("it adds");
    assert_eq!(added.index, None);
    assert_eq!(added.picks, vec![outcome("plc.Line1.Temp")]);
    assert_eq!(
        updated(READING, &added),
        r#"connector "plc" {
  kind = "points"
  node = "edge"

  # The oven.
  read "plc.oven" {
    tag = "ns=0;i=2258"
  }

  read "plc.Line1.Temp" {
    tag = "t1"
  }
}

channel "plc.clock" {
  kind = "index"
}

channel "plc.oven" {
  data_type = "f64"
  index     = "plc.clock"
}

channel "plc.Line1.Temp" {
  data_type = "f64"
  index = "plc.clock"
}
"#
    );
}

#[test]
fn leaves_out_a_point_that_the_kind_reads_already_in_another_text() {
    let wants = [
        named("oven", "i=2258", "plc.oven"),
        want("Line1/Temp", "t1"),
    ];
    let added = added(&[READING], &wants).expect("it adds");
    assert_eq!(
        added.picks,
        vec![Outcome::Present, outcome("plc.Line1.Temp")]
    );
}

#[test]
fn leaves_out_a_point_that_an_earlier_pick_has_and_adds_no_index_for_none() {
    let wants = [want("A/x", "t1"), want("B/x", "t1")];
    let first = added(&[PLANT], &wants).expect("it adds");
    assert_eq!(first.picks, vec![outcome("plc.A.x"), Outcome::Present]);
    let wants = [want("A/x", "i=2258")];
    let none = added(&[READING], &wants).expect("it adds nothing");
    assert_eq!(none.picks, vec![Outcome::Present]);
    assert_eq!(none.index, None);
    let wants = [want(&vec!["a"; 130].join("/"), "t9")];
    let unnamed = added(&[PLANT], &wants).expect("it adds nothing");
    assert_eq!(unnamed.picks, vec![Outcome::Unnamed]);
    assert_eq!(unnamed.index, None);
    let text = updated(PLANT, &unnamed);
    assert_eq!(text, PLANT);
}

#[test]
fn adds_a_point_whose_earlier_pick_has_no_name() {
    let wants = [want(&vec!["a"; 130].join("/"), "t8"), want("B/x", "t8")];
    let added = added(&[PLANT], &wants).expect("it adds");
    assert_eq!(added.picks, vec![Outcome::Unnamed, outcome("plc.B.x")]);
    let text = updated(PLANT, &added);
    assert_eq!(text.matches(r#"tag = "t8""#).count(), 1);
}

#[test]
#[should_panic(
    expected = "invariant: a kind writes each call `fill` as `fill` does, and the kind \
                \"points\" does not for the tag \"unfilled\""
)]
fn panics_on_a_call_fill_of_a_kind_with_no_text() {
    drop(added(&[PLANT], &[want("A/x", "unfilled")]));
}

#[test]
fn names_each_given_name_then_the_index_then_the_defaults() {
    let wants = [
        want("time", "t1"),
        named("x", "t2", "plc.time"),
        want("Line1/Temp", "t3"),
    ];
    let added = added(&[PLANT], &wants).expect("it adds");
    assert_eq!(added.index, Some(name("plc.time_2")));
    assert_eq!(
        added.picks,
        vec![
            outcome("plc.time_3"),
            outcome("plc.time"),
            outcome("plc.Line1.Temp")
        ]
    );
}

#[test]
fn gives_no_default_a_name_that_the_files_define_or_imply() {
    let wants = [want("oven", "t1"), want("status/time", "t2")];
    let added = added(&[READING], &wants).expect("it adds");
    assert_eq!(
        added.picks,
        vec![outcome("plc.oven_2"), outcome("plc.status.time_2")]
    );
}

#[test]
fn refuses_each_bad_given_name_with_its_path_and_its_default() {
    let wants = [
        named("A/x", "t1", "plc.@x"),
        named("A/y", "t2", "PLC.Oven"),
        named("A/z", "t3", "plc.status.time"),
        named("B/u", "t4", "kept"),
        named("B/v", "t5", "KEPT"),
        named("oven", "i=2258", "plc.oven"),
    ];
    let problems = problems(added(&[READING], &wants).expect_err("bad names"));
    let fix =
        |default: &str| format!("Give another name, or leave it out for {default:?}");
    assert_eq!(
        problems,
        vec![
            problem(
                "config.reserved-name",
                "the name \"plc.@x\" of the point at \"A/x\" has a segment that starts \
                 with `@`, which is reserved",
                &fix("plc.A.x"),
            ),
            problem(
                "config.duplicate-name",
                "the name \"PLC.Oven\" of the point at \"A/y\" is in use in the files",
                &fix("plc.A.y"),
            ),
            problem(
                "config.duplicate-name",
                "the name \"plc.status.time\" of the point at \"A/z\" is in use in the \
                 files",
                &fix("plc.A.z"),
            ),
            problem(
                "config.duplicate-name",
                "the points at \"B/u\" and \"B/v\" have the name \"KEPT\"",
                &fix("plc.B.v"),
            ),
        ]
    );
}

#[test]
fn refuses_a_given_name_that_another_connector_implies() {
    let other = "connector \"mqtt\" {\n  kind = \"points\"\n  node = \"edge\"\n}\n";
    let wants = [named("x", "t1", "mqtt.status.time")];
    let problems = problems(added(&[PLANT, other], &wants).expect_err("in use"));
    assert_eq!(
        problems,
        vec![problem(
            "config.duplicate-name",
            "the name \"mqtt.status.time\" of the point at \"x\" is in use in the \
             files",
            "Give another name, or leave it out for \"plc.x\"",
        )]
    );
}

#[test]
fn refuses_a_given_name_that_another_connector_counts_or_has() {
    let other = "connector \"mqtt\" {\n  kind = \"counting\"\n  node = \"edge\"\n}\n";
    let wants = [named("x", "t1", "mqtt.status.in"), named("y", "t2", "mqtt")];
    let problems = problems(added(&[PLANT, other], &wants).expect_err("in use"));
    assert_eq!(
        problems,
        vec![
            problem(
                "config.duplicate-name",
                "the name \"mqtt.status.in\" of the point at \"x\" is in use in the \
                 files",
                "Give another name, or leave it out for \"plc.x\"",
            ),
            problem(
                "config.duplicate-name",
                "the name \"mqtt\" of the point at \"y\" is in use in the files",
                "Give another name, or leave it out for \"plc.y\"",
            ),
        ]
    );
}

#[test]
fn gives_no_default_a_count_of_another_connector() {
    let other =
        "connector \"plc.Line1\" {\n  kind = \"counting\"\n  node = \"edge\"\n}\n";
    let added = added(&[PLANT, other], &[want("Line1/status/in", "t1")]);
    let added = added.expect("it adds");
    assert_eq!(added.picks, vec![outcome("plc.Line1.status.in_2")]);
}

#[test]
fn refuses_a_given_name_with_the_default_on_the_index_of_the_connector() {
    let wants = [named("time", "t1", "plc.oven")];
    let problems = problems(added(&[READING], &wants).expect_err("in use"));
    assert_eq!(
        problems,
        vec![problem(
            "config.duplicate-name",
            "the name \"plc.oven\" of the point at \"time\" is in use in the files",
            "Give another name, or leave it out for \"plc.time\"",
        )]
    );
}

#[test]
fn gives_no_pick_a_channel_that_the_connector_writes_with_no_channel_block() {
    let text = "connector \"plc\" {\n  kind = \"points\"\n  node = \"edge\"\n\n  \
                read \"plc.oven\" {\n    tag = \"t0\"\n  }\n}\n";
    config::check(&[read(0, text)], &kinds()).expect("it checks");
    let wants = [want("oven", "t1"), named("x", "t2", "PLC.OVEN")];
    let problems = problems(added(&[text], &wants).expect_err("in use"));
    assert_eq!(
        problems,
        vec![problem(
            "config.duplicate-name",
            "the name \"PLC.OVEN\" of the point at \"x\" is in use in the files",
            "Give another name, or leave it out for \"plc.x\"",
        )]
    );
    let added = added(&[text], &[want("oven", "t1")]).expect("it adds");
    assert_eq!(added.picks, vec![outcome("plc.oven_2")]);
}

#[test]
fn moves_the_new_index_off_a_given_name_in_another_case() {
    let wants = [named("x", "t1", "PLC.TIME"), want("y", "t2")];
    let added = added(&[PLANT], &wants).expect("it adds");
    assert_eq!(added.index, Some(name("plc.time_2")));
    assert_eq!(added.picks, vec![outcome("PLC.TIME"), outcome("plc.y")]);
}

#[test]
fn takes_the_index_of_the_first_channel_that_the_connector_writes() {
    let text = r#"connector "plc" {
  kind = "points"
  node = "edge"

  read "plc.a" {
    tag = "ta"
  }

  read "plc.b" {
    tag = "tb"
  }
}

channel "plc.b" {
  data_type = "f64"
  index     = "plc.second"
}

channel "plc.a" {
  data_type = "f64"
  index     = "plc.first"
}
"#;
    let added = added(&[text], &[want("c", "tc")]).expect("it adds");
    assert_eq!(added.index, None);
    let last = added.document.document().blocks.last().expect("a channel");
    let index = &last.body.attributes.get("index").expect("an index").value;
    assert_eq!(index.kind, value::Kind::String("plc.first".into()));
}

#[test]
fn gives_the_problem_of_an_index_that_is_not_a_name() {
    let text = READING.replace("index     = \"plc.clock\"", "index     = 5");
    let problems = problems(added(&[&text], &[want("A/x", "t1")]).expect_err("bad"));
    assert_eq!(
        problems,
        vec![problem(
            "document.bad-name",
            "a name is a string or a reference, not an integer",
            "Write a name such as \"site_a.node_1\"",
        )]
    );
}

#[test]
fn gives_the_problems_of_the_check_of_the_kind() {
    let text = "connector \"plc\" {\n  kind = \"points\"\n  node = \"edge\"\n\n  \
                read \"plc.refused\" {\n    tag = \"t0\"\n  }\n}\n";
    let problems = problems(added(&[text], &[want("A/x", "t1")]).expect_err("refused"));
    assert_eq!(
        problems,
        vec![problem(
            "points.refused",
            "the check refuses plc.refused",
            "Read another",
        )]
    );
}

#[test]
fn adds_beside_a_block_of_the_body_that_has_no_tag() {
    let text = "connector \"plc\" {\n  kind = \"points\"\n  node = \"edge\"\n\n  \
                poll {\n    every = \"1s\"\n  }\n}\n";
    let added = added(&[text], &[want("A/x", "t1")]).expect("it adds");
    assert_eq!(added.picks, vec![outcome("plc.A.x")]);
}

#[test]
fn refuses_a_bad_given_name_with_no_default_with_a_fix_to_give_another() {
    let long = vec!["a"; 130].join("/");
    let wants = [named(&long, "t1", "plc.@x")];
    let problems = problems(added(&[PLANT], &wants).expect_err("a bad name"));
    let message = format!(
        "the name \"plc.@x\" of the point at {:?} has a segment that starts with `@`, \
         which is reserved",
        path(&long).to_string()
    );
    assert_eq!(
        problems,
        vec![problem(
            "config.reserved-name",
            &message,
            "Give another name"
        )]
    );
}

#[test]
fn adds_a_point_after_two_picks_of_its_tag_with_no_name() {
    let wants = [
        want(&vec!["a"; 130].join("/"), "t8"),
        want(&vec!["b"; 130].join("/"), "t8"),
        want("x", "t8"),
    ];
    let added = added(&[PLANT], &wants).expect("it adds");
    assert_eq!(
        added.picks,
        vec![Outcome::Unnamed, Outcome::Unnamed, outcome("plc.x")]
    );
}

#[test]
fn refuses_a_status_name_of_another_connector_whose_check_fails() {
    let other = "connector \"mqtt\" {\n  kind = \"points\"\n  node = \"edge\"\n\n  \
                 read \"mqtt.refused\" {\n    tag = \"t0\"\n  }\n}\n";
    let wants = [named("x", "t1", "mqtt.status.time")];
    let problems = problems(added(&[PLANT, other], &wants).expect_err("in use"));
    assert_eq!(
        problems,
        vec![problem(
            "config.duplicate-name",
            "the name \"mqtt.status.time\" of the point at \"x\" is in use in the files",
            "Give another name, or leave it out for \"plc.x\"",
        )]
    );
}

#[test]
fn gives_the_fix_the_default_of_the_run_without_the_refused_names() {
    let long = vec!["a"; 130].join("/");
    let left = [want(&long, "t1"), want("x y", "t1"), want("x_y", "t2")];
    let rerun = added(&[PLANT], &left).expect("it adds");
    assert_eq!(
        rerun.picks,
        vec![Outcome::Unnamed, outcome("plc.x_y"), outcome("plc.x_y_2")]
    );
    let wants = [
        named(&long, "t1", "plc"),
        want("x y", "t1"),
        named("x_y", "t2", "plc"),
    ];
    let problems = problems(added(&[PLANT], &wants).expect_err("in use"));
    let message = format!(
        "the name \"plc\" of the point at {:?} is in use in the files",
        path(&long).to_string()
    );
    assert_eq!(
        problems,
        vec![
            problem("config.duplicate-name", &message, "Give another name"),
            problem(
                "config.duplicate-name",
                "the name \"plc\" of the point at \"x_y\" is in use in the files",
                "Give another name, or leave it out for \"plc.x_y_2\"",
            ),
        ]
    );
}

#[test]
fn gives_no_default_when_the_run_without_the_refused_names_refuses_a_name() {
    // `plc.` and 250 bytes: `<default>_2` is too long. So the default of the first pick
    // leaves the pick of "t2" `Unnamed`, which opens the pick at "B".
    let space = format!("{} ", "a".repeat(249));
    let under = format!("{}_", "a".repeat(249));
    let left = [
        want(&space, "t1"),
        want(&under, "t2"),
        named("A", "t3", "n"),
        named("B", "t2", "n"),
    ];
    let rerun = problems(added(&[PLANT], &left).expect_err("a name of two picks"));
    assert_eq!(
        rerun,
        vec![problem(
            "config.duplicate-name",
            "the points at \"A\" and \"B\" have the name \"n\"",
            "Give another name, or leave it out for \"plc.B\"",
        )]
    );
    let wants = [
        named(&space, "t1", "plc"),
        want(&under, "t2"),
        named("A", "t3", "n"),
        named("B", "t2", "n"),
    ];
    let problems = problems(added(&[PLANT], &wants).expect_err("in use"));
    let message = format!(
        "the name \"plc\" of the point at {:?} is in use in the files",
        path(&space).to_string()
    );
    assert_eq!(
        problems,
        vec![problem(
            "config.duplicate-name",
            &message,
            "Give another name"
        )]
    );
}

#[test]
fn gives_no_default_when_leaving_a_name_out_opens_a_pick_outside_the_start() {
    // The pick at "Other/x" is present by the pick of "t2", and only a pick that is not
    // present may not be under the start without a name.
    let space = format!("S/{} ", "a".repeat(249));
    let under = format!("S/{}_", "a".repeat(249));
    let wants = [
        named(&space, "t1", "plc"),
        want(&under, "t2"),
        want("Other/x", "t2"),
    ];
    let problems =
        problems(added_from(&[PLANT], &path("S"), &wants).expect_err("in use"));
    let message = format!(
        "the name \"plc\" of the point at {:?} is in use in the files",
        path(&space).to_string()
    );
    assert_eq!(
        problems,
        vec![problem(
            "config.duplicate-name",
            &message,
            "Give another name"
        )]
    );
}

#[test]
fn gives_no_default_for_a_refused_name_outside_the_start() {
    let wants = [named("Other/x", "t1", "plc")];
    let problems =
        problems(added_from(&[PLANT], &path("S"), &wants).expect_err("in use"));
    assert_eq!(
        problems,
        vec![problem(
            "config.duplicate-name",
            "the name \"plc\" of the point at \"Other/x\" is in use in the files",
            "Give another name"
        )]
    );
    let at_start = [named("S", "t1", "plc")];
    let problems = self::problems(
        added_from(&[PLANT], &path("S"), &at_start).expect_err("in use"),
    );
    assert_eq!(
        problems,
        vec![problem(
            "config.duplicate-name",
            "the name \"plc\" of the point at \"S\" is in use in the files",
            "Give another name"
        )]
    );
}

#[test]
fn gives_no_default_a_channel_that_another_connector_writes() {
    let other = "connector \"line\" {\n  kind = \"points\"\n  node = \"cloud\"\n\n  \
                 read \"plc.A.x\" {\n    tag = \"t0\"\n  }\n}\n";
    let members = std::collections::BTreeSet::from([name("edge"), name("cloud")]);
    let pointer = spec::Pointer {
        version: 0,
        root: types::digest::Digest::of(b""),
    };
    let applied = std::collections::BTreeMap::new();
    let plan = |texts: &[&str]| {
        let documents: Vec<Document> = (0..)
            .zip(texts)
            .map(|(source, text)| read(source, text))
            .collect();
        config::plan::plan(&documents, pointer, &applied, &members, &kinds())
            .map(|_| ())
            .map_err(problems)
    };
    assert_eq!(plan(&[PLANT, other]), Ok(()));
    let added = added(&[PLANT, other], &[want("A/x", "t1")]).expect("it adds");
    let text = updated(PLANT, &added);
    assert_eq!(plan(&[&text, other]), Ok(()));
    assert_eq!(added.picks, vec![outcome("plc.A.x_2")]);
}

#[test]
fn adds_no_index_that_another_connector_writes() {
    let other = "connector \"line\" {\n  kind = \"points\"\n  node = \"edge\"\n\n  \
                 read \"plc.time\" {\n    tag = \"t0\"\n  }\n}\n";
    config::check(&[read(0, PLANT), read(1, other)], &kinds()).expect("it checks");
    let added = added(&[PLANT, other], &[want("A/x", "t1")]).expect("it adds");
    assert_eq!(added.index, Some(name("plc.time_2")));
}

#[test]
fn gives_each_fill_of_the_blocks_of_a_pick() {
    let wants = [
        Want {
            facts: Facts::default(),
            ..want("A/x", "scaled1")
        },
        want("A/y", "t2"),
    ];
    let added = added(&[PLANT], &wants).expect("it adds");
    let fills = vec!["the scale".to_owned(), "the sample type".to_owned()];
    assert_eq!(
        added.picks,
        vec![
            Outcome::Added {
                name: name("plc.A.x"),
                fills
            },
            outcome("plc.A.y")
        ]
    );
}

#[test]
fn adds_again_after_an_import_that_wrote_a_fill_outside_the_connector() {
    let wants = [Want {
        facts: Facts::default(),
        ..want("A/x", "t1")
    }];
    let first = added(&[PLANT], &wants).expect("it adds");
    let text = updated(PLANT, &first);
    let second = added(&[&text], &[want("A/x", "t1"), want("A/y", "t2")]);
    let second = second.expect("it adds again");
    assert_eq!(second.index, None);
    assert_eq!(second.picks, vec![Outcome::Present, outcome("plc.A.y")]);
}

#[test]
fn gives_each_problem_of_the_blocks_of_the_kind() {
    let wants = [want("A/x", "bad1"), want("A/y", "t2"), want("A/z", "bad2")];
    let problems = problems(added(&[PLANT], &wants).expect_err("no blocks"));
    let refused = |name: &str| {
        problem(
            "points.bad",
            &format!("no block for {name}"),
            "Pick another",
        )
    };
    assert_eq!(problems, vec![refused("plc.A.x"), refused("plc.A.z")]);
}

#[test]
fn writes_into_the_file_of_the_connector_block() {
    let channels = "channel \"plc.Temp\" {\n  kind = \"index\"\n}\n";
    let added = added(&[channels, PLANT], &[want("Temp", "t1")]).expect("it adds");
    assert_eq!(added.source, Source(1));
    assert_eq!(added.picks, vec![outcome("plc.Temp_2")]);
    let blocks = &added.document.document().blocks;
    let keywords: Vec<&str> = blocks.iter().map(|block| &*block.keyword).collect();
    assert_eq!(keywords, ["connector", "channel", "channel"]);
}

#[test]
fn gives_the_problems_of_the_connector_block() {
    let problems = problems(added(&["channel \"x\" {}\n"], &[]).expect_err("none"));
    assert_eq!(
        problems,
        vec![problem(
            "config.no-connector",
            "no `connector` block of the files has the name \"plc\"",
            "Add a `connector` block with the name \"plc\"",
        )]
    );
}

#[test]
fn gives_the_problem_of_a_connector_of_no_kind() {
    let text = "connector \"plc\" {\n  kind = \"nope\"\n  node = \"edge\"\n}\n";
    let problems = problems(added(&[text], &[want("A/x", "t1")]).expect_err("no kind"));
    assert_eq!(
        problems,
        vec![problem(
            "connector.unknown-kind",
            "this build has no connector kind \"nope\"",
            "Use one of [\"counting\", \"points\"]",
        )]
    );
}

proptest! {
    /// Each import of distinct points under one folder checks, and a second import of
    /// them adds nothing.
    #[test]
    fn adds_points_that_check_and_adds_them_once(
        names in proptest::collection::btree_set("[A-Za-z][A-Za-z0-9 ]{0,8}", 1..8),
    ) {
        let tags: Vec<String> = (0..names.len()).map(|at| format!("t{at}")).collect();
        let wants: Vec<Want<'_>> = names
            .iter()
            .zip(&tags)
            .map(|(point, tag)| want(&format!("Line1/{point}"), tag))
            .collect();
        let first = added(&[PLANT], &wants).expect("it adds");
        let text = updated(PLANT, &first);
        let checked = config::check(&[read(0, &text)], &kinds()).expect("it checks");
        for outcome in &first.picks {
            let Outcome::Added { name, fills } = outcome else {
                panic!("each pick is added: {outcome:?}");
            };
            prop_assert!(fills.is_empty());
            prop_assert!(checked.contains_key(name));
        }
        let second = added(&[&text], &wants).expect("it adds nothing");
        prop_assert!(second.picks.iter().all(|outcome| *outcome == Outcome::Present));
        prop_assert_eq!(updated(&text, &second), text);
    }
}

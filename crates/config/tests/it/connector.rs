//! The `connector` block, read from HCL and checked by the kinds of a table.

use config::{Definition, Entry};
use connector::kind::{Kind as _, Table};
use document::diagnostic::{Code, Diagnostic};
use document::encoding::DEPTH_MAX;
use document::value::{self, Value};
use document::{Attribute, Document, Map, Source};
use spec::connector::Connector;
use types::name::Name;

const FIXTURE: &str = include_str!("../../../acceptance/tests/it/fixtures/influx.hcl");

fn read(text: &str) -> Document {
    config_hcl::read(Source(0), text).expect("the text is HCL")
}

fn kinds() -> Table {
    Table::new().with("influx", connector_influx::Kind::default())
}

fn name(text: &str) -> Name {
    text.parse().expect("a name")
}

/// Each problem of `text` as its code, the offset where it starts, its message, and
/// its fix.
fn problems(text: &str) -> Vec<(&'static str, Option<u32>, String, String)> {
    let diagnostics = config::check(&[read(text)], &kinds()).expect_err("problems");
    diagnostics
        .into_iter()
        .map(|diagnostic| {
            (
                diagnostic.code.as_str(),
                diagnostic.span.map(|span| span.start().offset),
                diagnostic.message,
                diagnostic.fix,
            )
        })
        .collect()
}

/// A problem at the first `at` in `text`.
fn problem(
    text: &str,
    code: &'static str,
    at: &str,
    message: &str,
    fix: &str,
) -> (&'static str, Option<u32>, String, String) {
    let offset = text.find(at).expect("`at` is in the text");
    (
        code,
        Some(u32::try_from(offset).expect("a short text")),
        message.into(),
        fix.into(),
    )
}

fn connector(entry: &Entry) -> &Connector {
    match &entry.definition {
        Definition::Spec(spec::definition::Definition::Connector(connector)) => {
            connector
        }
        definition => panic!("not a connector: {definition:?}"),
    }
}

#[test]
fn checks_the_influx_fixture_into_one_connector() {
    let entries = config::check(&[read(FIXTURE)], &kinds()).expect("no problems");
    assert_eq!(entries.keys().collect::<Vec<_>>(), [&name("influx")]);
    let connector = connector(&entries[&name("influx")]);
    assert_eq!(connector.kind(), &name("influx"));
    assert_eq!(connector.node(), &name("cloud"));
    let config = connector.config().document();
    let expected = read(
        "address = \"http://influx:8086\"\nselect = \"edge.*\"\n\
         reader {\n  name = \"influx\"\n  mode = \"complete\"\n  hold = \"2h\"\n}\n",
    );
    assert_eq!(config, &expected);
    let parsed = connector_influx::Kind::default().parse(config);
    assert_eq!(parsed, connector_influx::Kind::default().parse(&expected));
    assert!(parsed.is_ok(), "{parsed:?}");
}

#[test]
fn refuses_a_kind_that_the_table_does_not_have() {
    let text = "connector \"plc\" {\n  kind = \"modbus\"\n  node = \"edge\"\n}\n";
    assert_eq!(
        problems(text),
        [problem(
            text,
            "connector.unknown-kind",
            "\"modbus\"",
            "this build has no connector kind \"modbus\"",
            "Use one of [\"influx\"]",
        )]
    );
}

#[test]
fn refuses_a_connector_with_no_kind_or_no_node() {
    let text = "connector \"a\" {\n  node = \"edge\"\n}\n\
                connector \"b\" {\n  kind = \"influx\"\n}\n";
    let b = text.find("connector \"b\"").expect("b");
    let mut expected = vec![problem(
        text,
        "document.missing-attribute",
        "connector",
        "the `connector` block has no `kind`",
        "Add a `kind` attribute with the connector's kind",
    )];
    expected.push((
        "document.missing-attribute",
        Some(u32::try_from(b).expect("short")),
        "the `connector` block has no `node`".into(),
        "Add a `node` attribute with the name of the node that runs it, such as \
         \"edge\""
            .into(),
    ));
    let kind = problem(
        text,
        "document.missing-attribute",
        "\"influx\"",
        "the connector has no `address`",
        "Add an `address` attribute with the InfluxDB endpoint, such as \
         \"http://influx:8086\"",
    );
    let select = problem(
        text,
        "document.missing-attribute",
        "\"influx\"",
        "the connector has no `select`",
        "Add a `select` attribute with the channels it reads, such as \"site_a.**\"",
    );
    expected.extend([select, kind]);
    assert_eq!(problems(text), expected);
}

#[test]
fn places_the_problems_of_the_kind_in_source_order() {
    let text = "connector \"influx\" {\n  kind = \"influx\"\n  node = 3\n  \
                address = \"https://influx\"\n  select = \"edge.*\"\n  port = 1\n}\n";
    assert_eq!(
        problems(text),
        [
            problem(
                text,
                "document.bad-name",
                "3",
                "a name is a string or a reference, not an integer",
                "Write a name such as \"site_a.node_1\"",
            ),
            problem(
                text,
                "connector.bad-uri",
                "\"https://influx\"",
                "the scheme of the URI is not http",
                "Write an `http` URI such as \"http://10.0.0.2:8086\"",
            ),
            problem(
                text,
                "document.unknown-attribute",
                "port",
                "`port` is not an attribute of the connector",
                "Use `select` or `address`, or remove it",
            ),
        ]
    );
}

#[test]
fn refuses_a_connector_and_a_channel_of_one_name_in_either_order() {
    let channel = "channel \"edge.time\" {\n  kind = \"index\"\n}\n";
    let connector = "connector \"Edge.Time\" {\n  kind = \"influx\"\n  node = \"cloud\"\n  \
                     address = \"http://influx:8086\"\n  select = \"edge.*\"\n}\n";
    for (text, first, second, earlier, repeat) in [
        (
            format!("{channel}{connector}"),
            "channel",
            "connector",
            "edge.time",
            "Edge.Time",
        ),
        (
            format!("{connector}{channel}"),
            "connector",
            "channel",
            "Edge.Time",
            "edge.time",
        ),
    ] {
        let diagnostics =
            config::check(&[read(&text)], &kinds()).expect_err("problems");
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("one problem: {diagnostics:?}");
        };
        assert_eq!(diagnostic.code.as_str(), "config.duplicate-name");
        assert_eq!(
            diagnostic.message,
            format!(
                "the name {repeat:?} repeats the earlier `{first}` name {earlier:?}"
            )
        );
        assert_eq!(
            diagnostic.fix,
            format!(
                "Give each `{first}` and `{second}` block a name that differs by more \
                 than case"
            )
        );
        let offset = |label: &str| {
            text.find(&format!("{label:?}"))
                .and_then(|at| u32::try_from(at).ok())
        };
        assert_eq!(
            diagnostic.span.map(|span| span.start().offset),
            offset(repeat)
        );
        let notes: Vec<_> = diagnostic
            .notes
            .iter()
            .map(|note| (note.span.start().offset, note.text.as_str()))
            .collect();
        assert_eq!(
            notes,
            [(offset(earlier).expect("a note"), "the earlier name")]
        );
    }
}

/// A front end other than HCL can give a config that nests too deep.
#[test]
fn refuses_a_config_that_nests_too_deep() {
    let text = "connector \"influx\" {\n  kind = \"influx\"\n  node = \"cloud\"\n}\n";
    let mut document = read(text);
    let body = &mut document.blocks[0].body;
    let mut deep = Value {
        kind: value::Kind::Integer(1),
        span: None,
    };
    for _ in 0..=DEPTH_MAX {
        deep = Value {
            kind: value::Kind::List(vec![deep]),
            span: None,
        };
    }
    let mut attributes: Vec<Attribute> = body.attributes.iter().cloned().collect();
    attributes.push(Attribute {
        key: "deep".into(),
        key_span: None,
        value: deep,
    });
    body.attributes = Map::new(attributes).expect("unique keys");
    let diagnostics = config::check(&[document], &kinds()).expect_err("problems");
    assert_eq!(
        diagnostics,
        [Diagnostic::new(
            Code::new("document.too-deep"),
            None,
            "the document nests deeper than 64 levels".into(),
            "Make it flatter".into(),
        )]
    );
}

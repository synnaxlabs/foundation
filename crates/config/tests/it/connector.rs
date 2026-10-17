//! The `connector` block, read from HCL and checked by the kinds of a table.

use config::{Definition, Entry};
use connector::kind::{self, Channels, Context, Kind, Table};
use document::diagnostic::{Code, Diagnostic};
use document::encoding::DEPTH_MAX;
use document::value::{self, Value};
use document::{Attribute, Document, Map, Source};
use proptest::prelude::*;
use spec::connector::Connector;
use spec::definition::Definition as Stored;
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
fn checks_the_influx_fixture_into_one_connector_and_its_status() {
    let entries = config::check(&[read(FIXTURE)], &kinds()).expect("no problems");
    let keys: Vec<_> = entries.keys().map(Name::as_str).collect();
    let expected = [
        "influx",
        "influx.status.backoff",
        "influx.status.class",
        "influx.status.confirmed",
        "influx.status.error",
        "influx.status.lost",
        "influx.status.note",
        "influx.status.oversized",
        "influx.status.restarts",
        "influx.status.state",
        "influx.status.time",
    ];
    assert_eq!(keys, expected);
    let connector = connector(&entries[&name("influx")]);
    assert_eq!(connector.kind(), &name("influx"));
    assert_eq!(connector.node(), &name("cloud"));
    let config = connector.config().document();
    let expected = read(
        "address = \"http://influx:8086\"\ndatabase = \"edge\"\n\
         measurement = \"telemetry\"\nselect = \"edge.*\"\n\
         reader {\n  mode = \"complete\"\n  hold = \"2h\"\n}\n",
    );
    assert_eq!(config, &expected);
    let parsed = connector_influx::Kind::default().parse(config);
    parsed.expect("a config");
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
fn hides_the_problems_of_the_config_when_the_kind_is_not_a_name() {
    let text = "connector \"x\" {\n  kind = 3\n  node = \"edge\"\n  port = 1\n}\n";
    assert_eq!(
        problems(text),
        [problem(
            text,
            "document.bad-name",
            "3",
            "a name is a string or a reference, not an integer",
            "Write a name such as \"site_a.node_1\"",
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
    let database = problem(
        text,
        "document.missing-attribute",
        "\"influx\"",
        "the connector has no `database` or `bucket`",
        "Add a `bucket` attribute and an `org` attribute, such as \"plant\", or a \
         `database` attribute for InfluxDB 1",
    );
    let measurement = problem(
        text,
        "document.missing-attribute",
        "\"influx\"",
        "the connector has no `measurement`",
        "Add a `measurement` attribute with the name of each line, such as \
         \"telemetry\"",
    );
    expected.extend([select, kind, database, measurement]);
    assert_eq!(problems(text), expected);
}

#[test]
fn places_the_problems_of_the_kind_in_source_order() {
    let text = "connector \"influx\" {\n  kind = \"influx\"\n  node = 3\n  \
                address = \"ftp://influx\"\n  database = \"edge\"\n  \
                measurement = \"telemetry\"\n  select = \"edge.*\"\n  port = 1\n}\n";
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
                "\"ftp://influx\"",
                "the scheme of the URI is not http or https",
                "Write an `http` or `https` URI such as \"http://10.0.0.2:8086\"",
            ),
            problem(
                text,
                "document.unknown-attribute",
                "port",
                "`port` is not an attribute of the connector",
                "Use `select`, `address`, `database`, `bucket`, `org`, `measurement`, \
                 `token`, or `ca`, or remove it",
            ),
        ]
    );
}

#[test]
fn refuses_a_connector_and_a_channel_of_one_name_in_either_order() {
    let channel = "channel \"edge.time\" {\n  kind = \"index\"\n}\n";
    let connector = "connector \"Edge.Time\" {\n  kind = \"influx\"\n  node = \"cloud\"\n  \
                     address = \"http://influx:8086\"\n  database = \"edge\"\n  \
                     measurement = \"telemetry\"\n  select = \"edge.*\"\n}\n";
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
    let documents = [document];
    let deep = [Diagnostic::new(
        Code::new("document.too-deep"),
        None,
        "the document nests deeper than 64 levels".into(),
        "Make it flatter".into(),
    )];
    let diagnostics = config::check(&documents, &kinds()).expect_err("problems");
    assert_eq!(diagnostics, deep);
    let found = config::connector(&documents, &name("influx")).expect_err("problems");
    assert_eq!(found, deep);
}

/// Each problem of `config::connector` of `name` in `text`, as [`problems`] gives it.
fn refused(text: &str, name: &str) -> Vec<(&'static str, Option<u32>, String, String)> {
    let diagnostics =
        config::connector(&[read(text)], &self::name(name)).expect_err("problems");
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

#[test]
fn finds_a_connector_of_kind_node_and_address_beside_a_fill_and_a_bad_block() {
    let text = "connector \"influx\" {\n  kind = \"influx\"\n  node = \"cloud\"\n  \
                address = \"http://influx:8086\"\n}\n\
                channel \"edge.temp\" {\n  kind = \"data\"\n  unit = fill(\"unit\")\n}\n\
                connector \"plc\" {\n  kind = \"modbus\"\n}\n";
    let connector = config::connector(&[read(text)], &name("influx")).expect("found");
    assert_eq!(connector.kind(), &name("influx"));
    assert_eq!(connector.node(), &name("cloud"));
    let expected = read("address = \"http://influx:8086\"\n");
    assert_eq!(connector.config().document(), &expected);
}

#[test]
fn finds_a_connector_in_the_second_document() {
    let first = read("channel \"edge.time\" {\n  kind = \"index\"\n}\n");
    let second = config_hcl::read(
        Source(1),
        "connector \"plant\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n",
    )
    .expect("the text is HCL");
    let connector = config::connector(&[first, second], &name("plant")).expect("found");
    assert_eq!(connector.kind(), &name("opcua"));
    assert_eq!(connector.node(), &name("edge"));
    assert_eq!(connector.config().document(), &Document::default());
}

#[test]
fn refuses_a_name_that_no_connector_block_has_and_lists_the_names() {
    let text = "connector \"b\" {\n  kind = \"influx\"\n  node = \"cloud\"\n}\n\
                connector \"Plant\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n\
                channel \"plant\" {\n  kind = \"index\"\n}\n";
    assert_eq!(
        refused(text, "plant"),
        [(
            "config.no-connector",
            None,
            "no `connector` block of the files has the name \"plant\"".into(),
            "Use one of [\"Plant\", \"b\"]".into(),
        )]
    );
}

#[test]
fn refuses_a_name_in_files_with_no_connector_block() {
    let text = "channel \"edge.time\" {\n  kind = \"index\"\n}\n";
    assert_eq!(
        refused(text, "plant"),
        [(
            "config.no-connector",
            None,
            "no `connector` block of the files has the name \"plant\"".into(),
            "Add a `connector` block with the name \"plant\"".into(),
        )]
    );
}

#[test]
fn refuses_two_connector_blocks_of_one_name_in_any_case() {
    let text = "connector \"plant\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n\
                connector \"Plant\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n";
    let diagnostics =
        config::connector(&[read(text)], &name("plant")).expect_err("problems");
    let offset = |at: &str| text.find(at).and_then(|at| u32::try_from(at).ok());
    let [diagnostic] = diagnostics.as_slice() else {
        panic!("one problem: {diagnostics:?}");
    };
    assert_eq!(diagnostic.code.as_str(), "config.duplicate-name");
    assert_eq!(
        diagnostic.message,
        "the name \"Plant\" repeats the earlier `connector` name \"plant\""
    );
    assert_eq!(
        diagnostic.fix,
        "Give each `connector` block a name that differs by more than case"
    );
    assert_eq!(
        diagnostic.span.map(|span| span.start().offset),
        offset("\"Plant\"")
    );
    let notes: Vec<_> = diagnostic
        .notes
        .iter()
        .map(|note| (Some(note.span.start().offset), note.text.as_str()))
        .collect();
    assert_eq!(notes, [(offset("\"plant\""), "the earlier name")]);
}

#[test]
fn refuses_a_connector_with_no_kind_and_no_node() {
    let text = "connector \"plant\" {\n  address = \"opc.tcp://plc:4840\"\n}\n";
    let at = |message: &str, fix: &str| {
        problem(
            text,
            "document.missing-attribute",
            "connector",
            message,
            fix,
        )
    };
    assert_eq!(
        refused(text, "plant"),
        [
            at(
                "the `connector` block has no `kind`",
                "Add a `kind` attribute with the connector's kind",
            ),
            at(
                "the `connector` block has no `node`",
                "Add a `node` attribute with the name of the node that runs it, such \
                 as \"edge\"",
            ),
        ]
    );
}

#[test]
fn refuses_a_kind_that_is_not_a_name() {
    let text = "connector \"plant\" {\n  kind = 3\n  node = \"edge\"\n}\n";
    assert_eq!(
        refused(text, "plant"),
        [problem(
            text,
            "document.bad-name",
            "3",
            "a name is a string or a reference, not an integer",
            "Write a name such as \"site_a.node_1\"",
        )]
    );
}

#[test]
fn refuses_files_with_a_private_key_in_another_block() {
    let text = "connector \"plant\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n\
                connector \"other\" {\n  kind = \"opcua\"\n  node = \"edge\"\n  \
                key = \"-----BEGIN PRIVATE KEY-----\\nMC4CAQAwBQYDK2VwBCIEIA==\\n\"\n}\n";
    let alarms = config::check(&[read(text)], &kinds()).expect_err("an alarm");
    assert_eq!(alarms[0].code.as_str(), "config.private-key");
    let found = config::connector(&[read(text)], &name("plant"));
    assert_eq!(found.expect_err("an alarm"), alarms);
}

#[test]
fn refuses_a_fill_in_kind_as_check_does() {
    let text = "connector \"plant\" {\n  kind = fill(\"kind\")\n  node = \"edge\"\n}\n";
    let unfilled = vec![(
        "config.unfilled",
        Some(29),
        "the import found no value for kind".to_owned(),
        "Write the value in place of `fill`".to_owned(),
    )];
    assert_eq!(problems(text), unfilled);
    assert_eq!(refused(text, "plant"), unfilled);
}

#[test]
fn refuses_a_fill_in_the_body_of_the_connector() {
    let text = "connector \"plant\" {\n  kind = \"opcua\"\n  node = \"edge\"\n  \
                address = fill(\"address\")\n}\n";
    assert_eq!(
        refused(text, "plant"),
        vec![(
            "config.unfilled",
            Some(65),
            "the import found no value for address".to_owned(),
            "Write the value in place of `fill`".to_owned(),
        )]
    );
}

#[test]
fn refuses_a_reserved_label_as_check_does() {
    let text = "connector \"@plant\" {\n  kind = \"influx\"\n  node = \"edge\"\n}\n";
    let reserved = vec![(
        "config.reserved-name",
        Some(10),
        "\"@plant\" has a segment that starts with `@`, which is reserved".to_owned(),
        "Remove the `@` from each segment".to_owned(),
    )];
    assert_eq!(problems(text)[..1], reserved[..]);
    assert_eq!(refused(text, "@plant"), reserved);
    assert_eq!(refused(text, "@PLANT"), reserved);
}

#[test]
fn keeps_the_first_connector_block_in_span_order_at_a_repeated_name() {
    let block = |label: &str| {
        format!("connector \"{label}\" {{\n  kind = \"opcua\"\n  node = \"edge\"\n}}\n")
    };
    let later = config_hcl::read(Source(1), &block("Plant")).expect("the text is HCL");
    let documents = [later, read(&block("plant"))];
    let diagnostics =
        config::connector(&documents, &name("plant")).expect_err("problems");
    let messages: Vec<_> = diagnostics.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(
        messages,
        ["the name \"Plant\" repeats the earlier `connector` name \"plant\""]
    );
}

#[test]
fn finds_a_connector_beside_two_other_connector_blocks_of_one_name_in_any_case() {
    let text = "connector \"plant\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n\
                connector \"b\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n\
                connector \"B\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n";
    let found = config::connector(&[read(text)], &name("plant")).expect("found");
    assert_eq!(found.kind(), &name("opcua"));
    assert_eq!(found.node(), &name("edge"));
}

#[test]
fn refuses_what_check_refuses_at_a_connector_of_the_name() {
    let cases = [
        (
            "connector \"plant\" {\n  kind = fill(\"kind\")\n  node = \"edge\"\n}\n\
             connector \"Plant\" {\n  kind = \"influx\"\n  node = \"edge\"\n}\n",
            "plant",
            "config.unfilled",
        ),
        (
            "connector \"@plant\" {\n  kind = fill(\"kind\")\n  node = \"edge\"\n}\n",
            "@plant",
            "config.unfilled",
        ),
        (
            "connector \"plant\" {\n  kind = \"influx\"\n  node = \"edge\"\n}\n\
             connector \"Plant\" {\n  kind = fill(\"kind\")\n  node = \"edge\"\n}\n",
            "plant",
            "config.unfilled",
        ),
    ];
    for (text, name, code) in cases {
        let found = refused(text, name);
        let codes: Vec<_> = found.iter().map(|(code, ..)| *code).collect();
        assert_eq!(codes, [code], "{text}");
        assert_eq!(found, problems(text), "{text}");
    }
}

#[test]
fn refuses_a_connector_at_the_name_of_an_earlier_channel_as_check_does() {
    let text = "channel \"Plant\" {\n  kind = \"index\"\n}\n\
                connector \"plant\" {\n  kind = \"influx\"\n  node = \"edge\"\n  \
                url = \"http://influx:8086\"\n  bucket = \"b\"\n}\n";
    let duplicate = (
        "config.duplicate-name",
        Some(47),
        "the name \"plant\" repeats the earlier `channel` name \"Plant\"".to_owned(),
        "Give each `channel` and `connector` block a name that differs by more than case"
            .to_owned(),
    );
    let checked = problems(text);
    assert!(checked.contains(&duplicate), "{checked:?}");
    assert_eq!(refused(text, "plant"), [duplicate]);
}

#[test]
fn finds_a_connector_before_a_channel_of_its_name() {
    let text = "connector \"plant\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n\
                channel \"Plant\" {\n  kind = \"index\"\n}\n";
    let found = config::connector(&[read(text)], &name("plant")).expect("found");
    assert_eq!(found.kind(), &name("opcua"));
}

#[test]
fn names_no_reserved_connector_in_the_fix_of_a_missing_name() {
    let text = "connector \"plant.@x\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n";
    assert_eq!(
        refused(text, "other"),
        [(
            "config.no-connector",
            None,
            "no `connector` block of the files has the name \"other\"".to_owned(),
            "Add a `connector` block with the name \"other\"".to_owned(),
        )]
    );
}

#[test]
fn names_no_repeated_connector_in_the_fix_of_a_missing_name() {
    let text = "connector \"other\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n\
                connector \"Other\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n";
    assert_eq!(
        refused(text, "plant"),
        [(
            "config.no-connector",
            None,
            "no `connector` block of the files has the name \"plant\"".to_owned(),
            "Use one of [\"other\"]".to_owned(),
        )]
    );
}

#[test]
fn names_no_connector_that_repeats_a_channel_in_the_fix_of_a_missing_name() {
    let text = "channel \"Other\" {\n  kind = \"index\"\n}\n\
                connector \"other\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n";
    assert_eq!(
        refused(text, "plant"),
        [(
            "config.no-connector",
            None,
            "no `connector` block of the files has the name \"plant\"".to_owned(),
            "Add a `connector` block with the name \"plant\"".to_owned(),
        )]
    );
}

#[test]
fn gives_the_problems_of_kind_and_node_in_the_order_of_the_text() {
    let text = "connector \"plant\" {\n  node = 2\n  kind = 1\n}\n";
    let refused = refused(text, "plant");
    let checked = problems(text);
    assert_eq!(refused, checked);
    assert_eq!(
        refused.iter().map(|problem| problem.1).collect::<Vec<_>>(),
        [Some(29), Some(40)]
    );
}

#[test]
fn finds_a_connector_at_a_status_name_of_another_connector() {
    let text = "connector \"plant\" {\n  kind = \"any\"\n  node = \"edge\"\n}\n\
                connector \"plant.status.time\" {\n  kind = \"any\"\n\
                node = \"edge\"\n}\n";
    let kinds = Table::new().with("any", Any);
    let checked = config::check(&[read(text)], &kinds).expect_err("problems");
    let codes: Vec<_> = checked
        .iter()
        .map(|problem| problem.code.as_str())
        .collect();
    assert_eq!(codes, ["config.implied-channel"]);
    let found = config::connector(&[read(text)], &name("plant.status.time"));
    assert_eq!(found.expect("found").kind(), &name("any"));
}

#[test]
fn refuses_a_connector_whose_status_name_is_long() {
    let label = "a".repeat(Name::MAX_BYTES);
    let text =
        format!("connector \"{label}\" {{\n  kind = \"any\"\n  node = \"edge\"\n}}\n");
    let message = format!(
        "the name of a status channel of the connector `{label}` is {} bytes, and the \
         most is {}",
        Name::MAX_BYTES + ".status.time".len(),
        Name::MAX_BYTES
    );
    let long = (
        "config.long-name",
        Some(10),
        message,
        "Shorten the name of the connector".to_owned(),
    );
    let kinds = Table::new().with("any", Any);
    let checked = config::check(&[read(&text)], &kinds).expect_err("problems");
    let checked: Vec<_> = checked
        .iter()
        .map(|problem| {
            let at = problem.span.map(|span| span.start().offset);
            let (message, fix) = (problem.message.clone(), problem.fix.clone());
            (problem.code.as_str(), at, message, fix)
        })
        .collect();
    assert_eq!(checked, std::slice::from_ref(&long));
    assert_eq!(refused(&text, &label), [long]);
}

#[test]
fn finds_a_connector_beside_a_reserved_connector_of_another_name() {
    let text = "connector \"@other\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n\
                connector \"plant\" {\n  kind = \"opcua\"\n  node = \"edge\"\n}\n";
    let found = config::connector(&[read(text)], &name("plant")).expect("found");
    assert_eq!(found.kind(), &name("opcua"));
    assert_eq!(found.node(), &name("edge"));
}

#[test]
fn refuses_a_reserved_connector_that_check_hides_behind_a_fill_in_another_block() {
    let text = "connector \"@plant\" {\n}\n\
                connector \"plant\" {\n  kind = \"influx\"\n  node = fill(\"n\")\n}\n";
    assert_eq!(
        problems(text),
        [problem(
            text,
            "config.unfilled",
            "fill(",
            "the import found no value for n",
            "Write the value in place of `fill`",
        )]
    );
    assert_eq!(
        refused(text, "@plant"),
        [problem(
            text,
            "config.reserved-name",
            "\"@plant\"",
            "\"@plant\" has a segment that starts with `@`, which is reserved",
            "Remove the `@` from each segment",
        )]
    );
}

/// A kind that takes each config and gives no channels.
struct Any;

impl Kind for Any {
    type Config = ();

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), kind::Error>> {
        std::future::ready(Ok(()))
    }
}

const LABELS: [&str; 7] = [
    "plant",
    "Plant",
    "PLANT",
    "@plant",
    "plant.@x",
    "plant.status.time",
    "other",
];

/// A `channel` or `connector` block: whether it is a connector, its label, its text,
/// and its text with each `fill` as a plain value of the same length, so that each
/// span stays.
fn block() -> impl Strategy<Value = (bool, &'static str, String, String)> {
    let label = proptest::sample::select(LABELS.to_vec());
    let kind = proptest::sample::select(vec![
        ("  kind = \"any\"\n", "  kind = \"any\"\n"),
        ("  kind = fill(\"k\")\n", "  kind = \"another\"\n"),
        ("", ""),
    ]);
    let node = proptest::sample::select(vec![
        ("  node = \"edge\"\n", "  node = \"edge\"\n"),
        ("  node = fill(\"n\")\n", "  node = \"edgetwo\"\n"),
        ("", ""),
    ]);
    prop_oneof![
        (label.clone(), proptest::bool::ANY).prop_map(|(label, fill)| {
            let text = |kind| format!("channel \"{label}\" {{\n  kind = {kind}\n}}\n");
            let plain = text("\"index\"  ");
            (
                false,
                label,
                if fill {
                    text("fill(\"i\")")
                } else {
                    plain.clone()
                },
                plain,
            )
        }),
        (label, kind, node).prop_map(|(label, kind, node)| {
            let text =
                |kind, node| format!("connector \"{label}\" {{\n{kind}{node}}}\n");
            (true, label, text(kind.0, node.0), text(kind.1, node.1))
        }),
    ]
}

proptest! {
    /// At each label, with `check` on the files where each `fill` outside the
    /// `connector` blocks of the label in any ASCII case is a plain value: a connector
    /// that `check` gives, `find` gives; a problem of `find` other than
    /// `config.no-connector`, `check` gives; and a problem of `check` other than
    /// `config.implied-channel` in a `connector` block of the label makes `find` fail.
    #[test]
    fn finds_what_check_finds_at_each_name(
        blocks in proptest::collection::vec(block(), 1..=4),
    ) {
        let text: String = blocks.iter().map(|(_, _, text, _)| text.as_str()).collect();
        let documents = [read(&text)];
        let kinds = Table::new().with("any", Any).with("another", Any);
        let mut start = 0;
        let mut spans = Vec::new();
        for (connector, label, text, _) in &blocks {
            spans.push((*connector, *label, start..start + text.len()));
            start += text.len();
        }
        for label in LABELS {
            let Ok(at) = label.parse::<Name>() else {
                continue;
            };
            let found = config::connector(&documents, &at);
            let plain: String = blocks
                .iter()
                .map(|(connector, block, text, plain)| {
                    if *connector && block.eq_ignore_ascii_case(label) {
                        text
                    } else {
                        plain
                    }
                })
                .map(String::as_str)
                .collect();
            let checked = config::check(&[read(&plain)], &kinds);
            if let Ok(entries) = &checked
                && let Some(entry) = entries.get(&at)
                && let Definition::Spec(Stored::Connector(entry)) = &entry.definition
            {
                prop_assert_eq!(found.as_ref(), Ok(entry), "{}", text);
            }
            if let Err(problems) = &found {
                let missing = problems
                    .iter()
                    .filter(|problem| problem.code.as_str() != "config.no-connector");
                for problem in missing {
                    let checked = checked.as_ref().err();
                    prop_assert!(
                        checked.is_some_and(|checked| checked.contains(problem)),
                        "{label}: {problem:?} not in {checked:?}\n{text}",
                    );
                }
            }
            if let Err(problems) = &checked {
                let implied = |problem: &&Diagnostic| {
                    problem.code.as_str() != "config.implied-channel"
                };
                let at_block = problems.iter().filter(implied).find(|problem| {
                    problem.span.is_some_and(|span| {
                        let offset = usize::try_from(span.start().offset)
                            .expect("a short text");
                        spans.iter().any(|(connector, block, range)| {
                            *connector
                                && block.eq_ignore_ascii_case(label)
                                && range.contains(&offset)
                        })
                    })
                });
                if let Some(problem) = at_block {
                    prop_assert!(
                        found.is_err(),
                        "{label}: {problem:?}, found {found:?}\n{text}",
                    );
                }
            }
        }
    }
}

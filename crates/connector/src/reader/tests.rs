use document::diagnostic::Note;
use document::value::Kind;
use document::{Attribute, Label, Map, Position, Source};
use proptest::prelude::*;

use super::*;

/// A one-byte span at `offset`.
fn at(offset: u32) -> Option<document::Span> {
    let position = |offset| Position {
        offset,
        line: 0,
        column: offset,
    };
    document::Span::new(Source(0), position(offset), position(offset + 1))
}

fn string(text: &str) -> Kind {
    Kind::String(text.into())
}

/// A document with `attributes`, each key at its offset and its value one byte after.
fn document(attributes: &[(u32, &str, Kind)], blocks: Vec<Block>) -> Document {
    let attributes = attributes
        .iter()
        .map(|(offset, key, kind)| Attribute {
            key: (*key).into(),
            key_span: at(*offset),
            value: Value {
                kind: kind.clone(),
                span: at(offset + 1),
            },
        })
        .collect();
    Document {
        attributes: Map::new(attributes).expect("unique keys"),
        blocks,
    }
}

/// A block with `keyword` at `offset`.
fn block(offset: u32, keyword: &str, body: Document) -> Block {
    Block {
        keyword: keyword.into(),
        keyword_span: at(offset),
        labels: Vec::new(),
        body,
        span: at(offset),
    }
}

/// A config that selects `edge.*`, with `reader` as its one block.
fn config(reader: &[(u32, &str, Kind)]) -> Document {
    let reader = block(50, "reader", document(reader, Vec::new()));
    document(&[(0, "select", string("edge.*"))], vec![reader])
}

fn name(text: &str) -> Name {
    text.parse().expect("a name")
}

fn selector(pattern: &str) -> Selector {
    Selector::new([pattern]).expect("a selector")
}

fn refused(code: &'static str, at: u32, message: &str, fix: &str) -> Diagnostic {
    Diagnostic::new(Code::new(code), self::at(at), message.into(), fix.into())
}

/// The diagnostic for a second `reader` block at 95, after the one of `config`.
fn second_reader() -> Diagnostic {
    let mut second = refused(
        "document.repeated-block",
        95,
        "the connector has a second `reader` block",
        "Join the two into one",
    );
    second.notes.push(Note {
        span: at(50).expect("a span"),
        text: "the first `reader` block".into(),
    });
    second
}

#[test]
fn reads_a_named_complete_reader_with_a_hold() {
    let config = config(&[
        (60, "name", string("influx")),
        (70, "mode", string("complete")),
        (80, "hold", string("2h")),
    ]);
    let expected = Settings {
        name: Some(name("influx")),
        select: selector("edge.*"),
        mode: Mode::Complete,
        hold: "2h".parse().expect("a span"),
    };
    assert_eq!(read(&config, &[], &[]), Ok(expected));
}

#[test]
fn reads_a_complete_reader_with_no_name_and_no_reader_block() {
    let config = document(&[(0, "select", string("edge.*"))], Vec::new());
    let expected = Settings {
        name: None,
        select: selector("edge.*"),
        mode: Mode::Complete,
        hold: Span::ZERO,
    };
    assert_eq!(read(&config, &[], &[]), Ok(expected));
}

#[test]
fn reads_a_latest_mode_written_as_a_reference() {
    let config = config(&[(70, "mode", Kind::Reference(name("latest")))]);
    let settings = read(&config, &[], &[]).expect("settings");
    assert_eq!((settings.mode, settings.hold), (Mode::Latest, Span::ZERO));
}

#[test]
fn leaves_the_keys_of_the_kind_to_the_kind() {
    let mut config = config(&[]);
    config = document(
        &[
            (0, "select", string("edge.*")),
            (10, "address", string("x")),
        ],
        config.blocks,
    );
    config
        .blocks
        .push(block(90, "channel", Document::default()));
    assert_eq!(
        read(&config, &["address"], &["channel"]).map(|settings| settings.select),
        Ok(selector("edge.*"))
    );
}

#[test]
fn refuses_a_key_that_neither_it_nor_the_kind_reads() {
    let config = document(
        &[
            (0, "select", string("edge.*")),
            (10, "address", string("x")),
            (20, "port", string("y")),
        ],
        vec![
            block(30, "reader", Document::default()),
            block(40, "channel", Document::default()),
            block(45, "inner", Document::default()),
        ],
    );
    assert_eq!(
        read(&config, &["address"], &["channel"]),
        Err(vec![
            refused(
                "document.unknown-attribute",
                20,
                "`port` is not an attribute of the connector",
                "Use `select` or `address`, or remove it",
            ),
            refused(
                "document.unknown-block",
                45,
                "the connector cannot hold the `inner` block",
                "Use `reader` or `channel`, or remove it",
            ),
        ])
    );
}

#[test]
#[should_panic(
    expected = "a kind lists `select` or `reader`, which `read` reads itself"
)]
fn panics_when_the_kind_lists_select() {
    drop(read(&config(&[]), &["select", "address"], &[]));
}

#[test]
#[should_panic(
    expected = "a kind lists `select` or `reader`, which `read` reads itself"
)]
fn panics_when_the_kind_lists_reader() {
    drop(read(&config(&[]), &[], &["reader"]));
}

#[test]
fn refuses_a_config_with_no_select() {
    let config = document(&[], Vec::new());
    let expected = Diagnostic::new(
        Code::new("document.missing-attribute"),
        None,
        "the connector has no `select`".into(),
        "Add a `select` attribute with the channels it reads, such as \"site_a.**\""
            .into(),
    );
    assert_eq!(read(&config, &[], &[]), Err(vec![expected]));
}

#[test]
fn refuses_a_select_that_does_not_read() {
    let config = document(&[(0, "select", Kind::Integer(3))], Vec::new());
    let expected = refused(
        "document.bad-selector",
        1,
        "a pattern is a string or a reference, not an integer",
        "Write a string such as \"site_a.*\"",
    );
    assert_eq!(read(&config, &[], &[]), Err(vec![expected]));
}

#[test]
fn reads_a_hold_with_no_name() {
    let config = config(&[(80, "hold", string("2h"))]);
    let expected = Settings {
        name: None,
        select: selector("edge.*"),
        mode: Mode::Complete,
        hold: "2h".parse().expect("a span"),
    };
    assert_eq!(read(&config, &[], &[]), Ok(expected));
}

#[test]
fn refuses_a_negative_hold_at_its_value() {
    let config = config(&[(60, "name", string("influx")), (80, "hold", string("-1s"))]);
    let expected = refused(
        "document.negative-span",
        81,
        "the span -1s is below zero",
        "Write a span of zero or more",
    );
    assert_eq!(read(&config, &[], &[]), Err(vec![expected]));
}

#[test]
fn reads_a_zero_hold() {
    let config = config(&[(60, "name", string("influx")), (80, "hold", string("0s"))]);
    assert_eq!(
        read(&config, &[], &[]).map(|settings| settings.hold),
        Ok(Span::ZERO)
    );
}

#[test]
fn refuses_a_hold_in_latest_mode() {
    let config = config(&[
        (60, "name", string("influx")),
        (70, "mode", string("latest")),
        (80, "hold", string("0s")),
    ]);
    let expected = refused(
        "connector.latest-hold",
        80,
        "the reader has a `hold` in `latest` mode, and only a complete reader holds",
        "Use `mode = \"complete\"`, or remove the `hold`",
    );
    assert_eq!(read(&config, &[], &[]), Err(vec![expected]));
}

#[test]
fn refuses_an_unknown_mode_and_a_mode_that_is_not_text() {
    let fix = "Write \"complete\" or \"latest\"";
    let config_of = |kind| config(&[(70, "mode", kind)]);
    assert_eq!(
        read(&config_of(string("all")), &[], &[]),
        Err(vec![refused(
            "connector.bad-mode",
            71,
            "the reader has no mode \"all\"",
            fix
        )])
    );
    assert_eq!(
        read(&config_of(Kind::Bool(true)), &[], &[]),
        Err(vec![refused(
            "connector.bad-mode",
            71,
            "a mode is a string or a reference, not a bool",
            fix
        )])
    );
}

/// The diagnostic of the name "a..b" at offset 61.
fn bad_name() -> Diagnostic {
    refused(
        "document.bad-name",
        61,
        "a segment is not valid: \"\" in \"a..b\"",
        "Use one or more ASCII letters, digits, `_`, and `-` in that segment, after an \
         optional leading `@`",
    )
}

#[test]
fn refuses_a_name_and_a_hold_that_do_not_read() {
    let config = config(&[(60, "name", string("a..b")), (80, "hold", string("x"))]);
    assert_eq!(
        read(&config, &[], &[]),
        Err(vec![
            bad_name(),
            refused(
                "document.bad-span",
                81,
                "cannot read the span \"x\": a span is not a number and a unit",
                "Write a span such as \"250us\", \"1.5s\", or \"3d\"",
            ),
        ])
    );
}

#[test]
fn refuses_only_a_bad_name_with_a_hold() {
    let config = config(&[(60, "name", string("a..b")), (80, "hold", string("2h"))]);
    assert_eq!(read(&config, &[], &[]), Err(vec![bad_name()]));
}

#[test]
fn refuses_only_a_bad_mode_with_a_hold() {
    let config = config(&[
        (60, "name", string("influx")),
        (70, "mode", string("all")),
        (80, "hold", string("2h")),
    ]);
    assert_eq!(
        read(&config, &[], &[]),
        Err(vec![refused(
            "connector.bad-mode",
            71,
            "the reader has no mode \"all\"",
            "Write \"complete\" or \"latest\"",
        )])
    );
}

#[test]
fn refuses_a_second_reader_block_after_a_good_one() {
    let mut config = config(&[(60, "name", string("influx"))]);
    let mut second =
        block(95, "reader", document(&[(97, "from", string("a"))], vec![]));
    second.labels.push(Label {
        text: "r".into(),
        span: at(96),
    });
    config.blocks.push(second);
    assert_eq!(read(&config, &[], &[]), Err(vec![second_reader()]));
}

#[test]
fn refuses_what_the_reader_block_does_not_take() {
    let mut config =
        config(&[(60, "name", string("influx")), (65, "from", string("x"))]);
    let reader = &mut config.blocks[0];
    reader.labels.push(Label {
        text: "r".into(),
        span: at(55),
    });
    reader
        .body
        .blocks
        .push(block(90, "inner", Document::default()));
    config.blocks.push(block(95, "reader", Document::default()));
    assert_eq!(
        read(&config, &[], &[]),
        Err(vec![
            second_reader(),
            refused(
                "document.label-count",
                55,
                "the `reader` block has 1 label, and it takes none",
                "Remove each label, and name the reader with a `name` attribute",
            ),
            refused(
                "document.unknown-attribute",
                65,
                "`from` is not an attribute of the `reader` block",
                "Use `name`, `mode`, or `hold`, or remove it",
            ),
            refused(
                "document.unknown-block",
                90,
                "the `reader` block cannot hold the `inner` block",
                "Remove it",
            ),
        ])
    );
}

fn names() -> impl Strategy<Value = Name> {
    "[a-z][a-z0-9_]{0,6}(\\.[a-z][a-z0-9_]{0,6}){0,2}".prop_map(|text| name(&text))
}

/// Settings, with the pattern of their selector.
fn settings() -> impl Strategy<Value = (Settings, String)> {
    let named = (names(), 0..=1_000_000i64).prop_map(|(name, millis)| {
        (
            Some(name),
            Mode::Complete,
            Span::from_nanos(millis * 1_000_000),
        )
    });
    let unheld =
        (proptest::option::of(names()), any::<bool>()).prop_map(|(name, latest)| {
            let mode = if latest { Mode::Latest } else { Mode::Complete };
            (name, mode, Span::ZERO)
        });
    (prop_oneof![named, unheld], names(), any::<bool>()).prop_map(
        |((name, mode, hold), select, wild)| {
            let pattern = if wild {
                format!("{select}.*")
            } else {
                select.as_str().to_owned()
            };
            let settings = Settings {
                name,
                select: selector(&pattern),
                mode,
                hold,
            };
            (settings, pattern)
        },
    )
}

/// The config that writes `settings`, which select `pattern`, with only the keys that
/// differ from the defaults.
fn written(settings: &Settings, pattern: &str) -> Document {
    let mut reader = Vec::new();
    if let Some(name) = &settings.name {
        reader.push((60, "name", string(name.as_str())));
    }
    if settings.mode == Mode::Latest {
        reader.push((70, "mode", string("latest")));
    }
    if settings.hold != Span::ZERO {
        reader.push((80, "hold", string(&settings.hold.to_string())));
    }
    let blocks = if reader.is_empty() {
        Vec::new()
    } else {
        vec![block(50, "reader", document(&reader, Vec::new()))]
    };
    document(&[(0, "select", string(pattern))], blocks)
}

proptest! {
    #[test]
    fn reads_back_the_settings_a_config_writes((settings, pattern) in settings()) {
        prop_assert_eq!(read(&written(&settings, &pattern), &[], &[]), Ok(settings));
    }
}

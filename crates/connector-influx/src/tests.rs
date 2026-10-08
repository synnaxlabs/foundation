#![expect(clippy::arithmetic_side_effects, reason = "a test may panic")]

use std::sync::Arc;

use connector::kind::{Kind as _, Table};
use connector::supervisor::Supervisor;
use document::value::Kind as Value;
use document::{Attribute, Block, Map, Position, Source, Span};
use env::clock::Clock;
use env::entropy::Entropy;

use super::*;

/// A one-byte span at `offset`.
fn at(offset: u32) -> Option<Span> {
    let position = |offset| Position {
        offset,
        line: 0,
        column: offset,
    };
    Span::new(Source(0), position(offset), position(offset + 1))
}

fn string(text: &str) -> Value {
    Value::String(text.into())
}

/// A document with `attributes`, each key at its offset and its value one byte after.
fn document(attributes: &[(u32, &str, Value)], blocks: Vec<Block>) -> Document {
    let attributes = attributes
        .iter()
        .map(|(offset, key, kind)| Attribute {
            key: (*key).into(),
            key_span: at(*offset),
            value: document::value::Value {
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

/// A block with `keyword` at `offset` and `attributes`.
fn block(offset: u32, keyword: &str, attributes: &[(u32, &str, Value)]) -> Block {
    Block {
        keyword: keyword.into(),
        keyword_span: at(offset),
        labels: Vec::new(),
        body: document(attributes, Vec::new()),
        span: at(offset),
    }
}

/// The config of `fixtures/influx.hcl`, with `address` set to `address`.
fn config(address: Value) -> Document {
    document(
        &[(0, "address", address), (10, "select", string("edge.*"))],
        vec![block(
            20,
            "reader",
            &[
                (30, "name", string("influx")),
                (40, "mode", string("complete")),
                (50, "hold", string("2h")),
            ],
        )],
    )
}

fn refused(code: &'static str, at: u32, message: &str, fix: &str) -> Diagnostic {
    Diagnostic::new(Code::new(code), self::at(at), message.into(), fix.into())
}

fn bad_address(message: &str) -> Diagnostic {
    refused(
        "influx.bad-address",
        1,
        message,
        "Write an address such as \"http://influx:8086\"",
    )
}

#[test]
fn reads_the_address_and_the_reader() {
    let config = config(string("http://influx:8086"));
    let expected = Config {
        address: "http://influx:8086".parse().expect("a URI"),
        reader: reader::read(&config, &["address"], &[]).expect("the reader settings"),
    };
    assert_eq!(Kind.parse(&config), Ok(expected));
}

#[test]
fn checks_to_no_device_channel() {
    let table = Table::new().with("influx", Kind);
    assert_eq!(
        table.check("influx", None, &config(string("http://influx:8086"))),
        Ok(Channels::default())
    );
}

#[test]
fn refuses_an_address_that_send_refuses() {
    for (address, message) in [
        ("https://influx:8086", "the scheme of the URI is not http"),
        (
            "http://admin:hunter2@influx:8086",
            "the URI holds user info; give a credential through a secret",
        ),
        (
            "http://[influx]:8086",
            "the URI has no valid host: \"[influx]\"",
        ),
        (
            "http://influx:0",
            "the port \"0\" of the URI is not a number from 1 to 65535",
        ),
        ("influx", "the scheme of the URI is not http"),
    ] {
        assert_eq!(
            Kind.parse(&config(string(address))),
            Err(vec![bad_address(message)]),
            "{address}"
        );
    }
}

#[test]
fn refuses_an_address_that_is_not_a_uri() {
    assert_eq!(
        Kind.parse(&config(string("http://in flux"))),
        Err(vec![bad_address(
            "the address is not a URI: invalid uri character"
        )])
    );
}

#[test]
fn refuses_an_address_that_is_not_a_string() {
    assert_eq!(
        Kind.parse(&config(Value::Integer(8086))),
        Err(vec![bad_address("an address is a string, not an integer")])
    );
}

#[test]
fn refuses_a_config_with_no_address_or_select() {
    let config = document(&[], Vec::new());
    assert_eq!(
        Kind.parse(&config),
        Err(vec![
            Diagnostic::new(
                Code::new("document.missing-attribute"),
                None,
                "the connector has no `select`".into(),
                "Add a `select` attribute with the channels it reads, such as \
                 \"site_a.**\""
                    .into(),
            ),
            Diagnostic::new(
                Code::new("document.missing-attribute"),
                None,
                "the connector has no `address`".into(),
                "Add an `address` attribute with the InfluxDB endpoint, such as \
                 \"http://influx:8086\""
                    .into(),
            ),
        ])
    );
}

#[test]
fn refuses_each_key_it_does_not_take() {
    let mut config = config(string("http://influx:8086"));
    config.attributes = Map::new(
        config
            .attributes
            .iter()
            .cloned()
            .chain([Attribute {
                key: "database".into(),
                key_span: at(60),
                value: document::value::Value {
                    kind: string("site"),
                    span: at(61),
                },
            }])
            .collect(),
    )
    .expect("unique keys");
    config.blocks.push(block(70, "tls", &[]));
    assert_eq!(
        Kind.parse(&config),
        Err(vec![
            refused(
                "document.unknown-attribute",
                60,
                "`database` is not an attribute of the connector",
                "Use `select` or `address`, or remove it",
            ),
            refused(
                "document.unknown-block",
                70,
                "the connector cannot hold the `tls` block",
                "Use `reader`, or remove it",
            ),
        ])
    );
}

#[test]
fn stops_its_run_with_a_config_error_until_it_can_run() {
    let config = config(string("http://influx:8086"));
    let error = connector_run(config);
    let Err(Error::Config(diagnostics)) = error else {
        panic!("a config error, not {error:?}");
    };
    assert_eq!(
        diagnostics,
        vec![Diagnostic::new(
            Code::new("influx.not-yet"),
            None,
            "this build cannot run the influx kind yet".into(),
            "Remove the connector, or run it on a build that has the influx writer"
                .into(),
        )]
    );
}

#[test]
fn discovers_nothing() {
    let found = run(|_, _| async {
        Table::new()
            .with("influx", Kind)
            .discover("influx", &cancel::Token::new())
            .await
    });
    assert!(matches!(found.as_deref(), Ok([])), "{found:?}");
}

/// The result of supervising one influx connector with `config`.
fn connector_run(config: Document) -> Result<(), Error> {
    run(move |clock, entropy| async move {
        let kinds = Arc::new(Table::new().with("influx", Kind));
        Supervisor::new(kinds, clock, entropy)
            .run(
                "influx",
                "influx".parse().expect("a name"),
                &config,
                &cancel::Token::new(),
            )
            .await
    })
}

/// Runs `main` on a shard of one simulated node and returns its output.
fn run<T, F>(main: impl FnOnce(Clock, Entropy) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    let mut sim = ::sim::Sim::new(::sim::Config::default());
    let node = sim.node(::sim::node::Config::default());
    sim.run_on(&node, |node, _| main(node.clock(), node.entropy()))
        .expect("the run ends")
}

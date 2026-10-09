#![expect(clippy::arithmetic_side_effects, reason = "a test may panic")]

use connector::kind::{Kind as _, Table};
use connector::supervisor::Supervisor;
use connector::testing;
use document::value::Kind as Value;
use document::{Attribute, Block, Map, Position, Source, Span};
use env::tasks::Tasks;

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
            &[(40, "mode", string("complete")), (50, "hold", string("2h"))],
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
        "Remove it, as in \"http://influx:8086\"",
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
fn reads_an_address_with_a_path_of_slash() {
    let config = config(string("http://influx:8086/"));
    assert_eq!(
        Kind.parse(&config).map(|config| config.address),
        Ok("http://influx:8086/".parse().expect("a URI"))
    );
}

#[test]
fn refuses_an_address_that_uri_refuses() {
    for (address, message) in [
        (
            string("https://influx:8086"),
            "the scheme of the URI is not http",
        ),
        (
            string("http://admin:hunter2@influx:8086"),
            "the URI holds user info; give a credential through a secret",
        ),
        (string("http://[influx]:8086"), "the URI has no valid host"),
        (
            string("http://influx:0"),
            "the port of the URI is not a number from 1 to 65535",
        ),
        (string("influx"), "the scheme of the URI is not http"),
        (
            string("http://influx:8086/#site"),
            "the URI has a fragment, which no request sends",
        ),
        (
            string("http://in flux"),
            "the text is not a URI: invalid uri character",
        ),
        (Value::Integer(8086), "a URI is a string, not an integer"),
    ] {
        assert_eq!(
            Kind.parse(&config(address.clone())),
            Err(vec![refused(
                "connector.bad-uri",
                1,
                message,
                "Write an `http` URI such as \"http://10.0.0.2:8086\"",
            )]),
            "{address:?}"
        );
    }
}

#[test]
fn refuses_an_address_with_a_path_or_a_query() {
    for (address, part) in [
        ("http://influx:8086/api/v2", "a path"),
        ("http://influx:8086//", "a path"),
        ("http://influx:8086/?db=site", "a query"),
        ("http://influx:8086?", "a query"),
    ] {
        assert_eq!(
            Kind.parse(&config(string(address))),
            Err(vec![bad_address(&format!(
                "the address has {part}, which the influx kind does not take"
            ))]),
            "{address}"
        );
    }
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
    run(move |node, tasks| async move {
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks,
        };
        let kinds = Table::new().with("influx", Kind);
        let (inputs, _) = testing::create_config(env, node.net(), kinds).await;
        Supervisor::new(inputs)
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
fn run<T, F>(main: impl FnOnce(::sim::node::Node, Tasks) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    let mut sim = ::sim::Sim::new(::sim::Config::default());
    let node = sim.node(::sim::node::Config::default());
    sim.run_on(&node, main).expect("the run ends")
}

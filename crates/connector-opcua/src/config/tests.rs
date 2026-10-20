#![expect(clippy::arithmetic_side_effects, reason = "a test may panic")]

use connector::browse::{Access, Facts};
use document::value::Kind as Value;
use document::{Position, Source, Span};
use proptest::prelude::*;

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

/// A block with `keyword` at `offset`, each label one byte after the one before, and
/// `attributes`.
fn block(
    offset: u32,
    keyword: &str,
    labels: &[&str],
    attributes: &[(u32, &str, Value)],
) -> Block {
    let labels = (offset + 1..).zip(labels).map(|(offset, text)| Label {
        text: (*text).into(),
        span: at(offset),
    });
    Block {
        keyword: keyword.into(),
        keyword_span: at(offset),
        labels: labels.collect(),
        body: document(attributes, Vec::new()),
        span: at(offset),
    }
}

/// A `read` block of `channel` at `offset`, with `node_id` at `offset + 10`.
fn read(offset: u32, channel: &str, node_id: &str) -> Block {
    block(
        offset,
        "read",
        &[channel],
        &[(offset + 10, "node_id", string(node_id))],
    )
}

/// The config of the connector at `address`, with `reads`.
fn config(address: Value, reads: Vec<Block>) -> Document {
    document(
        &[(0, "address", address), (10, "rate", string("10Hz"))],
        reads,
    )
}

fn refused(code: &'static str, at: u32, message: &str, fix: &str) -> Diagnostic {
    Diagnostic::new(Code::new(code), self::at(at), message.into(), fix.into())
}

const ADDRESS: &str = "opc.tcp://plc:4840";

#[test]
fn reads_the_address_the_rate_and_each_read() {
    let reads = vec![
        read(20, "plant.spike", "ns=3;s=SpikeData"),
        read(40, "plant.dip", "ns=3;i=1001"),
    ];
    let read = |channel: &str, node: &str| Read {
        channel: channel.parse().expect("a name"),
        node: node.parse().expect("a node"),
    };
    assert_eq!(
        parse(&config(string(ADDRESS), reads)),
        Ok(Config {
            address: CString::new(ADDRESS).expect("no NUL"),
            rate: Rate::new(10, 1).expect("a rate"),
            reads: vec![
                read("plant.spike", "ns=3;s=SpikeData"),
                read("plant.dip", "ns=3;i=1001"),
            ],
            security: None,
        })
    );
}

/// A connector with no `read` block reads nothing until an import adds reads.
#[test]
fn reads_a_config_with_no_read() {
    assert_eq!(
        parse(&config(string(ADDRESS), Vec::new())),
        Ok(Config {
            address: CString::new(ADDRESS).expect("no NUL"),
            rate: Rate::new(10, 1).expect("a rate"),
            reads: Vec::new(),
            security: None,
        })
    );
}

#[test]
fn refuses_a_bad_address() {
    let fix = "Write an address such as \"opc.tcp://plc:4840\"";
    for (address, message) in [
        (
            string("http://plc:4840"),
            "the address \"http://plc:4840\" does not start with opc.tcp://",
        ),
        (string("opc.tcp://plc\0:4840"), "the address holds a NUL"),
        (
            Value::Integer(4840),
            "an address is a string, not an integer",
        ),
        (
            Value::Reference("plc".parse().expect("a name")),
            "an address is a string, not a reference",
        ),
    ] {
        let reads = vec![read(20, "plant.spike", "ns=3;s=SpikeData")];
        assert_eq!(
            parse(&config(address.clone(), reads)),
            Err(vec![refused("opcua.bad-address", 1, message, fix)]),
            "{address:?}"
        );
    }
}

/// Each address that open62541 refuses at the connect, or that has no host, is a config
/// error, never a failed connect that the supervisor retries.
#[test]
fn refuses_an_address_that_open62541_refuses() {
    let fix = "Write an address such as \"opc.tcp://plc:4840\"";
    for address in [
        "opc.tcp://",
        "opc.tcp://:4840",
        "opc.tcp://[]:4840",
        "opc.tcp://[::1",
        "opc.tcp://plc:",
        "opc.tcp://plc:65536",
        "opc.tcp://plc:99999",
        "opc.tcp://plc:4840x",
        "opc.tcp://plc:x",
    ] {
        let reads = vec![read(20, "plant.spike", "ns=3;s=SpikeData")];
        let message = format!(
            "the address {address:?} has no host, or a port that is not 0 to 65535"
        );
        assert_eq!(
            parse(&config(string(address), reads)),
            Err(vec![refused("opcua.bad-address", 1, &message, fix)]),
            "{address:?}"
        );
    }
}

#[test]
fn refuses_a_port_past_u32() {
    let fix = "Write an address such as \"opc.tcp://plc:4840\"";
    for address in [
        "opc.tcp://plc:4294967296",
        "opc.tcp://plc:4294972136",
        "opc.tcp://plc:8589934592/path",
    ] {
        let reads = vec![read(20, "plant.spike", "ns=3;s=SpikeData")];
        let message = format!(
            "the address {address:?} has no host, or a port that is not 0 to 65535"
        );
        assert_eq!(
            parse(&config(string(address), reads)),
            Err(vec![refused("opcua.bad-address", 1, &message, fix)]),
            "{address:?}"
        );
    }
}

#[test]
fn reads_an_address_with_no_port_a_path_or_an_ipv6_host() {
    for address in [
        "opc.tcp://plc",
        "opc.tcp://plc/",
        "opc.tcp://plc:65535/path/of/server",
        "opc.tcp://[::1]:4840",
    ] {
        let reads = vec![read(20, "plant.spike", "ns=3;s=SpikeData")];
        let config = parse(&config(string(address), reads)).expect("the config");
        assert_eq!(config.address.to_str(), Ok(address));
    }
}

#[test]
fn refuses_a_config_with_no_address_or_rate() {
    assert_eq!(
        parse(&document(&[], Vec::new())),
        Err(vec![
            Diagnostic::new(
                Code::new("document.missing-attribute"),
                None,
                "the connector has no `address`".into(),
                "Add an `address` attribute with the endpoint of the server, such as \
                 \"opc.tcp://plc:4840\""
                    .into(),
            ),
            Diagnostic::new(
                Code::new("document.missing-attribute"),
                None,
                "the connector has no `rate`".into(),
                "Add a `rate` attribute with the reads per second, such as \"10Hz\""
                    .into(),
            ),
        ])
    );
}

#[test]
fn refuses_a_bad_rate() {
    let config = document(
        &[(0, "address", string(ADDRESS)), (10, "rate", string("0Hz"))],
        vec![read(20, "plant.spike", "ns=3;s=SpikeData")],
    );
    assert_eq!(
        parse(&config),
        Err(vec![refused(
            "document.bad-rate",
            11,
            "cannot read the rate \"0Hz\": a rate has a zero numerator or denominator",
            "Write a rate above zero, such as \"10Hz\"",
        )])
    );
}

#[test]
fn refuses_a_bad_node_id() {
    let fix = "Write a node ID such as \"ns=3;s=SpikeData\" or \"ns=3;i=1001\"";
    for (node_id, message) in [
        (
            string("nsu=urn:plant;s=SpikeData"),
            "a namespace by URI (nsu=) is not supported: give its index (ns=)",
        ),
        (
            string("ns=3;g=09087e75-8e5e-499b-954f-f2a9603db28a"),
            "a GUID (g=) or opaque (b=) key is not supported: give a number (i=) or a \
             name (s=)",
        ),
        (
            string("ns=3;s="),
            "a node ID is ns=<index>;i=<number> or ns=<index>;s=<name>, with an index \
             to 65535, a number to 4294967295, and a name that is not empty",
        ),
        (
            Value::Integer(1001),
            "a node ID is a string, not an integer",
        ),
    ] {
        let reads = vec![block(
            20,
            "read",
            &["plant.spike"],
            &[(30, "node_id", node_id.clone())],
        )];
        assert_eq!(
            parse(&config(string(ADDRESS), reads)),
            Err(vec![refused("opcua.bad-node-id", 31, message, fix)]),
            "{node_id:?}"
        );
    }
}

#[test]
fn refuses_a_read_with_a_bad_label_or_no_node_id() {
    let reads = vec![
        block(20, "read", &[], &[(30, "node_id", string("ns=3;i=1"))]),
        block(
            40,
            "read",
            &["a", "b"],
            &[(50, "node_id", string("ns=3;i=2"))],
        ),
        block(60, "read", &["plant spike"], &[]),
    ];
    let label = "Name the channel of the block, such as `read \"plant.spike\"`";
    let name = "plant spike".parse::<Name>().expect_err("not a name");
    assert_eq!(
        parse(&config(string(ADDRESS), reads)),
        Err(vec![
            refused(
                "document.label-count",
                20,
                "the `read` block has no labels, and it takes 1 label",
                label,
            ),
            refused(
                "document.label-count",
                42,
                "the `read` block has 2 labels, and it takes 1 label",
                label,
            ),
            refused("document.bad-name", 61, &name.to_string(), name.fix()),
            refused(
                "document.missing-attribute",
                60,
                "the `read` block has no `node_id`",
                "Add a `node_id` attribute with the node of the channel, such as \
                 \"ns=3;s=SpikeData\"",
            ),
        ])
    );
}

#[test]
fn refuses_a_channel_in_two_reads() {
    let reads = vec![
        read(20, "plant.spike", "ns=3;s=SpikeData"),
        read(40, "plant.dip", "ns=3;i=1001"),
        block(
            60,
            "read",
            &["plant.spike"],
            &[(70, "node_id", Value::Integer(1))],
        ),
    ];
    let mut expected = refused(
        "opcua.repeated-channel",
        61,
        "the connector reads `plant.spike` in another `read` block",
        "Remove one of the two `read` blocks",
    );
    expected.notes.push(Note {
        span: at(21).expect("a span"),
        text: "the first `read` block of `plant.spike`".into(),
    });
    let bad = refused(
        "opcua.bad-node-id",
        71,
        "a node ID is a string, not an integer",
        "Write a node ID such as \"ns=3;s=SpikeData\" or \"ns=3;i=1001\"",
    );
    assert_eq!(
        parse(&config(string(ADDRESS), reads)),
        Err(vec![bad, expected])
    );
}

#[test]
fn refuses_each_key_and_block_it_does_not_take() {
    let mut reads = vec![block(
        20,
        "read",
        &["plant.spike"],
        &[
            (30, "node_id", string("ns=3;i=1")),
            (34, "sampling", string("1s")),
        ],
    )];
    reads.push(block(40, "subscribe", &[], &[]));
    let mut config = config(string(ADDRESS), reads);
    config.attributes = Map::new(
        config
            .attributes
            .iter()
            .cloned()
            .chain([Attribute {
                key: "security".into(),
                key_span: at(60),
                value: document::value::Value {
                    kind: string("none"),
                    span: at(61),
                },
            }])
            .collect(),
    )
    .expect("unique keys");
    assert_eq!(
        parse(&config),
        Err(vec![
            refused(
                "document.unknown-attribute",
                34,
                "`sampling` is not an attribute of the `read` block",
                "Use `node_id`, or remove it",
            ),
            refused(
                "document.unknown-attribute",
                60,
                "`security` is not an attribute of the connector",
                "Use `address`, `rate`, `certificate`, `key`, `trust`, or \
                 `unencrypted`, or remove it",
            ),
            refused(
                "document.unknown-block",
                40,
                "the connector cannot hold the `subscribe` block",
                "Use `read`, or remove it",
            ),
        ])
    );
}

/// `file("<path>")`, with its name at `offset` and its argument one byte after.
fn file(offset: u32, path: &str) -> Value {
    Value::Call(document::value::Call {
        function: "file".into(),
        function_span: at(offset),
        arguments: vec![document::value::Value {
            kind: string(path),
            span: at(offset + 1),
        }],
    })
}

/// The source `file("<path>")`.
fn source(path: &str) -> secret::Source {
    secret::Source::read(&document::value::Value {
        kind: file(0, path),
        span: None,
    })
    .expect("a source")
}

/// The config of the connector at [`ADDRESS`] with `attributes` added.
fn plus(attributes: &[(u32, &str, Value)]) -> Document {
    let mut config = config(string(ADDRESS), Vec::new());
    let added = document(attributes, Vec::new()).attributes;
    let attributes = config.attributes.iter().chain(added.iter()).cloned();
    config.attributes = Map::new(attributes.collect()).expect("unique keys");
    config
}

/// The paths of the sources of [`reads_the_security_of_each_subset_of_its_attributes`].
const CERTIFICATE: &str = "/etc/opcua/node.der";
const KEY: &str = "/etc/opcua/node-key.der";
const TRUST: &str = "/etc/opcua/plc.der";

/// What `parse` gives for the subset of `certificate`, `key`, `trust`, and
/// `unencrypted = true` whose bits 0 to 3 `has` holds.
fn expected(has: impl Fn(u8) -> bool) -> Result<Option<Security>, Vec<Diagnostic>> {
    if !has(0) {
        let refused = |at, key: &str| {
            refused(
                "document.missing-attribute",
                at,
                "the connector has no `certificate`",
                &format!(
                    "Add a `certificate` with the certificate of the node, or \
                     remove `{key}`"
                ),
            )
        };
        let missing: Vec<_> =
            [(1, 30, "key"), (2, 40, "trust"), (3, 50, "unencrypted")]
                .into_iter()
                .filter(|(bit, _, _)| has(*bit))
                .map(|(_, at, key)| refused(at, key))
                .collect();
        if missing.is_empty() {
            Ok(None)
        } else {
            Err(missing)
        }
    } else if has(1) && has(2) {
        Ok(Some(Security {
            certificate: source(CERTIFICATE),
            key: source(KEY),
            trust: source(TRUST),
            mode: if has(3) {
                client::Mode::Sign
            } else {
                client::Mode::SignAndEncrypt
            },
        }))
    } else {
        let mut missing = Vec::new();
        if !has(1) {
            missing.push(refused(
                "document.missing-attribute",
                20,
                "the connector has no `key`",
                "Add a `key` with the private key of the certificate, or remove \
                 `certificate`",
            ));
        }
        if !has(2) {
            missing.push(refused(
                "document.missing-attribute",
                20,
                "the connector has no `trust`",
                "Add a `trust` with the certificate of the server or of its CA, or \
                 remove `certificate`",
            ));
        }
        Err(missing)
    }
}

/// Each of the 16 subsets of `certificate`, `key`, `trust`, and `unencrypted = true`
/// gives its security, or one diagnostic for each attribute that is missing or has no
/// `certificate`.
#[test]
fn reads_the_security_of_each_subset_of_its_attributes() {
    for subset in 0..16_u8 {
        let has = |bit: u8| subset & (1 << bit) != 0;
        let all = [
            (20, "certificate", file(21, CERTIFICATE)),
            (30, "key", file(31, KEY)),
            (40, "trust", file(41, TRUST)),
            (50, "unencrypted", Value::Bool(true)),
        ];
        let attributes: Vec<_> = (0..4)
            .filter(|bit| has(*bit))
            .map(|bit| all[usize::from(bit)].clone())
            .collect();
        let parsed = parse(&plus(&attributes)).map(|config| config.security);
        let expected = expected(has);
        assert_eq!(parsed, expected, "subset {subset:04b}");
    }
}

#[test]
fn reads_unencrypted_false_as_sign_and_encrypt() {
    let config = plus(&[
        (20, "certificate", file(21, "/c.der")),
        (30, "key", file(31, "/k.der")),
        (40, "trust", file(41, "/t.der")),
        (50, "unencrypted", Value::Bool(false)),
    ]);
    let security = parse(&config).map(|config| config.security.map(|s| s.mode));
    assert_eq!(security, Ok(Some(client::Mode::SignAndEncrypt)));
}

#[test]
fn refuses_a_credential_that_is_not_a_source_and_an_unencrypted_that_is_no_bool() {
    let config = plus(&[
        (20, "certificate", file(21, "/c.der")),
        (30, "key", string("-----BEGIN PRIVATE KEY-----")),
        (40, "trust", file(41, "plc.der")),
        (50, "unencrypted", string("true")),
    ]);
    assert_eq!(
        parse(&config),
        Err(vec![
            refused(
                "secret.source",
                31,
                "a secret takes where its value comes from, never the value",
                "Write `env(\"NAME\")`, `file(\"/absolute/path\")`, or \
                 `secret(\"name\")`",
            ),
            refused(
                "secret.source",
                41,
                "the argument of `file` is not an absolute path with no `..` and no \
                 NUL",
                "Write the whole path from the root, such as \
                 `file(\"/run/secrets/influx_token\")`",
            ),
            refused(
                "opcua.bad-unencrypted",
                51,
                "`unencrypted` is a bool, not a string",
                "Write `unencrypted = true`, or remove it",
            ),
        ])
    );
}

/// The block that the kind gives for `tag` with `access`, as `plant.spike`.
fn point(tag: &str, access: Option<Access>) -> Result<Vec<Block>, Vec<Diagnostic>> {
    let facts = Facts {
        access,
        ..Facts::default()
    };
    connector::kind::Kind::block(&crate::Kind, tag, &facts, &spike())
}

fn spike() -> Name {
    "plant.spike".parse().expect("a name")
}

#[test]
fn gives_a_read_block_for_each_point_that_a_connector_can_read() {
    for access in [None, Some(Access::Read), Some(Access::ReadWrite)] {
        assert_eq!(
            point("ns=3;i=1001", access),
            Ok(vec![read(0, "plant.spike", "ns=3;i=1001")]),
            "{access:?}"
        );
    }
}

#[test]
fn refuses_the_block_of_a_tag_that_is_not_a_node_id() {
    assert_eq!(
        point("ns=3;g=09087e75-8e5e-499b-954f-f2a9603db28a", None),
        Err(vec![Diagnostic::new(
            Code::new("opcua.bad-node-id"),
            None,
            "the tag \"ns=3;g=09087e75-8e5e-499b-954f-f2a9603db28a\": a GUID (g=) or \
             opaque (b=) key is not supported: give a number (i=) or a name (s=)"
                .into(),
            "Write a node ID such as \"ns=3;s=SpikeData\" or \"ns=3;i=1001\"".into(),
        )])
    );
}

fn tag(block: &Block) -> Option<String> {
    connector::kind::Kind::tag(&crate::Kind, block)
}

#[test]
fn gives_the_tag_of_a_read_in_the_form_of_a_browse() {
    assert_eq!(
        tag(&read(0, "plant.spike", "ns=0;i=2258")),
        Some("i=2258".into())
    );
    assert_eq!(
        tag(&read(0, "plant.spike", "ns=3;s=a")),
        Some("ns=3;s=a".into())
    );
}

#[test]
fn gives_no_tag_for_another_block_or_a_bad_node_id() {
    let mut other = read(0, "plant.spike", "ns=3;i=1001");
    other.keyword = "write".into();
    assert_eq!(tag(&other), None);
    assert_eq!(tag(&read(0, "plant.spike", "ns=3;g=x")), None);
    let mut empty = read(0, "plant.spike", "ns=3;i=1001");
    empty.body = Document::default();
    assert_eq!(tag(&empty), None);
}

proptest! {
    /// The tag of each block that the kind gives for a tag of a browse is that tag.
    #[test]
    fn gives_the_tag_of_each_block_that_it_gives(node in crate::node::tests::node()) {
        let text = node.to_string();
        for block in point(&text, None).expect("a block") {
            prop_assert_eq!(tag(&block), Some(text.clone()));
        }
    }

    /// Each text of a node that `Node` reads parses as the `node_id` of a read.
    #[test]
    fn reads_each_node_id_that_node_reads(
        namespace in any::<u16>(),
        key in prop_oneof![
            any::<u32>().prop_map(|number| format!("i={number}")),
            "[^\0]{1,32}".prop_map(|name| format!("s={name}")),
        ],
    ) {
        let text = format!("ns={namespace};{key}");
        let node: Node = text.parse().expect("a node");
        let config = config(string(ADDRESS), vec![read(20, "plant.spike", &text)]);
        let reads = parse(&config).map(|config| config.reads);
        prop_assert_eq!(
            reads,
            Ok(vec![Read { channel: "plant.spike".parse().expect("a name"), node }])
        );
    }

    /// The block of each node that `Node` reads is a read of that node, which
    /// `parse` reads back.
    #[test]
    fn parses_the_block_of_each_node(node in crate::node::tests::node()) {
        let text = node.to_string();
        let blocks = point(&text, None).expect("a block");
        prop_assert_eq!(&blocks, &vec![read(0, "plant.spike", &text)]);
        let reads = parse(&config(string(ADDRESS), blocks)).map(|config| config.reads);
        prop_assert_eq!(
            reads,
            Ok(vec![Read { channel: "plant.spike".parse().expect("a name"), node }])
        );
    }
}

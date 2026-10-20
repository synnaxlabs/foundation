//! The config of one `opcua` connector, read from its document.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::ffi::CString;

use connector::{kind, secret};
use document::diagnostic::{Code, Diagnostic, Note};
use document::schema::{self, Body, Presence};
use document::value::Value;
use document::{Attribute, Block, Document, Label, Map, read};
use types::name::Name;
use types::time::Rate;

use crate::client;
use crate::node::Node;

#[cfg(test)]
mod tests;

const BAD_ADDRESS: Code = Code::new("opcua.bad-address");
const BAD_NODE_ID: Code = Code::new("opcua.bad-node-id");
const BAD_UNENCRYPTED: Code = Code::new("opcua.bad-unencrypted");
const NO_CERTIFICATE: Code = Code::new("opcua.no-certificate");
const NO_KEY: Code = Code::new("opcua.no-key");
const NO_TRUST: Code = Code::new("opcua.no-trust");
const REPEATED_CHANNEL: Code = Code::new("opcua.repeated-channel");

/// The scheme of each address.
const SCHEME: &str = "opc.tcp://";

/// One opcua connector's config: the endpoint of the server, the rate, and the node of
/// each channel. Only [`Kind::parse`](connector::kind::Kind::parse) of
/// [`Kind`](crate::Kind) makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The endpoint URL, which open62541 reads as a host and an optional port and
    /// path.
    pub(crate) address: CString,
    pub(crate) rate: Rate,
    /// Each of another channel. None until an import adds them.
    pub(crate) reads: Vec<Read>,
    /// With no `certificate`, `None`, and the channel has the mode `None`.
    pub(crate) security: Option<Security>,
}

/// The sources of the credentials of a secure channel, and its mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Security {
    pub(crate) certificate: secret::Source,
    pub(crate) key: secret::Source,
    pub(crate) trust: secret::Source,
    /// `Sign` with `unencrypted = true`, else `SignAndEncrypt`.
    pub(crate) mode: client::Mode,
}

/// The body of the config.
pub(crate) const BODY: Body = Body {
    summary: "Polls the nodes of an OPC UA server at a rate.",
    attributes: &[
        schema::Attribute {
            name: "address",
            value: "an `opc.tcp://` URL",
            presence: Presence::Required,
            summary: "The endpoint of the server.",
        },
        schema::Attribute {
            name: "rate",
            value: "a rate, such as \"10Hz\"",
            presence: Presence::Required,
            summary: "The reads per second.",
        },
        schema::Attribute {
            name: "certificate",
            value: "a secret",
            presence: Presence::Default("no secure channel"),
            summary: "The DER or PEM certificate of the node, with an RSA key and its \
                      application URI in the subject alternative name. With it, the \
                      channel signs and encrypts.",
        },
        schema::Attribute {
            name: "key",
            value: "a secret",
            presence: Presence::Optional,
            summary: "The DER or PEM private key of `certificate`, with no password. \
                      A connector with `certificate` needs it.",
        },
        schema::Attribute {
            name: "trust",
            value: "a secret",
            presence: Presence::Optional,
            summary: "The DER certificate, or one or more PEM certificates, of the \
                      server or of the CA that issued it. A connector with \
                      `certificate` needs it.",
        },
        schema::Attribute {
            name: "unencrypted",
            value: "a bool",
            presence: Presence::Default("false"),
            summary: "With `true`, the channel signs and does not encrypt, for a \
                      server with no encrypted endpoint. Only a connector with \
                      `certificate` takes it.",
        },
    ],
    blocks: &[schema::Block {
        keyword: "read",
        label: Some("the channel"),
        forms: &[READ],
    }],
    examples: &[EXAMPLE, SECURE],
};

/// A connector to one server.
const EXAMPLE: &str = "\
address = \"opc.tcp://plc:4840\"
rate    = \"10Hz\"

read \"plant.pressure\" {
  node_id = \"ns=3;s=Pressure\"
}
";

/// A connector over a channel that signs and encrypts.
const SECURE: &str = "\
address     = \"opc.tcp://plc:4840\"
rate        = \"10Hz\"
certificate = file(\"/etc/foundation/opcua/node.der\")
key         = secret(\"plant.opcua_key\")
trust       = file(\"/etc/foundation/opcua/plc.der\")
";

/// The body of a `read` block.
const READ: Body = Body {
    summary: "The node of one channel. Zero or more.",
    attributes: &[schema::Attribute {
        name: "node_id",
        value: "a node ID, such as \"ns=3;s=SpikeData\"",
        presence: Presence::Required,
        summary: "The node whose value the channel holds.",
    }],
    blocks: &[],
    examples: &[],
};

/// The node of one channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Read {
    pub(crate) channel: Name,
    pub(crate) node: Node,
}

/// Reads `address`, `rate`, the attributes of [`security`], and zero or more `read`
/// blocks, and no other key or block.
pub(crate) fn parse(config: &Document) -> Result<Config, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let address = endpoint(config);
    let rate = read::required(
        config,
        kind::NOUN,
        None,
        "rate",
        read::rate,
        "Add a `rate` attribute with the reads per second, such as \"10Hz\"".into(),
    );
    let address = address.map_err(|error| diagnostics.push(error));
    let rate = rate.map_err(|error| diagnostics.push(error));
    let reads = reads(config, &mut diagnostics);
    let security = security(config).map_err(|errors| diagnostics.extend(errors));
    diagnostics.extend(read::unknown(config, kind::NOUN, &BODY));
    match (address, rate, security) {
        (Ok(address), Ok(rate), Ok(security)) if diagnostics.is_empty() => Ok(Config {
            address,
            rate,
            reads,
            security,
        }),
        _ => Err(diagnostics),
    }
}

/// Reads `certificate`, `key`, and `trust`, each a source that
/// [`secret::Source::read`] takes, and the bool `unencrypted`. Gives `None` with none
/// of them.
///
/// # Errors
///
/// Each diagnostic of a source, `opcua.bad-unencrypted` for an `unencrypted` that is
/// not a bool, `opcua.no-certificate` for each of `key`, `trust`, and `unencrypted`
/// with no `certificate`, and `opcua.no-key` and `opcua.no-trust` for a
/// `certificate` with no `key` or no `trust`.
pub(crate) fn security(config: &Document) -> Result<Option<Security>, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let attributes = &config.attributes;
    let mut source = |key| {
        let attribute = attributes.get(key)?;
        let read = secret::Source::read(&attribute.value);
        Some(read.map_err(|error| diagnostics.push(error)))
    };
    let certificate = source("certificate");
    let key = source("key");
    let trust = source("trust");
    let unencrypted = attributes.get("unencrypted").map(|attribute| {
        unencrypted(&attribute.value).map_err(|error| diagnostics.push(error))
    });
    let Some(certificate) = certificate else {
        diagnostics.extend(no_certificate(attributes));
        return if diagnostics.is_empty() {
            Ok(None)
        } else {
            Err(diagnostics)
        };
    };
    let at = attributes
        .get("certificate")
        .and_then(|attribute| attribute.key_span);
    let needs = [
        (
            key.is_none(),
            NO_KEY,
            "key",
            "the private key of the certificate",
        ),
        (
            trust.is_none(),
            NO_TRUST,
            "trust",
            "the certificate of the server or of its CA",
        ),
    ];
    for (missing, code, name, what) in needs {
        if missing {
            diagnostics.push(Diagnostic::new(
                code,
                at,
                format!("the connector has a `certificate` but no `{name}`"),
                format!("Add a `{name}` with {what}, or remove `certificate`"),
            ));
        }
    }
    match (certificate, key, trust, unencrypted.unwrap_or(Ok(false))) {
        (Ok(certificate), Some(Ok(key)), Some(Ok(trust)), Ok(unencrypted)) => {
            let mode = if unencrypted {
                client::Mode::Sign
            } else {
                client::Mode::SignAndEncrypt
            };
            Ok(Some(Security {
                certificate,
                key,
                trust,
                mode,
            }))
        }
        _ => Err(diagnostics),
    }
}

/// The `opcua.no-certificate` error of each of `key`, `trust`, and `unencrypted` in
/// `attributes`.
fn no_certificate(attributes: &Map) -> Vec<Diagnostic> {
    let present = ["key", "trust", "unencrypted"]
        .into_iter()
        .filter_map(|name| Some((name, attributes.get(name)?)));
    present
        .map(|(name, attribute)| {
            Diagnostic::new(
                NO_CERTIFICATE,
                attribute.key_span,
                format!("the connector has a `{name}` but no `certificate`"),
                format!(
                    "Add a `certificate` with the certificate of the node, or remove \
                     `{name}`"
                ),
            )
        })
        .collect()
}

/// Reads the bool of `unencrypted`.
fn unencrypted(value: &Value) -> Result<bool, Diagnostic> {
    match value.kind {
        document::value::Kind::Bool(unencrypted) => Ok(unencrypted),
        ref kind => Err(Diagnostic::new(
            BAD_UNENCRYPTED,
            value.span,
            format!("`unencrypted` is a bool, not {}", kind.noun()),
            "Write `unencrypted = true`, or remove it".into(),
        )),
    }
}

/// Reads `address` of `config`, the endpoint of the server, as [`parse`] does.
pub(crate) fn endpoint(config: &Document) -> Result<CString, Diagnostic> {
    read::required(
        config,
        kind::NOUN,
        None,
        "address",
        address,
        "Add an `address` attribute with the endpoint of the server, such as \
         \"opc.tcp://plc:4840\""
            .into(),
    )
}

/// Reads an endpoint URL: `opc.tcp://`, a host, and an optional port and path, as
/// open62541 reads it at the connect, with no NUL.
fn address(value: &Value) -> Result<CString, Diagnostic> {
    let refused = |message| {
        Diagnostic::new(
            BAD_ADDRESS,
            value.span,
            message,
            "Write an address such as \"opc.tcp://plc:4840\"".into(),
        )
    };
    let document::value::Kind::String(text) = &value.kind else {
        return Err(refused(format!(
            "an address is a string, not {}",
            value.kind.noun()
        )));
    };
    if !text.starts_with(SCHEME) {
        return Err(refused(format!(
            "the address {text:?} does not start with {SCHEME}"
        )));
    }
    if !client::endpoint(text) {
        return Err(refused(format!(
            "the address {text:?} has no host, or a port that is not 0 to 65535"
        )));
    }
    CString::new(text.as_bytes())
        .map_err(|_nul| refused("the address holds a NUL".into()))
}

/// Reads the `read` blocks of `config`, and adds each problem to `diagnostics`.
fn reads(config: &Document, diagnostics: &mut Vec<Diagnostic>) -> Vec<Read> {
    const OF: &str = "the `read` block";
    let blocks = config.blocks.iter();
    let blocks: Vec<&Block> =
        blocks.filter(|block| &*block.keyword == "read").collect();
    let mut channels: BTreeMap<Name, &Block> = BTreeMap::new();
    let mut reads = Vec::with_capacity(blocks.len());
    for block in blocks {
        let fix = "Name the channel of the block, such as `read \"plant.spike\"`";
        let channel = read::labels::<1>(block, fix.into())
            .and_then(|[label]| read::label(label))
            .map_err(|error| diagnostics.push(error));
        let node = read::required(
            &block.body,
            OF,
            block.keyword_span,
            "node_id",
            node,
            "Add a `node_id` attribute with the node of the channel, such as \
             \"ns=3;s=SpikeData\""
                .into(),
        );
        let node = node.map_err(|error| diagnostics.push(error));
        diagnostics.extend(read::unknown(&block.body, OF, &READ));
        let Ok(channel) = channel else { continue };
        match channels.entry(channel) {
            Entry::Occupied(first) => {
                diagnostics.push(repeated(first.key(), block, first.get()));
            }
            Entry::Vacant(slot) => {
                if let Ok(node) = node {
                    reads.push(Read {
                        channel: slot.key().clone(),
                        node,
                    });
                }
                slot.insert(block);
            }
        }
    }
    reads
}

/// The `read` block of the node `tag` as the channel `channel`, as
/// [`Kind::block`](connector::kind::Kind::block) asks: `read "<channel>" { node_id =
/// "<tag>" }`.
pub(crate) fn block(tag: &str, channel: &Name) -> Result<Vec<Block>, Vec<Diagnostic>> {
    let value = Value {
        kind: document::value::Kind::String(tag.into()),
        span: None,
    };
    node(&value).map_err(|mut error| {
        error.message = format!("the tag {tag:?}: {}", error.message);
        vec![error]
    })?;
    Ok(vec![read_block(channel, value)])
}

/// The node of the `read` block `block` as a browse gives its tag, as
/// [`Kind::tag`](connector::kind::Kind::tag) asks.
pub(crate) fn tag(block: &Block) -> Option<String> {
    if &*block.keyword != "read" {
        return None;
    }
    let node_id = block.body.attributes.get("node_id")?;
    node(&node_id.value).ok().map(|node| node.to_string())
}

/// `read "<channel>" { node_id = <node_id> }`.
fn read_block(channel: &Name, node_id: Value) -> Block {
    let node_id = Attribute {
        key: "node_id".into(),
        key_span: None,
        value: node_id,
    };
    Block {
        keyword: "read".into(),
        keyword_span: None,
        labels: vec![Label {
            text: channel.as_str().into(),
            span: None,
        }],
        body: Document {
            attributes: Map::new(vec![node_id]).expect("invariant: one key"),
            blocks: Vec::new(),
        },
        span: None,
    }
}

/// Reads a node ID, such as `"ns=3;s=SpikeData"`.
fn node(value: &Value) -> Result<Node, Diagnostic> {
    let refused = |message| {
        Diagnostic::new(
            BAD_NODE_ID,
            value.span,
            message,
            "Write a node ID such as \"ns=3;s=SpikeData\" or \"ns=3;i=1001\"".into(),
        )
    };
    let document::value::Kind::String(text) = &value.kind else {
        return Err(refused(format!(
            "a node ID is a string, not {}",
            value.kind.noun()
        )));
    };
    text.parse()
        .map_err(|error: crate::node::Error| refused(error.to_string()))
}

/// The diagnostic of a `read` block of `channel` after the block `first` of it.
fn repeated(channel: &Name, block: &Block, first: &Block) -> Diagnostic {
    let label = |block: &Block| block.labels.first().and_then(|label| label.span);
    let mut diagnostic = Diagnostic::new(
        REPEATED_CHANNEL,
        label(block),
        format!("the connector reads `{channel}` in another `read` block"),
        "Remove one of the two `read` blocks".into(),
    );
    diagnostic.notes.extend(label(first).map(|span| Note {
        span,
        text: format!("the first `read` block of `{channel}`"),
    }));
    diagnostic
}

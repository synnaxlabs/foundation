use std::collections::BTreeMap;

use document::diagnostic::{Code, Diagnostic};
use document::value::{Kind, Value};
use document::{Document, Map, Span};
use spec::channel;
use spec::definition::Definition;
use spec::time::Peers;
use spec::unit::Unit;
use types::name::{Name, Selector, Written};

const PRIVATE_KEY: Code = Code::new("config.private-key");
/// Text that only a private key holds: the OpenSSH, PEM, and RFC 4716 forms, a `.ppk`
/// file of `PuTTYgen`, the base64 start of an OpenSSH body, and the base64 of the
/// algorithm and key header (`30 05 06 03 2B 65 70 04 22 04 20`) of an Ed25519
/// PKCS #8 body of any version, with no header lines. The length of the body moves
/// that header, so it has one mark for each of its offsets modulo 3. Each base64 mark
/// is whole 3-byte groups at an offset of whole groups, so the bytes around it do not
/// change it.
const MARKS: [&str; 6] = [
    "PRIVATE KEY",
    "PuTTY-User-Key-File",
    "b3BlbnNzaC1rZXktdjEA",
    "BQYDK2VwBCIE",
    "MAUGAytlcAQi",
    "BgMrZXAEIgQg",
];

/// `config.private-key` at each string of `documents` that holds a private key, in
/// any block, label, key, or value at any depth. The alarms are in the order of their
/// [`document::Source`], then in source order, and quote none of the text.
pub(crate) fn alarms(documents: &[Document]) -> Vec<Diagnostic> {
    let mut alarms = Vec::new();
    for document in documents {
        in_document(document, &mut alarms);
    }
    crate::sort(&mut alarms);
    alarms
}

/// `config.private-key` for each string of `definitions` that holds a private key: a
/// tree key, a pattern, a name, a unit, or a string of a connector config. The alarms
/// are in name order. Only an alarm in a connector config has a span: the span that
/// the config holds.
pub(crate) fn in_definitions(
    definitions: &BTreeMap<Name, Definition>,
) -> Vec<Diagnostic> {
    let mut alarms = Vec::new();
    for (key, definition) in definitions {
        let mut names = vec![key];
        let mut selectors = Vec::new();
        let mut unit = None;
        match definition {
            Definition::Access(policy) => {
                selectors.extend([policy.subjects(), policy.select()]);
            }
            Definition::Connector(connector) => {
                names.extend([connector.kind(), connector.node()]);
                in_document(connector.config().document(), &mut alarms);
            }
            Definition::Region(delegation) => names.extend(delegation.initial_voters()),
            Definition::NodeSettings(policy) => selectors.push(policy.select()),
            Definition::Compression(policy) => selectors.push(&policy.select),
            Definition::Placement(policy) => {
                selectors.push(policy.select());
                names.extend(crate::placement::nodes(policy));
            }
            Definition::Time(policy) => {
                selectors.push(policy.select());
                if let Peers::Listed(peers) = policy.peers() {
                    names.extend(peers);
                }
            }
            Definition::Channel(channel) => {
                if let channel::Kind::Data(data) = &channel.kind {
                    unit = data.unit();
                }
            }
            Definition::Retention(policy) => selectors.push(policy.select()),
            Definition::Subject(_) => {}
        }
        let patterns = selectors.into_iter().flat_map(Selector::written);
        let patterns = patterns.map(|pattern| match pattern {
            Written::Include(text) | Written::Exclude(text) => text,
        });
        let texts = names.into_iter().map(Name::as_str).chain(patterns);
        for text in texts.chain(unit.map(Unit::as_str)) {
            in_text(text, None, &mut alarms);
        }
    }
    alarms
}

fn in_document(document: &Document, alarms: &mut Vec<Diagnostic>) {
    in_map(&document.attributes, alarms);
    for block in &document.blocks {
        in_text(&block.keyword, block.keyword_span, alarms);
        for label in &block.labels {
            in_text(&label.text, label.span, alarms);
        }
        in_document(&block.body, alarms);
    }
}

fn in_map(map: &Map, alarms: &mut Vec<Diagnostic>) {
    for attribute in map.iter() {
        in_text(&attribute.key, attribute.key_span, alarms);
        in_value(&attribute.value, alarms);
    }
}

fn in_value(value: &Value, alarms: &mut Vec<Diagnostic>) {
    match &value.kind {
        Kind::String(text) => in_text(text, value.span, alarms),
        Kind::Reference(name) => in_text(name.as_str(), value.span, alarms),
        Kind::List(items) => items.iter().for_each(|item| in_value(item, alarms)),
        Kind::Map(map) => in_map(map, alarms),
        Kind::Call(call) => {
            in_text(&call.function, call.function_span, alarms);
            call.arguments
                .iter()
                .for_each(|item| in_value(item, alarms));
        }
        Kind::Bool(_) | Kind::Integer(_) | Kind::Float(_) => {}
    }
}

fn in_text(text: &str, span: Option<Span>, alarms: &mut Vec<Diagnostic>) {
    if MARKS.iter().any(|mark| text.contains(mark)) {
        alarms.push(Diagnostic::new(
            PRIVATE_KEY,
            span,
            "the value is a private key, which must never be in a file".into(),
            "Remove the private key from this file now, and use the one line of its \
             `.pub` file"
                .into(),
        ));
    }
}

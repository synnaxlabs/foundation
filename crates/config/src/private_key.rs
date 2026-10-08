use document::diagnostic::{Code, Diagnostic};
use document::value::{Kind, Value};
use document::{Document, Map, Span};

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

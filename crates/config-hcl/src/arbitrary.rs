//! Documents that HCL can hold, and a writer that gives their HCL text.

use std::fmt::Write as _;

use document::value::{Call, Float, Kind, Value};
use document::{Attribute, Block, Document, Label, Map};
use proptest::prelude::*;

fn identifier() -> impl Strategy<Value = String> {
    "[a-z_][a-z0-9_-]{0,6}"
}

fn text() -> impl Strategy<Value = String> {
    prop_oneof!["\\PC{0,8}", "[\"\\\\$%{}\n\r\t\u{1}a]{0,8}"]
}

fn name() -> impl Strategy<Value = Kind> {
    "[a-z_][a-z0-9_]{0,4}(\\.@?[a-z0-9_]{1,5}){0,2}"
        .prop_filter("a keyword is not a reference", |name| {
            !matches!(name.as_str(), "true" | "false" | "null" | "for")
        })
        .prop_map(|name| Kind::Reference(name.parse().unwrap()))
}

fn value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        any::<bool>().prop_map(Kind::Bool),
        any::<i128>().prop_map(Kind::Integer),
        any::<f64>()
            .prop_filter_map("finite", Float::new)
            .prop_map(Kind::Float),
        text().prop_map(|text| Kind::String(text.into())),
        name(),
    ];
    let leaf = leaf.prop_map(|kind| Value { kind, span: None });
    leaf.prop_recursive(3, 16, 4, |inner| {
        let key = prop_oneof![identifier(), text()];
        let kind = prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Kind::List),
            prop::collection::btree_map(key, inner.clone(), 0..4)
                .prop_map(|entries| Kind::Map(map(entries))),
            (identifier(), prop::collection::vec(inner, 0..4)).prop_map(
                |(function, arguments)| {
                    Kind::Call(Call {
                        function: function.into(),
                        function_span: None,
                        arguments,
                    })
                }
            ),
        ];
        kind.prop_map(|kind| Value { kind, span: None })
    })
}

fn map(entries: impl IntoIterator<Item = (String, Value)>) -> Map {
    let attributes = entries
        .into_iter()
        .map(|(key, value)| Attribute {
            key: key.into(),
            key_span: None,
            value,
        })
        .collect();
    Map::new(attributes).unwrap()
}

fn attributes() -> impl Strategy<Value = Map> {
    prop::collection::btree_map(identifier(), value(), 0..4).prop_map(map)
}

/// A Document that HCL can hold: keys in a body are identifiers.
pub(crate) fn document() -> impl Strategy<Value = Document> {
    let leaf = attributes().prop_map(|attributes| Document {
        attributes,
        blocks: Vec::new(),
    });
    leaf.prop_recursive(3, 12, 3, |inner| {
        let label = text().prop_map(|text| Label {
            text: text.into(),
            span: None,
        });
        let block = (identifier(), prop::collection::vec(label, 0..3), inner).prop_map(
            |(keyword, labels, body)| Block {
                keyword: keyword.into(),
                keyword_span: None,
                labels,
                body,
                span: None,
            },
        );
        (attributes(), prop::collection::vec(block, 0..3))
            .prop_map(|(attributes, blocks)| Document { attributes, blocks })
    })
}

/// Writes `document` as HCL.
pub(crate) fn write(document: &Document) -> String {
    let mut out = String::new();
    body(&mut out, document, 0);
    out
}

fn body(out: &mut String, document: &Document, indent: usize) {
    let pad = "  ".repeat(indent);
    for attribute in document.attributes.iter() {
        out.push_str(&pad);
        out.push_str(&attribute.key);
        out.push_str(" = ");
        value_text(out, &attribute.value);
        out.push('\n');
    }
    for block in &document.blocks {
        out.push_str(&pad);
        out.push_str(&block.keyword);
        for label in &block.labels {
            out.push(' ');
            quoted(out, &label.text);
        }
        out.push_str(" {\n");
        body(out, &block.body, indent.saturating_add(1));
        out.push_str(&pad);
        out.push_str("}\n");
    }
}

fn value_text(out: &mut String, value: &Value) {
    match &value.kind {
        Kind::Bool(b) => write!(out, "{b}").unwrap(),
        Kind::Integer(n) => write!(out, "{n}").unwrap(),
        Kind::Float(float) => write!(out, "{:?}", float.get()).unwrap(),
        Kind::String(text) if text.ends_with('\n') && !text.contains("\r\n") => {
            heredoc(out, text);
        }
        Kind::String(text) => quoted(out, text),
        Kind::Reference(name) => out.push_str(name.as_str()),
        Kind::List(items) => {
            out.push('[');
            values(out, items);
            out.push(']');
        }
        Kind::Map(map) => {
            out.push('{');
            for (i, attribute) in map.iter().enumerate() {
                // The new line after a heredoc ends its entry.
                if i == 0 {
                    out.push(' ');
                } else if !out.ends_with('\n') {
                    out.push_str(", ");
                }
                if identifier_text(&attribute.key) {
                    out.push_str(&attribute.key);
                } else {
                    quoted(out, &attribute.key);
                }
                out.push_str(" = ");
                value_text(out, &attribute.value);
            }
            out.push_str(" }");
        }
        Kind::Call(call) => {
            out.push_str(&call.function);
            out.push('(');
            values(out, &call.arguments);
            out.push(')');
        }
    }
}

fn values(out: &mut String, values: &[Value]) {
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        value_text(out, value);
    }
}

fn identifier_text(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// Writes `text`, which ends in `\n` and has no `\r\n`, as a heredoc and a new line.
fn heredoc(out: &mut String, text: &str) {
    let mut marker = String::from("EOT");
    while text
        .lines()
        .any(|line| line.trim_matches([' ', '\t']) == marker)
    {
        marker.push('_');
    }
    writeln!(out, "<<{marker}").unwrap();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if matches!(c, '$' | '%') && chars.peek() == Some(&'{') {
            out.push(c);
        }
    }
    out.push_str(&marker);
    out.push('\n');
}

fn quoted(out: &mut String, text: &str) {
    out.push('"');
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '$' | '%' if chars.peek() == Some(&'{') => {
                out.push(c);
                out.push(c);
            }
            c if c.is_control() => write!(out, "\\u{:04x}", u32::from(c)).unwrap(),
            c => out.push(c),
        }
    }
    out.push('"');
}

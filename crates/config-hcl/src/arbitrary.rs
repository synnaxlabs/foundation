//! Documents that HCL can hold.

use document::value::{Call, Float, Kind, Value};
use document::{Attribute, Block, Document, Label, Map};
use proptest::prelude::*;

fn identifier() -> impl Strategy<Value = String> {
    "[a-z_éü][a-z0-9_\u{301}éü-]{0,6}"
}

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        "\\PC{0,8}",
        "[\"\\\\$%{}\n\r\t\u{1}\u{b}\u{3000} aEOT]{0,8}",
        // Long enough that a list of a few folds.
        "[a-zé ]{20,40}",
    ]
}

fn name() -> impl Strategy<Value = Kind> {
    "[a-z_][a-z0-9_]{0,4}(\\.@?[a-z0-9_]{1,5}){0,2}"
        .prop_filter("a keyword is not a reference", |name| {
            !matches!(name.as_str(), "true" | "false" | "null")
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
        let key = prop_oneof![identifier(), text(), "-?(0|[1-9][0-9]{0,40})"];
        let kind = prop_oneof![
            prop::collection::vec(inner.clone(), 0..4)
                .prop_filter("HCL reads `[for` as a for expression", |items| {
                    !items.first().is_some_and(|item| match &item.kind {
                        Kind::Reference(name) => name.segments().next() == Some("for"),
                        Kind::Call(call) => &*call.function == "for",
                        _ => false,
                    })
                })
                .prop_map(Kind::List),
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

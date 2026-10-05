//! Random documents for property tests, and a way to rebuild one with edits.

use proptest::prelude::*;

use crate::value::{Call, Float, Kind, Value};
use crate::{Attribute, Block, Document, Label, Map, Position, Source, Span};

fn position(offset: u32) -> Position {
    Position {
        offset,
        line: 0,
        column: offset,
    }
}

fn span() -> impl Strategy<Value = Option<Span>> {
    prop::option::of((0u32..1000, 0u32..1000).prop_map(|(a, b)| {
        Span::new(Source(1), position(a.min(b)), position(a.max(b))).unwrap()
    }))
}

fn map(value: impl Strategy<Value = Value>) -> impl Strategy<Value = Map> {
    let entries = prop::collection::btree_map("[a-z]{1,4}", (value, span()), 0..4);
    entries.prop_map(|entries| {
        let attributes =
            entries
                .into_iter()
                .map(|(key, (value, key_span))| Attribute {
                    key: key.into(),
                    key_span,
                    value,
                });
        Map::new(attributes.collect()).unwrap()
    })
}

fn value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        any::<bool>().prop_map(Kind::Bool),
        any::<i128>().prop_map(Kind::Integer),
        any::<f64>()
            .prop_filter_map("finite", Float::new)
            .prop_map(Kind::Float),
        "\\PC{0,8}".prop_map(|s| Kind::String(s.into())),
        "[a-z]{1,4}(\\.[a-z]{1,4}){0,2}"
            .prop_map(|s| Kind::Reference(s.parse().unwrap())),
    ];
    let leaf = (leaf, span()).prop_map(|(kind, span)| Value { kind, span });
    leaf.prop_recursive(3, 24, 4, |inner| {
        let call = (
            "[a-z]{1,6}",
            span(),
            prop::collection::vec(inner.clone(), 0..3),
        );
        let kind = prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Kind::List),
            map(inner).prop_map(Kind::Map),
            call.prop_map(|(function, function_span, arguments)| {
                Kind::Call(Call {
                    function: function.into(),
                    function_span,
                    arguments,
                })
            }),
        ];
        (kind, span()).prop_map(|(kind, span)| Value { kind, span })
    })
}

fn label() -> impl Strategy<Value = Label> {
    ("\\PC{0,8}", span()).prop_map(|(text, span)| Label {
        text: text.into(),
        span,
    })
}

/// Random documents, with and without spans.
pub(crate) fn document() -> impl Strategy<Value = Document> {
    let leaf = map(value()).prop_map(|attributes| Document {
        attributes,
        blocks: Vec::new(),
    });
    leaf.prop_recursive(2, 8, 3, |inner| {
        let block = (
            "[a-z]{1,8}",
            span(),
            prop::collection::vec(label(), 0..3),
            inner,
            span(),
        );
        let block =
            block.prop_map(|(keyword, keyword_span, labels, body, span)| Block {
                keyword: keyword.into(),
                keyword_span,
                labels,
                body,
                span,
            });
        (map(value()), prop::collection::vec(block, 0..3))
            .prop_map(|(attributes, blocks)| Document { attributes, blocks })
    })
}

/// What [`rebuild`] does to each part, in document order. `text` sees each key,
/// keyword, label, and function name. `leaf` sees each value that is not a list,
/// map, or call.
pub(crate) struct Edits<'a> {
    pub(crate) span: &'a mut dyn FnMut(Option<Span>) -> Option<Span>,
    pub(crate) text: &'a mut dyn FnMut(&str) -> Box<str>,
    pub(crate) leaf: &'a mut dyn FnMut(&Kind) -> Kind,
}

/// Rebuilds `document` with `edits` applied to every part.
pub(crate) fn rebuild(document: &Document, edits: &mut Edits<'_>) -> Document {
    let attributes = rebuild_map(&document.attributes, edits);
    let mut blocks = Vec::new();
    for block in &document.blocks {
        let keyword = (edits.text)(&block.keyword);
        let keyword_span = (edits.span)(block.keyword_span);
        let mut labels = Vec::new();
        for label in &block.labels {
            labels.push(Label {
                text: (edits.text)(&label.text),
                span: (edits.span)(label.span),
            });
        }
        blocks.push(Block {
            keyword,
            keyword_span,
            labels,
            body: rebuild(&block.body, edits),
            span: (edits.span)(block.span),
        });
    }
    Document { attributes, blocks }
}

fn rebuild_map(map: &Map, edits: &mut Edits<'_>) -> Map {
    let mut attributes = Vec::new();
    for attribute in map.iter() {
        attributes.push(Attribute {
            key: (edits.text)(&attribute.key),
            key_span: (edits.span)(attribute.key_span),
            value: rebuild_value(&attribute.value, edits),
        });
    }
    Map::new(attributes).unwrap()
}

fn rebuild_value(value: &Value, edits: &mut Edits<'_>) -> Value {
    let kind = match &value.kind {
        Kind::List(items) => Kind::List(
            items
                .iter()
                .map(|item| rebuild_value(item, edits))
                .collect(),
        ),
        Kind::Map(map) => Kind::Map(rebuild_map(map, edits)),
        Kind::Call(call) => {
            let function = (edits.text)(&call.function);
            let function_span = (edits.span)(call.function_span);
            let arguments = call
                .arguments
                .iter()
                .map(|argument| rebuild_value(argument, edits))
                .collect();
            Kind::Call(Call {
                function,
                function_span,
                arguments,
            })
        }
        leaf => (edits.leaf)(leaf),
    };
    Value {
        kind,
        span: (edits.span)(value.span),
    }
}

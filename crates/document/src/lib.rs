//! Defines the syntax-neutral Document with source positions, diagnostics, and shared
//! value readers.
//!
//! Each syntax is a front end that reads files into a [`Document`] and writes one back.
//! SDK code may build a document directly. Files hold data only, so a document has no
//! expressions, variables, or loops.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

mod block;
mod map;
mod span;
pub mod value;

use std::fmt;

pub use block::{Block, Label};
pub use map::{Attribute, Map};
pub use span::{Position, Source, Span};

/// Attributes and blocks. A file, the body of a block, and a connector's config in the
/// spec are each one document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    /// The attributes, by key.
    pub attributes: Map,
    /// The blocks, in the order the producer gave them.
    pub blocks: Vec<Block>,
}

/// A document that is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Two attributes in one map have the same key.
    DuplicateKey {
        /// The key.
        key: Box<str>,
        /// Where the first attribute's key is.
        first: Option<Span>,
        /// Where the second attribute's key is.
        second: Option<Span>,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateKey { key, .. } => write!(
                f,
                "the key {key:?} appears two times. Remove one, or give it a different \
                 key"
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Call, Float, Kind, Value};
    use proptest::prelude::*;

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
        prop::collection::btree_map("[a-z]{1,4}", (value, span()), 0..4).prop_map(
            |entries| {
                let attributes =
                    entries
                        .into_iter()
                        .map(|(key, (value, key_span))| Attribute {
                            key: key.into(),
                            key_span,
                            value,
                        });
                Map::new(attributes.collect()).unwrap()
            },
        )
    }

    fn value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            any::<bool>().prop_map(Kind::Bool),
            any::<i128>().prop_map(Kind::Integer),
            any::<f64>()
                .prop_filter_map("finite", Float::new)
                .prop_map(Kind::Float),
            "[a-z ]{0,8}".prop_map(|s| Kind::String(s.into())),
            "[a-z]{1,4}(\\.[a-z]{1,4}){0,2}"
                .prop_map(|s| Kind::Reference(s.parse().unwrap())),
        ];
        let leaf = (leaf, span()).prop_map(|(kind, span)| Value { kind, span });
        leaf.prop_recursive(3, 24, 4, |inner| {
            let kind = prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Kind::List),
                map(inner.clone()).prop_map(Kind::Map),
                ("[a-z]{1,6}", prop::collection::vec(inner, 0..3)).prop_map(
                    |(function, arguments)| Kind::Call(Call {
                        function: function.into(),
                        arguments,
                    })
                ),
            ];
            (kind, span()).prop_map(|(kind, span)| Value { kind, span })
        })
    }

    fn label() -> impl Strategy<Value = Label> {
        ("[a-z_.]{0,8}", span()).prop_map(|(text, span)| Label {
            text: text.into(),
            span,
        })
    }

    fn document() -> impl Strategy<Value = Document> {
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
            )
                .prop_map(|(keyword, keyword_span, labels, body, span)| Block {
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

    /// Rebuilds `document` with `f` applied to every span.
    fn map_spans(
        document: &Document,
        f: &mut impl FnMut(Option<Span>) -> Option<Span>,
    ) -> Document {
        Document {
            attributes: map_spans_in_map(&document.attributes, f),
            blocks: document
                .blocks
                .iter()
                .map(|block| Block {
                    keyword: block.keyword.clone(),
                    keyword_span: f(block.keyword_span),
                    labels: block
                        .labels
                        .iter()
                        .map(|label| Label {
                            text: label.text.clone(),
                            span: f(label.span),
                        })
                        .collect(),
                    body: map_spans(&block.body, f),
                    span: f(block.span),
                })
                .collect(),
        }
    }

    fn map_spans_in_map(
        map: &Map,
        f: &mut impl FnMut(Option<Span>) -> Option<Span>,
    ) -> Map {
        let attributes = map.iter().map(|attribute| Attribute {
            key: attribute.key.clone(),
            key_span: f(attribute.key_span),
            value: map_spans_in_value(&attribute.value, f),
        });
        Map::new(attributes.collect()).unwrap()
    }

    fn map_spans_in_value(
        value: &Value,
        f: &mut impl FnMut(Option<Span>) -> Option<Span>,
    ) -> Value {
        let kind = match &value.kind {
            Kind::List(items) => Kind::List(
                items
                    .iter()
                    .map(|item| map_spans_in_value(item, f))
                    .collect(),
            ),
            Kind::Map(map) => Kind::Map(map_spans_in_map(map, f)),
            Kind::Call(call) => Kind::Call(Call {
                function: call.function.clone(),
                arguments: call
                    .arguments
                    .iter()
                    .map(|argument| map_spans_in_value(argument, f))
                    .collect(),
            }),
            leaf => leaf.clone(),
        };
        Value {
            kind,
            span: f(value.span),
        }
    }

    fn string(text: &str) -> Value {
        Value {
            kind: Kind::String(text.into()),
            span: None,
        }
    }

    fn connector(url: Value) -> Document {
        let attributes = vec![Attribute {
            key: "url".into(),
            key_span: None,
            value: url,
        }];
        let block = Block {
            keyword: "connector".into(),
            keyword_span: None,
            labels: vec![Label {
                text: "opcua".into(),
                span: None,
            }],
            body: Document {
                attributes: Map::new(attributes).unwrap(),
                blocks: Vec::new(),
            },
            span: None,
        };
        Document {
            attributes: Map::default(),
            blocks: vec![block],
        }
    }

    mod eq {
        use super::*;

        proptest! {
            #[test]
            fn does_not_read_spans(document in document()) {
                let stripped = map_spans(&document, &mut |_| None);
                let mut left = Vec::new();
                map_spans(&stripped, &mut |span| {
                    left.extend(span);
                    span
                });
                prop_assert_eq!(left, []);
                prop_assert_eq!(&stripped, &document);
            }
        }

        #[test]
        fn reads_a_keyword() {
            let mut other = connector(string("a"));
            other.blocks[0].keyword = "channel".into();
            assert_ne!(other, connector(string("a")));
        }

        #[test]
        fn reads_a_label() {
            let mut other = connector(string("a"));
            other.blocks[0].labels[0].text = "modbus".into();
            assert_ne!(other, connector(string("a")));
        }

        #[test]
        fn reads_a_key() {
            let renamed = Attribute {
                key: "address".into(),
                key_span: None,
                value: string("a"),
            };
            let mut other = connector(string("a"));
            other.blocks[0].body.attributes = Map::new(vec![renamed]).unwrap();
            assert_ne!(other, connector(string("a")));
        }

        #[test]
        fn reads_a_value() {
            assert_ne!(connector(string("a")), connector(string("b")));
        }

        #[test]
        fn reads_a_value_inside_a_list() {
            let list = |text| Value {
                kind: Kind::List(vec![string(text)]),
                span: None,
            };
            assert_ne!(connector(list("a")), connector(list("b")));
        }
    }

    mod error {
        use super::*;

        #[test]
        fn duplicate_key_names_the_key_and_the_fix() {
            let err = Error::DuplicateKey {
                key: "url".into(),
                first: None,
                second: None,
            };
            assert_eq!(
                err.to_string(),
                "the key \"url\" appears two times. Remove one, or give it a different key"
            );
        }
    }
}

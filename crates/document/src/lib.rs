//! Defines the syntax-neutral Document with source positions, diagnostics, shared
//! value readers, and its canonical encoding.
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

#[cfg(test)]
mod arbitrary;
mod block;
pub mod diagnostic;
pub mod encoding;
mod map;
mod span;
pub mod value;

use std::fmt;

pub use block::{Block, Label};
pub use map::{Attribute, Map};
pub use span::{Position, Source, Span};

use diagnostic::{Code, Diagnostic, Note};

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

impl From<&Error> for Diagnostic {
    fn from(error: &Error) -> Self {
        match error {
            Error::DuplicateKey { key, first, second } => {
                let mut diagnostic = Self::new(
                    DUPLICATE_KEY,
                    *second,
                    format!("the key {key:?} repeats an earlier key"),
                    "Remove it, or give it a different key".into(),
                );
                diagnostic.notes.extend(first.map(|span| Note {
                    span,
                    text: "the earlier key".into(),
                }));
                diagnostic
            }
        }
    }
}

const DUPLICATE_KEY: Code = Code::new("document.duplicate-key");

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Diagnostic::from(self), f)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::arbitrary::{Edits, document, rebuild};
    use crate::value::{Float, Kind};
    use proptest::prelude::*;

    fn spans(document: &Document) -> Vec<Span> {
        let mut found = Vec::new();
        rebuild(
            document,
            &mut Edits {
                span: &mut |span| {
                    found.extend(span);
                    span
                },
                text: &mut |text| text.into(),
                leaf: &mut Clone::clone,
            },
        );
        found
    }

    /// Counts the keys, keywords, labels, function names, and leaf values.
    fn parts(document: &Document) -> usize {
        let seen = Cell::new(0usize);
        let count = || seen.set(seen.get().saturating_add(1));
        rebuild(
            document,
            &mut Edits {
                span: &mut |span| span,
                text: &mut |text| {
                    count();
                    text.into()
                },
                leaf: &mut |kind| {
                    count();
                    kind.clone()
                },
            },
        );
        seen.get()
    }

    /// Changes the part at `target`, counted as [`parts`] counts them.
    fn change(document: &Document, target: usize) -> Document {
        let seen = Cell::new(0usize);
        let hit = || {
            let n = seen.get();
            seen.set(n.saturating_add(1));
            n == target
        };
        rebuild(
            document,
            &mut Edits {
                span: &mut |span| span,
                text: &mut |text| {
                    if hit() {
                        format!("{text}_").into()
                    } else {
                        text.into()
                    }
                },
                leaf: &mut |kind| if hit() { changed(kind) } else { kind.clone() },
            },
        )
    }

    fn changed(kind: &Kind) -> Kind {
        match kind {
            Kind::Bool(b) => Kind::Bool(!b),
            Kind::Integer(n) => Kind::Integer(n.wrapping_add(1)),
            Kind::Float(f) => {
                let other = if f.get() > 1.0 { 1.0 } else { 2.0 };
                Kind::Float(Float::new(other).unwrap())
            }
            Kind::String(text) => Kind::String(format!("{text}_").into()),
            Kind::Reference(name) => {
                Kind::Reference(format!("{name}.z").parse().unwrap())
            }
            Kind::List(_) | Kind::Map(_) | Kind::Call(_) => {
                panic!("not a leaf: {kind:?}")
            }
        }
    }

    mod eq {
        use super::*;

        proptest! {
            #[test]
            fn does_not_read_spans(document in document()) {
                let stripped = rebuild(
                    &document,
                    &mut Edits {
                        span: &mut |_| None,
                        text: &mut |text| text.into(),
                        leaf: &mut Clone::clone,
                    },
                );
                prop_assert_eq!(spans(&stripped), []);
                prop_assert_eq!(&stripped, &document);
            }

            #[test]
            fn reads_every_part(
                document in document(),
                target in any::<prop::sample::Index>(),
            ) {
                let parts = parts(&document);
                prop_assume!(parts > 0);
                prop_assert_ne!(change(&document, target.index(parts)), document);
            }
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
                Diagnostic::from(&err),
                Diagnostic::new(
                    Code::new("document.duplicate-key"),
                    None,
                    "the key \"url\" repeats an earlier key".into(),
                    "Remove it, or give it a different key".into(),
                )
            );
            assert_eq!(
                err.to_string(),
                "the key \"url\" repeats an earlier key. Remove it, or give it a \
                 different key"
            );
        }

        #[test]
        fn duplicate_key_points_at_both_keys() {
            let at = |offset| Position {
                offset,
                line: 0,
                column: offset,
            };
            let span = |start, end| Span::new(Source(0), at(start), at(end)).unwrap();
            let err = Error::DuplicateKey {
                key: "a".into(),
                first: Some(span(0, 1)),
                second: Some(span(6, 7)),
            };
            let mut expected = Diagnostic::new(
                Code::new("document.duplicate-key"),
                Some(span(6, 7)),
                "the key \"a\" repeats an earlier key".into(),
                "Remove it, or give it a different key".into(),
            );
            expected.notes.push(Note {
                span: span(0, 1),
                text: "the earlier key".into(),
            });
            assert_eq!(Diagnostic::from(&err), expected);
        }
    }
}

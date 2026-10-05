//! Reads HCL files as Documents.
//!
//! Files hold data only: booleans, numbers, strings, lists, objects, names, function
//! calls, attributes, and blocks. Each other HCL form is an error.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

#[cfg(test)]
mod arbitrary;
mod lex;
mod parse;

use std::fmt;

use document::Span;
use types::name;

pub use parse::read;

/// A problem in HCL text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The text breaks the grammar.
    Syntax {
        /// What is there instead.
        span: Span,
        /// What must come there.
        expected: Expected,
    },
    /// An HCL form that a file cannot hold.
    Form {
        /// Where the form starts.
        span: Span,
        /// The form.
        form: Form,
    },
    /// A reference that is not a valid name.
    Name {
        /// Where the reference is.
        span: Span,
        /// Why the name is not valid.
        error: name::Error,
    },
    /// An integer outside `i128`, or a float that an `f64` cannot hold: one past the
    /// largest, or one that rounds to zero from digits that are not all zero.
    Number {
        /// Where the number is.
        span: Span,
    },
    /// A string escape that HCL does not have.
    Escape {
        /// Where the escape is.
        span: Span,
    },
    /// Nesting deeper than [`document::encoding::DEPTH_MAX`].
    TooDeep {
        /// Where the first level past the limit starts.
        span: Span,
    },
    /// A document that is not valid, such as a key that repeats an earlier key.
    Document(document::Error),
    /// A file with more bytes than a span can count.
    TooLarge {
        /// The length of the file.
        bytes: usize,
    },
}

impl Error {
    /// Where the problem starts, to sort problems in source order.
    fn offset(&self) -> u32 {
        match self {
            Self::Syntax { span, .. }
            | Self::Form { span, .. }
            | Self::Name { span, .. }
            | Self::Number { span }
            | Self::Escape { span }
            | Self::TooDeep { span } => span.start().offset,
            Self::Document(document::Error::DuplicateKey { second, .. }) => {
                second
                    .expect("invariant: the reader gives each key a span")
                    .start()
                    .offset
            }
            Self::TooLarge { .. } => 0,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax { expected, .. } => {
                write!(f, "the file needs {expected} here")
            }
            Self::Form { form, .. } => write!(f, "{form}"),
            Self::Name { error, .. } => {
                write!(f, "the reference is not a valid name: {error}")
            }
            Self::Number { .. } => write!(
                f,
                "the number is out of range. Use an integer that fits in 128 bits, \
                 or a float that fits in 64 bits"
            ),
            Self::Escape { .. } => write!(
                f,
                "the string has an escape that HCL does not have. Use `\\n`, `\\r`, \
                 `\\t`, `\\\"`, `\\\\`, `\\uNNNN`, or `\\UNNNNNNNN`"
            ),
            Self::TooDeep { .. } => write!(
                f,
                "the file nests deeper than {} levels. Make it flatter",
                document::encoding::DEPTH_MAX
            ),
            Self::Document(error) => write!(f, "{error}"),
            Self::TooLarge { bytes } => write!(
                f,
                "the file has {bytes} bytes, and the limit is {}. Split it into \
                 smaller files",
                u32::MAX
            ),
        }
    }
}

impl std::error::Error for Error {}

/// An HCL form that a file cannot hold. Each message has its fix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// `null`.
    Null,
    /// An interpolation `${` or a directive `%{` in a string.
    Template,
}

impl fmt::Display for Form {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Null => {
                "`null` does not exist in Foundation files. Remove the attribute to \
                 use its default"
            }
            Self::Template => {
                "templates do not exist in Foundation files. Write `$${` or `%%{` for \
                 the text `${` or `%{`"
            }
        })
    }
}

/// What the grammar needs at a syntax error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expected {
    /// A key, a block keyword, or the end of the body.
    Item,
    /// `=`, a label, or `{` after a name in a body.
    AttributeOrBlock,
    /// `=` after the key in a one-line block.
    Equals,
    /// A label or `{` after a block's labels.
    BlockStart,
    /// `}` after the attribute in a one-line block.
    BlockEnd,
    /// A value.
    Value,
    /// A new line after an attribute or a block.
    Newline,
    /// `,` or `]` in a list.
    ListEnd,
    /// A key or `}` in an object or a one-line block.
    Key,
    /// `=` or `:` after a key in an object.
    ObjectEquals,
    /// `,`, a new line, or `}` in an object.
    ObjectEnd,
    /// `,` or `)` in a call.
    ArgumentsEnd,
    /// `"` at the end of a string.
    Quote,
    /// `*/` at the end of a comment.
    CommentEnd,
}

impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Item => "a key, a block, or the end of the body",
            Self::AttributeOrBlock => "`=`, a label, or `{` after the name",
            Self::Equals => "`=` after the key",
            Self::BlockStart => "a label or `{`",
            Self::BlockEnd => "`}` to end the one-line block",
            Self::Value => "a value",
            Self::Newline => "a new line",
            Self::ListEnd => "`,` or `]`",
            Self::Key => "a key or `}`",
            Self::ObjectEquals => "`=` or `:` after the key",
            Self::ObjectEnd => "`,`, a new line, or `}`",
            Self::ArgumentsEnd => "`,` or `)`",
            Self::Quote => "`\"` to end the string",
            Self::CommentEnd => "`*/` to end the comment",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use document::{Position, Source};

    fn span() -> Span {
        let at = Position {
            offset: 0,
            line: 0,
            column: 0,
        };
        Span::new(Source(0), at, at).unwrap()
    }

    #[test]
    fn each_expected_has_its_message() {
        let cases = [
            (Expected::Item, "a key, a block, or the end of the body"),
            (
                Expected::AttributeOrBlock,
                "`=`, a label, or `{` after the name",
            ),
            (Expected::Equals, "`=` after the key"),
            (Expected::BlockStart, "a label or `{`"),
            (Expected::BlockEnd, "`}` to end the one-line block"),
            (Expected::Value, "a value"),
            (Expected::Newline, "a new line"),
            (Expected::ListEnd, "`,` or `]`"),
            (Expected::Key, "a key or `}`"),
            (Expected::ObjectEquals, "`=` or `:` after the key"),
            (Expected::ObjectEnd, "`,`, a new line, or `}`"),
            (Expected::ArgumentsEnd, "`,` or `)`"),
            (Expected::Quote, "`\"` to end the string"),
            (Expected::CommentEnd, "`*/` to end the comment"),
        ];
        for (expected, phrase) in cases {
            let error = Error::Syntax {
                span: span(),
                expected,
            };
            assert_eq!(error.to_string(), format!("the file needs {phrase} here"));
        }
    }

    #[test]
    fn each_form_has_its_fix() {
        let cases = [
            (
                Form::Null,
                "`null` does not exist in Foundation files. Remove the attribute to \
                 use its default",
            ),
            (
                Form::Template,
                "templates do not exist in Foundation files. Write `$${` or `%%{` for \
                 the text `${` or `%{`",
            ),
        ];
        for (form, message) in cases {
            let error = Error::Form { span: span(), form };
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn too_large_names_the_limit() {
        let error = Error::TooLarge {
            bytes: 4_294_967_296,
        };
        assert_eq!(
            error.to_string(),
            "the file has 4294967296 bytes, and the limit is 4294967295. Split it into \
             smaller files"
        );
    }
}

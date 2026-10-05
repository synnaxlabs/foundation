//! Reads HCL files as Documents, and writes Documents as HCL files.
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
mod write;

use std::fmt;

use document::Span;
use document::diagnostic::{Code, Diagnostic};
use document::encoding::{DEPTH_MAX, TooDeep};
use types::name::{self, Name};

pub use parse::read;
pub use write::write;

/// A problem in HCL text, or a part of a Document that HCL text cannot hold.
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
        /// The token that shows the form, such as the operator or `?`.
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
        /// An empty span at the start of the file.
        span: Span,
        /// The length of the file.
        bytes: usize,
    },
    /// A part of a Document that HCL text cannot hold, from [`write`].
    Unwritable {
        /// Where the part is, or `None` for a Document with no spans.
        span: Option<Span>,
        /// The part.
        part: Unwritable,
    },
}

impl Error {
    /// Where the problem starts, to sort the problems from `read` in source order.
    fn offset(&self) -> u32 {
        match self {
            Self::Syntax { span, .. }
            | Self::Form { span, .. }
            | Self::Name { span, .. }
            | Self::Number { span }
            | Self::Escape { span }
            | Self::TooDeep { span }
            | Self::TooLarge { span, .. } => span.start().offset,
            Self::Document(document::Error::DuplicateKey { second, .. }) => {
                second
                    .expect("invariant: the reader gives each key a span")
                    .start()
                    .offset
            }
            Self::Unwritable { .. } => {
                unreachable!("invariant: read gives no Unwritable")
            }
        }
    }
}

/// Gives each problem a diagnostic with a stable `hcl.*` code, or `document`'s own
/// diagnostic for [`Error::Document`].
impl From<&Error> for Diagnostic {
    fn from(error: &Error) -> Self {
        match error {
            Error::Syntax { span, expected } => Self::new(
                SYNTAX,
                Some(*span),
                format!("the file needs {expected} here"),
                "Write it here, or correct the text before it".into(),
            ),
            Error::Form { span, form } => form.diagnostic(*span),
            Error::Name { span, .. } => Self::new(
                NAME,
                Some(*span),
                "the reference is not a valid name".into(),
                format!(
                    "Use segments of ASCII letters, digits, `_`, and `-`, split by \
                     dots, with at most {} bytes in all",
                    Name::MAX_BYTES
                ),
            ),
            Error::Number { span } => Self::new(
                NUMBER,
                Some(*span),
                "the number is out of range".into(),
                "Use an integer that fits in 128 bits, or a float that fits in 64 bits"
                    .into(),
            ),
            Error::Escape { span } => Self::new(
                ESCAPE,
                Some(*span),
                "the string has an escape that HCL does not have".into(),
                "Use `\\n`, `\\r`, `\\t`, `\\\"`, `\\\\`, `\\uNNNN`, or \
                 `\\UNNNNNNNN`"
                    .into(),
            ),
            Error::TooDeep { span } => Self::new(
                TOO_DEEP,
                Some(*span),
                format!("the file nests deeper than {DEPTH_MAX} levels"),
                "Make it flatter".into(),
            ),
            Error::Document(error) => Self::from(error),
            Error::TooLarge { span, bytes } => Self::new(
                TOO_LARGE,
                Some(*span),
                format!("the file has {bytes} bytes, and the limit is {}", u32::MAX),
                "Split it into smaller files".into(),
            ),
            Error::Unwritable { span, part } => part.diagnostic(*span),
        }
    }
}

const SYNTAX: Code = Code::new("hcl.syntax");
const NULL: Code = Code::new("hcl.null");
const TEMPLATE: Code = Code::new("hcl.template");
const OPERATOR: Code = Code::new("hcl.operator");
const CONDITIONAL: Code = Code::new("hcl.conditional");
const FOR: Code = Code::new("hcl.for");
const INDEX: Code = Code::new("hcl.index");
const SPLAT: Code = Code::new("hcl.splat");
const PARENTHESES: Code = Code::new("hcl.parentheses");
const NAMESPACE: Code = Code::new("hcl.namespace");
const EXPANSION: Code = Code::new("hcl.expansion");
const NUMBER_KEY: Code = Code::new("hcl.number-key");
const NAME: Code = Code::new("hcl.name");
const NUMBER: Code = Code::new("hcl.number");
const ESCAPE: Code = Code::new("hcl.escape");
const TOO_DEEP: Code = Code::new("hcl.too-deep");
const TOO_LARGE: Code = Code::new("hcl.too-large");
const UNWRITABLE_KEY: Code = Code::new("hcl.unwritable-key");
const UNWRITABLE_KEYWORD: Code = Code::new("hcl.unwritable-keyword");
const UNWRITABLE_FUNCTION: Code = Code::new("hcl.unwritable-function");
const UNWRITABLE_REFERENCE: Code = Code::new("hcl.unwritable-reference");

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Diagnostic::from(self), f)
    }
}

impl std::error::Error for Error {}

/// An HCL form that a file cannot hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// `null`.
    Null,
    /// An interpolation `${` or a directive `%{` in a string or a heredoc.
    Template,
    /// An operator, such as `+`, `==`, `!`, or `-` before a value that is not a
    /// number.
    Operator,
    /// A conditional, such as `a ? b : c`.
    Conditional,
    /// A `for` expression, such as `[for x in xs : x]`.
    For,
    /// An index or an attribute access after a value, such as `a[0]` or `f().b`.
    Index,
    /// A splat, such as `a[*].b` or `a.*.b`.
    Splat,
    /// Parentheses around a value.
    Parentheses,
    /// A function in a namespace, such as `provider::aws::arn_parse(x)`.
    Namespace,
    /// An argument expanded with `...`, such as `f(xs...)`.
    Expansion,
    /// An object key that is a number HCL rounds: one with a fraction or an exponent,
    /// or an integer of more than 154 digits, such as `{ 1.5 = 1 }`.
    NumberKey,
}

impl Form {
    fn diagnostic(self, span: Span) -> Diagnostic {
        let (code, message, fix) = match self {
            Self::Null => (
                NULL,
                "`null` does not exist in Foundation files",
                "Remove the attribute to use its default",
            ),
            Self::Template => (
                TEMPLATE,
                "templates do not exist in Foundation files",
                "Write `$${` or `%%{` for the text `${` or `%{`",
            ),
            Self::Operator => (
                OPERATOR,
                "operators do not exist in Foundation files",
                "Write the result as a value",
            ),
            Self::Conditional => (
                CONDITIONAL,
                "conditionals do not exist in Foundation files",
                "Write the value that applies",
            ),
            Self::For => (
                FOR,
                "`for` expressions do not exist in Foundation files",
                "Write each item",
            ),
            Self::Index => (
                INDEX,
                "indexes and attribute access do not exist in Foundation files",
                "Write the value itself",
            ),
            Self::Splat => (
                SPLAT,
                "splats do not exist in Foundation files",
                "Write each value",
            ),
            Self::Parentheses => (
                PARENTHESES,
                "parentheses do not exist in Foundation files",
                "Remove them",
            ),
            Self::Namespace => (
                NAMESPACE,
                "function namespaces do not exist in Foundation files",
                "Call the function by its name only",
            ),
            Self::Expansion => (
                EXPANSION,
                "argument expansion does not exist in Foundation files",
                "Write each argument",
            ),
            Self::NumberKey => (
                NUMBER_KEY,
                "number keys with a fraction, an exponent, or more than 154 \
                 digits do not exist in Foundation files",
                "Write the key as a quoted string",
            ),
        };
        Diagnostic::new(code, Some(span), message.into(), fix.into())
    }
}

/// A part of a Document that no HCL text reads back as the same part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unwritable {
    /// A key of a body that is not an identifier, such as `my key`. HCL has no quoted
    /// key in a body. A key in a map value can be any text.
    Key,
    /// A block keyword that is not an identifier.
    Keyword,
    /// A function name that is not an identifier.
    Function,
    /// A name that HCL does not read as a reference, such as `true`, `null`, `7a`, or
    /// `-a`.
    Reference,
    /// A block or a value nested deeper than [`document::encoding::DEPTH_MAX`], which
    /// [`read`] refuses.
    Depth,
}

impl Unwritable {
    fn diagnostic(self, span: Option<Span>) -> Diagnostic {
        let (code, message, fix) = match self {
            Self::Key => (
                UNWRITABLE_KEY,
                "a key of a body must be an identifier, such as `retry_limit`",
                "Rename the key, or move it into a map value",
            ),
            Self::Keyword => (
                UNWRITABLE_KEYWORD,
                "a block keyword must be an identifier, such as `channel`",
                "Rename the keyword",
            ),
            Self::Function => (
                UNWRITABLE_FUNCTION,
                "a function name must be an identifier, such as `secret`",
                "Rename the function",
            ),
            Self::Reference => (
                UNWRITABLE_REFERENCE,
                "the name does not read as a reference in HCL",
                "Start it with a letter, `_`, or `@`, and do not use `true`, `false`, \
                 or `null`",
            ),
            Self::Depth => return Diagnostic::from(&TooDeep { span }),
        };
        Diagnostic::new(code, span, message.into(), fix.into())
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
    /// A marker, such as `EOT`, and a new line after `<<` or `<<-`.
    HeredocStart,
    /// The marker on a line of its own at the end of a heredoc.
    HeredocEnd,
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
            Self::HeredocStart => {
                "a marker, such as `EOT`, and a new line to start the heredoc"
            }
            Self::HeredocEnd => "the marker on a line of its own to end the heredoc",
            Self::CommentEnd => "`*/` to end the comment",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use document::diagnostic::Note;
    use document::{Position, Source};

    fn span(offset: u32) -> Span {
        let at = Position {
            offset,
            line: 0,
            column: offset,
        };
        Span::new(Source(0), at, at).unwrap()
    }

    fn check(error: &Error, code: &'static str, message: &str, fix: &str) {
        let expected =
            Diagnostic::new(Code::new(code), Some(span(7)), message.into(), fix.into());
        assert_eq!(Diagnostic::from(error), expected, "{error:?}");
        assert_eq!(error.to_string(), format!("{message}. {fix}"), "{error:?}");
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
            (
                Expected::HeredocStart,
                "a marker, such as `EOT`, and a new line to start the heredoc",
            ),
            (
                Expected::HeredocEnd,
                "the marker on a line of its own to end the heredoc",
            ),
            (Expected::CommentEnd, "`*/` to end the comment"),
        ];
        for (expected, phrase) in cases {
            let error = Error::Syntax {
                span: span(7),
                expected,
            };
            check(
                &error,
                "hcl.syntax",
                &format!("the file needs {phrase} here"),
                "Write it here, or correct the text before it",
            );
        }
    }

    const FORMS: [(Form, &str, &str, &str); 11] = [
        (
            Form::Null,
            "hcl.null",
            "`null` does not exist in Foundation files",
            "Remove the attribute to use its default",
        ),
        (
            Form::Template,
            "hcl.template",
            "templates do not exist in Foundation files",
            "Write `$${` or `%%{` for the text `${` or `%{`",
        ),
        (
            Form::Operator,
            "hcl.operator",
            "operators do not exist in Foundation files",
            "Write the result as a value",
        ),
        (
            Form::Conditional,
            "hcl.conditional",
            "conditionals do not exist in Foundation files",
            "Write the value that applies",
        ),
        (
            Form::For,
            "hcl.for",
            "`for` expressions do not exist in Foundation files",
            "Write each item",
        ),
        (
            Form::Index,
            "hcl.index",
            "indexes and attribute access do not exist in Foundation files",
            "Write the value itself",
        ),
        (
            Form::Splat,
            "hcl.splat",
            "splats do not exist in Foundation files",
            "Write each value",
        ),
        (
            Form::Parentheses,
            "hcl.parentheses",
            "parentheses do not exist in Foundation files",
            "Remove them",
        ),
        (
            Form::Namespace,
            "hcl.namespace",
            "function namespaces do not exist in Foundation files",
            "Call the function by its name only",
        ),
        (
            Form::Expansion,
            "hcl.expansion",
            "argument expansion does not exist in Foundation files",
            "Write each argument",
        ),
        (
            Form::NumberKey,
            "hcl.number-key",
            "number keys with a fraction, an exponent, or more than 154 digits do \
             not exist in Foundation files",
            "Write the key as a quoted string",
        ),
    ];

    #[test]
    fn each_form_has_its_code_and_fix() {
        for (form, code, message, fix) in FORMS {
            let error = Error::Form {
                span: span(7),
                form,
            };
            check(&error, code, message, fix);
        }
    }

    #[test]
    fn each_error_has_its_code_and_fix() {
        let cases = [
            (
                Error::Name {
                    span: span(7),
                    error: "a.@".parse::<Name>().unwrap_err(),
                },
                "hcl.name",
                "the reference is not a valid name".to_owned(),
                "Use segments of ASCII letters, digits, `_`, and `-`, split by dots, \
                 with at most 255 bytes in all",
            ),
            (
                Error::Number { span: span(7) },
                "hcl.number",
                "the number is out of range".to_owned(),
                "Use an integer that fits in 128 bits, or a float that fits in 64 bits",
            ),
            (
                Error::Escape { span: span(7) },
                "hcl.escape",
                "the string has an escape that HCL does not have".to_owned(),
                "Use `\\n`, `\\r`, `\\t`, `\\\"`, `\\\\`, `\\uNNNN`, or `\\UNNNNNNNN`",
            ),
            (
                Error::TooDeep { span: span(7) },
                "hcl.too-deep",
                "the file nests deeper than 64 levels".to_owned(),
                "Make it flatter",
            ),
            (
                Error::TooLarge {
                    span: span(7),
                    bytes: 4_294_967_296,
                },
                "hcl.too-large",
                "the file has 4294967296 bytes, and the limit is 4294967295".to_owned(),
                "Split it into smaller files",
            ),
        ];
        for (error, code, message, fix) in cases {
            check(&error, code, &message, fix);
        }
    }

    #[test]
    fn each_unwritable_part_has_its_code_and_fix() {
        let cases = [
            (
                Unwritable::Key,
                "hcl.unwritable-key",
                "a key of a body must be an identifier, such as `retry_limit`",
                "Rename the key, or move it into a map value",
            ),
            (
                Unwritable::Keyword,
                "hcl.unwritable-keyword",
                "a block keyword must be an identifier, such as `channel`",
                "Rename the keyword",
            ),
            (
                Unwritable::Function,
                "hcl.unwritable-function",
                "a function name must be an identifier, such as `secret`",
                "Rename the function",
            ),
            (
                Unwritable::Reference,
                "hcl.unwritable-reference",
                "the name does not read as a reference in HCL",
                "Start it with a letter, `_`, or `@`, and do not use `true`, `false`, \
                 or `null`",
            ),
        ];
        for (part, code, message, fix) in cases {
            let error = Error::Unwritable {
                span: Some(span(7)),
                part,
            };
            check(&error, code, message, fix);
            let error = Error::Unwritable { span: None, part };
            assert_eq!(Diagnostic::from(&error).span, None, "{part:?}");
        }
    }

    #[test]
    fn a_document_too_deep_to_write_keeps_documents_diagnostic() {
        for span in [Some(span(7)), None] {
            let error = Error::Unwritable {
                span,
                part: Unwritable::Depth,
            };
            let expected = Diagnostic::new(
                Code::new("document.too-deep"),
                span,
                "the document nests deeper than 64 levels".into(),
                "Make it flatter".into(),
            );
            assert_eq!(Diagnostic::from(&error), expected);
            assert_eq!(
                error.to_string(),
                "the document nests deeper than 64 levels. Make it flatter"
            );
        }
    }

    #[test]
    fn a_document_error_keeps_its_own_diagnostic() {
        let document = document::Error::DuplicateKey {
            key: "a".into(),
            first: Some(span(2)),
            second: Some(span(7)),
        };
        let error = Error::Document(document.clone());
        let mut expected = Diagnostic::new(
            Code::new("document.duplicate-key"),
            Some(span(7)),
            "the key \"a\" repeats an earlier key".into(),
            "Remove it, or give it a different key".into(),
        );
        expected.notes.push(Note {
            span: span(2),
            text: "the earlier key".into(),
        });
        assert_eq!(Diagnostic::from(&error), expected);
        assert_eq!(Diagnostic::from(&error), Diagnostic::from(&document));
        assert_eq!(error.to_string(), document.to_string());
    }
}

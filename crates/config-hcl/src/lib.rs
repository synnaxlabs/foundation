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
mod update;
mod write;

use std::fmt;

use document::Span;
use document::diagnostic::{Code, Diagnostic};
use document::encoding::TooDeep;
use types::name::{self, Name};

pub use parse::read;
pub use update::update;
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
    /// A number that a Document cannot hold, or text that is not a number.
    Number {
        /// Where the number is, with its `-`.
        span: Span,
        /// What is wrong with it.
        problem: Number,
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
            | Self::Number { span, .. }
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
/// diagnostic for [`Error::Document`] and for nesting past the depth limit.
impl From<&Error> for Diagnostic {
    fn from(error: &Error) -> Self {
        match error {
            Error::Syntax { span, expected } => Self::new(
                SYNTAX,
                Some(*span),
                format!("the file needs {expected} here"),
                "Write it here, or correct the text here or before it".into(),
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
            Error::Number { span, problem } => problem.diagnostic(*span),
            Error::Escape { span } => Self::new(
                ESCAPE,
                Some(*span),
                "the string has an escape that HCL does not have".into(),
                "Use `\\n`, `\\r`, `\\t`, `\\\"`, `\\\\`, `\\uNNNN`, or \
                 `\\UNNNNNNNN`"
                    .into(),
            ),
            Error::TooDeep { span } => Self::from(&TooDeep { span: Some(*span) }),
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
const TOO_LARGE: Code = Code::new("hcl.too-large");
const UNWRITABLE_KEY: Code = Code::new("hcl.unwritable-key");
const UNWRITABLE_KEYWORD: Code = Code::new("hcl.unwritable-keyword");
const UNWRITABLE_FUNCTION: Code = Code::new("hcl.unwritable-function");
const UNWRITABLE_REFERENCE: Code = Code::new("hcl.unwritable-reference");
const UNWRITABLE_FOR: Code = Code::new("hcl.unwritable-for");

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
    /// A `for` expression, such as `[for x in xs : x]`. HCL reads a list or an object
    /// that starts with the word `for` as one, such as `[for]` or `{ for = 1 }`.
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
                "Write each item. Quote a key named `for`, or put another item before \
                 an item named `for`",
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

/// What is wrong with a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Number {
    /// A number that HCL reads but a Document cannot hold: an integer outside
    /// `i128`, or a float that an `f64` cannot hold, past the largest or rounded to
    /// zero from digits that are not all zero.
    Range,
    /// Text that HCL scans as one number and refuses, such as `1.2.3`, `1e5e5`, or an
    /// exponent outside `i64`.
    Malformed,
}

impl Number {
    fn diagnostic(self, span: Span) -> Diagnostic {
        let (message, fix) = match self {
            Self::Range => (
                "the number is out of range",
                "Use an integer that fits in 128 bits, or a float that fits in 64 bits",
            ),
            Self::Malformed => (
                "the number is not valid",
                "Write a number such as `1.5e3`, or quote the text to make a string",
            ),
        };
        Diagnostic::new(NUMBER, Some(span), message.into(), fix.into())
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
    /// A list whose first item starts with the word `for`, such as the reference `for`
    /// or `for.x`, or the call `for(1)`. HCL reads `[for` as a `for` expression.
    For,
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
            Self::For => (
                UNWRITABLE_FOR,
                "a list cannot start with the word `for`, because HCL reads `[for` as a \
                 `for` expression",
                "Put another item first, or rename it",
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
    use std::collections::BTreeSet;

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

    const EXPECTED: [(Expected, &str); 16] = [
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

    #[test]
    fn each_expected_has_its_message() {
        for (expected, phrase) in EXPECTED {
            // A new variant fails this match, so it joins `EXPECTED`.
            match expected {
                Expected::Item
                | Expected::AttributeOrBlock
                | Expected::Equals
                | Expected::BlockStart
                | Expected::BlockEnd
                | Expected::Value
                | Expected::Newline
                | Expected::ListEnd
                | Expected::Key
                | Expected::ObjectEquals
                | Expected::ObjectEnd
                | Expected::ArgumentsEnd
                | Expected::Quote
                | Expected::HeredocStart
                | Expected::HeredocEnd
                | Expected::CommentEnd => {}
            }
            let error = Error::Syntax {
                span: span(7),
                expected,
            };
            check(
                &error,
                "hcl.syntax",
                &format!("the file needs {phrase} here"),
                "Write it here, or correct the text here or before it",
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
            "Write each item. Quote a key named `for`, or put another item before an \
             item named `for`",
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
            // A new variant fails this match, so it joins `FORMS`.
            match form {
                Form::Null
                | Form::Template
                | Form::Operator
                | Form::Conditional
                | Form::For
                | Form::Index
                | Form::Splat
                | Form::Parentheses
                | Form::Namespace
                | Form::Expansion
                | Form::NumberKey => {}
            }
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
                "the reference is not a valid name",
                "Use segments of ASCII letters, digits, `_`, and `-`, split by dots, \
                 with at most 255 bytes in all",
            ),
            (
                Error::Number {
                    span: span(7),
                    problem: Number::Range,
                },
                "hcl.number",
                "the number is out of range",
                "Use an integer that fits in 128 bits, or a float that fits in 64 bits",
            ),
            (
                Error::Number {
                    span: span(7),
                    problem: Number::Malformed,
                },
                "hcl.number",
                "the number is not valid",
                "Write a number such as `1.5e3`, or quote the text to make a string",
            ),
            (
                Error::Escape { span: span(7) },
                "hcl.escape",
                "the string has an escape that HCL does not have",
                "Use `\\n`, `\\r`, `\\t`, `\\\"`, `\\\\`, `\\uNNNN`, or `\\UNNNNNNNN`",
            ),
        ];
        for (error, code, message, fix) in cases {
            check(&error, code, message, fix);
        }
    }

    #[test]
    fn too_large_has_its_code_and_fix_at_the_start_of_the_file() {
        let error = Error::TooLarge {
            span: span(0),
            bytes: 4_294_967_296,
        };
        let message = "the file has 4294967296 bytes, and the limit is 4294967295";
        let fix = "Split it into smaller files";
        let expected = Diagnostic::new(
            Code::new("hcl.too-large"),
            Some(span(0)),
            message.into(),
            fix.into(),
        );
        assert_eq!(Diagnostic::from(&error), expected);
        assert_eq!(error.to_string(), format!("{message}. {fix}"));
    }

    const UNWRITABLE: [(Unwritable, &str, &str, &str); 5] = [
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
            "Start it with a letter, `_`, or `@`, and do not use `true`, `false`, or \
             `null`",
        ),
        (
            Unwritable::For,
            "hcl.unwritable-for",
            "a list cannot start with the word `for`, because HCL reads `[for` as a \
             `for` expression",
            "Put another item first, or rename it",
        ),
    ];

    #[test]
    fn each_unwritable_part_has_its_code_and_fix() {
        for (part, code, message, fix) in UNWRITABLE {
            // A new variant fails this match, so it joins `UNWRITABLE`.
            match part {
                Unwritable::Key
                | Unwritable::Keyword
                | Unwritable::Function
                | Unwritable::Reference
                | Unwritable::For => {}
                Unwritable::Depth => unreachable!("`Depth` has its own test"),
            }
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
    fn too_deep_keeps_documents_diagnostic() {
        let read = Error::TooDeep { span: span(7) };
        let write = |span| Error::Unwritable {
            span,
            part: Unwritable::Depth,
        };
        for (error, span) in [
            (read, Some(span(7))),
            (write(Some(span(7))), Some(span(7))),
            (write(None), None),
        ] {
            let expected = Diagnostic::new(
                Code::new("document.too-deep"),
                span,
                "the document nests deeper than 64 levels".into(),
                "Make it flatter".into(),
            );
            assert_eq!(Diagnostic::from(&error), expected, "{error:?}");
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
        assert_eq!(Diagnostic::from(&document), expected);
        assert_eq!(error.to_string(), document.to_string());
    }

    /// One error of each variant, each `Expected`, each `Form`, and each `Unwritable`
    /// part.
    fn every() -> Vec<Error> {
        let mut every = vec![
            Error::Name {
                span: span(7),
                error: "a.@".parse::<Name>().unwrap_err(),
            },
            Error::Number {
                span: span(7),
                problem: Number::Range,
            },
            Error::Number {
                span: span(7),
                problem: Number::Malformed,
            },
            Error::Escape { span: span(7) },
            Error::TooDeep { span: span(7) },
            Error::Document(document::Error::DuplicateKey {
                key: "a".into(),
                first: Some(span(2)),
                second: Some(span(7)),
            }),
            Error::TooLarge {
                span: span(0),
                bytes: 4_294_967_296,
            },
            Error::Unwritable {
                span: None,
                part: Unwritable::Depth,
            },
        ];
        every.extend(EXPECTED.map(|(expected, _)| Error::Syntax {
            span: span(7),
            expected,
        }));
        every.extend(FORMS.map(|(form, ..)| Error::Form {
            span: span(7),
            form,
        }));
        every.extend(
            UNWRITABLE.map(|(part, ..)| Error::Unwritable { span: None, part }),
        );
        for error in &every {
            // A new variant fails this match, so it joins `every`.
            match error {
                Error::Syntax { .. }
                | Error::Form { .. }
                | Error::Name { .. }
                | Error::Number { .. }
                | Error::Escape { .. }
                | Error::TooDeep { .. }
                | Error::Document(_)
                | Error::TooLarge { .. }
                | Error::Unwritable { .. } => {}
            }
        }
        every
    }

    #[test]
    fn each_kind_of_error_has_its_own_code() {
        let mut codes = BTreeSet::new();
        for error in every() {
            let code = Diagnostic::from(&error).code.as_str();
            let new = codes.insert(code);
            match error {
                Error::Syntax { .. } => assert_eq!(code, "hcl.syntax"),
                Error::Number { .. } => assert_eq!(code, "hcl.number"),
                Error::TooDeep { .. }
                | Error::Document(_)
                | Error::Unwritable {
                    part: Unwritable::Depth,
                    ..
                } => assert!(code.starts_with("document."), "{code}"),
                _ => assert!(new, "{code} repeats: {error:?}"),
            }
        }
    }

    #[test]
    fn each_message_is_a_clause_and_each_fix_a_sentence() {
        for error in every() {
            let diagnostic = Diagnostic::from(&error);
            let (message, fix) = (&diagnostic.message, &diagnostic.fix);
            assert!(!message.ends_with('.'), "{message}");
            assert!(!fix.ends_with('.'), "{fix}");
            assert!(!message.starts_with(char::is_uppercase), "{message}");
            assert!(fix.starts_with(char::is_uppercase), "{fix}");
        }
    }
}

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
mod unwritable;
mod update;
mod write;

use std::fmt;

use document::Span;
use document::diagnostic::{Code, Diagnostic, Note};
use document::encoding::TooDeep;
use types::name;

pub use parse::read;
pub use unwritable::Unwritable;
pub use update::{Refusal, update};
pub use write::write;

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
    /// A string, a heredoc, or a comment with no closer.
    Unclosed {
        /// An empty span where the closer must go: the end of the line for a string,
        /// the end of the file for a heredoc or a comment.
        span: Span,
        /// The opener: `"`, `<<EOT`, or `/*`.
        opener: Span,
        /// The part with no closer.
        part: Unclosed,
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
}

impl Error {
    /// The start of the problem's span, to sort the problems from `read` in source
    /// order.
    fn offset(&self) -> u32 {
        match self {
            Self::Syntax { span, .. }
            | Self::Unclosed { span, .. }
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
        }
    }
}

/// Gives each problem a diagnostic with a stable `hcl.*` code, or `document`'s own
/// diagnostic for [`Error::Document`] and for nesting past the depth limit.
impl From<&Error> for Diagnostic {
    fn from(error: &Error) -> Self {
        match error {
            Error::Syntax { span, expected } => syntax(*span, expected),
            Error::Unclosed { span, opener, part } => part.diagnostic(*span, *opener),
            Error::Form { span, form } => form.diagnostic(*span),
            Error::Name { span, error } => {
                Self::new(NAME, Some(*span), error.to_string(), error.fix().into())
            }
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
const EXPRESSION_KEY: Code = Code::new("hcl.expression-key");
const NAME: Code = Code::new("hcl.name");
const NUMBER: Code = Code::new("hcl.number");
const ESCAPE: Code = Code::new("hcl.escape");
const TOO_LARGE: Code = Code::new("hcl.too-large");

/// A syntax error at `span`, which needs `needed` there.
fn syntax(span: Span, needed: impl fmt::Display) -> Diagnostic {
    Diagnostic::new(
        SYNTAX,
        Some(span),
        format!("the file needs {needed} here"),
        "Write it here, or correct the text here or before it".into(),
    )
}

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
    /// An index or an attribute access after a value, such as `a[0]`, `a.0`, or
    /// `f().b`. A string index on a reference is a segment, not this form.
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
    /// An object key that is an expression, such as `{ f() = 1 }`, which HCL
    /// evaluates.
    ExpressionKey,
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
                "Write the value itself. Write a name segment that is not an \
                 identifier as a string index, such as `plc[\"40001\"]`",
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
            Self::ExpressionKey => (
                EXPRESSION_KEY,
                "an object key here is an expression",
                "Write the key as a name or a quoted string",
            ),
        };
        Diagnostic::new(code, Some(span), message.into(), fix.into())
    }
}

/// What is wrong with a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Number {
    /// A number that a Document cannot hold: an integer outside `i128`, or a float
    /// that an `f64` cannot hold, past the largest or rounded to zero from digits that
    /// are not all zero.
    Range,
    /// Text that HCL scans as one number but that is not a number: it has two dots,
    /// two exponents, a dot in its exponent, or an exponent outside `i64`.
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
                "Write a number such as `1.5e3`, or put the text in quotes to make a \
                 string",
            ),
        };
        Diagnostic::new(NUMBER, Some(span), message.into(), fix.into())
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
    /// A new line after an attribute, a block, or the marker that ends a heredoc.
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
    /// A marker, such as `EOT`, and a new line after `<<` or `<<-`.
    HeredocStart,
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
            Self::HeredocStart => {
                "a marker, such as `EOT`, and a new line to start the heredoc"
            }
        })
    }
}

/// A part of HCL text that needs a closer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unclosed {
    /// A quoted string, which ends at `"` on its line.
    String,
    /// A heredoc, which ends at its marker on a line of its own.
    Heredoc,
    /// A block comment, which ends at `*/`.
    Comment,
}

impl Unclosed {
    fn diagnostic(self, span: Span, opener: Span) -> Diagnostic {
        let (closer, part) = match self {
            Self::String => ("`\"` to end the string", "string"),
            Self::Heredoc => (
                "the marker on a line of its own to end the heredoc",
                "heredoc",
            ),
            Self::Comment => ("`*/` to end the comment", "comment"),
        };
        let mut diagnostic = syntax(span, closer);
        diagnostic.notes.push(Note {
            span: opener,
            text: format!("the {part} starts here"),
        });
        diagnostic
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
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

    const EXPECTED: [(Expected, &str); 13] = [
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
        (
            Expected::HeredocStart,
            "a marker, such as `EOT`, and a new line to start the heredoc",
        ),
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
                | Expected::HeredocStart => {}
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

    const UNCLOSED: [(Unclosed, &str, &str); 3] = [
        (Unclosed::String, "`\"` to end the string", "string"),
        (
            Unclosed::Heredoc,
            "the marker on a line of its own to end the heredoc",
            "heredoc",
        ),
        (Unclosed::Comment, "`*/` to end the comment", "comment"),
    ];

    #[test]
    fn each_unclosed_part_needs_its_closer_and_notes_its_opener() {
        for (part, phrase, noun) in UNCLOSED {
            // A new variant fails this match, so it joins `UNCLOSED`.
            match part {
                Unclosed::String | Unclosed::Heredoc | Unclosed::Comment => {}
            }
            let error = Error::Unclosed {
                span: span(7),
                opener: span(2),
                part,
            };
            let message = format!("the file needs {phrase} here");
            let fix = "Write it here, or correct the text here or before it";
            let mut expected = Diagnostic::new(
                Code::new("hcl.syntax"),
                Some(span(7)),
                message.clone(),
                fix.into(),
            );
            expected.notes.push(Note {
                span: span(2),
                text: format!("the {noun} starts here"),
            });
            assert_eq!(Diagnostic::from(&error), expected, "{error:?}");
            assert_eq!(error.to_string(), format!("{message}. {fix}"), "{error:?}");
        }
    }

    const FORMS: [(Form, &str, &str, &str); 12] = [
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
            "Write the value itself. Write a name segment that is not an identifier \
             as a string index, such as `plc[\"40001\"]`",
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
        (
            Form::ExpressionKey,
            "hcl.expression-key",
            "an object key here is an expression",
            "Write the key as a name or a quoted string",
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
                | Form::NumberKey
                | Form::ExpressionKey => {}
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
                    error: "a.@".parse::<name::Name>().unwrap_err(),
                },
                "hcl.name",
                "\"a.@\" has a segment that is not valid: \"@\"",
                "Use one or more ASCII letters, digits, `_`, and `-` in that segment, \
                 and no other character",
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
                "Write a number such as `1.5e3`, or put the text in quotes to make \
                 a string",
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

    #[test]
    fn too_deep_keeps_documents_diagnostic() {
        let error = Error::TooDeep { span: span(7) };
        let expected = Diagnostic::new(
            Code::new("document.too-deep"),
            Some(span(7)),
            "the document nests deeper than 64 levels".into(),
            "Make it flatter".into(),
        );
        assert_eq!(Diagnostic::from(&error), expected);
        assert_eq!(
            error.to_string(),
            "the document nests deeper than 64 levels. Make it flatter"
        );
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

    /// One error of each variant, each `Expected`, each `Unclosed` part, and each
    /// `Form`.
    fn every() -> Vec<Error> {
        let mut every = vec![
            Error::Name {
                span: span(7),
                error: "a.@".parse::<name::Name>().unwrap_err(),
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
        ];
        every.extend(EXPECTED.map(|(expected, _)| Error::Syntax {
            span: span(7),
            expected,
        }));
        every.extend(UNCLOSED.map(|(part, ..)| Error::Unclosed {
            span: span(7),
            opener: span(2),
            part,
        }));
        every.extend(FORMS.map(|(form, ..)| Error::Form {
            span: span(7),
            form,
        }));
        for error in &every {
            // A new variant fails this match, so it joins `every`.
            match error {
                Error::Syntax { .. }
                | Error::Unclosed { .. }
                | Error::Form { .. }
                | Error::Name { .. }
                | Error::Number { .. }
                | Error::Escape { .. }
                | Error::TooDeep { .. }
                | Error::Document(_)
                | Error::TooLarge { .. } => {}
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
                Error::Syntax { .. } | Error::Unclosed { .. } => {
                    assert_eq!(code, "hcl.syntax");
                }
                Error::Number { .. } => assert_eq!(code, "hcl.number"),
                Error::TooDeep { .. } | Error::Document(_) => {
                    assert!(code.starts_with("document."), "{code}");
                }
                _ => assert!(new, "{code} repeats: {error:?}"),
            }
        }
        for part in unwritable::tests::every(None) {
            let code = Diagnostic::from(&part).code.as_str();
            if matches!(part, Unwritable::TooDeep(_)) {
                assert_eq!(code, "document.too-deep");
            } else {
                assert!(codes.insert(code), "{code} repeats: {part:?}");
            }
        }
    }

    #[test]
    fn each_message_is_a_clause_and_each_fix_a_sentence() {
        let (errors, parts) = (every(), unwritable::tests::every(None));
        let diagnostics = errors.iter().map(Diagnostic::from);
        for diagnostic in diagnostics.chain(parts.iter().map(Diagnostic::from)) {
            let (message, fix) = (&diagnostic.message, &diagnostic.fix);
            assert!(!message.ends_with('.'), "{message}");
            assert!(!fix.ends_with('.'), "{fix}");
            assert!(!message.starts_with(char::is_uppercase), "{message}");
            assert!(fix.starts_with(char::is_uppercase), "{fix}");
        }
    }
}

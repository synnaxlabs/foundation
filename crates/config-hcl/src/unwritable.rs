use std::fmt;

use document::Span;
use document::diagnostic::{Code, Diagnostic};
use document::encoding::TooDeep;

const KEY: Code = Code::new("hcl.unwritable-key");
const KEYWORD: Code = Code::new("hcl.unwritable-keyword");
const FUNCTION: Code = Code::new("hcl.unwritable-function");
const REFERENCE: Code = Code::new("hcl.unwritable-reference");
const FOR: Code = Code::new("hcl.unwritable-for");

/// A part of a Document that no HCL text reads back as the same part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unwritable {
    /// A key of a body that is not an identifier, such as `my key`. HCL has no quoted
    /// key in a body. A key in a map value can be any text.
    Key {
        /// Where the key is, or `None` for a key with no span.
        span: Option<Span>,
    },
    /// A block keyword that is not an identifier.
    Keyword {
        /// Where the keyword is, or `None` for a keyword with no span.
        span: Option<Span>,
    },
    /// A function name that is not an identifier.
    Function {
        /// Where the function name is, or `None` for a name with no span.
        span: Option<Span>,
    },
    /// A name whose first segment does not start a reference in HCL: it does not
    /// start with a letter or `_`, or it is `true`, `false`, or `null`, such as
    /// `7a.b`, `@a.b`, or `true.x`. A later segment is never refused: it is written
    /// as a string index, such as `plc["40001"]`.
    Reference {
        /// Where the reference is, or `None` for a reference with no span.
        span: Option<Span>,
    },
    /// A list whose first item starts with the word `for`, such as the reference `for`
    /// or `for.x`, or the call `for(1)`. HCL reads `[for` as a `for` expression.
    For {
        /// Where the reference or the function name is, or `None` for one with no
        /// span.
        span: Option<Span>,
    },
    /// A block or a value nested deeper than [`document::encoding::DEPTH_MAX`], which
    /// [`read`](crate::read) refuses.
    TooDeep(TooDeep),
}

/// Gives each part a diagnostic with a stable `hcl.unwritable-*` code, or `document`'s
/// own diagnostic for nesting past the depth limit.
impl From<&Unwritable> for Diagnostic {
    fn from(unwritable: &Unwritable) -> Self {
        let (code, span, message, fix) = match *unwritable {
            Unwritable::Key { span } => (
                KEY,
                span,
                "a key of a body must be an identifier, such as `retry_limit`",
                "Rename the key, or move it into a map value",
            ),
            Unwritable::Keyword { span } => (
                KEYWORD,
                span,
                "a block keyword must be an identifier, such as `channel`",
                "Rename the keyword",
            ),
            Unwritable::Function { span } => (
                FUNCTION,
                span,
                "a function name must be an identifier, such as `secret`",
                "Rename the function",
            ),
            Unwritable::Reference { span } => (
                REFERENCE,
                span,
                "the first segment of a reference in HCL starts with a letter or `_` \
                 and is not `true`, `false`, or `null`",
                "Write the name as a quoted string where a kind takes a name",
            ),
            Unwritable::For { span } => (
                FOR,
                span,
                "a list cannot start with the word `for`, because HCL reads `[for` as \
                 a `for` expression",
                "Put another item first, or rename it",
            ),
            Unwritable::TooDeep(too_deep) => return Self::from(&too_deep),
        };
        Self::new(code, span, message.into(), fix.into())
    }
}

impl fmt::Display for Unwritable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Diagnostic::from(self), f)
    }
}

impl std::error::Error for Unwritable {}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use document::{Position, Source};

    fn span() -> Span {
        let at = Position {
            offset: 7,
            line: 0,
            column: 7,
        };
        Span::new(Source(0), at, at).unwrap()
    }

    /// One part of each kind, each at `span`.
    pub(crate) fn every(span: Option<Span>) -> [Unwritable; 6] {
        let every = [
            Unwritable::Key { span },
            Unwritable::Keyword { span },
            Unwritable::Function { span },
            Unwritable::Reference { span },
            Unwritable::For { span },
            Unwritable::TooDeep(TooDeep { span }),
        ];
        for part in every {
            // A new variant fails this match, so it joins `every`.
            match part {
                Unwritable::Key { .. }
                | Unwritable::Keyword { .. }
                | Unwritable::Function { .. }
                | Unwritable::Reference { .. }
                | Unwritable::For { .. }
                | Unwritable::TooDeep(_) => {}
            }
        }
        every
    }

    #[test]
    fn each_part_has_its_code_and_fix() {
        let cases = [
            (
                "hcl.unwritable-key",
                "a key of a body must be an identifier, such as `retry_limit`",
                "Rename the key, or move it into a map value",
            ),
            (
                "hcl.unwritable-keyword",
                "a block keyword must be an identifier, such as `channel`",
                "Rename the keyword",
            ),
            (
                "hcl.unwritable-function",
                "a function name must be an identifier, such as `secret`",
                "Rename the function",
            ),
            (
                "hcl.unwritable-reference",
                "the first segment of a reference in HCL starts with a letter or `_` \
                 and is not `true`, `false`, or `null`",
                "Write the name as a quoted string where a kind takes a name",
            ),
            (
                "hcl.unwritable-for",
                "a list cannot start with the word `for`, because HCL reads `[for` as \
                 a `for` expression",
                "Put another item first, or rename it",
            ),
            (
                "document.too-deep",
                "the document nests deeper than 64 levels",
                "Make it flatter",
            ),
        ];
        for span in [Some(span()), None] {
            let parts = every(span);
            assert_eq!(parts.len(), cases.len());
            for (part, (code, message, fix)) in parts.iter().zip(cases) {
                let expected =
                    Diagnostic::new(Code::new(code), span, message.into(), fix.into());
                assert_eq!(Diagnostic::from(part), expected, "{part:?}");
                assert_eq!(part.to_string(), format!("{message}. {fix}"), "{part:?}");
            }
        }
    }

    #[test]
    fn too_deep_keeps_documents_diagnostic() {
        for span in [Some(span()), None] {
            assert_eq!(
                Diagnostic::from(&Unwritable::TooDeep(TooDeep { span })),
                Diagnostic::from(&TooDeep { span })
            );
        }
    }
}

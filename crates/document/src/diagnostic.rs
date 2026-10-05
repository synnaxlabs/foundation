//! Problems in Documents, in one form for every syntax and kind.

use std::fmt;

use crate::{Error, Span};

/// A problem in a Document, or in the file it came from, that a person or an agent
/// fixes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// What kind of problem it is. It never changes between releases.
    pub code: Code,
    /// Where the problem is. `None` for a Document with no spans.
    pub span: Option<Span>,
    /// What is wrong, as a clause that starts in lower case, such as "the file nests
    /// deeper than 64 levels".
    pub message: String,
    /// How to fix it, as a sentence, such as "Make it flatter".
    pub fix: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}. {}", self.message, self.fix)
    }
}

impl From<Error> for Diagnostic {
    fn from(error: Error) -> Self {
        let (code, span) = match &error {
            Error::DuplicateKey { second, .. } => (REPEATED_KEY, *second),
        };
        Self {
            code,
            span,
            message: error.message(),
            fix: error.fix().into(),
        }
    }
}

const REPEATED_KEY: Code = Code::new("document.repeated-key");

/// A stable name for a kind of problem: `<producer>.<problem>`, such as `hcl.syntax`
/// or `document.repeated-key`. The producer is a crate or a kind name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Code(&'static str);

impl Code {
    /// Makes a code. Use it in a `const`, so a bad code fails the build.
    ///
    /// # Panics
    ///
    /// Panics when `text` is not two parts split by one dot, each a lower-case ASCII
    /// letter and then lower-case ASCII letters, digits, and `-`.
    #[must_use]
    pub const fn new(text: &'static str) -> Self {
        assert!(
            valid(text.as_bytes()),
            "a code is two parts split by one dot, each a lower-case ASCII letter and \
             then lower-case ASCII letters, digits, and `-`"
        );
        Self(text)
    }

    /// The code as text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

const fn valid(mut rest: &[u8]) -> bool {
    let mut dotted = false;
    let mut start = true;
    while let [b, tail @ ..] = rest {
        match *b {
            b'.' if !start && !dotted => {
                dotted = true;
                start = true;
            }
            b'a'..=b'z' => start = false,
            b'0'..=b'9' | b'-' if !start => {}
            _ => return false,
        }
        rest = tail;
    }
    dotted && !start
}

#[cfg(test)]
mod tests {
    use std::panic;

    use super::*;
    use crate::{Position, Source};

    #[test]
    fn makes_a_code_in_a_const() {
        const SYNTAX: Code = Code::new("hcl.syntax");
        assert_eq!(SYNTAX.as_str(), "hcl.syntax");
        assert_eq!(SYNTAX.to_string(), "hcl.syntax");
        for text in [
            "document.repeated-key",
            "modbus-tcp.bad-port",
            "a.b",
            "x1.y2-z",
        ] {
            assert_eq!(Code::new(text).as_str(), text);
        }
    }

    #[test]
    fn refuses_a_code_with_a_bad_form() {
        let bad = [
            "",
            "hcl",
            "hcl.",
            ".syntax",
            "hcl.syn.tax",
            "Hcl.syntax",
            "hcl.Syntax",
            "hcl.syn tax",
            "hcl.-syntax",
            "1hcl.syntax",
            "hcl..syntax",
            "hcl.é",
            "hcl_x.syntax",
        ];
        for text in bad {
            let payload = panic::catch_unwind(|| Code::new(text)).unwrap_err();
            assert_eq!(
                payload.downcast_ref::<&str>(),
                Some(
                    &"a code is two parts split by one dot, each a lower-case ASCII \
                      letter and then lower-case ASCII letters, digits, and `-`"
                ),
                "{text:?}"
            );
        }
    }

    #[test]
    fn makes_a_diagnostic_from_a_repeated_key() {
        let at = |offset| Position {
            offset,
            line: 0,
            column: offset,
        };
        let span = |start, end| Span::new(Source(0), at(start), at(end)).unwrap();
        let error = Error::DuplicateKey {
            key: "a".into(),
            first: Some(span(0, 1)),
            second: Some(span(6, 7)),
        };
        let diagnostic = Diagnostic::from(error.clone());
        let expected = Diagnostic {
            code: Code::new("document.repeated-key"),
            span: Some(span(6, 7)),
            message: "the key \"a\" repeats an earlier key".into(),
            fix: "Remove it, or give it a different key".into(),
        };
        assert_eq!(diagnostic, expected);
        assert_eq!(diagnostic.to_string(), error.to_string());
        assert_eq!(
            diagnostic.to_string(),
            "the key \"a\" repeats an earlier key. Remove it, or give it a different \
             key"
        );
    }
}

//! Problems in Documents, in one form for every syntax and kind.

use std::fmt;

use crate::Span;

/// A problem in a Document, or in the file it came from, that a person or an agent
/// fixes.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diagnostic {
    /// What kind of problem it is.
    pub code: Code,
    /// Where the problem is. `None` only for a Document with no spans. A problem with
    /// a whole file has an empty span at the start of the file.
    pub span: Option<Span>,
    /// What is wrong, as a clause that starts in lower case and has no final period,
    /// such as "the file nests deeper than 64 levels".
    pub message: String,
    /// How to fix it, as a sentence with no final period, such as "Make it flatter".
    pub fix: String,
    /// Other places that help explain the problem.
    pub notes: Vec<Note>,
}

impl Diagnostic {
    /// Makes a diagnostic with no notes.
    #[must_use]
    pub fn new(code: Code, span: Option<Span>, message: String, fix: String) -> Self {
        Self {
            code,
            span,
            message,
            fix,
            notes: Vec::new(),
        }
    }
}

/// Writes the message and the fix. The caller shows the code, the span, and the
/// notes.
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}. {}", self.message, self.fix)
    }
}

/// A place that helps explain a problem, such as where a repeated key first appears.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// The place.
    pub span: Span,
    /// What is there, as a clause that starts in lower case and has no final period,
    /// such as "the earlier key".
    pub text: String,
}

/// A stable name for a kind of problem: `<producer>.<problem>`, such as `hcl.syntax`
/// or `document.duplicate-key`. A code never changes between releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Code(&'static str);

impl Code {
    /// Makes a code. Declare each code as a `const` item, so a bad code fails the
    /// build:
    ///
    /// ```
    /// use document::diagnostic::Code;
    ///
    /// const SYNTAX: Code = Code::new("hcl.syntax");
    /// assert_eq!(SYNTAX.as_str(), "hcl.syntax");
    /// ```
    ///
    /// ```compile_fail
    /// use document::diagnostic::Code;
    ///
    /// const SYNTAX: Code = Code::new("hcl.Syntax");
    /// assert_eq!(SYNTAX.as_str(), "hcl.Syntax");
    /// ```
    ///
    /// # Panics
    ///
    /// Panics when `text` is not two parts split by one dot. Each part is lower-case
    /// ASCII letters and digits, starts with a letter, and may join words with single
    /// `-`. In a `const` item, the panic is a build error.
    #[must_use]
    pub const fn new(text: &'static str) -> Self {
        assert!(
            valid(text.as_bytes()),
            "a code is two parts split by one dot. Each part is lower-case ASCII \
             letters and digits, starts with a letter, and may join words with \
             single `-`"
        );
        Self(text)
    }

    /// The code as text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
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
    // The part is empty or ends with `-`, so it cannot end or take a `-` here.
    let mut open = true;
    while let [b, tail @ ..] = rest {
        match *b {
            b'.' if !open && !dotted => {
                dotted = true;
                start = true;
                open = true;
            }
            b'a'..=b'z' => {
                start = false;
                open = false;
            }
            b'0'..=b'9' if !start => open = false,
            b'-' if !open => open = true,
            _ => return false,
        }
        rest = tail;
    }
    dotted && !open
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
            "document.duplicate-key",
            "modbus-tcp.bad-port",
            "iec-61850.bad-1-x",
            "a.b",
            "x0.y9",
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
            "hcl-.syntax",
            "hcl.syntax-",
            "hcl.syn--tax",
            "1hcl.syntax",
            "hcl.1syntax",
            "hcl..syntax",
            "hcl.é",
            "hcl_x.syntax",
            "hcl.a/",
            "hcl.a:",
            "hcl.a`",
            "hcl.a{",
        ];
        for text in bad {
            let payload = panic::catch_unwind(|| Code::new(text)).unwrap_err();
            assert_eq!(
                payload.downcast_ref::<&str>(),
                Some(
                    &"a code is two parts split by one dot. Each part is lower-case \
                      ASCII letters and digits, starts with a letter, and may join \
                      words with single `-`"
                ),
                "{text:?}"
            );
        }
    }

    #[test]
    fn writes_the_message_and_the_fix() {
        let at = Position {
            offset: 0,
            line: 0,
            column: 0,
        };
        let mut diagnostic = Diagnostic::new(
            Code::new("hcl.syntax"),
            Span::new(Source(0), at, at),
            "the file ends inside a list".into(),
            "Add `]`".into(),
        );
        diagnostic.notes.push(Note {
            span: Span::new(Source(0), at, at).unwrap(),
            text: "the list starts here".into(),
        });
        assert_eq!(
            diagnostic.to_string(),
            "the file ends inside a list. Add `]`"
        );
    }
}

use document::{Position, Source, Span};

use crate::{Error, Expected, Form};

#[derive(Clone, Debug)]
pub(crate) struct Token<'a> {
    pub(crate) kind: Kind,
    /// The token as written.
    pub(crate) text: &'a str,
    pub(crate) span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Letters, digits, `_`, and `-`, after a letter or `_`.
    Identifier,
    /// A word with `.` or `@`, which only a reference can be: letters, digits, `_`,
    /// `-`, `@`, and dots, which `types::name` checks.
    Reference,
    /// Digits, then an optional fraction and an optional exponent.
    Number,
    /// A quoted string, with its escapes read.
    String(Box<str>),
    /// A heredoc, with `\n` for each line end and, after `<<-`, without the indent
    /// that its lines share.
    Heredoc(Box<str>),
    /// `!`, `==`, `!=`, `<`, `<=`, `>`, `>=`, `&&`, `||`, `+`, `/`, or `%`.
    Operator,
    Star,
    Question,
    Dot,
    /// `...`.
    Ellipsis,
    /// `::`.
    DoubleColon,
    Equals,
    Colon,
    Comma,
    Minus,
    OpenBrace,
    CloseBrace,
    OpenBracket,
    CloseBracket,
    OpenParenthesis,
    CloseParenthesis,
    /// `\n` or `\r\n`.
    Newline,
    /// Any other character.
    Other,
    /// The end of the text.
    End,
    /// A problem that stops reading.
    Error(Error),
}

/// Splits text into tokens, skipping spaces, tabs, and comments.
#[derive(Clone)]
pub(crate) struct Tokens<'a> {
    source: Source,
    rest: &'a str,
    at: Position,
}

impl<'a> Tokens<'a> {
    /// Starts at the beginning of `text`, after a byte order mark.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TooLarge`] when a span cannot count the bytes of `text`.
    pub(crate) fn new(source: Source, text: &'a str) -> Result<Self, Error> {
        check_size(text.len())?;
        let rest = text.strip_prefix('\u{feff}').unwrap_or(text);
        let offset = text
            .len()
            .checked_sub(rest.len())
            .and_then(|bom| u32::try_from(bom).ok())
            .expect("invariant: a byte order mark has 3 bytes");
        Ok(Self {
            source,
            rest,
            at: Position {
                offset,
                line: 0,
                column: 0,
            },
        })
    }

    /// Reads the next token. Call it no more after [`Kind::End`] or [`Kind::Error`].
    pub(crate) fn next(&mut self) -> Token<'a> {
        self.read().unwrap_or_else(|error| Token {
            kind: Kind::Error(error),
            text: "",
            span: self.span(self.at),
        })
    }

    /// Reads the next token that is not a new line.
    pub(crate) fn next_past_lines(&mut self) -> Token<'a> {
        // Each pass moves past a new line or returns.
        for _ in 0..=self.rest.len() {
            let token = self.next();
            if token.kind != Kind::Newline {
                return token;
            }
        }
        unreachable!("invariant: each pass moves past a new line")
    }

    fn read(&mut self) -> Result<Token<'a>, Error> {
        self.skip()?;
        let start = self.at;
        let rest = self.rest;
        if self.eat_newline() {
            return Ok(self.token(Kind::Newline, rest, start));
        }
        let Some(c) = self.bump() else {
            return Ok(self.token(Kind::End, rest, start));
        };
        let kind = match c {
            '"' => Kind::String(self.string(start)?),
            '=' if self.eat('=') => Kind::Operator,
            '=' => Kind::Equals,
            '<' if self.eat('<') => Kind::Heredoc(self.heredoc(start)?),
            '!' | '<' | '>' => {
                self.eat('=');
                Kind::Operator
            }
            '&' if self.eat('&') => Kind::Operator,
            '|' if self.eat('|') => Kind::Operator,
            '+' | '/' | '%' => Kind::Operator,
            '*' => Kind::Star,
            '?' => Kind::Question,
            '.' if self.rest.starts_with("..") => {
                self.skip_bytes(2);
                Kind::Ellipsis
            }
            '.' => Kind::Dot,
            ':' if self.eat(':') => Kind::DoubleColon,
            ':' => Kind::Colon,
            ',' => Kind::Comma,
            '-' => Kind::Minus,
            '{' => Kind::OpenBrace,
            '}' => Kind::CloseBrace,
            '[' => Kind::OpenBracket,
            ']' => Kind::CloseBracket,
            '(' => Kind::OpenParenthesis,
            ')' => Kind::CloseParenthesis,
            '0'..='9' => {
                self.number();
                Kind::Number
            }
            c if identifier_start(c) || c == '@' || c.is_alphabetic() => {
                self.word(c, start)?
            }
            _ => Kind::Other,
        };
        Ok(self.token(kind, rest, start))
    }

    fn token(&self, kind: Kind, rest: &'a str, start: Position) -> Token<'a> {
        let text = rest
            .len()
            .checked_sub(self.rest.len())
            .and_then(|len| rest.get(..len))
            .expect("invariant: a token is a prefix of the text where it starts");
        Token {
            kind,
            text,
            span: self.span(start),
        }
    }

    fn span(&self, start: Position) -> Span {
        Span::new(self.source, start, self.at)
            .expect("invariant: the lexer moves only forward")
    }

    fn peek(&self) -> Option<char> {
        self.rest.chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let mut chars = self.rest.chars();
        let c = chars.next()?;
        self.rest = chars.as_str();
        self.advance(c);
        Some(c)
    }

    fn advance(&mut self, c: char) {
        let bytes =
            u32::try_from(c.len_utf8()).expect("invariant: a char has 4 bytes or less");
        let at = &mut self.at;
        at.offset = at
            .offset
            .checked_add(bytes)
            .expect("invariant: `check_size` bounds the offset");
        if c == '\n' {
            at.line = at
                .line
                .checked_add(1)
                .expect("invariant: lines are bytes or less");
            at.column = 0;
        } else {
            at.column = at
                .column
                .checked_add(1)
                .expect("invariant: columns are bytes or less");
        }
    }

    fn eat(&mut self, c: char) -> bool {
        let found = self.peek() == Some(c);
        if found {
            self.bump();
        }
        found
    }

    fn skip_bytes(&mut self, len: usize) {
        let (skipped, rest) = self
            .rest
            .split_at_checked(len)
            .expect("invariant: `len` ends on a character boundary in the text");
        self.rest = rest;
        skipped.chars().for_each(|c| self.advance(c));
    }

    fn eat_while(&mut self, f: impl Fn(char) -> bool) {
        let len = self.rest.find(|c| !f(c)).unwrap_or(self.rest.len());
        self.skip_bytes(len);
    }

    /// Reports whether the text after the next character starts with `f`'s match.
    fn second(&self, f: impl FnOnce(char) -> bool) -> bool {
        let mut chars = self.rest.chars();
        chars.next();
        chars.next().is_some_and(f)
    }

    fn skip(&mut self) -> Result<(), Error> {
        // Each pass moves past a character or returns.
        for _ in 0..=self.rest.len() {
            match self.peek() {
                Some(' ' | '\t') => {
                    self.bump();
                }
                Some('#') => self.eat_while(|c| c != '\n'),
                Some('/') if self.second(|c| c == '/') => self.eat_while(|c| c != '\n'),
                Some('/') if self.second(|c| c == '*') => self.comment()?,
                _ => return Ok(()),
            }
        }
        unreachable!("invariant: each pass moves past a character")
    }

    fn comment(&mut self) -> Result<(), Error> {
        let start = self.at;
        let end = self.rest.get(2..).and_then(|body| body.find("*/"));
        if let Some(len) = end.and_then(|end| end.checked_add(4)) {
            self.skip_bytes(len);
            Ok(())
        } else {
            self.skip_bytes(self.rest.len());
            Err(Error::Syntax {
                span: self.span(start),
                expected: Expected::CommentEnd,
            })
        }
    }

    /// Moves past the rest of a number after its first digit.
    fn number(&mut self) {
        let bytes = self.rest.as_bytes();
        let digits = |from: usize| {
            let run = bytes.get(from..).map_or(0, |rest| {
                rest.iter().take_while(|b| b.is_ascii_digit()).count()
            });
            from.saturating_add(run)
        };
        let mut len = digits(0);
        let fraction = len.saturating_add(1);
        if bytes.get(len) == Some(&b'.') && digits(fraction) > fraction {
            len = digits(fraction);
        }
        if matches!(bytes.get(len), Some(b'e' | b'E')) {
            let sign = len.saturating_add(1);
            let start = sign.saturating_add(usize::from(matches!(
                bytes.get(sign),
                Some(b'+' | b'-')
            )));
            if digits(start) > start {
                len = digits(start);
            }
        }
        self.skip_bytes(len);
    }

    /// Moves past the rest of a word after its first character, `first`, which
    /// starts at `start`.
    fn word(&mut self, first: char, start: Position) -> Result<Kind, Error> {
        let rest = self.rest;
        // A dot is part of the word unless it starts a splat or an expansion.
        let dot = |i: usize| {
            let after = rest.get(i..).unwrap_or_default();
            !after.starts_with(".*") && !after.starts_with("...")
        };
        let len = rest
            .char_indices()
            .find(|&(i, c)| {
                !(identifier_part(c)
                    || !(c.is_ascii() || c.is_whitespace())
                    || c == '@'
                    || c == '.' && dot(i))
            })
            .map_or(rest.len(), |(i, _)| i);
        let word = rest
            .get(..len)
            .expect("invariant: a word ends on a character boundary");
        self.skip_bytes(len);
        if !(first.is_ascii() && word.is_ascii()) {
            return Err(Error::Form {
                span: self.span(start),
                form: Form::UnicodeIdentifier,
            });
        }
        Ok(if first == '@' || word.contains(['.', '@']) {
            Kind::Reference
        } else {
            Kind::Identifier
        })
    }

    /// Reads a quoted string after its opening quote at `start`.
    fn string(&mut self, start: Position) -> Result<Box<str>, Error> {
        let mut text = String::new();
        // Each pass moves past a character or returns.
        for _ in 0..=self.rest.len() {
            let at = self.at;
            match self.peek() {
                None | Some('\n') => {
                    return Err(Error::Syntax {
                        span: self.span(start),
                        expected: Expected::Quote,
                    });
                }
                Some('"') => {
                    self.bump();
                    return Ok(text.into());
                }
                Some('\\') => {
                    self.bump();
                    text.push(self.escape(at)?);
                }
                Some(c @ ('$' | '%')) => {
                    self.bump();
                    self.sigil(c, at, &mut text)?;
                }
                Some(c) => {
                    self.bump();
                    text.push(c);
                }
            }
        }
        unreachable!("invariant: each pass moves past a character")
    }

    /// Reads what follows `sigil`, a `$` or `%` at `start`, into `text`: `$${` and
    /// `%%{` are the text `${` and `%{`.
    ///
    /// # Errors
    ///
    /// Returns [`Form::Template`] when `sigil` starts a template.
    fn sigil(
        &mut self,
        sigil: char,
        start: Position,
        text: &mut String,
    ) -> Result<(), Error> {
        if self.eat('{') {
            return Err(Error::Form {
                span: self.span(start),
                form: Form::Template,
            });
        }
        text.push(sigil);
        if self.peek() == Some(sigil) && self.second(|c| c == '{') {
            self.bump();
            self.bump();
            text.push('{');
        }
        Ok(())
    }

    /// Reads a heredoc after its `<<` at `start`.
    fn heredoc(&mut self, start: Position) -> Result<Box<str>, Error> {
        let indented = self.eat('-');
        let marker = self.marker();
        if marker.is_empty() || !self.eat_newline() {
            return Err(Error::Syntax {
                span: self.span(start),
                expected: Expected::HeredocStart,
            });
        }
        let mut text = String::new();
        let mut line_start = true;
        // Each pass moves past a character or returns.
        for _ in 0..=self.rest.len() {
            if line_start && self.close(marker) {
                return Ok(if indented { dedent(&text) } else { text }.into());
            }
            let at = self.at;
            line_start = self.eat_newline();
            if line_start {
                text.push('\n');
                continue;
            }
            match self.bump() {
                None => {
                    return Err(Error::Syntax {
                        span: self.span(start),
                        expected: Expected::HeredocEnd,
                    });
                }
                Some(c @ ('$' | '%')) => self.sigil(c, at, &mut text)?,
                Some(c) => text.push(c),
            }
        }
        unreachable!("invariant: each pass moves past a character")
    }

    /// Moves past an identifier, and returns it, or `""` when none starts here.
    fn marker(&mut self) -> &'a str {
        let rest = self.rest;
        let len = if rest.starts_with(identifier_start) {
            rest.find(|c| !identifier_part(c)).unwrap_or(rest.len())
        } else {
            0
        };
        self.skip_bytes(len);
        rest.get(..len)
            .expect("invariant: an identifier ends on a character boundary")
    }

    /// Moves past the rest of the line when it holds only `marker`, with whitespace
    /// around it, and reports whether it did.
    fn close(&mut self, marker: &str) -> bool {
        let mut end = self.clone();
        end.eat_while(space);
        if !end.rest.starts_with(marker) {
            return false;
        }
        end.skip_bytes(marker.len());
        end.eat_while(space);
        let closed = end.rest.is_empty() || end.line_end() > 0;
        if closed {
            *self = end;
        }
        closed
    }

    /// Returns the length of the line end here: 1 for `\n`, 2 for `\r\n`, else 0.
    fn line_end(&self) -> usize {
        if self.rest.starts_with('\n') {
            1
        } else if self.rest.starts_with("\r\n") {
            2
        } else {
            0
        }
    }

    /// Moves past a `\n` or `\r\n`, and reports whether one was there.
    fn eat_newline(&mut self) -> bool {
        let len = self.line_end();
        self.skip_bytes(len);
        len > 0
    }

    /// Reads an escape after its backslash at `start`.
    fn escape(&mut self, start: Position) -> Result<char, Error> {
        let simple = match self.peek() {
            Some('n') => Some('\n'),
            Some('r') => Some('\r'),
            Some('t') => Some('\t'),
            Some('"') => Some('"'),
            Some('\\') => Some('\\'),
            _ => None,
        };
        let c = if simple.is_some() {
            self.bump();
            simple
        } else if self.eat('u') {
            self.unicode(4)
        } else if self.eat('U') {
            self.unicode(8)
        } else {
            if self.peek().is_some_and(|c| c != '\n') {
                self.bump();
            }
            None
        };
        c.ok_or_else(|| Error::Escape {
            span: self.span(start),
        })
    }

    fn unicode(&mut self, digits: usize) -> Option<char> {
        let mut value: u32 = 0;
        for _ in 0..digits {
            let digit = self.peek()?.to_digit(16)?;
            self.bump();
            value = value.checked_mul(16)?.checked_add(digit)?;
        }
        char::from_u32(value)
    }
}

/// Reports whether `c` can start an identifier.
pub(crate) fn identifier_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// Reports whether `c` can follow the first character of an identifier.
pub(crate) fn identifier_part(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

/// Reports whether `c` is whitespace inside a line, as a heredoc counts it: Unicode
/// whitespace other than `\n` and `\r`.
pub(crate) fn space(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\n' | '\r')
}

/// Removes the indent, in characters, that the lines of `text` that are not blank
/// share. A blank line holds only whitespace, and stays as written, as in HCL.
fn dedent(text: &str) -> String {
    let blank = |line: &str| line.trim_start_matches(space) == "\n";
    let shared = text
        .split_inclusive('\n')
        .filter(|line| !blank(line))
        .map(|line| line.chars().take_while(|&c| space(c)).count())
        .min()
        .unwrap_or(0);
    text.split_inclusive('\n')
        .map(|line| {
            if blank(line) {
                return line;
            }
            let mut chars = line.chars();
            chars.by_ref().take(shared).for_each(drop);
            chars.as_str()
        })
        .collect()
}

/// Refuses a text whose offsets do not fit in a span.
fn check_size(bytes: usize) -> Result<(), Error> {
    if u32::try_from(bytes).is_ok() {
        Ok(())
    } else {
        Err(Error::TooLarge { bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_a_text_past_the_largest_offset() {
        let bytes = usize::try_from(u32::MAX).unwrap();
        assert_eq!(check_size(bytes), Ok(()));
        let bytes = bytes.checked_add(1).unwrap();
        assert_eq!(check_size(bytes), Err(Error::TooLarge { bytes }));
    }
}

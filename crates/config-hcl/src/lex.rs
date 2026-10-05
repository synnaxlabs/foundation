use document::{Position, Source, Span};

use crate::{Error, Expected, Form};

/// A token and where it is.
#[derive(Clone, Debug)]
pub(crate) struct Token<'a> {
    pub(crate) kind: Kind,
    /// The token as written.
    pub(crate) text: &'a str,
    pub(crate) span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Letters, digits, `_`, `-`, and `@`, in parts split by single dots. It starts
    /// with a letter, `_`, or `@`.
    Word,
    /// Digits, then an optional fraction and an optional exponent.
    Number,
    /// A quoted string, with its escapes read.
    String(Box<str>),
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
}

/// Splits text into tokens, skipping spaces, tabs, and comments.
pub(crate) struct Lexer<'a> {
    source: Source,
    rest: &'a str,
    at: Position,
}

impl<'a> Lexer<'a> {
    /// Starts at the beginning of `text`, after a byte order mark.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Large`] when a span cannot count the bytes of `text`.
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

    /// Reads the next token.
    ///
    /// # Errors
    ///
    /// Returns an error for a string or comment that does not end, a string escape
    /// that HCL does not have, or a template in a string.
    pub(crate) fn next(&mut self) -> Result<Token<'a>, Error> {
        self.skip()?;
        let start = self.at;
        let rest = self.rest;
        let Some(c) = self.bump() else {
            return Ok(self.token(Kind::End, rest, start));
        };
        let kind = match c {
            '\n' => Kind::Newline,
            '\r' if self.eat('\n') => Kind::Newline,
            '"' => Kind::String(self.string(start)?),
            '=' => Kind::Equals,
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
            c if c.is_ascii_alphabetic() || c == '_' || c == '@' => {
                self.word();
                Kind::Word
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

    /// Moves the position past `c`.
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

    /// Moves past the next `len` bytes.
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

    /// Moves past the rest of a word after its first character.
    fn word(&mut self) {
        let bytes = self.rest.as_bytes();
        let part = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'@');
        let len = bytes
            .iter()
            .enumerate()
            .position(|(i, &b)| {
                let next = bytes.get(i.saturating_add(1)).copied();
                !(part(b) || b == b'.' && next.is_some_and(part))
            })
            .unwrap_or(bytes.len());
        self.skip_bytes(len);
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
                    if self.peek() == Some('{') {
                        self.bump();
                        return Err(Error::Form {
                            span: self.span(at),
                            form: Form::Template,
                        });
                    }
                    if self.peek() == Some(c) && self.second(|c| c == '{') {
                        self.bump();
                        self.bump();
                        text.push(c);
                        text.push('{');
                    } else {
                        text.push(c);
                    }
                }
                Some(c) => {
                    self.bump();
                    text.push(c);
                }
            }
        }
        unreachable!("invariant: each pass moves past a character")
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

/// Refuses a text whose offsets do not fit in a span.
fn check_size(bytes: usize) -> Result<(), Error> {
    if u32::try_from(bytes).is_ok() {
        Ok(())
    } else {
        Err(Error::Large { bytes })
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
        assert_eq!(check_size(bytes), Err(Error::Large { bytes }));
    }
}

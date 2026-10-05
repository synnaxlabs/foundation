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
    /// An [`identifier_start`], then [`identifier_part`]s.
    Identifier,
    /// A word with `.` or `@`, which only a reference can be: identifier parts, `@`,
    /// and dots, which `types::name` checks.
    Reference,
    /// A digit, then digits, dots, and exponents, as HCL scans a number. It can be
    /// text that is not a number, such as `1.2.3`.
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
        check_size(source, text.len())?;
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

    /// Reads the next token. Each token but [`Kind::End`] and [`Kind::Error`] covers
    /// one byte or more. Call it no more after `End` or `Error`.
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
            c if identifier_start(c) || c == '@' => self.word(c),
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

    /// The next character, or `None` at a line end or the end of the text.
    fn peek_in_line(&self) -> Option<char> {
        self.peek().filter(|_| self.line_end() == 0)
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
                Some('#') => self.line_comment(),
                Some('/') if self.second(|c| c == '/') => self.line_comment(),
                Some('/') if self.second(|c| c == '*') => self.block_comment()?,
                _ => return Ok(()),
            }
        }
        unreachable!("invariant: each pass moves past a character")
    }

    /// Moves past a comment up to the end of its line.
    fn line_comment(&mut self) {
        // Each pass moves past a character or returns.
        for _ in 0..=self.rest.len() {
            if self.peek_in_line().is_none() {
                return;
            }
            self.bump();
        }
        unreachable!("invariant: each pass moves past a character")
    }

    fn block_comment(&mut self) -> Result<(), Error> {
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

    /// Moves past the rest of a number after its first digit: digits, dots, and
    /// exponents such as `e5` or `E-5`, up to the last that is not a dot.
    fn number(&mut self) {
        let bytes = self.rest.as_bytes();
        let (mut at, mut end) = (0, 0);
        while let Some(rest) = bytes.get(at..) {
            let len = match rest {
                [b'0'..=b'9' | b'.', ..] => 1,
                [b'e' | b'E', b'0'..=b'9', ..] => 2,
                [b'e' | b'E', b'+' | b'-', b'0'..=b'9', ..] => 3,
                _ => break,
            };
            at = at.saturating_add(len);
            if rest.first() != Some(&b'.') {
                end = at;
            }
        }
        self.skip_bytes(end);
    }

    /// Moves past the rest of a word after its first character, `first`.
    fn word(&mut self, first: char) -> Kind {
        let rest = self.rest;
        // A dot is part of the word unless it starts a splat or an expansion.
        let dot = |i: usize| {
            let after = rest.get(i..).unwrap_or_default();
            !after.starts_with(".*") && !after.starts_with("...")
        };
        let len = rest
            .char_indices()
            .find(|&(i, c)| !(identifier_part(c) || c == '@' || c == '.' && dot(i)))
            .map_or(rest.len(), |(i, _)| i);
        let word = rest
            .get(..len)
            .expect("invariant: a word ends on a character boundary");
        self.skip_bytes(len);
        if first == '@' || word.contains(['.', '@']) {
            Kind::Reference
        } else {
            Kind::Identifier
        }
    }

    /// Reads a quoted string after its opening quote at `start`.
    fn string(&mut self, start: Position) -> Result<Box<str>, Error> {
        let mut text = String::new();
        // Each pass moves past a character or returns.
        for _ in 0..=self.rest.len() {
            let at = self.at;
            match self.peek_in_line() {
                None => {
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
            if self.peek_in_line().is_some() {
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

/// The kind of the word that all of `text` reads as: [`Kind::Identifier`] or
/// [`Kind::Reference`]. `None` when `text` reads as any other token, or as more than
/// one.
pub(crate) fn word(text: &str) -> Option<Kind> {
    let token = Tokens::new(Source(0), text).ok()?.next();
    (matches!(token.kind, Kind::Identifier | Kind::Reference) && token.text == text)
        .then_some(token.kind)
}

/// Reports whether `c` can start an identifier. HCL uses `ID_Start`, and the
/// compatibility characters outside `XID_Start` are refused on purpose.
pub(crate) fn identifier_start(c: char) -> bool {
    unicode_ident::is_xid_start(c) || c == '_'
}

/// Reports whether `c` can follow the first character of an identifier.
pub(crate) fn identifier_part(c: char) -> bool {
    unicode_ident::is_xid_continue(c) || c == '-'
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
fn check_size(source: Source, bytes: usize) -> Result<(), Error> {
    if u32::try_from(bytes).is_ok() {
        return Ok(());
    }
    let start = Position {
        offset: 0,
        line: 0,
        column: 0,
    };
    let span =
        Span::new(source, start, start).expect("invariant: an empty span is ordered");
    Err(Error::TooLarge { span, bytes })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn each_token_but_the_last_has_a_byte(
            text in "[a-z0-9 \t\r\n\"\\\\\\{}\\[\\]().,:=<>!&|+*/%?#@$-]{0,40}|\\PC{0,20}"
        ) {
            let mut tokens = Tokens::new(Source(0), &text).unwrap();
            for _ in 0..=text.len() {
                let token = tokens.next();
                if matches!(token.kind, Kind::End | Kind::Error(_)) {
                    return Ok(());
                }
                prop_assert!(!token.text.is_empty(), "{:?} has no text", token.kind);
            }
            prop_assert!(false, "more tokens than bytes in {text:?}");
        }
    }

    /// The kind and text of each token of `text` before the end, through the first
    /// error.
    fn tokens(text: &str) -> Vec<(Kind, &str)> {
        let mut tokens = Tokens::new(Source(0), text).unwrap();
        let mut found = Vec::new();
        for _ in 0..=text.len() {
            let token = tokens.next();
            match token.kind {
                Kind::End => return found,
                Kind::Error(_) => {
                    found.push((token.kind, token.text));
                    return found;
                }
                kind => found.push((kind, token.text)),
            }
        }
        panic!("more tokens than bytes in {text:?}")
    }

    #[test]
    fn scans_a_number_as_hcl_does() {
        use Kind::{Dot, Ellipsis, Identifier, Number, Operator};
        let cases: [(&str, &[(Kind, &str)]); 15] = [
            ("1.5e3", &[(Number, "1.5e3")]),
            ("1.e5", &[(Number, "1.e5")]),
            ("1.E+5", &[(Number, "1.E+5")]),
            ("0.e-5", &[(Number, "0.e-5")]),
            ("1.2.3", &[(Number, "1.2.3")]),
            ("1e5e5", &[(Number, "1e5e5")]),
            ("1..5", &[(Number, "1..5")]),
            ("1.e5.e5", &[(Number, "1.e5.e5")]),
            ("1.", &[(Number, "1"), (Dot, ".")]),
            ("1e5.", &[(Number, "1e5"), (Dot, ".")]),
            ("1...", &[(Number, "1"), (Ellipsis, "...")]),
            ("1e", &[(Number, "1"), (Identifier, "e")]),
            ("1.e", &[(Number, "1"), (Dot, "."), (Identifier, "e")]),
            (
                "1.e+",
                &[
                    (Number, "1"),
                    (Dot, "."),
                    (Identifier, "e"),
                    (Operator, "+"),
                ],
            ),
            ("1.ex", &[(Number, "1"), (Dot, "."), (Identifier, "ex")]),
        ];
        for (text, expected) in cases {
            assert_eq!(tokens(text), expected, "{text:?}");
        }
    }

    #[test]
    fn ends_a_line_comment_before_its_line_end() {
        let expected = [(Kind::Newline, "\r\n"), (Kind::Newline, "\r\n")];
        assert_eq!(tokens("# a\r\n// b\r\n"), expected);
    }

    #[test]
    fn refuses_a_text_past_the_largest_offset() {
        let bytes = usize::try_from(u32::MAX).unwrap();
        assert_eq!(check_size(Source(3), bytes), Ok(()));
        let bytes = bytes.checked_add(1).unwrap();
        let start = Position {
            offset: 0,
            line: 0,
            column: 0,
        };
        let span = Span::new(Source(3), start, start).unwrap();
        assert_eq!(
            check_size(Source(3), bytes),
            Err(Error::TooLarge { span, bytes })
        );
    }
}

use document::encoding::DEPTH_MAX;
use document::value::{self, Call, Float, Value};
use document::{Attribute, Block, Document, Label, Map, Source, Span};
use types::name::Name;

use crate::lex::{self, Token, Tokens};
use crate::{Error, Expected, Form};

/// Reads HCL text as a Document. Each key, keyword, label, function name, and value
/// has a span in `source`. A heredoc's lines end in `\n`, whatever the file uses. An
/// integer key in an object reads as HCL reads it: its digits without leading zeros,
/// after a `-` if it has one.
///
/// # Errors
///
/// Returns each problem found, in source order. A syntax error, a string escape that
/// HCL does not have, a template, or nesting past the limit stops reading, so it is
/// the last one.
pub fn read(source: Source, text: &str) -> Result<Document, Vec<Error>> {
    let mut tokens = Tokens::new(source, text).map_err(|error| vec![error])?;
    let token = tokens.next();
    let mut parser = Parser {
        tokens,
        token,
        errors: Vec::new(),
    };
    let document = parser.file();
    let mut errors = parser.errors;
    match document {
        Ok(document) if errors.is_empty() => return Ok(document),
        Ok(_) => {}
        Err(error) => errors.push(error),
    }
    errors.sort_by_key(Error::offset);
    Err(errors)
}

/// What ends an item, besides a close bracket at its level and the end of the text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ends {
    /// A `,` or a new line, as in a body or an object.
    Line,
    /// A `,`, as in a list or a call, where new lines do not end an item.
    Comma,
    /// Only the close bracket, as in a `for` expression.
    Close,
}

struct Parser<'a> {
    tokens: Tokens<'a>,
    /// The next token, not yet taken.
    token: Token<'a>,
    /// Problems that do not stop reading.
    errors: Vec<Error>,
}

impl<'a> Parser<'a> {
    fn file(&mut self) -> Result<Document, Error> {
        let document = self.body(0)?;
        if self.token.kind == lex::Kind::End {
            Ok(document)
        } else {
            Err(self.syntax(Expected::Item))
        }
    }

    /// Reads attributes and blocks up to `}` or the end, and leaves that token.
    fn body(&mut self, depth: usize) -> Result<Document, Error> {
        let mut attributes = Vec::new();
        let mut blocks = Vec::new();
        let items = self.items(&mut attributes, &mut blocks, depth);
        let attributes = self.map(attributes);
        items?;
        Ok(Document { attributes, blocks })
    }

    fn items(
        &mut self,
        attributes: &mut Vec<Attribute>,
        blocks: &mut Vec<Block>,
        depth: usize,
    ) -> Result<(), Error> {
        loop {
            self.skip_newlines()?;
            match self.token.kind {
                lex::Kind::End | lex::Kind::CloseBrace => return Ok(()),
                lex::Kind::Identifier => {}
                _ => return Err(self.syntax(Expected::Item)),
            }
            let name = self.take()?;
            if self.token.kind == lex::Kind::Equals {
                self.take()?;
                let value = self.value(depth, Ends::Line)?;
                self.end_line()?;
                attributes.extend(value.map(|value| attribute(name, value)));
            } else {
                blocks.push(self.block(&name, depth)?);
            }
        }
    }

    fn block(&mut self, keyword: &Token<'a>, depth: usize) -> Result<Block, Error> {
        let mut labels = Vec::new();
        loop {
            match self.token.kind {
                lex::Kind::String(_) | lex::Kind::Identifier => {}
                lex::Kind::OpenBrace => break,
                _ if labels.is_empty() => {
                    return Err(self.syntax(Expected::AttributeOrBlock));
                }
                _ => return Err(self.syntax(Expected::BlockStart)),
            }
            let label = self.take()?;
            let span = Some(label.span);
            labels.push(Label {
                text: text(label),
                span,
            });
        }
        let depth = enter(depth).ok_or(Error::TooDeep { span: keyword.span })?;
        self.take()?;
        let body = if self.token.kind == lex::Kind::Newline {
            let body = self.body(depth)?;
            if self.token.kind != lex::Kind::CloseBrace {
                return Err(self.syntax(Expected::Item));
            }
            body
        } else {
            self.one_line(depth)?
        };
        let end = self.take()?.span;
        self.end_line()?;
        Ok(Block {
            keyword: keyword.text.into(),
            keyword_span: Some(keyword.span),
            labels,
            body,
            span: Some(join(keyword.span, end)),
        })
    }

    /// Reads the body of a one-line block, which holds one attribute or none, and
    /// leaves the `}`.
    fn one_line(&mut self, depth: usize) -> Result<Document, Error> {
        let mut attributes = Vec::new();
        if self.token.kind == lex::Kind::Identifier {
            let key = self.take()?;
            if self.token.kind != lex::Kind::Equals {
                return Err(self.syntax(Expected::Equals));
            }
            self.take()?;
            let value = self.value(depth, Ends::Line)?;
            attributes.extend(value.map(|value| attribute(key, value)));
            if self.token.kind != lex::Kind::CloseBrace {
                return Err(self.syntax(Expected::BlockEnd));
            }
        } else if self.token.kind != lex::Kind::CloseBrace {
            return Err(self.syntax(Expected::Key));
        }
        Ok(Document {
            attributes: self.map(attributes),
            blocks: Vec::new(),
        })
    }

    /// Reads a value in an item that `ends` ends. Returns `None` when the value has a
    /// problem that does not stop reading.
    fn value(&mut self, depth: usize, ends: Ends) -> Result<Option<Value>, Error> {
        if let Some(form) = self.leading_form() {
            self.refuse(form, ends)?;
            return Ok(None);
        }
        let value = self.term(depth)?;
        if ends == Ends::Comma {
            self.skip_newlines()?;
        }
        if let Some(form) = self.trailing_form() {
            self.refuse(form, ends)?;
            return Ok(None);
        }
        Ok(value)
    }

    /// Reads a value that starts with no HCL form.
    fn term(&mut self, depth: usize) -> Result<Option<Value>, Error> {
        if self.token.kind == lex::Kind::End {
            return Err(self.syntax(Expected::Value));
        }
        let token = self.take()?;
        let kind = match token.kind {
            lex::Kind::Identifier | lex::Kind::Reference => {
                return self.word(&token, depth);
            }
            lex::Kind::Number => return Ok(self.number(&token, None)),
            lex::Kind::Minus if self.token.kind == lex::Kind::Number => {
                let digits = self.take()?;
                return Ok(self.number(&digits, Some(token.span)));
            }
            lex::Kind::String(text) | lex::Kind::Heredoc(text) => {
                value::Kind::String(text)
            }
            lex::Kind::OpenBracket => return self.list(&token, depth).map(Some),
            lex::Kind::OpenBrace => return self.object(&token, depth).map(Some),
            _ => {
                return Err(Error::Syntax {
                    span: token.span,
                    expected: Expected::Value,
                });
            }
        };
        Ok(Some(Value {
            kind,
            span: Some(token.span),
        }))
    }

    /// The HCL form that the next token starts, where a value must start.
    fn leading_form(&self) -> Option<Form> {
        match self.token.kind {
            lex::Kind::Operator | lex::Kind::Star => Some(Form::Operator),
            lex::Kind::Minus if self.after().kind != lex::Kind::Number => {
                Some(Form::Operator)
            }
            lex::Kind::OpenParenthesis => Some(Form::Parentheses),
            _ => None,
        }
    }

    /// The HCL form that the next token starts, after a value.
    fn trailing_form(&self) -> Option<Form> {
        match self.token.kind {
            lex::Kind::Operator | lex::Kind::Minus | lex::Kind::Star => {
                Some(Form::Operator)
            }
            lex::Kind::Question => Some(Form::Conditional),
            lex::Kind::OpenBracket | lex::Kind::Dot
                if self.after().kind == lex::Kind::Star =>
            {
                Some(Form::Splat)
            }
            lex::Kind::OpenBracket | lex::Kind::Dot => Some(Form::Index),
            lex::Kind::DoubleColon => Some(Form::Namespace),
            lex::Kind::Ellipsis => Some(Form::Expansion),
            _ => None,
        }
    }

    /// Refuses a `for` expression at the start of a list or an object.
    fn refuse_for(&mut self) -> Result<(), Error> {
        self.skip_newlines()?;
        if self.token.kind == lex::Kind::Identifier
            && self.token.text == "for"
            && self.after().kind == lex::Kind::Identifier
        {
            self.refuse(Form::For, Ends::Close)?;
        }
        Ok(())
    }

    /// Keeps the problem of `form` at the next token, then moves past the rest of the
    /// item to the token that ends it, and leaves that token.
    fn refuse(&mut self, form: Form, ends: Ends) -> Result<(), Error> {
        self.errors.push(Error::Form {
            span: self.token.span,
            form,
        });
        let mut depth = 0usize;
        loop {
            match self.token.kind {
                lex::Kind::End => return Ok(()),
                lex::Kind::OpenBrace
                | lex::Kind::OpenBracket
                | lex::Kind::OpenParenthesis => depth = depth.saturating_add(1),
                lex::Kind::CloseBrace
                | lex::Kind::CloseBracket
                | lex::Kind::CloseParenthesis => match depth.checked_sub(1) {
                    Some(outer) => depth = outer,
                    None => return Ok(()),
                },
                lex::Kind::Comma if depth == 0 && ends != Ends::Close => return Ok(()),
                lex::Kind::Newline if depth == 0 && ends == Ends::Line => {
                    return Ok(());
                }
                _ => {}
            }
            self.take()?;
        }
    }

    fn word(&mut self, word: &Token<'a>, depth: usize) -> Result<Option<Value>, Error> {
        let kind = match word.text {
            "true" => value::Kind::Bool(true),
            "false" => value::Kind::Bool(false),
            "null" => {
                self.errors.push(Error::Form {
                    span: word.span,
                    form: Form::Null,
                });
                return Ok(None);
            }
            _ if self.token.kind == lex::Kind::OpenParenthesis
                && word.kind == lex::Kind::Identifier =>
            {
                return self.call(word, depth).map(Some);
            }
            text => match text.parse::<Name>() {
                Ok(name) => value::Kind::Reference(name),
                Err(error) => {
                    self.errors.push(Error::Name {
                        span: word.span,
                        error,
                    });
                    return Ok(None);
                }
            },
        };
        Ok(Some(Value {
            kind,
            span: Some(word.span),
        }))
    }

    /// Reads a number, after its minus sign if `minus` holds the sign's span. Returns
    /// `None` when the number is out of range.
    fn number(&mut self, digits: &Token<'a>, minus: Option<Span>) -> Option<Value> {
        let span = minus.map_or(digits.span, |minus| join(minus, digits.span));
        let kind = if digits.text.bytes().all(|b| b.is_ascii_digit()) {
            let magnitude = digits.text.parse::<u128>().ok();
            magnitude
                .and_then(|n| match minus {
                    Some(_) => 0i128.checked_sub_unsigned(n),
                    None => i128::try_from(n).ok(),
                })
                .map(value::Kind::Integer)
        } else {
            let float = digits.text.parse::<f64>().ok();
            float
                .filter(|&f| f != 0.0 || !significant(digits.text))
                .map(|f| if minus.is_some() { -f } else { f })
                .and_then(Float::new)
                .map(value::Kind::Float)
        };
        if kind.is_none() {
            self.errors.push(Error::Number { span });
        }
        kind.map(|kind| Value {
            kind,
            span: Some(span),
        })
    }

    fn list(&mut self, open: &Token<'a>, depth: usize) -> Result<Value, Error> {
        let depth = enter(depth).ok_or(Error::TooDeep { span: open.span })?;
        self.refuse_for()?;
        let mut items = Vec::new();
        let close = loop {
            self.skip_newlines()?;
            if self.token.kind == lex::Kind::CloseBracket {
                break self.take()?;
            }
            items.extend(self.value(depth, Ends::Comma)?);
            self.skip_newlines()?;
            match self.token.kind {
                lex::Kind::Comma => {
                    self.take()?;
                }
                lex::Kind::CloseBracket => break self.take()?,
                _ => return Err(self.syntax(Expected::ListEnd)),
            }
        };
        Ok(Value {
            kind: value::Kind::List(items),
            span: Some(join(open.span, close.span)),
        })
    }

    fn object(&mut self, open: &Token<'a>, depth: usize) -> Result<Value, Error> {
        let depth = enter(depth).ok_or(Error::TooDeep { span: open.span })?;
        let mut attributes = Vec::new();
        let close = self.entries(&mut attributes, depth);
        let map = self.map(attributes);
        Ok(Value {
            kind: value::Kind::Map(map),
            span: Some(join(open.span, close?.span)),
        })
    }

    /// Reads the entries of an object after its `{`, and returns the `}`.
    fn entries(
        &mut self,
        attributes: &mut Vec<Attribute>,
        depth: usize,
    ) -> Result<Token<'a>, Error> {
        self.refuse_for()?;
        loop {
            self.skip_newlines()?;
            match self.token.kind {
                lex::Kind::CloseBrace => return self.take(),
                _ => {
                    if let Some(form) = self.leading_form() {
                        self.refuse(form, Ends::Line)?;
                    } else if let Some((key, key_span)) = self.key()? {
                        if !matches!(
                            self.token.kind,
                            lex::Kind::Equals | lex::Kind::Colon
                        ) {
                            return Err(self.syntax(Expected::ObjectEquals));
                        }
                        self.take()?;
                        let value = self.value(depth, Ends::Line)?;
                        attributes.extend(value.map(|value| Attribute {
                            key,
                            key_span: Some(key_span),
                            value,
                        }));
                    }
                }
            }
            match self.token.kind {
                lex::Kind::Comma | lex::Kind::Newline => {
                    self.take()?;
                }
                lex::Kind::CloseBrace => return self.take(),
                _ => return Err(self.syntax(Expected::ObjectEnd)),
            }
        }
    }

    /// Reads an object key, where no HCL form starts: a string, an identifier, or an
    /// integer, which reads as HCL reads it: its digits without leading zeros, after a
    /// `-` if it has one. Returns `None` for a number that HCL rounds, after it keeps
    /// the problem and moves past the entry. Any other token is `Expected::Key`.
    fn key(&mut self) -> Result<Option<(Box<str>, Span)>, Error> {
        if matches!(
            self.token.kind,
            lex::Kind::String(_) | lex::Kind::Identifier
        ) {
            let key = self.take()?;
            let span = key.span;
            return Ok(Some((text(key), span)));
        }
        let minus = if self.token.kind == lex::Kind::Minus {
            Some(self.take()?)
        } else {
            None
        };
        if self.token.kind != lex::Kind::Number {
            return Err(self.syntax(Expected::Key));
        }
        let Some(digits) = integer_key(self.token.text) else {
            self.refuse(Form::NumberKey, Ends::Line)?;
            return Ok(None);
        };
        let number = self.take()?;
        Ok(Some(match minus {
            Some(minus) => (format!("-{digits}").into(), join(minus.span, number.span)),
            None => (digits.into(), number.span),
        }))
    }

    fn call(&mut self, function: &Token<'a>, depth: usize) -> Result<Value, Error> {
        let depth = enter(depth).ok_or(Error::TooDeep {
            span: function.span,
        })?;
        self.take()?;
        let mut arguments = Vec::new();
        let close = loop {
            self.skip_newlines()?;
            if self.token.kind == lex::Kind::CloseParenthesis {
                break self.take()?;
            }
            arguments.extend(self.value(depth, Ends::Comma)?);
            self.skip_newlines()?;
            match self.token.kind {
                lex::Kind::Comma => {
                    self.take()?;
                }
                lex::Kind::CloseParenthesis => break self.take()?,
                _ => return Err(self.syntax(Expected::ArgumentsEnd)),
            }
        };
        Ok(Value {
            kind: value::Kind::Call(Call {
                function: function.text.into(),
                function_span: Some(function.span),
                arguments,
            }),
            span: Some(join(function.span, close.span)),
        })
    }

    /// Builds a map, and keeps its problems. The map is empty when it has problems,
    /// because `read` then returns no document.
    fn map(&mut self, attributes: Vec<Attribute>) -> Map {
        Map::new(attributes).unwrap_or_else(|errors| {
            self.errors.extend(errors.into_iter().map(Error::Document));
            Map::default()
        })
    }

    fn end_line(&mut self) -> Result<(), Error> {
        match self.token.kind {
            lex::Kind::Newline => self.take().map(drop),
            lex::Kind::End => Ok(()),
            _ => Err(self.syntax(Expected::Newline)),
        }
    }

    fn skip_newlines(&mut self) -> Result<(), Error> {
        while self.token.kind == lex::Kind::Newline {
            self.take()?;
        }
        Ok(())
    }

    /// Takes the next token and reads the one after it.
    ///
    /// # Errors
    ///
    /// Returns the lexer's error when the next token is [`lex::Kind::Error`].
    ///
    /// # Panics
    ///
    /// Panics at the end of the text, which no rule takes. So every loop that takes
    /// tokens ends.
    fn take(&mut self) -> Result<Token<'a>, Error> {
        if let lex::Kind::Error(error) = &self.token.kind {
            return Err(error.clone());
        }
        assert!(
            self.token.kind != lex::Kind::End,
            "invariant: no rule takes the end, at {:?}",
            self.token.span
        );
        let next = self.tokens.next();
        Ok(std::mem::replace(&mut self.token, next))
    }

    /// The first token after the next one that is not a new line.
    fn after(&self) -> Token<'a> {
        self.tokens.clone().next_past_lines()
    }

    /// The problem at the next token: the lexer's error, or `expected`.
    fn syntax(&self, expected: Expected) -> Error {
        match &self.token.kind {
            lex::Kind::Error(error) => error.clone(),
            _ => Error::Syntax {
                span: self.token.span,
                expected,
            },
        }
    }
}

/// The digits of an integer key without leading zeros, or `None` when HCL rounds the
/// number. HCL reads a number key through a 512-bit float, which holds each integer of
/// up to 154 digits.
fn integer_key(number: &str) -> Option<&str> {
    let digits = match number.trim_start_matches('0') {
        "" => "0",
        digits => digits,
    };
    (digits.len() <= 154 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then_some(digits)
}

fn attribute(key: Token<'_>, value: Value) -> Attribute {
    let key_span = Some(key.span);
    Attribute {
        key: text(key),
        key_span,
        value,
    }
}

/// The text of a key or a label: a string's contents, or the identifier.
fn text(token: Token<'_>) -> Box<str> {
    match token.kind {
        lex::Kind::String(text) => text,
        lex::Kind::Identifier => token.text.into(),
        kind => unreachable!("invariant: a key or a label is not {kind:?}"),
    }
}

/// Reports whether the digits of a float before its exponent are not all zero.
fn significant(digits: &str) -> bool {
    digits
        .bytes()
        .take_while(|b| !matches!(b, b'e' | b'E'))
        .any(|b| matches!(b, b'1'..=b'9'))
}

/// The depth inside one more level, or `None` past [`DEPTH_MAX`].
fn enter(depth: usize) -> Option<usize> {
    depth.checked_add(1).filter(|&inner| inner <= DEPTH_MAX)
}

fn join(start: Span, end: Span) -> Span {
    Span::new(start.source(), start.start(), end.end())
        .expect("invariant: `end` ends after `start` starts")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrary::{document, write};
    use document::Position;
    use proptest::prelude::*;

    fn at(offset: u32, line: u32, column: u32) -> Position {
        Position {
            offset,
            line,
            column,
        }
    }

    fn span(start: Position, end: Position) -> Span {
        Span::new(Source(0), start, end).unwrap()
    }

    /// A span on the first line of an ASCII text.
    fn on(start: u32, end: u32) -> Span {
        span(at(start, 0, start), at(end, 0, end))
    }

    fn ok(text: &str) -> Document {
        read(Source(0), text).unwrap()
    }

    fn value(kind: value::Kind) -> Value {
        Value { kind, span: None }
    }

    fn attributes(attributes: Vec<(&str, value::Kind)>) -> Document {
        Document {
            attributes: map(attributes),
            blocks: Vec::new(),
        }
    }

    fn map(attributes: Vec<(&str, value::Kind)>) -> Map {
        let attributes = attributes
            .into_iter()
            .map(|(key, kind)| Attribute {
                key: key.into(),
                key_span: None,
                value: value(kind),
            })
            .collect();
        Map::new(attributes).unwrap()
    }

    fn integer(n: i128) -> value::Kind {
        value::Kind::Integer(n)
    }

    fn float(f: f64) -> value::Kind {
        value::Kind::Float(Float::new(f).unwrap())
    }

    fn string(text: &str) -> value::Kind {
        value::Kind::String(text.into())
    }

    fn reference(name: &str) -> value::Kind {
        value::Kind::Reference(name.parse().unwrap())
    }

    fn block(keyword: &str, labels: &[&str], body: Document) -> Block {
        Block {
            keyword: keyword.into(),
            keyword_span: None,
            labels: labels
                .iter()
                .map(|&text| Label {
                    text: text.into(),
                    span: None,
                })
                .collect(),
            body,
            span: None,
        }
    }

    fn check(text: &str, expected: &[(Error, &str)]) {
        let errors = read(Source(0), text).unwrap_err();
        let messages: Vec<String> = errors.iter().map(ToString::to_string).collect();
        let (errors_expected, messages_expected): (Vec<Error>, Vec<&str>) =
            expected.iter().cloned().unzip();
        assert_eq!(errors, errors_expected);
        assert_eq!(messages, messages_expected);
    }

    fn syntax(span: Span, expected: Expected) -> Error {
        Error::Syntax { span, expected }
    }

    const TEMPLATE: &str = "templates do not exist in Foundation files. Write `$${` \
                            or `%%{` for the text `${` or `%{`";
    const NULL: &str = "`null` does not exist in Foundation files. Remove the \
                        attribute to use its default";

    mod values {
        use super::*;

        #[test]
        fn reads_booleans() {
            let expected = attributes(vec![
                ("a", value::Kind::Bool(true)),
                ("b", value::Kind::Bool(false)),
            ]);
            assert_eq!(ok("a = true\nb = false\n"), expected);
        }

        #[test]
        fn reads_integers_exactly() {
            let text = "a = 42\nb = -3\n\
                        c = 170141183460469231731687303715884105727\n\
                        d = -170141183460469231731687303715884105728\n\
                        e = - 7\nf = 007\n";
            let expected = attributes(vec![
                ("a", integer(42)),
                ("b", integer(-3)),
                ("c", integer(i128::MAX)),
                ("d", integer(i128::MIN)),
                ("e", integer(-7)),
                ("f", integer(7)),
            ]);
            assert_eq!(ok(text), expected);
        }

        #[test]
        fn reads_floats() {
            let text = "a = 1.5\nb = -0.25\nc = 1e3\nd = 2.5E-3\ne = -0.0\nf = 1e+2\n";
            let expected = attributes(vec![
                ("a", float(1.5)),
                ("b", float(-0.25)),
                ("c", float(1000.0)),
                ("d", float(0.0025)),
                ("e", float(0.0)),
                ("f", float(100.0)),
            ]);
            assert_eq!(ok(text), expected);
        }

        #[test]
        fn reads_string_escapes() {
            let text = r#"a = "x\n\r\t\"\\\u00b0\U0001F600"
b = "$${x} %%{y} $ % $$ %% $$${z}"
c = "°C # not a comment"
"#;
            let expected = attributes(vec![
                ("a", string("x\n\r\t\"\\°\u{1F600}")),
                ("b", string("${x} %{y} $ % $$ %% $${z}")),
                ("c", string("°C # not a comment")),
            ]);
            assert_eq!(ok(text), expected);
        }

        #[test]
        fn reads_references() {
            let text = "a = site_a.pt_1\nb = site_a.1\nc = @system.x\nd = a-b.c-d\n";
            let expected = attributes(vec![
                ("a", reference("site_a.pt_1")),
                ("b", reference("site_a.1")),
                ("c", reference("@system.x")),
                ("d", reference("a-b.c-d")),
            ]);
            assert_eq!(ok(text), expected);
        }

        #[test]
        fn reads_lists() {
            let text = "a = []\nb = [1, 2,]\nc = [\n  1,\n\n  [true]\n]\n";
            let expected = attributes(vec![
                ("a", value::Kind::List(Vec::new())),
                (
                    "b",
                    value::Kind::List(vec![value(integer(1)), value(integer(2))]),
                ),
                (
                    "c",
                    value::Kind::List(vec![
                        value(integer(1)),
                        value(value::Kind::List(vec![value(value::Kind::Bool(true))])),
                    ]),
                ),
            ]);
            assert_eq!(ok(text), expected);
        }

        #[test]
        fn reads_objects() {
            let text =
                "a = {}\nb = { k = 1, \"a b\" = 2 }\nc = {\n  x: 1\n  y = 2,\n\n}\n";
            let expected = attributes(vec![
                ("a", value::Kind::Map(Map::default())),
                (
                    "b",
                    value::Kind::Map(map(vec![("k", integer(1)), ("a b", integer(2))])),
                ),
                (
                    "c",
                    value::Kind::Map(map(vec![("x", integer(1)), ("y", integer(2))])),
                ),
            ]);
            assert_eq!(ok(text), expected);
        }

        #[test]
        fn reads_integer_keys_as_hcl_does() {
            let digits = "9".repeat(154);
            let text = format!(
                "a = {{ 40001 = 1, 007 = 2, -12 = 3, -0 = 4, 00{digits} = 5 }}"
            );
            let document = ok(&text);
            let a = document.attributes.get("a").unwrap();
            let value::Kind::Map(object) = &a.value.kind else {
                panic!("not a map: {a:?}");
            };
            let keys = [
                ("40001", 6, 11, 1),
                ("7", 17, 20, 2),
                ("-12", 26, 29, 3),
                ("-0", 35, 37, 4),
                (digits.as_str(), 43, 199, 5),
            ];
            assert_eq!(object.iter().len(), keys.len());
            for (key, start, end, n) in keys {
                let entry = object.get(key).unwrap();
                assert_eq!(entry.key_span, Some(on(start, end)), "{key}");
                assert_eq!(entry.value.kind, integer(n), "{key}");
            }
        }

        #[test]
        fn reads_non_ascii_text_in_strings() {
            let expected = attributes(vec![
                ("a", string("température")),
                (
                    "b",
                    value::Kind::Map(map(vec![("température", integer(1))])),
                ),
            ]);
            assert_eq!(
                ok("a = \"température\"\nb = { \"température\" = 1 }\n"),
                expected
            );
        }

        #[test]
        fn reads_for_as_a_word_when_no_identifier_follows() {
            let expected = attributes(vec![
                ("a", value::Kind::List(vec![value(reference("for"))])),
                ("b", value::Kind::Map(map(vec![("for", integer(1))]))),
            ]);
            assert_eq!(ok("a = [for]\nb = { for = 1 }\n"), expected);
        }

        #[test]
        fn reads_calls() {
            let text = "a = secret(\"pw\")\nb = f()\nc = g(1, [2], h(3),)\n\
                        d = f(\n  1,\n  2\n)\n";
            let call = |function: &str, arguments: Vec<Value>| {
                value::Kind::Call(Call {
                    function: function.into(),
                    function_span: None,
                    arguments,
                })
            };
            let expected = attributes(vec![
                ("a", call("secret", vec![value(string("pw"))])),
                ("b", call("f", Vec::new())),
                (
                    "c",
                    call(
                        "g",
                        vec![
                            value(integer(1)),
                            value(value::Kind::List(vec![value(integer(2))])),
                            value(call("h", vec![value(integer(3))])),
                        ],
                    ),
                ),
                ("d", call("f", vec![value(integer(1)), value(integer(2))])),
            ]);
            assert_eq!(ok(text), expected);
        }
    }

    mod blocks {
        use super::*;

        #[test]
        fn reads_labels_and_nested_blocks() {
            let text = "connector \"opcua\" \"site_a.plc_7\" {\n  \
                        url = \"opc.tcp://x\"\n\n  in {\n    \
                        channel = site_a.pt_1\n  }\n}\n";
            let input = block(
                "in",
                &[],
                attributes(vec![("channel", reference("site_a.pt_1"))]),
            );
            let mut body = attributes(vec![("url", string("opc.tcp://x"))]);
            body.blocks.push(input);
            let expected = Document {
                attributes: Map::default(),
                blocks: vec![block("connector", &["opcua", "site_a.plc_7"], body)],
            };
            assert_eq!(ok(text), expected);
        }

        #[test]
        fn reads_identifier_labels() {
            let expected = Document {
                attributes: Map::default(),
                blocks: vec![block("resource", &["aws", "x-1"], Document::default())],
            };
            assert_eq!(ok("resource aws x-1 {\n}\n"), expected);
        }

        #[test]
        fn reads_one_line_blocks() {
            let text = "a {}\nb { x = 1 }\nc \"l\" { y = [1, 2] }";
            let expected = Document {
                attributes: Map::default(),
                blocks: vec![
                    block("a", &[], Document::default()),
                    block("b", &[], attributes(vec![("x", integer(1))])),
                    block(
                        "c",
                        &["l"],
                        attributes(vec![(
                            "y",
                            value::Kind::List(vec![
                                value(integer(1)),
                                value(integer(2)),
                            ]),
                        )]),
                    ),
                ],
            };
            assert_eq!(ok(text), expected);
        }

        #[test]
        fn keeps_block_order() {
            let channel = |label| block("channel", &[label], Document::default());
            let expected = Document {
                attributes: Map::default(),
                blocks: vec![channel("b"), channel("a"), channel("b")],
            };
            assert_eq!(
                ok("channel \"b\" {}\nchannel \"a\" {}\nchannel \"b\" {}\n"),
                expected
            );
        }
    }

    mod lines {
        use super::*;

        #[test]
        fn reads_an_empty_file() {
            assert_eq!(ok(""), Document::default());
            assert_eq!(ok("\n\n# only a comment\n"), Document::default());
        }

        #[test]
        fn skips_comments() {
            let text =
                "# c\na = 1 # c\n// c\nb = /* c */ 2\n/* multi\nline */\nc = 3 // c";
            let expected = attributes(vec![
                ("a", integer(1)),
                ("b", integer(2)),
                ("c", integer(3)),
            ]);
            assert_eq!(ok(text), expected);
        }

        #[test]
        fn reads_tabs() {
            assert_eq!(ok("a\t=\t1\t# c\n"), attributes(vec![("a", integer(1))]));
        }

        #[test]
        fn reads_crlf_lines() {
            let document = ok("a = 1\r\nb = \"x\"\r\n");
            let b = document.attributes.get("b").unwrap();
            assert_eq!(b.value, value(string("x")));
            assert_eq!(b.key_span, Some(span(at(7, 1, 0), at(8, 1, 1))));
        }

        #[test]
        fn skips_a_byte_order_mark() {
            let document = ok("\u{feff}a = 1\n");
            let a = document.attributes.get("a").unwrap();
            assert_eq!(a.key_span, Some(span(at(3, 0, 0), at(4, 0, 1))));
        }
    }

    mod spans {
        use super::*;

        #[test]
        fn names_each_part() {
            let text = "m = { u = \"°C\", v = 1 }\nb \"l\" {\n  r = f(x)\n}\n";
            let document = ok(text);
            let outer = document.attributes.get("m").unwrap();
            assert_eq!(outer.key_span, Some(on(0, 1)));
            assert_eq!(outer.value.span, Some(span(at(4, 0, 4), at(24, 0, 23))));
            let value::Kind::Map(object) = &outer.value.kind else {
                panic!("not a map: {outer:?}");
            };
            let u = object.get("u").unwrap();
            assert_eq!(u.key_span, Some(on(6, 7)));
            assert_eq!(u.value.span, Some(span(at(10, 0, 10), at(15, 0, 14))));
            let v = object.get("v").unwrap();
            assert_eq!(v.key_span, Some(span(at(17, 0, 16), at(18, 0, 17))));
            assert_eq!(v.value.span, Some(span(at(21, 0, 20), at(22, 0, 21))));

            let b = &document.blocks[0];
            assert_eq!(b.keyword_span, Some(span(at(25, 1, 0), at(26, 1, 1))));
            assert_eq!(b.labels[0].span, Some(span(at(27, 1, 2), at(30, 1, 5))));
            assert_eq!(b.span, Some(span(at(25, 1, 0), at(45, 3, 1))));
            let r = b.body.attributes.get("r").unwrap();
            assert_eq!(r.key_span, Some(span(at(35, 2, 2), at(36, 2, 3))));
            assert_eq!(r.value.span, Some(span(at(39, 2, 6), at(43, 2, 10))));
            let value::Kind::Call(call) = &r.value.kind else {
                panic!("not a call: {r:?}");
            };
            assert_eq!(call.function_span, Some(span(at(39, 2, 6), at(40, 2, 7))));
            assert_eq!(
                call.arguments[0].span,
                Some(span(at(41, 2, 8), at(42, 2, 9)))
            );
        }

        #[test]
        fn covers_the_minus_sign() {
            let document = ok("a = -3\nb = - 3\n");
            let a = document.attributes.get("a").unwrap();
            assert_eq!(a.value.span, Some(on(4, 6)));
            let b = document.attributes.get("b").unwrap();
            assert_eq!(b.value.span, Some(span(at(11, 1, 4), at(14, 1, 7))));
        }

        #[test]
        fn covers_lists_and_strings() {
            let document = ok("a = [1, \"x\"]");
            let a = document.attributes.get("a").unwrap();
            assert_eq!(a.value.span, Some(on(4, 12)));
            let value::Kind::List(items) = &a.value.kind else {
                panic!("not a list: {a:?}");
            };
            assert_eq!(items[1].span, Some(on(8, 11)));
        }
    }

    mod heredocs {
        use super::*;

        const START: &str = "the file needs a marker, such as `EOT`, and a new line to \
                             start the heredoc here";
        const END: &str =
            "the file needs the marker on a line of its own to end the heredoc here";
        /// Checks that `a = ` and then each heredoc reads as its string.
        fn reads(cases: &[(&str, &str)]) {
            for &(heredoc, expected) in cases {
                let document = ok(&format!("a = {heredoc}"));
                let expected = attributes(vec![("a", string(expected))]);
                assert_eq!(document, expected, "{heredoc:?}");
            }
        }

        #[test]
        fn reads_lines_as_written() {
            reads(&[
                ("<<EOT\nhello\n  world\nEOT\n", "hello\n  world\n"),
                ("<<EOT\nEOT\n", ""),
                ("<<EOT\n\nEOT\n", "\n"),
                ("<<EOT\nx\n \t EOT\t \n", "x\n"),
                ("<<EOT\nx\n\u{a0}\u{b}EOT\u{3000}\n", "x\n"),
                (
                    "<<EOT\nEOT x\nxEOT\nEOTX\neot\nEOT\n",
                    "EOT x\nxEOT\nEOTX\neot\n",
                ),
                ("<<EOT\nx\nEOT", "x\n"),
                ("<<END-1_a\nx\nEND-1_a\n", "x\n"),
                ("<<_\nx\n_\n", "x\n"),
                ("<<EOT\n°C\nEOT\n", "°C\n"),
                (
                    "<<EOT\n\\n \\\" # a // b /* c \"\nEOT\n",
                    "\\n \\\" # a // b /* c \"\n",
                ),
                (
                    "<<EOT\n$${x} %%{y} $ % $$ %% $$${z}\nEOT\n",
                    "${x} %{y} $ % $$ %% $${z}\n",
                ),
            ]);
        }

        #[test]
        fn reads_crlf_line_ends_as_new_lines() {
            reads(&[
                ("<<EOT\r\nx\r\ny\rz\r\nEOT\r\n", "x\ny\rz\n"),
                ("<<EOT\r\nx\r\n EOT \r\n", "x\n"),
                ("<<EOT\nEOT\rx\nEOT\n", "EOT\rx\n"),
                ("<<-EOT\r\n  a\r\n    b\r\n  EOT\r\n", "a\n  b\n"),
            ]);
            let expected = attributes(vec![("a", string("x\n")), ("b", integer(1))]);
            assert_eq!(ok("a = <<EOT\r\nx\r\nEOT\r\nb = 1\r\n"), expected);
        }

        #[test]
        fn removes_the_indent_that_lines_share() {
            reads(&[
                ("<<-EOT\n    a\n      b\n    EOT\n", "a\n  b\n"),
                ("<<-EOT\n\ta\n\t\tb\nEOT\n", "a\n\tb\n"),
                ("<<-EOT\n \ta\n  b\nEOT\n", "a\nb\n"),
                ("<<-EOT\n\u{3000}a\n\u{3000}b\nEOT\n", "a\nb\n"),
                ("<<-EOT\n\u{3000}a\n\u{b} b\nEOT\n", "a\n b\n"),
                ("<<-EOT\na\n  b\nEOT\n", "a\n  b\n"),
                ("<<-EOT\n  $${a}\n    b\nEOT\n", "${a}\n  b\n"),
                ("<<EOT\n  a\n  EOT\n", "  a\n"),
            ]);
        }

        #[test]
        fn keeps_blank_lines_as_written() {
            reads(&[
                ("<<-EOT\n    a\n\n   \n      b\nEOT\n", "a\n\n   \n  b\n"),
                ("<<-EOT\n  \n\nEOT\n", "  \n\n"),
                ("<<-EOT\n  a\n\u{3000}\n  b\nEOT\n", "a\n\u{3000}\nb\n"),
            ]);
        }

        #[test]
        fn reads_a_heredoc_where_a_value_can_be() {
            let text = "a = [<<EOT\nx\nEOT\n, 1]\nb = f(\n  <<EOT\ny\nEOT\n)\n\
                        c = { k = <<EOT\nz\nEOT\n  j = 2 }\nd {\n  e = <<-EOT\n    \
                        w\n    EOT\n}\n";
            let document = ok(text);
            let call = value::Kind::Call(Call {
                function: "f".into(),
                function_span: None,
                arguments: vec![value(string("y\n"))],
            });
            let expected = Document {
                attributes: map(vec![
                    (
                        "a",
                        value::Kind::List(vec![
                            value(string("x\n")),
                            value(integer(1)),
                        ]),
                    ),
                    ("b", call),
                    (
                        "c",
                        value::Kind::Map(map(vec![
                            ("k", string("z\n")),
                            ("j", integer(2)),
                        ])),
                    ),
                ]),
                blocks: vec![block("d", &[], attributes(vec![("e", string("w\n"))]))],
            };
            assert_eq!(document, expected);
        }

        #[test]
        fn covers_the_opener_to_the_marker_line() {
            let document = ok("a = <<EOT\nx\nEOT\nb = 1\n");
            let a = document.attributes.get("a").unwrap();
            assert_eq!(a.value.span, Some(span(at(4, 0, 4), at(15, 2, 3))));
            let b = document.attributes.get("b").unwrap();
            assert_eq!(b.key_span, Some(span(at(16, 3, 0), at(17, 3, 1))));
            assert_eq!(b.value.span, Some(span(at(20, 3, 4), at(21, 3, 5))));

            let document = ok("a = <<-EOT\n  x\n  EOT\n");
            let a = document.attributes.get("a").unwrap();
            assert_eq!(a.value.span, Some(span(at(4, 0, 4), at(20, 2, 5))));

            let document = ok("a = <<EOT\nx\nEOT \t\r\n");
            let a = document.attributes.get("a").unwrap();
            assert_eq!(a.value.span, Some(span(at(4, 0, 4), at(17, 2, 5))));
        }

        #[test]
        fn refuses_an_opener_without_a_marker_and_a_new_line() {
            let cases = [
                ("a = << EOT\nx\nEOT\n", 6),
                ("a = <<\"EOT\"\nx\nEOT\n", 6),
                ("a = <<1\n", 6),
                ("a = <<-\nx\n-\n", 7),
                ("a = <<--EOT\n", 7),
                ("a = <<EOT x\nx\nEOT\n", 9),
                ("a = <<EOT # c\nx\nEOT\n", 9),
                ("a = <<EOT", 9),
            ];
            for (text, end) in cases {
                let start = syntax(on(4, end), Expected::HeredocStart);
                check(text, &[(start, START)]);
            }
        }

        #[test]
        fn refuses_a_heredoc_that_does_not_end() {
            let cases = [
                ("a = <<EOT\n", at(10, 1, 0)),
                ("a = <<EOT\nx", at(11, 1, 1)),
                ("a = <<EOT\nx\n", at(12, 2, 0)),
                ("a = <<EOT\nEOTX\nEOT x\n", at(21, 3, 0)),
                ("a = <<-EOT\n  x\n  eot\n", at(21, 3, 0)),
                ("a = <<EOT\nx\nEOT\r", at(16, 2, 4)),
            ];
            for (text, end) in cases {
                let open = syntax(span(at(4, 0, 4), end), Expected::HeredocEnd);
                check(text, &[(open, END)]);
            }
        }

        #[test]
        fn refuses_a_template() {
            let template = |start, end| {
                let form = Form::Template;
                (
                    Error::Form {
                        span: span(start, end),
                        form,
                    },
                    TEMPLATE,
                )
            };
            check(
                "a = <<EOT\nx ${y}\nEOT\n",
                &[template(at(12, 1, 2), at(14, 1, 4))],
            );
            check(
                "a = <<-EOT\n  %{ if x }\n  EOT\n",
                &[template(at(13, 1, 2), at(15, 1, 4))],
            );
            let null = Error::Form {
                span: on(4, 8),
                form: Form::Null,
            };
            check(
                "a = null\nb = <<EOT\n${x}\nEOT\nc = null\n",
                &[(null, NULL), template(at(19, 2, 0), at(21, 2, 2))],
            );
        }

        #[test]
        fn refuses_a_heredoc_as_a_label_or_a_key() {
            check(
                "b <<EOT\nx\nEOT\n {\n}\n",
                &[(
                    syntax(span(at(2, 0, 2), at(13, 2, 3)), Expected::AttributeOrBlock),
                    "the file needs `=`, a label, or `{` after the name here",
                )],
            );
            check(
                "b \"l\" <<EOT\nx\nEOT\n {\n}\n",
                &[(
                    syntax(span(at(6, 0, 6), at(17, 2, 3)), Expected::BlockStart),
                    "the file needs a label or `{` here",
                )],
            );
            check(
                "a = { <<EOT\nk\nEOT\n = 1 }\n",
                &[(
                    syntax(span(at(6, 0, 6), at(17, 2, 3)), Expected::Key),
                    "the file needs a key or `}` here",
                )],
            );
        }

        #[test]
        fn reads_on_after_a_form_before_a_heredoc() {
            let operator = |span| Error::Form {
                span,
                form: Form::Operator,
            };
            let operator_message = Form::Operator.to_string();
            check(
                "a = -<<EOT\nx\nEOT\n",
                &[(operator(on(4, 5)), &operator_message)],
            );
            let null = Error::Form {
                span: span(at(24, 3, 4), at(28, 3, 8)),
                form: Form::Null,
            };
            check(
                "a = 1 + <<EOT\nx\nEOT\nb = null\n",
                &[(operator(on(6, 7)), &operator_message), (null, NULL)],
            );
        }

        /// Lines with no `{`, so no line starts a template.
        fn lines() -> impl Strategy<Value = Vec<String>> {
            prop::collection::vec(
                "[ \t\u{b}\u{3000}]{0,3}[a-z$% \t\u{3000}]{0,5}",
                0..6,
            )
        }

        /// Writes `a = ` and a heredoc of `lines`, with `indent` before each line
        /// that is not blank and before the marker.
        fn heredoc(opener: &str, lines: &[String], indent: &str) -> String {
            let mut text = format!("a = {opener}EOT\n");
            for line in lines {
                if !line.trim_start_matches(char::is_whitespace).is_empty() {
                    text.push_str(indent);
                }
                text.push_str(line);
                text.push('\n');
            }
            text.push_str(indent);
            text.push_str("EOT\n");
            text
        }

        proptest! {
            #[test]
            fn reads_an_indented_heredoc_as_the_heredoc_it_indents(
                mut lines in lines(),
                line in "[a-z$%][a-z$% \t\u{3000}]{0,5}",
                at in any::<prop::sample::Index>(),
                indent in "[ \t\u{b}\u{a0}\u{3000}]{0,4}",
            ) {
                lines.insert(at.index(lines.len().saturating_add(1)), line);
                let indented = read(Source(0), &heredoc("<<-", &lines, &indent));
                let plain = read(Source(0), &heredoc("<<", &lines, ""));
                prop_assert_eq!(indented, plain);
            }
        }
    }

    mod errors {
        use super::*;

        const ESCAPE: &str = "the string has an escape that HCL does not have. \
                              Use `\\n`, `\\r`, `\\t`, `\\\"`, `\\\\`, `\\uNNNN`, or \
                              `\\UNNNNNNNN`";
        const NAME: &str = "the reference is not a valid name: \"a.@\" has a segment \
                            that is not valid: \"@\". Use letters, digits, `_`, and \
                            `-`, separated by dots";
        const NUMBER: &str = "the number is out of range. Use an integer that fits \
                              in 128 bits, or a float that fits in 64 bits";
        const REPEAT: &str = "the key \"a\" repeats an earlier key. Remove it, or \
                              give it a different key";

        #[test]
        fn refuses_an_escape_that_hcl_does_not_have() {
            let escape = |start, end| {
                (
                    Error::Escape {
                        span: on(start, end),
                    },
                    ESCAPE,
                )
            };
            check(r#"s = "a\qb""#, &[escape(6, 8)]);
            check(r#"s = "\ud800""#, &[escape(5, 11)]);
            check(r#"s = "\u12""#, &[escape(5, 9)]);
            check(r#"s = "\U00110000""#, &[escape(5, 15)]);
            check("s = \"\\\n\"", &[escape(5, 6)]);
        }

        #[test]
        fn refuses_a_template() {
            let template = |start, end| {
                let form = Form::Template;
                (
                    Error::Form {
                        span: on(start, end),
                        form,
                    },
                    TEMPLATE,
                )
            };
            check(r#"s = "a${b}""#, &[template(6, 8)]);
            check(r#"s = "%{if x}""#, &[template(5, 7)]);
            check(r#"s = "$ ${b}""#, &[template(7, 9)]);
        }

        #[test]
        fn refuses_each_form_at_the_token_that_shows_it() {
            let cases = [
                ("a = 1 + 2\n", on(6, 7), Form::Operator),
                ("a = 1 - 2\n", on(6, 7), Form::Operator),
                ("a = b * c\n", on(6, 7), Form::Operator),
                ("a = 1 / 2\n", on(6, 7), Form::Operator),
                ("a = 7 % 2\n", on(6, 7), Form::Operator),
                ("a = 1 == 2\n", on(6, 8), Form::Operator),
                ("a = 1 != 2\n", on(6, 8), Form::Operator),
                ("a = 1 < 2\n", on(6, 7), Form::Operator),
                ("a = 1 <= 2\n", on(6, 8), Form::Operator),
                ("a = 1 > 2\n", on(6, 7), Form::Operator),
                ("a = 1 >= 2\n", on(6, 8), Form::Operator),
                ("a = b && c\n", on(6, 8), Form::Operator),
                ("a = b || c\n", on(6, 8), Form::Operator),
                ("a = !b\n", on(4, 5), Form::Operator),
                ("a = -b\n", on(4, 5), Form::Operator),
                ("a = *b\n", on(4, 5), Form::Operator),
                ("a = /x\n", on(4, 5), Form::Operator),
                ("a = b ? 1 : 2\n", on(6, 7), Form::Conditional),
                ("a = [for x in y : x]\n", on(5, 8), Form::For),
                ("a = { for k, v in m : k => v }\n", on(6, 9), Form::For),
                (
                    "a = [\n  for x in y : x\n]\n",
                    span(at(8, 1, 2), at(11, 1, 5)),
                    Form::For,
                ),
                ("a = b[0]\n", on(5, 6), Form::Index),
                ("a = f(1).b\n", on(8, 9), Form::Index),
                ("a = 1.x\n", on(5, 6), Form::Index),
                ("a = [1][0]\n", on(7, 8), Form::Index),
                ("a = b[*].c\n", on(5, 6), Form::Splat),
                ("a = b[ * ]\n", on(5, 6), Form::Splat),
                ("a = b.*.c\n", on(5, 6), Form::Splat),
                ("a = (1)\n", on(4, 5), Form::Parentheses),
                ("a = provider::aws::f(1)\n", on(12, 14), Form::Namespace),
                ("a = f(xs...)\n", on(8, 11), Form::Expansion),
                ("a = [1 + 2]\n", on(7, 8), Form::Operator),
                ("a = { k = 1 + 2 }\n", on(12, 13), Form::Operator),
                ("a = f(1 + 2)\n", on(8, 9), Form::Operator),
                ("b { k = 1 + 2 }\n", on(10, 11), Form::Operator),
                (
                    "b {\n  k = 1 + 2\n}\n",
                    span(at(12, 1, 8), at(13, 1, 9)),
                    Form::Operator,
                ),
                ("a = { (k) = 1 }\n", on(6, 7), Form::Parentheses),
                ("a = { 1.5 = 1 }\n", on(6, 9), Form::NumberKey),
                ("a = { 1e3 = 1 }\n", on(6, 9), Form::NumberKey),
                ("a = { -1.5 = 1 }\n", on(7, 10), Form::NumberKey),
                ("a = { - = 1 }\n", on(6, 7), Form::Operator),
                ("a = { -x = 1 }\n", on(6, 7), Form::Operator),
            ];
            for (text, span, form) in cases {
                let message = form.to_string();
                check(text, &[(Error::Form { span, form }, &message)]);
            }
        }

        #[test]
        fn refuses_a_form_after_a_new_line_inside_brackets() {
            let cases = [
                (
                    "a = [\n  1\n  + 2\n]\n",
                    span(at(12, 2, 2), at(13, 2, 3)),
                    Form::Operator,
                ),
                (
                    "a = f(\n  1\n  + 2\n)\n",
                    span(at(13, 2, 2), at(14, 2, 3)),
                    Form::Operator,
                ),
                (
                    "a = [\n  b\n  ? 1 : 2\n]\n",
                    span(at(12, 2, 2), at(13, 2, 3)),
                    Form::Conditional,
                ),
                ("a = [for\n  x in y : x]\n", on(5, 8), Form::For),
                ("a = { for\n  k, v in m : k => v }\n", on(6, 9), Form::For),
                (
                    "a = {\n  for k, v in m : k => v\n}\n",
                    span(at(8, 1, 2), at(11, 1, 5)),
                    Form::For,
                ),
                ("a = b[\n  *\n]\n", on(5, 6), Form::Splat),
            ];
            for (text, span, form) in cases {
                let message = form.to_string();
                check(text, &[(Error::Form { span, form }, &message)]);
            }
        }

        #[test]
        fn keeps_a_problem_before_a_form() {
            let null = Error::Form {
                span: on(4, 8),
                form: Form::Null,
            };
            let operator = Error::Form {
                span: span(at(15, 1, 6), at(16, 1, 7)),
                form: Form::Operator,
            };
            let message = Form::Operator.to_string();
            check(
                "a = null\nb = 1 + 2\n",
                &[(null, NULL), (operator, &message)],
            );
        }

        #[test]
        fn refuses_a_number_key_that_hcl_rounds_and_reads_on() {
            let message = Form::NumberKey.to_string();
            let digits = "9".repeat(155);
            let long = Error::Form {
                span: on(6, 161),
                form: Form::NumberKey,
            };
            check(&format!("a = {{ {digits} = 1 }}\n"), &[(long, &message)]);
            let key = Error::Form {
                span: on(6, 9),
                form: Form::NumberKey,
            };
            let null = Error::Form {
                span: span(at(27, 1, 4), at(31, 1, 8)),
                form: Form::Null,
            };
            check(
                "a = { 1.5 = 1, b = 2 }\nc = null\n",
                &[(key, &message), (null, NULL)],
            );
        }

        #[test]
        fn refuses_a_string_that_does_not_end() {
            let quote = "the file needs `\"` to end the string here";
            check("s = \"abc", &[(syntax(on(4, 8), Expected::Quote), quote)]);
            check(
                "s = \"ab\nc\"\n",
                &[(syntax(on(4, 7), Expected::Quote), quote)],
            );
        }

        #[test]
        fn refuses_a_comment_that_does_not_end() {
            check(
                "a = 1 /* x",
                &[(
                    syntax(on(6, 10), Expected::CommentEnd),
                    "the file needs `*/` to end the comment here",
                )],
            );
        }

        #[test]
        fn names_what_the_grammar_needs() {
            let cases = [
                ("a = \n", span(at(4, 0, 4), at(5, 1, 0)), Expected::Value),
                ("a = $\n", on(4, 5), Expected::Value),
                ("a =", on(3, 3), Expected::Value),
                ("a = [", on(5, 5), Expected::Value),
                ("a = ?\n", on(4, 5), Expected::Value),
                ("a = .5\n", on(4, 5), Expected::Value),
                ("a = -\n7\n", on(4, 5), Expected::Value),
                ("a = 1\rb = 2\n", on(5, 6), Expected::Newline),
                ("a = 1ex\n", on(5, 7), Expected::Newline),
                ("a = 1 b = 2\n", on(6, 7), Expected::Newline),
                ("a 1\n", on(2, 3), Expected::AttributeOrBlock),
                ("b \"x\" 1\n", on(6, 7), Expected::BlockStart),
                ("b { a = 1 c = 2 }\n", on(10, 11), Expected::BlockEnd),
                ("b { = }\n", on(4, 5), Expected::Key),
                ("b { a 1 }\n", on(6, 7), Expected::Equals),
                ("b { a {} }\n", on(6, 7), Expected::Equals),
                ("b { a.b = 1 }\n", on(4, 7), Expected::Key),
                ("b a.b {}\n", on(2, 5), Expected::AttributeOrBlock),
                ("b {}  x\n", on(6, 7), Expected::Newline),
                (
                    "b {\n  a = 1 }\n",
                    span(at(12, 1, 8), at(13, 1, 9)),
                    Expected::Newline,
                ),
                ("b {\n", span(at(4, 1, 0), at(4, 1, 0)), Expected::Item),
                ("}\n", on(0, 1), Expected::Item),
                ("a.b = 1\n", on(0, 3), Expected::Item),
                ("a = [1 2]\n", on(7, 8), Expected::ListEnd),
                (
                    "a = [1, 2\n",
                    span(at(10, 1, 0), at(10, 1, 0)),
                    Expected::ListEnd,
                ),
                ("a = { = 1 }\n", on(6, 7), Expected::Key),
                ("a = { k.j = 1 }\n", on(6, 9), Expected::Key),
                ("a = { k 1 }\n", on(8, 9), Expected::ObjectEquals),
                ("a = { k = 1 j = 2 }\n", on(12, 13), Expected::ObjectEnd),
                ("a = f(1 2)\n", on(8, 9), Expected::ArgumentsEnd),
                ("40001 = 1\n", on(0, 5), Expected::Item),
                ("b 1 {}\n", on(2, 3), Expected::AttributeOrBlock),
            ];
            for (text, span, expected) in cases {
                let message = format!("the file needs {expected} here");
                check(text, &[(syntax(span, expected), &message)]);
            }
        }

        #[test]
        fn keeps_reading_after_a_form() {
            let form = |span, form: Form| {
                let message = form.to_string();
                (Error::Form { span, form }, message)
            };
            let null = |span| Error::Form {
                span,
                form: Form::Null,
            };
            let cases = [
                (
                    "a = 1 + 2\nb = null\n",
                    form(on(6, 7), Form::Operator),
                    span(at(14, 1, 4), at(18, 1, 8)),
                ),
                (
                    "a = [1 +\n  2, null]\n",
                    form(on(7, 8), Form::Operator),
                    span(at(14, 1, 5), at(18, 1, 9)),
                ),
                (
                    "a = [for x, y in z : x]\nb = null\n",
                    form(on(5, 8), Form::For),
                    span(at(28, 1, 4), at(32, 1, 8)),
                ),
                (
                    "a = 1 + [2, [3]]\nb = null\n",
                    form(on(6, 7), Form::Operator),
                    span(at(21, 1, 4), at(25, 1, 8)),
                ),
                (
                    "a = 1 + {\n  x = 2\n}\nb = null\n",
                    form(on(6, 7), Form::Operator),
                    span(at(24, 3, 4), at(28, 3, 8)),
                ),
                (
                    "a = { (k) = 1, j = null }\n",
                    form(on(6, 7), Form::Parentheses),
                    on(19, 23),
                ),
            ];
            for (text, (error, message), at) in cases {
                check(text, &[(error, &message), (null(at), NULL)]);
            }
        }

        #[test]
        fn refuses_a_reference_that_is_not_a_name() {
            let error = "a.@".parse::<Name>().unwrap_err();
            let name = Error::Name {
                span: on(4, 7),
                error,
            };
            check("r = a.@\n", &[(name, NAME)]);

            for (text, name) in [("r = a.\n", "a."), ("r = a..b\n", "a..b")] {
                let error = name.parse::<Name>().unwrap_err();
                let message = format!("the reference is not a valid name: {error}");
                let end = u32::try_from(name.len()).unwrap() + 4;
                let name = Error::Name {
                    span: on(4, end),
                    error,
                };
                check(text, &[(name, &message)]);
            }

            let long = "a".repeat(256);
            let error = long.parse::<Name>().unwrap_err();
            let name = Error::Name {
                span: on(4, 260),
                error,
            };
            let message = "the reference is not a valid name: a name or pattern is 256 \
                           bytes long. The limit is 255 bytes";
            check(&format!("r = {long}"), &[(name, message)]);
        }

        #[test]
        fn refuses_a_number_out_of_range() {
            let number = |start, end| {
                (
                    Error::Number {
                        span: on(start, end),
                    },
                    NUMBER,
                )
            };
            check(
                "i = 170141183460469231731687303715884105728\n",
                &[number(4, 43)],
            );
            check(
                "i = -170141183460469231731687303715884105729\n",
                &[number(4, 44)],
            );
            check(
                "i = 999999999999999999999999999999999999999999\n",
                &[number(4, 46)],
            );
            check("f = 1e400\n", &[number(4, 9)]);
            check("f = -1e400\n", &[number(4, 10)]);
            check("f = 1e-400\n", &[number(4, 10)]);
            check("f = 2e-324\n", &[number(4, 10)]);
        }

        #[test]
        fn refuses_a_repeated_key() {
            let repeat = |first, second| {
                let error = document::Error::DuplicateKey {
                    key: "a".into(),
                    first: Some(first),
                    second: Some(second),
                };
                (Error::Document(error), REPEAT)
            };
            let second = span(at(6, 1, 0), at(7, 1, 1));
            check("a = 1\na = 2\n", &[repeat(on(0, 1), second)]);
            check("m = { a = 1, a = 2 }", &[repeat(on(6, 7), on(13, 14))]);
            let error = document::Error::DuplicateKey {
                key: "1".into(),
                first: Some(on(6, 8)),
                second: Some(on(14, 17)),
            };
            check(
                "m = { 01 = 1, \"1\" = 2 }",
                &[(
                    Error::Document(error),
                    "the key \"1\" repeats an earlier key. Remove it, or give it a \
                     different key",
                )],
            );
            assert_eq!(ok("b { a = 1 }\nb { a = 2 }\n").blocks.len(), 2);
        }

        #[test]
        fn keeps_a_repeated_key_before_a_stop() {
            let repeat = |first, second| {
                let error = document::Error::DuplicateKey {
                    key: "a".into(),
                    first: Some(first),
                    second: Some(second),
                };
                (Error::Document(error), REPEAT)
            };
            let value = "the file needs a value here";
            let second = span(at(6, 1, 0), at(7, 1, 1));
            let end = syntax(span(at(16, 2, 4), at(17, 3, 0)), Expected::Value);
            check(
                "a = 1\na = 2\nc = \n",
                &[repeat(on(0, 1), second), (end, value)],
            );
            let end = syntax(on(24, 25), Expected::Value);
            check(
                "m = { a = 1, a = 2, b = }",
                &[repeat(on(6, 7), on(13, 14)), (end, value)],
            );
        }

        #[test]
        fn keeps_a_problem_before_a_bad_token() {
            let null = Error::Form {
                span: on(4, 8),
                form: Form::Null,
            };
            let escape = Error::Escape { span: on(10, 12) };
            check(r#"a = null "\q""#, &[(null, NULL), (escape, ESCAPE)]);
            let number = Error::Number { span: on(4, 9) };
            let escape = Error::Escape { span: on(11, 13) };
            check(r#"a = 1e400 "\q""#, &[(number, NUMBER), (escape, ESCAPE)]);
        }

        #[test]
        fn refuses_null_and_reads_on() {
            let null = Error::Form {
                span: on(4, 8),
                form: Form::Null,
            };
            check("a = null\nb = 1\n", &[(null, NULL)]);
        }

        #[test]
        fn returns_every_problem_in_source_order() {
            let text = "m = { a = 1, a = 2, b = null }\nr = a.@\nc = \n";
            let repeat = document::Error::DuplicateKey {
                key: "a".into(),
                first: Some(on(6, 7)),
                second: Some(on(13, 14)),
            };
            let null = Error::Form {
                span: on(24, 28),
                form: Form::Null,
            };
            let name = Error::Name {
                span: span(at(35, 1, 4), at(38, 1, 7)),
                error: "a.@".parse::<Name>().unwrap_err(),
            };
            let value = syntax(span(at(43, 2, 4), at(44, 3, 0)), Expected::Value);
            check(
                text,
                &[
                    (Error::Document(repeat), REPEAT),
                    (null, NULL),
                    (name, NAME),
                    (value, "the file needs a value here"),
                ],
            );

            let repeat = document::Error::DuplicateKey {
                key: "a".into(),
                first: Some(on(0, 1)),
                second: Some(span(at(15, 2, 0), at(16, 2, 1))),
            };
            let null = Error::Form {
                span: span(at(10, 1, 4), at(14, 1, 8)),
                form: Form::Null,
            };
            check(
                "a = 1\nb = null\na = 2\n",
                &[(null, NULL), (Error::Document(repeat), REPEAT)],
            );
        }
    }

    mod depth {
        use super::*;

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Level {
            Block,
            List,
            Object,
            Call,
        }

        fn blocks(levels: &[Level]) -> usize {
            levels
                .iter()
                .take_while(|&&level| level == Level::Block)
                .count()
        }

        /// A one-character span.
        fn char_at(offset: usize, line: usize, column: usize) -> Span {
            let [offset, line, column] =
                [offset, line, column].map(|n| u32::try_from(n).unwrap());
            let next = |n: u32| n.checked_add(1).unwrap();
            span(
                at(offset, line, column),
                at(next(offset), line, next(column)),
            )
        }

        /// A file nested through `levels`, outermost first, with `true` inside, and
        /// the span of the token that opens each level.
        fn nested(levels: &[Level]) -> (String, Vec<Span>) {
            let mut text = String::new();
            let mut spans = Vec::new();
            for line in 0..blocks(levels) {
                spans.push(char_at(text.len(), line, 0));
                text.push_str("b {\n");
            }
            text.push_str("a = ");
            let start = text.len().checked_sub(4).unwrap();
            let values = &levels[blocks(levels)..];
            for level in values {
                let column = text.len().checked_sub(start).unwrap();
                spans.push(char_at(text.len(), blocks(levels), column));
                text.push_str(match level {
                    Level::List => "[",
                    Level::Object => "{ k = ",
                    Level::Call => "f(",
                    Level::Block => panic!("a block inside a value: {levels:?}"),
                });
            }
            text.push_str("true");
            for level in values.iter().rev() {
                text.push_str(match level {
                    Level::List => "]",
                    Level::Object => " }",
                    Level::Call => ")",
                    Level::Block => panic!("a block inside a value: {levels:?}"),
                });
            }
            text.push('\n');
            for _ in 0..blocks(levels) {
                text.push_str("}\n");
            }
            (text, spans)
        }

        /// The file reads up to the limit, and the encoding takes what it reads.
        fn check_depth(levels: &[Level]) {
            let (text, spans) = nested(levels);
            if let Some(&span) = spans.get(DEPTH_MAX) {
                let expected = Err(vec![Error::TooDeep { span }]);
                assert_eq!(read(Source(0), &text), expected);
            } else {
                let document = read(Source(0), &text).unwrap();
                let bytes = document::encoding::encode(&document).unwrap();
                assert_eq!(document::encoding::decode(&bytes).unwrap(), document);
            }
        }

        #[test]
        fn counts_each_kind_of_level() {
            for level in [Level::Block, Level::List, Level::Object, Level::Call] {
                check_depth(&[level; DEPTH_MAX]);
                check_depth(&[level; DEPTH_MAX + 1]);
            }
        }

        #[test]
        fn too_deep_names_the_fix() {
            let (text, spans) = nested(&[Level::List; 65]);
            let errors = read(Source(0), &text).unwrap_err();
            assert_eq!(errors, vec![Error::TooDeep { span: spans[64] }]);
            assert_eq!(
                errors[0].to_string(),
                "the file nests deeper than 64 levels. Make it flatter"
            );
        }

        fn levels(total: usize) -> impl Strategy<Value = Vec<Level>> {
            (0..=total).prop_flat_map(move |blocks| {
                let value = prop_oneof![
                    Just(Level::List),
                    Just(Level::Object),
                    Just(Level::Call)
                ];
                prop::collection::vec(value, total.saturating_sub(blocks)).prop_map(
                    move |values| {
                        let mut levels = vec![Level::Block; blocks];
                        levels.extend(values);
                        levels
                    },
                )
            })
        }

        proptest! {
            #[test]
            fn counts_any_mix_of_levels(
                levels in prop_oneof![levels(DEPTH_MAX), levels(DEPTH_MAX + 1)],
            ) {
                check_depth(&levels);
            }
        }

        /// Runs `f` on a thread with a small stack, so that recursion past the limit
        /// would overflow it.
        fn small_stack(f: impl FnOnce() + Send + 'static) {
            #[expect(clippy::disallowed_methods, reason = "a test owns its threads")]
            let thread = std::thread::Builder::new().stack_size(2_097_152).spawn(f);
            thread.unwrap().join().unwrap();
        }

        #[test]
        fn refuses_a_hostile_depth_without_recursing() {
            small_stack(|| {
                for level in [Level::Block, Level::List, Level::Object, Level::Call] {
                    let (text, spans) = nested(&vec![level; 100_000]);
                    let span = spans[DEPTH_MAX];
                    assert_eq!(
                        read(Source(0), &text),
                        Err(vec![Error::TooDeep { span }])
                    );
                }
                for prefix in ["-", "!"] {
                    let text = format!("a = {}1", prefix.repeat(100_000));
                    let expected = Error::Form {
                        span: on(4, 5),
                        form: Form::Operator,
                    };
                    assert_eq!(read(Source(0), &text), Err(vec![expected]));
                }
                let text = format!("a = 1 + {}", "[".repeat(100_000));
                let expected = Error::Form {
                    span: on(6, 7),
                    form: Form::Operator,
                };
                assert_eq!(read(Source(0), &text), Err(vec![expected]));
            });
        }
    }

    mod properties {
        use super::*;

        fn edit() -> impl Strategy<Value = (prop::sample::Index, Option<char>)> {
            let c = prop::sample::select(
                &[
                    '{', '}', '[', ']', '(', ')', '"', '=', ',', '.', ':', '#', '/',
                    '*', '$', '%', '\\', '-', '\n', '\r', ' ', 'a', '1', '°', '<',
                ][..],
            );
            (any::<prop::sample::Index>(), prop::option::of(c))
        }

        proptest! {
            #[test]
            fn reads_what_the_writer_writes(document in document()) {
                let text = write(&document);
                prop_assert_eq!(read(Source(0), &text), Ok(document), "{}", text);
            }

            #[test]
            fn reads_random_text_back_or_points_inside_it(
                text in "[ -~\n\t°{}\\[\\]()\"=,.:#/*$%@\\\\-]{0,64}",
            ) {
                check_text(&text)?;
            }

            #[test]
            fn reads_an_edited_file_back_or_points_inside_it(
                document in document(),
                edits in prop::collection::vec(edit(), 1..4),
            ) {
                let mut chars: Vec<char> = write(&document).chars().collect();
                for (i, c) in &edits {
                    let i = i.index(chars.len().saturating_add(1));
                    match c {
                        Some(c) => chars.insert(i, *c),
                        None if i < chars.len() => drop(chars.remove(i)),
                        None => {}
                    }
                }
                check_text(&chars.into_iter().collect::<String>())?;
            }
        }

        /// A text that reads gives a Document that writes and reads back the same.
        /// Otherwise each problem starts inside the text.
        fn check_text(text: &str) -> Result<(), TestCaseError> {
            match read(Source(0), text) {
                Ok(document) => {
                    let written = write(&document);
                    prop_assert_eq!(
                        read(Source(0), &written),
                        Ok(document),
                        "{}",
                        text
                    );
                }
                Err(errors) => {
                    let inside = |error: &&Error| {
                        usize::try_from(error.offset()).is_ok_and(|at| at <= text.len())
                    };
                    let outside = errors.iter().find(|error| !inside(error));
                    prop_assert!(outside.is_none(), "{:?} in {:?}", outside, text);
                }
            }
            Ok(())
        }
    }
}

use document::encoding::DEPTH_MAX;
use document::value::{self, Call, Float, Value};
use document::{Attribute, Block, Document, Label, Map, Source, Span};
use types::name::Name;

use crate::lex::{self, Token, Tokens};
use crate::{Error, Expected, Form, Number};

/// Reads HCL text as a Document, as [`crate::read`] does, with each problem as an
/// [`Error`].
pub(crate) fn read(source: Source, text: &str) -> Result<Document, Vec<Error>> {
    let mut tokens = Tokens::new(source, text).map_err(|error| vec![error])?;
    let token = next(&mut tokens, Newlines::Kept);
    let mut parser = Parser {
        tokens,
        token,
        newlines: Newlines::Kept,
        errors: Vec::new(),
        len: text.len(),
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
    /// A `,`, as in a list or a call.
    Comma,
    /// Only the close bracket, as in a `for` expression.
    Close,
}

/// An object key that `read` reads.
enum Key {
    /// The key as HCL reads it: a name, a string, or an integer.
    Text(Box<str>),
    /// A number that HCL rounds. Its span is the number, without the `-`.
    Rounded,
    /// A number that HCL refuses.
    Malformed,
}

struct Parser<'a> {
    tokens: Tokens<'a>,
    /// The next token, not yet taken.
    token: Token<'a>,
    /// [`Newlines::Skipped`] inside `[` or `(`, as HCL reads them. Only
    /// [`Parser::enclosed`] sets it.
    newlines: Newlines,
    /// Problems that do not stop reading.
    errors: Vec<Error>,
    /// The length of the text in bytes. Each token but the last has one or more, so
    /// this bounds the passes of each loop.
    len: usize,
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
        // Each pass takes a token or returns.
        for _ in 0..=self.len {
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
        unreachable!("invariant: each pass takes a token")
    }

    fn block(&mut self, keyword: &Token<'a>, depth: usize) -> Result<Block, Error> {
        let labels = self.labels()?;
        let (body, span) = self.level(keyword.span, depth, |parser, depth| {
            if parser.token.kind != lex::Kind::Newline {
                return parser.one_line(depth);
            }
            let body = parser.body(depth)?;
            if parser.token.kind != lex::Kind::CloseBrace {
                return Err(parser.syntax(Expected::Item));
            }
            Ok(body)
        })?;
        self.end_line()?;
        Ok(Block {
            keyword: keyword.text.into(),
            keyword_span: Some(keyword.span),
            labels,
            body,
            span: Some(span),
        })
    }

    /// Reads the labels of a block up to its `{`, and leaves that token.
    fn labels(&mut self) -> Result<Vec<Label>, Error> {
        let mut labels = Vec::new();
        // Each pass takes a token or returns.
        for _ in 0..=self.len {
            match self.token.kind {
                lex::Kind::String(_) | lex::Kind::Identifier => {}
                lex::Kind::OpenBrace => return Ok(labels),
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
        unreachable!("invariant: each pass takes a token")
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
        if let Some(form) = self.trailing_form() {
            self.refuse(form, ends)?;
            return Ok(None);
        }
        Ok(value)
    }

    /// Reads a value that starts with no HCL form.
    fn term(&mut self, depth: usize) -> Result<Option<Value>, Error> {
        match self.token.kind {
            lex::Kind::End => return Err(self.syntax(Expected::Value)),
            lex::Kind::OpenBracket => return self.list(depth).map(Some),
            lex::Kind::OpenBrace => return self.object(depth).map(Some),
            _ => {}
        }
        let token = self.take()?;
        let kind = match token.kind {
            lex::Kind::Identifier => return self.identifier(&token, depth),
            lex::Kind::Number => return Ok(self.number(&token, None)),
            lex::Kind::Minus if self.token.kind == lex::Kind::Number => {
                let digits = self.take()?;
                return Ok(self.number(&digits, Some(token.span)));
            }
            lex::Kind::String(text) | lex::Kind::Heredoc(text) => {
                value::Kind::String(text)
            }
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
            lex::Kind::Minus if self.second_past_lines().kind != lex::Kind::Number => {
                Some(Form::Operator)
            }
            lex::Kind::OpenParenthesis => Some(Form::Parentheses),
            _ => None,
        }
    }

    /// The HCL form that the next token starts, after a value or an object key.
    fn trailing_form(&self) -> Option<Form> {
        match self.token.kind {
            lex::Kind::Operator | lex::Kind::Minus | lex::Kind::Star => {
                Some(Form::Operator)
            }
            lex::Kind::Question => Some(Form::Conditional),
            lex::Kind::OpenBracket | lex::Kind::Dot
                if self.second_past_lines().kind == lex::Kind::Star =>
            {
                Some(Form::Splat)
            }
            lex::Kind::OpenBracket => Some(Form::Index),
            // HCL reads a number after `.` as an index, and refuses any other token.
            lex::Kind::Dot
                if matches!(
                    self.second().kind,
                    lex::Kind::Identifier | lex::Kind::Number
                ) =>
            {
                Some(Form::Index)
            }
            lex::Kind::DoubleColon => Some(Form::Namespace),
            lex::Kind::Ellipsis => Some(Form::Expansion),
            _ => None,
        }
    }

    /// Refuses a `for` expression at the start of a list or an object.
    fn refuse_for(&mut self) -> Result<(), Error> {
        if self.token.kind == lex::Kind::Identifier && opens_for(self.token.text) {
            self.refuse(Form::For, Ends::Close)?;
        }
        Ok(())
    }

    /// Keeps the problem of `form` at the next token, then skips the item.
    fn refuse(&mut self, form: Form, ends: Ends) -> Result<(), Error> {
        self.errors.push(Error::Form {
            span: self.token.span,
            form,
        });
        self.skip_item(ends)
    }

    /// Moves past the rest of the item to the token that ends it, and leaves that
    /// token.
    fn skip_item(&mut self, ends: Ends) -> Result<(), Error> {
        let mut depth = 0usize;
        // Each pass takes a token or returns.
        for _ in 0..=self.len {
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
                lex::Kind::Comma | lex::Kind::Newline
                    if depth == 0 && ends != Ends::Close =>
                {
                    return Ok(());
                }
                _ => {}
            }
            self.take()?;
        }
        unreachable!("invariant: each pass takes a token")
    }

    /// Reads the value that starts with `first`: a call, a literal, or a reference.
    fn identifier(
        &mut self,
        first: &Token<'a>,
        depth: usize,
    ) -> Result<Option<Value>, Error> {
        if self.token.kind == lex::Kind::OpenParenthesis {
            return self.call(first, depth).map(Some);
        }
        if self.token.kind == lex::Kind::DoubleColon {
            // A namespace, not a name: `value` refuses its form.
            return Ok(None);
        }
        let kind = match literal(first.text) {
            Some(Literal::Bool(b)) => value::Kind::Bool(b),
            Some(Literal::Null) => {
                self.errors.push(Error::Form {
                    span: first.span,
                    form: Form::Null,
                });
                return Ok(None);
            }
            None => return self.reference(first),
        };
        Ok(Some(Value {
            kind,
            span: Some(first.span),
        }))
    }

    /// Reads a reference from its first part: each `.` and identifier after it, and
    /// each index that is a string, which reads as more segments.
    fn reference(&mut self, first: &Token<'a>) -> Result<Option<Value>, Error> {
        let mut text = String::from(first.text);
        let mut span = first.span;
        // Each pass takes two tokens or more, or returns.
        for _ in 0..=self.len {
            if self.token.kind == lex::Kind::Dot
                && self.second().kind == lex::Kind::Identifier
            {
                self.take()?;
                let part = self.take()?;
                text.push('.');
                text.push_str(part.text);
                span = join(span, part.span);
            } else if self.string_index() {
                let (part, whole) =
                    self.enclosed(span, |parser| match parser.take()?.kind {
                        lex::Kind::String(part) | lex::Kind::Heredoc(part) => Ok(part),
                        kind => unreachable!("invariant: the index is not {kind:?}"),
                    })?;
                text.push('.');
                text.push_str(&part);
                span = whole;
            } else {
                return match text.parse::<Name>() {
                    Ok(name) => Ok(Some(Value {
                        kind: value::Kind::Reference(name),
                        span: Some(span),
                    })),
                    Err(error) => {
                        self.errors.push(Error::Name { span, error });
                        Ok(None)
                    }
                };
            }
        }
        unreachable!("invariant: each pass takes two tokens or more")
    }

    /// Reports whether the next tokens are `[`, a string or a heredoc, and `]`: an
    /// index that HCL reads as a step of a reference, not as an expression. It also
    /// reports a lexer error after `[`, so that error is the only one, except a
    /// template, which HCL reads as an expression.
    fn string_index(&self) -> bool {
        if self.token.kind != lex::Kind::OpenBracket {
            return false;
        }
        let mut ahead = self.tokens.clone();
        match next(&mut ahead, Newlines::Skipped).kind {
            lex::Kind::String(_) | lex::Kind::Heredoc(_) => {
                next(&mut ahead, Newlines::Skipped).kind == lex::Kind::CloseBracket
            }
            lex::Kind::Error(Error::Form {
                form: Form::Template,
                ..
            }) => false,
            lex::Kind::Error(_) => true,
            _ => false,
        }
    }

    /// Reads a number, after its minus sign if `minus` holds the sign's span. Returns
    /// `None` when the number has a problem, after it keeps the problem.
    fn number(&mut self, digits: &Token<'a>, minus: Option<Span>) -> Option<Value> {
        let span = minus.map_or(digits.span, |minus| join(minus, digits.span));
        match read_number(digits.text, minus.is_some()) {
            Ok(kind) => Some(Value {
                kind,
                span: Some(span),
            }),
            Err(problem) => {
                self.errors.push(Error::Number { span, problem });
                None
            }
        }
    }

    fn list(&mut self, depth: usize) -> Result<Value, Error> {
        let (items, span) = self.level(self.token.span, depth, |parser, depth| {
            parser.refuse_for()?;
            parser.values(&lex::Kind::CloseBracket, Expected::ListEnd, depth)
        })?;
        Ok(Value {
            kind: value::Kind::List(items),
            span: Some(span),
        })
    }

    fn object(&mut self, depth: usize) -> Result<Value, Error> {
        let (map, span) = self.level(self.token.span, depth, |parser, depth| {
            let mut attributes = Vec::new();
            let entries = parser.entries(&mut attributes, depth);
            let map = parser.map(attributes);
            entries.map(|()| map)
        })?;
        Ok(Value {
            kind: value::Kind::Map(map),
            span: Some(span),
        })
    }

    /// Reads one level of nesting that starts at `start`, as [`Parser::enclosed`]
    /// does, with `inner` at the depth inside. Returns [`Error::TooDeep`] at `start`
    /// when the level is past [`DEPTH_MAX`].
    fn level<T>(
        &mut self,
        start: Span,
        depth: usize,
        inner: impl FnOnce(&mut Self, usize) -> Result<T, Error>,
    ) -> Result<(T, Span), Error> {
        let depth = enter(depth).ok_or(Error::TooDeep { span: start })?;
        self.enclosed(start, |parser| inner(parser, depth))
    }

    /// Takes the open bracket, reads the inside with `inner`, and takes the close.
    /// `start` is the keyword, function name, reference, or open bracket before the
    /// open bracket. Inside `[` or `(`, `take` skips new lines. Returns what `inner`
    /// read and the span from `start` to the close.
    fn enclosed<T>(
        &mut self,
        start: Span,
        inner: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<(T, Span), Error> {
        let newlines = match self.token.kind {
            lex::Kind::OpenBracket | lex::Kind::OpenParenthesis => Newlines::Skipped,
            lex::Kind::OpenBrace => Newlines::Kept,
            ref kind => unreachable!("invariant: {kind:?} opens nothing"),
        };
        // The mode changes before each bracket is taken, because `take` reads the
        // token after it.
        let outer = std::mem::replace(&mut self.newlines, newlines);
        self.take()?;
        let inside = inner(self)?;
        self.newlines = outer;
        let close = self.take()?;
        Ok((inside, join(start, close.span)))
    }

    /// Reads the entries of an object after its `{`, and leaves the `}`.
    fn entries(
        &mut self,
        attributes: &mut Vec<Attribute>,
        depth: usize,
    ) -> Result<(), Error> {
        self.skip_newlines()?;
        self.refuse_for()?;
        // Each pass takes a token or returns.
        for _ in 0..=self.len {
            self.skip_newlines()?;
            match self.token.kind {
                lex::Kind::CloseBrace => return Ok(()),
                _ => attributes.extend(self.entry(depth)?),
            }
            match self.token.kind {
                lex::Kind::Comma | lex::Kind::Newline => {
                    self.take()?;
                }
                lex::Kind::CloseBrace => return Ok(()),
                _ => return Err(self.syntax(Expected::ObjectEnd)),
            }
        }
        unreachable!("invariant: each pass takes a token")
    }

    /// Reads an object entry. Returns `None` when the entry has a problem that does
    /// not stop reading.
    fn entry(&mut self, depth: usize) -> Result<Option<Attribute>, Error> {
        if matches!(
            self.token.kind,
            lex::Kind::OpenBracket | lex::Kind::OpenBrace
        ) {
            self.refuse(Form::ExpressionKey, Ends::Line)?;
            return Ok(None);
        }
        if let Some(form) = self.leading_form() {
            self.refuse(form, Ends::Line)?;
            return Ok(None);
        }
        let named = self.token.kind == lex::Kind::Identifier;
        let (key, key_span) = self.key()?;
        let key = match key {
            Key::Text(key) => Some(key),
            Key::Rounded => None,
            Key::Malformed => {
                self.errors.push(Error::Number {
                    span: key_span,
                    problem: Number::Malformed,
                });
                self.skip_item(Ends::Line)?;
                return Ok(None);
            }
        };
        if !matches!(self.token.kind, lex::Kind::Equals | lex::Kind::Colon) {
            let call = named && self.token.kind == lex::Kind::OpenParenthesis;
            if call || self.trailing_form().is_some() {
                self.refuse(Form::ExpressionKey, Ends::Line)?;
                return Ok(None);
            }
            return Err(self.syntax(Expected::ObjectEquals));
        }
        if key.is_none() {
            self.errors.push(Error::Form {
                span: key_span,
                form: Form::NumberKey,
            });
        }
        self.take()?;
        let value = self.value(depth, Ends::Line)?;
        Ok(key.zip(value).map(|(key, value)| Attribute {
            key,
            key_span: Some(key_span),
            value,
        }))
    }

    /// Reads an object key, where no HCL form starts: a string, an identifier, or a
    /// number after an optional `-`, and returns it with its span. An integer reads as
    /// its digits without leading zeros, after its `-`. Any other token is
    /// `Expected::Key`.
    fn key(&mut self) -> Result<(Key, Span), Error> {
        if matches!(
            self.token.kind,
            lex::Kind::String(_) | lex::Kind::Identifier
        ) {
            let key = self.take()?;
            let span = key.span;
            return Ok((Key::Text(text(key)), span));
        }
        let minus = if self.token.kind == lex::Kind::Minus {
            Some(self.take()?)
        } else {
            None
        };
        if self.token.kind != lex::Kind::Number {
            return Err(self.syntax(Expected::Key));
        }
        let number = self.take()?;
        let span = minus
            .as_ref()
            .map_or(number.span, |minus| join(minus.span, number.span));
        if parts(number.text).is_none() {
            return Ok((Key::Malformed, span));
        }
        let Some(digits) = integer_key(number.text) else {
            return Ok((Key::Rounded, number.span));
        };
        let key = match minus {
            Some(_) => format!("-{digits}").into(),
            None => digits.into(),
        };
        Ok((Key::Text(key), span))
    }

    fn call(&mut self, function: &Token<'a>, depth: usize) -> Result<Value, Error> {
        let (arguments, span) = self.level(function.span, depth, |parser, depth| {
            parser.values(&lex::Kind::CloseParenthesis, Expected::ArgumentsEnd, depth)
        })?;
        Ok(Value {
            kind: value::Kind::Call(Call {
                function: function.text.into(),
                function_span: Some(function.span),
                arguments,
            }),
            span: Some(span),
        })
    }

    /// Reads values split by `,` up to `close`, and leaves the `close`. Any other
    /// token after a value is `expected`.
    fn values(
        &mut self,
        close: &lex::Kind,
        expected: Expected,
        depth: usize,
    ) -> Result<Vec<Value>, Error> {
        let mut values = Vec::new();
        // Each pass takes a token or returns.
        for _ in 0..=self.len {
            if self.token.kind == *close {
                return Ok(values);
            }
            values.extend(self.value(depth, Ends::Comma)?);
            match &self.token.kind {
                lex::Kind::Comma => {
                    self.take()?;
                }
                kind if kind == close => return Ok(values),
                _ => return Err(self.syntax(expected)),
            }
        }
        unreachable!("invariant: each pass takes a token")
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
        // Each pass takes a token or returns.
        for _ in 0..=self.len {
            if self.token.kind != lex::Kind::Newline {
                return Ok(());
            }
            self.take()?;
        }
        unreachable!("invariant: each pass takes a token")
    }

    /// Takes the next token and reads the one after it.
    ///
    /// # Errors
    ///
    /// Returns the lexer's error when the next token is [`lex::Kind::Error`].
    ///
    /// # Panics
    ///
    /// Panics at the end of the text, which no rule takes.
    fn take(&mut self) -> Result<Token<'a>, Error> {
        if let lex::Kind::Error(error) = &self.token.kind {
            return Err(error.clone());
        }
        let token = next(&mut self.tokens, self.newlines);
        Ok(std::mem::replace(&mut self.token, token))
    }

    /// The token after the next one, as [`Parser::take`] reads it.
    ///
    /// # Panics
    ///
    /// Panics when the next token is the end or an error.
    fn second(&self) -> Token<'a> {
        next(&mut self.tokens.clone(), self.newlines)
    }

    /// The first token after the next one that is not a new line.
    ///
    /// # Panics
    ///
    /// Panics when the next token is the end or an error.
    fn second_past_lines(&self) -> Token<'a> {
        next(&mut self.tokens.clone(), Newlines::Skipped)
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

/// Whether reading the next token passes new lines.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Newlines {
    Kept,
    Skipped,
}

/// The next token of `tokens`, past new lines when they are [`Newlines::Skipped`].
///
/// # Panics
///
/// Panics after the tokens end, which no rule reads past.
fn next<'a>(tokens: &mut Tokens<'a>, newlines: Newlines) -> Token<'a> {
    tokens
        .find(|token| newlines == Newlines::Kept || token.kind != lex::Kind::Newline)
        .expect("invariant: no rule reads past the end or an error")
}

/// An identifier that reads as a value, not as a reference.
pub(crate) enum Literal {
    Bool(bool),
    Null,
}

/// Reports whether HCL reads `identifier` after `[` or `{` as the start of a `for`
/// expression.
pub(crate) fn opens_for(identifier: &str) -> bool {
    identifier == "for"
}

/// Reports whether HCL reads the first segment of `name` as the root of a reference:
/// an identifier that is not a literal.
pub(crate) fn rooted(name: &Name) -> bool {
    name.segments()
        .next()
        .is_some_and(|first| lex::identifier(first) && literal(first).is_none())
}

/// The value that `identifier` reads as by itself, or `None` for a reference.
pub(crate) fn literal(identifier: &str) -> Option<Literal> {
    match identifier {
        "true" => Some(Literal::Bool(true)),
        "false" => Some(Literal::Bool(false)),
        "null" => Some(Literal::Null),
        _ => None,
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

/// The value of a number token, negated when `negative`.
fn read_number(text: &str, negative: bool) -> Result<value::Kind, Number> {
    let (whole, fraction, exponent) = parts(text).ok_or(Number::Malformed)?;
    if text.bytes().all(|b| b.is_ascii_digit()) {
        let magnitude = text.parse::<u128>().ok();
        return magnitude
            .and_then(|n| {
                if negative {
                    0i128.checked_sub_unsigned(n)
                } else {
                    i128::try_from(n).ok()
                }
            })
            .map(value::Kind::Integer)
            .ok_or(Number::Range);
    }
    nearest(whole, fraction, exponent)
        .map(|f| if negative { -f } else { f })
        .and_then(Float::new)
        .map(value::Kind::Float)
        .ok_or(Number::Range)
}

/// The whole digits, the fraction digits, and the exponent of a number token, or
/// `None` when it is not a number: it has two dots, two exponents, a dot in its
/// exponent, or an exponent outside `i64`. HCL refuses each, even on zero.
fn parts(text: &str) -> Option<(&str, &str, i64)> {
    let (mantissa, exponent) = text.split_once(['e', 'E']).unwrap_or((text, "0"));
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if fraction.contains('.') {
        return None;
    }
    Some((whole, fraction, exponent.parse().ok()?))
}

/// The `f64` nearest to `whole.fraction` times ten to `exponent`, infinite past
/// `f64::MAX`, or `None` when the number is not zero but rounds to zero.
fn nearest(whole: &str, fraction: &str, exponent: i64) -> Option<f64> {
    let digits = [whole, fraction].concat();
    let significant = digits.trim_start_matches('0');
    let mut rest = significant.chars();
    let Some(lead) = rest.next() else {
        return Some(0.0);
    };
    let zeros = digits.bytes().take_while(|&b| b == b'0').count();
    let length = |n: usize| i64::try_from(n).expect("invariant: a text fits in a span");
    // `str::parse::<f64>` stops reading the digits of a long exponent. Past 400, each
    // float is infinite or zero.
    let place = exponent
        .saturating_add(length(whole.len()))
        .saturating_sub(length(zeros))
        .saturating_sub(1)
        .clamp(-400, 400);
    let float: f64 = format!("{lead}.{}e{place}", rest.as_str())
        .parse()
        .expect("invariant: `d.ddde<n>` is a float");
    (float != 0.0).then_some(float)
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
    use crate::arbitrary::document;
    use crate::{Unclosed, write};
    use document::Position;
    use document::diagnostic::{Diagnostic, Note};
    use document::encoding::Checked;
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

    /// The text of a syntax error that needs `phrase`.
    fn needs(phrase: impl std::fmt::Display) -> String {
        format!(
            "the file needs {phrase} here. Write it here, or correct the text here or \
             before it"
        )
    }

    /// The text of an error for `form`.
    fn refused(form: Form) -> String {
        Error::Form {
            span: on(0, 0),
            form,
        }
        .to_string()
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
            let text = "a = 1.5\nb = -0.25\nc = 1e3\nd = 2.5E-3\ne = -0.0\nf = 1e+2\n\
                        g = 1.e5\nh = 1.E+5\ni = 0.e5\nj = -1.e-2\n\
                        k = 0e9223372036854775807\nl = 0.e-9223372036854775808\n";
            let expected = attributes(vec![
                ("a", float(1.5)),
                ("b", float(-0.25)),
                ("c", float(1000.0)),
                ("d", float(0.0025)),
                ("e", float(0.0)),
                ("f", float(100.0)),
                ("g", float(100_000.0)),
                ("h", float(100_000.0)),
                ("i", float(0.0)),
                ("j", float(-0.01)),
                ("k", float(0.0)),
                ("l", float(0.0)),
            ]);
            assert_eq!(ok(text), expected);
        }

        /// HCL reads each as exactly 1: it holds the whole exponent, which fits `i64`.
        #[test]
        fn reads_a_long_number_with_a_long_exponent() {
            let zeros = "0".repeat(655_359);
            let text = format!("a = 0.{zeros}1e655360\nb = 1{zeros}0e-655360\n");
            let expected = attributes(vec![("a", float(1.0)), ("b", float(1.0))]);
            assert_eq!(ok(&text), expected);
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
            let text = "a = site_a.pt_1\nb = a-b.c-d\nc = x.true.for\n";
            let expected = attributes(vec![
                ("a", reference("site_a.pt_1")),
                ("b", reference("a-b.c-d")),
                ("c", reference("x.true.for")),
            ]);
            assert_eq!(ok(text), expected);
        }

        #[test]
        fn reads_a_reference_past_spaces_and_bracketed_new_lines_as_hcl_does() {
            let first = |document: Document| {
                let value = document.attributes.iter().next().unwrap().value.clone();
                match value.kind {
                    value::Kind::List(items) => items.into_iter().next().unwrap(),
                    value::Kind::Call(call) => {
                        call.arguments.into_iter().next().unwrap()
                    }
                    _ => value,
                }
            };
            let cases = [
                ("a = x . b\n", on(4, 9)),
                ("a = [x\n.b]\n", span(at(5, 0, 5), at(9, 1, 2))),
                ("a = [x.\nb]\n", span(at(5, 0, 5), at(9, 1, 1))),
                ("a = [x\n# c\n.\n\nb]\n", span(at(5, 0, 5), at(15, 4, 1))),
                ("a = f(x.\nb)\n", span(at(6, 0, 6), at(10, 1, 1))),
            ];
            for (text, span) in cases {
                let value = first(ok(text));
                assert_eq!(value.kind, reference("x.b"), "{text:?}");
                assert_eq!(value.span, Some(span), "{text:?}");
            }
        }

        #[test]
        fn reads_a_string_index_as_a_segment() {
            let cases = [
                ("a = plc[\"40001\"]\n", "plc.40001", on(4, 16)),
                ("a = plc.a[\"-1\"].x\n", "plc.a.-1.x", on(4, 17)),
                ("a = a[\"b\"]\n", "a.b", on(4, 10)),
                ("a = site_a[\"@changes\"]\n", "site_a.@changes", on(4, 22)),
                ("a = plc[\"a.b\"]\n", "plc.a.b", on(4, 14)),
                ("a = plc [ \"1\" ] [\"2\"]\n", "plc.1.2", on(4, 21)),
                (
                    "a = plc[\n\"1\"\n]\n",
                    "plc.1",
                    span(at(4, 0, 4), at(14, 2, 1)),
                ),
            ];
            for (text, name, span) in cases {
                let value = ok(text).attributes.iter().next().unwrap().value.clone();
                assert_eq!(value.kind, reference(name), "{text:?}");
                assert_eq!(value.span, Some(span), "{text:?}");
            }
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
        fn reads_identifiers_outside_ascii_as_hcl_does() {
            let text =
                "température = 1\n_é-1 = é(2)\nx = { e\u{301}t = 3 }\nétape {\n}\n";
            let call = value::Kind::Call(Call {
                function: "é".into(),
                function_span: None,
                arguments: vec![value(integer(2))],
            });
            let mut expected = attributes(vec![
                ("température", integer(1)),
                ("_é-1", call),
                ("x", value::Kind::Map(map(vec![("e\u{301}t", integer(3))]))),
            ]);
            expected
                .blocks
                .push(block("étape", &[], Document::default()));
            let document = ok(text);
            assert_eq!(document, expected);

            let key = document.attributes.get("température").unwrap();
            assert_eq!(key.key_span, Some(span(at(0, 0, 0), at(12, 0, 11))));
            let x = document.attributes.get("x").unwrap();
            let value::Kind::Map(object) = &x.value.kind else {
                panic!("not a map: {x:?}");
            };
            let key = object.get("e\u{301}t").unwrap();
            assert_eq!(key.key_span, Some(span(at(37, 2, 6), at(41, 2, 9))));
        }

        #[test]
        fn reads_for_as_a_word_after_the_start_of_a_list_or_an_object() {
            let text = "a = [1, for]\nb = { k = 1, for = 2 }\nc = for(1)\nd = f(for)\n\
                        e = [\"for\"]\nf = { \"for\" = 1 }\ng = [for-x]\nfor = 1\n\
                        h { for = 1 }\n";
            let call = |function: &str, argument: value::Kind| {
                value::Kind::Call(Call {
                    function: function.into(),
                    function_span: None,
                    arguments: vec![value(argument)],
                })
            };
            let list = |kind| value::Kind::List(vec![value(kind)]);
            let mut expected = attributes(vec![
                (
                    "a",
                    value::Kind::List(vec![value(integer(1)), value(reference("for"))]),
                ),
                (
                    "b",
                    value::Kind::Map(map(vec![("k", integer(1)), ("for", integer(2))])),
                ),
                ("c", call("for", integer(1))),
                ("d", call("f", reference("for"))),
                ("e", list(string("for"))),
                ("f", value::Kind::Map(map(vec![("for", integer(1))]))),
                ("g", list(reference("for-x"))),
                ("for", integer(1)),
            ]);
            expected.blocks.push(block(
                "h",
                &[],
                attributes(vec![("for", integer(1))]),
            ));
            assert_eq!(ok(text), expected);
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

        #[test]
        fn reads_a_call_named_as_a_value_word_as_hcl_does() {
            let call = |function: &str, arguments: Vec<Value>| {
                value::Kind::Call(Call {
                    function: function.into(),
                    function_span: None,
                    arguments,
                })
            };
            let expected = attributes(vec![
                ("a", call("true", vec![value(integer(1))])),
                ("b", call("null", Vec::new())),
            ]);
            assert_eq!(ok("a = true(1)\nb = null()\n"), expected);
        }

        #[test]
        fn reads_past_new_lines_inside_brackets_as_hcl_does() {
            let list = |items: Vec<value::Kind>| {
                value::Kind::List(items.into_iter().map(value).collect())
            };
            let f = |argument| {
                value::Kind::Call(Call {
                    function: "f".into(),
                    function_span: None,
                    arguments: vec![value(argument)],
                })
            };
            let cases = [
                ("a = [-\n5]\n", list(vec![integer(-5)])),
                ("a = [-\n\n5]\n", list(vec![integer(-5)])),
                ("a = [-\n# c\n5]\n", list(vec![integer(-5)])),
                ("a = f(-\n5)\n", f(integer(-5))),
                ("a = [f\n(1)]\n", list(vec![f(integer(1))])),
                (
                    "a = { k = [-\n5] }\n",
                    value::Kind::Map(map(vec![("k", list(vec![integer(-5)]))])),
                ),
                (
                    "a = [{ k = 1 }\n, 2]\n",
                    list(vec![
                        value::Kind::Map(map(vec![("k", integer(1))])),
                        integer(2),
                    ]),
                ),
            ];
            for (text, expected) in cases {
                assert_eq!(ok(text), attributes(vec![("a", expected)]), "{text:?}");
            }
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

        const START: &str =
            "a marker, such as `EOT`, and a new line to start the heredoc";
        const END: &str = "the marker on a line of its own to end the heredoc";
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
                ("<<END-1_a\nx\nEND-1_a\n", "x\n"),
                ("<<_\nx\n_\n", "x\n"),
                ("<<ÉOT\nx\nÉOT\n", "x\n"),
                ("<<EOT\nx\nEOT\u{301}\nEOT\n", "x\nEOT\u{301}\n"),
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
                ("a = <<\u{301}EOT\n", 6),
            ];
            for (text, end) in cases {
                let start = syntax(on(4, end), Expected::HeredocStart);
                check(text, &[(start, &needs(START))]);
            }
        }

        #[test]
        fn points_an_unclosed_heredoc_at_the_end_of_the_text() {
            let cases = [
                ("a = <<EOT\n", at(10, 1, 0), on(4, 9)),
                ("a = <<EOT\nx", at(11, 1, 1), on(4, 9)),
                ("a = <<EOT\nx\n", at(12, 2, 0), on(4, 9)),
                ("a = <<EOT\nEOTX\nEOT x\n", at(21, 3, 0), on(4, 9)),
                ("a = <<-EOT\n  x\n  eot\n", at(21, 3, 0), on(4, 10)),
                ("a = <<EOT\nx\nEOT\r", at(16, 2, 4), on(4, 9)),
            ];
            for (text, end, opener) in cases {
                let unclosed = Error::Unclosed {
                    span: span(end, end),
                    opener,
                    part: Unclosed::Heredoc,
                };
                check(text, &[(unclosed, &needs(END))]);
            }
        }

        #[test]
        fn needs_a_line_end_after_a_heredoc_at_the_end_of_the_text() {
            let cases = [
                ("a = <<EOT\nx\nEOT", at(15, 2, 3)),
                ("a = <<EOT\nx\nEOT  ", at(17, 2, 5)),
                ("a = <<-EOT\n  x\n  EOT", at(20, 2, 5)),
            ];
            for (text, end) in cases {
                let newline = syntax(span(end, end), Expected::Newline);
                check(text, &[(newline, &needs(Expected::Newline))]);
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
                    &needs("`=`, a label, or `{` after the name"),
                )],
            );
            check(
                "b \"l\" <<EOT\nx\nEOT\n {\n}\n",
                &[(
                    syntax(span(at(6, 0, 6), at(17, 2, 3)), Expected::BlockStart),
                    &needs("a label or `{`"),
                )],
            );
            check(
                "a = { <<EOT\nk\nEOT\n = 1 }\n",
                &[(
                    syntax(span(at(6, 0, 6), at(17, 2, 3)), Expected::Key),
                    &needs("a key or `}`"),
                )],
            );
        }

        #[test]
        fn reads_on_after_a_form_before_a_heredoc() {
            let operator = |span| Error::Form {
                span,
                form: Form::Operator,
            };
            let operator_message = refused(Form::Operator);
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
        const NUMBER: &str = "the number is out of range. Use an integer that fits \
                              in 128 bits, or a float that fits in 64 bits";
        const MALFORMED: &str = "the number is not valid. Write a number such as \
                                 `1.5e3`, or put the text in quotes to make a string";
        const REPEAT: &str = "the key \"a\" repeats an earlier key. Remove it, or \
                              give it a different key";

        /// The text of a bad segment error: `message`, then its fix.
        fn bad_segment(message: &str) -> String {
            format!(
                "{message}. Use one or more ASCII letters, digits, `_`, and `-` in \
                 that segment, after an optional leading `@`"
            )
        }

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
            check("s = \"\\\r\n\"", &[escape(5, 6)]);
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
                ("a = [for]\n", on(5, 8), Form::For),
                ("a = [for, 1]\n", on(5, 8), Form::For),
                ("a = [for(1)]\n", on(5, 8), Form::For),
                ("a = [for.x]\n", on(5, 8), Form::For),
                ("a = { for = 1 }\n", on(6, 9), Form::For),
                ("a = { for : 1 }\n", on(6, 9), Form::For),
                ("a = { for.x = 1 }\n", on(6, 9), Form::For),
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
                (
                    "a = é::f()\n",
                    span(at(6, 0, 5), at(8, 0, 7)),
                    Form::Namespace,
                ),
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
                ("a = { 1.e5 = 1 }\n", on(6, 10), Form::NumberKey),
                ("a = { -1.5 = 1 }\n", on(7, 10), Form::NumberKey),
                ("a = { - 1.5 = 1 }\n", on(8, 11), Form::NumberKey),
                ("a = { - = 1 }\n", on(6, 7), Form::Operator),
                ("a = { -x = 1 }\n", on(6, 7), Form::Operator),
            ];
            for (text, span, form) in cases {
                let message = refused(form);
                check(text, &[(Error::Form { span, form }, &message)]);
            }
        }

        #[test]
        fn refuses_each_form_after_a_reference_or_a_keyword_at_its_token() {
            let cases = [
                ("a = true.f\n", on(8, 9), Form::Index),
                ("a = false.x\n", on(9, 10), Form::Index),
                ("a = b.0\n", on(5, 6), Form::Index),
                ("a = site_a.1\n", on(10, 11), Form::Index),
                ("a = b.1-2\n", on(5, 6), Form::Index),
                ("a = b.0c\n", on(5, 6), Form::Index),
                ("a = [kf1.5true]\n", on(8, 9), Form::Index),
                ("a = b.c[0]\n", on(7, 8), Form::Index),
                ("a = plc[0]\n", on(7, 8), Form::Index),
                ("a = plc[true]\n", on(7, 8), Form::Index),
                ("a = plc[x]\n", on(7, 8), Form::Index),
                ("a = plc[\"a\" + \"b\"]\n", on(7, 8), Form::Index),
                ("a = plc[\"a\"][0]\n", on(12, 13), Form::Index),
                ("a = plc[\"a\"].0\n", on(12, 13), Form::Index),
                ("a = true[\"x\"]\n", on(8, 9), Form::Index),
                ("a = f(1)[\"a\"]\n", on(8, 9), Form::Index),
                ("a = b.c.*\n", on(7, 8), Form::Splat),
            ];
            for (text, span, form) in cases {
                let message = refused(form);
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
                ("a = [for\n]\n", on(5, 8), Form::For),
                (
                    "a = {\nfor = 1 }\n",
                    span(at(6, 1, 0), at(9, 1, 3)),
                    Form::For,
                ),
                (
                    "a = {\n  for k, v in m : k => v\n}\n",
                    span(at(8, 1, 2), at(11, 1, 5)),
                    Form::For,
                ),
                ("a = b[\n  *\n]\n", on(5, 6), Form::Splat),
            ];
            for (text, span, form) in cases {
                let message = refused(form);
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
            let message = refused(Form::Operator);
            check(
                "a = null\nb = 1 + 2\n",
                &[(null, NULL), (operator, &message)],
            );
        }

        #[test]
        fn refuses_null_and_the_index_after_it_as_hcl_does() {
            let null = Error::Form {
                span: on(4, 8),
                form: Form::Null,
            };
            let index = Error::Form {
                span: on(8, 9),
                form: Form::Index,
            };
            check(
                "a = null.x\n",
                &[(null, NULL), (index, &refused(Form::Index))],
            );
        }

        #[test]
        fn refuses_an_object_key_that_is_an_expression_and_reads_on() {
            let cases = [
                ("a = { f() = 1 }\n", on(7, 8)),
                ("a = { b.c = 1 }\n", on(7, 8)),
                ("a = { b[0] = 1 }\n", on(7, 8)),
                ("a = { [1] = 1 }\n", on(6, 7)),
                ("a = { {} = 1 }\n", on(6, 7)),
                ("a = { 1 + 2 = 3 }\n", on(8, 9)),
                ("a = { b - 1 = 2 }\n", on(8, 9)),
                ("a = { b * 2 = 2 }\n", on(8, 9)),
                ("a = { 1.5 + 2 = 3 }\n", on(10, 11)),
                ("a = { \"k\" + \"j\" = 1 }\n", on(10, 11)),
                ("a = { b ? 1 : 2 = 3 }\n", on(8, 9)),
                ("a = { p::f() = 1 }\n", on(7, 9)),
                ("a = { k... = 1 }\n", on(7, 10)),
                ("a = {\n  b.c = 1\n}\n", span(at(9, 1, 3), at(10, 1, 4))),
            ];
            let message = refused(Form::ExpressionKey);
            for (text, span) in cases {
                let key = Error::Form {
                    span,
                    form: Form::ExpressionKey,
                };
                check(text, &[(key, &message)]);
            }
            let key = Error::Form {
                span: on(7, 8),
                form: Form::ExpressionKey,
            };
            let null = Error::Form {
                span: on(19, 23),
                form: Form::Null,
            };
            check(
                "a = { f() = 1, b = null }\n",
                &[(key, &message), (null, NULL)],
            );
            for (text, null) in [
                ("a = { [1] = 1, b = null }\n", on(19, 23)),
                ("a = { {} = 1, b = null }\n", on(18, 22)),
            ] {
                let key = Error::Form {
                    span: on(6, 7),
                    form: Form::ExpressionKey,
                };
                let null = Error::Form {
                    span: null,
                    form: Form::Null,
                };
                check(text, &[(key, &message), (null, NULL)]);
            }
            let not = Error::Form {
                span: on(6, 7),
                form: Form::Operator,
            };
            check("a = { !b = 1 }\n", &[(not, &refused(Form::Operator))]);
        }

        #[test]
        fn refuses_a_number_key_that_hcl_rounds_and_reads_on() {
            let message = refused(Form::NumberKey);
            let digits = "9".repeat(155);
            let long = Error::Form {
                span: on(6, 161),
                form: Form::NumberKey,
            };
            check(&format!("a = {{ {digits} = 1 }}\n"), &[(long, &message)]);
            let past = Error::Form {
                span: on(6, 18),
                form: Form::NumberKey,
            };
            check("a = { 1e3000000000 = 1 }\n", &[(past, &message)]);
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
                &[(key.clone(), &message), (null, NULL)],
            );
            let value = Error::Form {
                span: on(12, 16),
                form: Form::Null,
            };
            check("a = { 1.5 = null }\n", &[(key, &message), (value, NULL)]);
        }

        #[test]
        fn points_an_unclosed_string_at_the_end_of_its_line() {
            let quote = &needs("`\"` to end the string");
            let unclosed = |end| Error::Unclosed {
                span: on(end, end),
                opener: on(4, 5),
                part: Unclosed::String,
            };
            check("s = \"abc", &[(unclosed(8), quote)]);
            for text in ["s = \"ab\nc\"\n", "s = \"ab\r\nc\"\r\n"] {
                check(text, &[(unclosed(7), quote)]);
            }
            let errors = read(Source(0), "s = \"ab\nc\"\n").unwrap_err();
            let note = Note {
                span: on(4, 5),
                text: "the string starts here".into(),
            };
            assert_eq!(Diagnostic::from(&errors[0]).notes, vec![note]);
        }

        #[test]
        fn points_an_unclosed_comment_at_the_end_of_the_text() {
            let cases = [
                ("a = 1 /* x", at(10, 0, 10)),
                ("a = 1 /* x\ny", at(12, 1, 1)),
                ("a = 1 /*/", at(9, 0, 9)),
            ];
            for (text, end) in cases {
                let unclosed = Error::Unclosed {
                    span: span(end, end),
                    opener: on(6, 8),
                    part: Unclosed::Comment,
                };
                check(text, &[(unclosed, &needs("`*/` to end the comment"))]);
            }
        }

        #[test]
        fn names_what_the_grammar_needs() {
            let cases = [
                ("a = \n", span(at(4, 0, 4), at(5, 1, 0)), Expected::Value),
                (
                    "a = # c\r\n",
                    span(at(7, 0, 7), at(9, 1, 0)),
                    Expected::Value,
                ),
                ("a = $\n", on(4, 5), Expected::Value),
                ("a =", on(3, 3), Expected::Value),
                ("a = [", on(5, 5), Expected::Value),
                ("a = ?\n", on(4, 5), Expected::Value),
                ("a = .5\n", on(4, 5), Expected::Value),
                ("a = -\n7\n", on(4, 5), Expected::Value),
                ("a = [{ k = -\n7 }]\n", on(11, 12), Expected::Value),
                ("a = 1\rb = 2\n", on(5, 6), Expected::Newline),
                ("a = 1ex\n", on(5, 7), Expected::Newline),
                ("a = 1 b = 2\n", on(6, 7), Expected::Newline),
                ("a 1\n", on(2, 3), Expected::AttributeOrBlock),
                ("b \"x\" 1\n", on(6, 7), Expected::BlockStart),
                ("b { a = 1 c = 2 }\n", on(10, 11), Expected::BlockEnd),
                ("b { = }\n", on(4, 5), Expected::Key),
                ("b { a 1 }\n", on(6, 7), Expected::Equals),
                ("b { a {} }\n", on(6, 7), Expected::Equals),
                ("b { a.b = 1 }\n", on(5, 6), Expected::Equals),
                ("b a.b {}\n", on(3, 4), Expected::BlockStart),
                ("b {}  x\n", on(6, 7), Expected::Newline),
                (
                    "b {\n  a = 1 }\n",
                    span(at(12, 1, 8), at(13, 1, 9)),
                    Expected::Newline,
                ),
                ("b {\n", span(at(4, 1, 0), at(4, 1, 0)), Expected::Item),
                ("}\n", on(0, 1), Expected::Item),
                ("a.b = 1\n", on(1, 2), Expected::AttributeOrBlock),
                ("@a = 1\n", on(0, 1), Expected::Item),
                ("a = @system.x\n", on(4, 5), Expected::Value),
                ("a = b.\n", on(5, 6), Expected::Newline),
                ("a = x.\nb = 1\n", on(5, 6), Expected::Newline),
                ("a = true.\n", on(8, 9), Expected::Newline),
                ("a = b..c\n", on(5, 6), Expected::Newline),
                ("a = b.-c\n", on(5, 6), Expected::Newline),
                ("a = b.c.@d\n", on(7, 8), Expected::Newline),
                ("a = [b.]\n", on(6, 7), Expected::ListEnd),
                (
                    "a = x\n.b\n",
                    span(at(6, 1, 0), at(7, 1, 1)),
                    Expected::Item,
                ),
                ("a = [1 2]\n", on(7, 8), Expected::ListEnd),
                (
                    "a = [1, 2\n",
                    span(at(10, 1, 0), at(10, 1, 0)),
                    Expected::ListEnd,
                ),
                ("a = { = 1 }\n", on(6, 7), Expected::Key),
                ("a = { k 1 }\n", on(8, 9), Expected::ObjectEquals),
                ("a = { a b = 1 }\n", on(8, 9), Expected::ObjectEquals),
                ("a = { \"k\"(1) = 2 }\n", on(9, 10), Expected::ObjectEquals),
                ("a = { 1(2) = 3 }\n", on(7, 8), Expected::ObjectEquals),
                ("a = { -1(2) = 3 }\n", on(8, 9), Expected::ObjectEquals),
                ("a = { 1.5(2) = 3 }\n", on(9, 10), Expected::ObjectEquals),
                ("a = { k = 1 j = 2 }\n", on(12, 13), Expected::ObjectEnd),
                ("a = f(1 2)\n", on(8, 9), Expected::ArgumentsEnd),
                ("40001 = 1\n", on(0, 5), Expected::Item),
                ("b 1 {}\n", on(2, 3), Expected::AttributeOrBlock),
            ];
            for (text, span, expected) in cases {
                let message = needs(expected);
                check(text, &[(syntax(span, expected), &message)]);
            }
        }

        #[test]
        fn refuses_the_close_of_another_bracket() {
            let cases = [
                ("a = [1)\n", on(6, 7), Expected::ListEnd),
                ("a = [)\n", on(5, 6), Expected::Value),
                ("a = f(1]\n", on(7, 8), Expected::ArgumentsEnd),
                ("a = f(]\n", on(6, 7), Expected::Value),
            ];
            for (text, span, expected) in cases {
                let message = needs(expected);
                check(text, &[(syntax(span, expected), &message)]);
            }
        }

        #[test]
        fn keeps_reading_after_a_form() {
            let form = |span, form: Form| {
                let message = refused(form);
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
            let long = "a".repeat(256);
            let error = long.parse::<Name>().unwrap_err();
            let name = Error::Name {
                span: on(4, 260),
                error,
            };
            let message = "a name or pattern is 256 bytes long, more than the limit of \
                           255 bytes. Use fewer or shorter segments";
            check(&format!("r = {long}"), &[(name, message)]);
        }

        #[test]
        fn refuses_each_reference_outside_ascii() {
            let name = |text: &str, span| Error::Name {
                span,
                error: text.parse::<Name>().unwrap_err(),
            };
            check(
                "a = x.température\nb = [é]\nc = f(x.é)\n",
                &[
                    (
                        name("x.température", span(at(4, 0, 4), at(18, 0, 17))),
                        &bad_segment(
                            "a segment is not valid: \"température\" in \
                             \"x.température\"",
                        ),
                    ),
                    (
                        name("é", span(at(24, 1, 5), at(26, 1, 6))),
                        &bad_segment(r#"a segment is not valid: "é" in "é""#),
                    ),
                    (
                        name("x.é", span(at(34, 2, 6), at(38, 2, 9))),
                        &bad_segment(r#"a segment is not valid: "é" in "x.é""#),
                    ),
                ],
            );
        }

        #[test]
        fn refuses_a_string_index_that_is_not_a_segment() {
            let name = |text: &str, span| Error::Name {
                span,
                error: text.parse::<Name>().unwrap_err(),
            };
            let wildcard = "a wildcard is out of place: \"plc.*\". \
                            Use `*` and `**` only as whole segments of a pattern, \
                            never in a name";
            check(
                "a = plc[\"\"]\nb = plc[\"*\"]\nc = plc[<<EOT\nx\nEOT\n]\n",
                &[
                    (
                        name("plc.", on(4, 11)),
                        &bad_segment(r#"a segment is not valid: "" in "plc.""#),
                    ),
                    (name("plc.*", span(at(16, 1, 4), at(24, 1, 12))), wildcard),
                    (
                        name("plc.x\n", span(at(29, 2, 4), at(46, 5, 1))),
                        &bad_segment(r#"a segment is not valid: "x\n" in "plc.x\n""#),
                    ),
                ],
            );
            let index = Error::Form {
                span: on(7, 8),
                form: Form::Index,
            };
            let template = Error::Form {
                span: on(9, 11),
                form: Form::Template,
            };
            check(
                "a = plc[\"${x}\"]\n",
                &[(index, &refused(Form::Index)), (template, TEMPLATE)],
            );
        }

        #[test]
        fn gives_only_the_string_error_in_a_string_index() {
            check(
                "a = plc[\"\\q\"]\n",
                &[(Error::Escape { span: on(9, 11) }, ESCAPE)],
            );
            let unclosed = Error::Unclosed {
                span: on(12, 12),
                opener: on(8, 9),
                part: Unclosed::String,
            };
            check(
                "a = plc[\"abc",
                &[(unclosed, &needs("`\"` to end the string"))],
            );
        }

        #[test]
        fn refuses_a_character_that_no_identifier_holds() {
            let cases = [
                (
                    "\u{200b}a = 1\n",
                    span(at(0, 0, 0), at(3, 0, 1)),
                    Expected::Item,
                ),
                (
                    "a\u{200b} = 1\n",
                    span(at(1, 0, 1), at(4, 0, 2)),
                    Expected::AttributeOrBlock,
                ),
            ];
            for (text, span, expected) in cases {
                let message = needs(expected);
                check(text, &[(syntax(span, expected), &message)]);
            }
        }

        #[test]
        fn refuses_a_compatibility_character_that_hcl_reads() {
            let span = span(at(0, 0, 0), at(2, 0, 1));
            let message = needs(Expected::Item);
            check("\u{37a} = 1\n", &[(syntax(span, Expected::Item), &message)]);
        }

        #[test]
        fn refuses_a_number_out_of_range() {
            let number = |start, end| {
                let span = on(start, end);
                let problem = Number::Range;
                (Error::Number { span, problem }, NUMBER)
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
            check("f = 9e-400\n", &[number(4, 10)]);
            check("f = 10e9223372036854775807\n", &[number(4, 26)]);
            check("f = 0.01e-9223372036854775808\n", &[number(4, 29)]);
            check("f = 2e-324\n", &[number(4, 10)]);
            check("f = 1e2147483647\n", &[number(4, 16)]);
            check("f = 1e-3000000000\n", &[number(4, 17)]);
        }

        #[test]
        fn refuses_text_that_hcl_scans_as_a_number_but_cannot_read() {
            let malformed = |start, end| {
                let span = on(start, end);
                let problem = Number::Malformed;
                (Error::Number { span, problem }, MALFORMED)
            };
            let cases = [
                ("a = 1.2.3\n", malformed(4, 9)),
                ("a = 1e5e5\n", malformed(4, 9)),
                ("a = 1..5\n", malformed(4, 8)),
                ("a = 1.e5.e5\n", malformed(4, 11)),
                ("a = -1.2.3\n", malformed(4, 10)),
                ("a = [1.2.3, 2]\n", malformed(5, 10)),
                ("a = 0e9223372036854775808\n", malformed(4, 25)),
                ("a = 0E9223372036854775808\n", malformed(4, 25)),
                ("a = 0.e9223372036854775808\n", malformed(4, 26)),
                ("a = 0.e-9223372036854775809\n", malformed(4, 27)),
                ("a = { -1.2.3 = 1 }\n", malformed(6, 12)),
            ];
            for (text, error) in cases {
                check(text, &[error]);
            }
            let null = Error::Form {
                span: on(21, 25),
                form: Form::Null,
            };
            check(
                "a = { 1.2.3 = 1, b = null }\n",
                &[malformed(6, 11), (null, NULL)],
            );
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
            let value = &needs("a value");
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
            let number = Error::Number {
                span: on(4, 9),
                problem: Number::Range,
            };
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
            let text = "m = { a = 1, a = 2, b = null }\nr = é\nc = \n";
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
                span: span(at(35, 1, 4), at(37, 1, 5)),
                error: "é".parse::<Name>().unwrap_err(),
            };
            let value = syntax(span(at(42, 2, 4), at(43, 3, 0)), Expected::Value);
            check(
                text,
                &[
                    (Error::Document(repeat), REPEAT),
                    (null, NULL),
                    (name, &bad_segment(r#"a segment is not valid: "é" in "é""#)),
                    (value, &needs("a value")),
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
                let bytes = Checked::new(document.clone()).unwrap().encode();
                assert_eq!(
                    document::encoding::decode(&bytes).unwrap().document(),
                    &document
                );
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
                "the document nests deeper than 64 levels. Make it flatter"
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
                let document = Checked::new(document).unwrap();
                let mut chars: Vec<char> = write(&document).unwrap().chars().collect();
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
                    let checked = Checked::new(document.clone()).map_err(|error| {
                        TestCaseError::fail(format!("{error:?} from {text:?}"))
                    })?;
                    let written = write(&checked);
                    let Ok(written) = written else {
                        return Err(TestCaseError::fail(format!(
                            "{written:?} from {text:?}"
                        )));
                    };
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

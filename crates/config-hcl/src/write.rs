use std::fmt::Write as _;

use document::encoding;
use document::value::{Call, Kind, Value};
use document::{Attribute, Block, Document, Map};

use crate::Unwritable;
use crate::lex;
use crate::parse::{opens_for, rooted};

/// The widest line, in characters, that holds a list, a map, or a call on one line.
const WIDTH: usize = 88;

/// What each level of a body or a value goes in by.
pub(crate) const INDENT: &str = "  ";

/// What follows a value on its line. In a list, a map, or a call, it is what follows
/// when each item is on its own line.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum After {
    /// A line end, so the value can be a heredoc.
    Line,
    /// A `,`, which counts in the width of the line.
    Comma,
    /// A comment, or the end of a text with no line end, so the value is not a
    /// heredoc. A comment does not count in the width of the line.
    Other,
}

/// Writes `document` as HCL text that [`read`](crate::read) reads as an equal
/// Document.
///
/// Attributes come first, in key order, then each block after a blank line. A list,
/// a map, or a call is on one line when that line fits in 88 characters. If not, each
/// item is on its own line, 2 spaces in. A string that ends in a new line and holds
/// no control character but new lines and tabs is a heredoc where it is the value of
/// a key.
///
/// A text of more than `u32::MAX` bytes is written too, and `read` refuses it with
/// [`Error::TooLarge`](crate::Error::TooLarge).
///
/// # Errors
///
/// Returns each part that HCL text cannot hold, in Document order. A Document nested
/// deeper than [`encoding::DEPTH_MAX`] gives only [`Unwritable::TooDeep`], at the
/// first level past the limit.
pub fn write(document: &Document) -> Result<String, Vec<Unwritable>> {
    encoding::check(document)
        .map_err(|too_deep| vec![Unwritable::TooDeep(too_deep)])?;
    let mut writer = Writer::default();
    writer.body(document.attributes.iter(), &document.blocks, 0, false);
    writer.finish()
}

/// A value with items.
#[derive(Clone, Copy)]
enum Items<'a> {
    List(&'a [Value]),
    Call(&'a Call),
    Map(&'a Map),
}

impl<'a> Items<'a> {
    fn of(kind: &'a Kind) -> Option<Self> {
        match kind {
            Kind::List(values) => Some(Self::List(values)),
            Kind::Call(call) => Some(Self::Call(call)),
            Kind::Map(map) => Some(Self::Map(map)),
            Kind::Bool(_)
            | Kind::Integer(_)
            | Kind::Float(_)
            | Kind::String(_)
            | Kind::Reference(_) => None,
        }
    }

    fn len(self) -> usize {
        match self {
            Self::List(values) => values.len(),
            Self::Call(call) => call.arguments.len(),
            Self::Map(map) => map.iter().len(),
        }
    }

    fn close(self) -> char {
        match self {
            Self::List(_) => ']',
            Self::Call(_) => ')',
            Self::Map(_) => '}',
        }
    }
}

/// Recurses once per level, so it runs only on a Document that [`encoding::check`]
/// accepts.
#[derive(Default)]
pub(crate) struct Writer<'a> {
    out: String,
    errors: Vec<Unwritable>,
    /// What each line starts with, before its indent.
    margin: &'a str,
    /// The column where the first line starts.
    start: usize,
    /// The text goes in a file whose lines end in `\r\n`. A heredoc there keeps a
    /// `\r` in its value, so each string is quoted, and each `\n` written ends a line.
    crlf: bool,
}

impl<'a> Writer<'a> {
    /// A writer for text that goes at column `start` of a line that starts with
    /// `margin`, in a file whose lines end in `\r\n` when `crlf`.
    pub(crate) fn new(margin: &'a str, start: usize, crlf: bool) -> Self {
        Self {
            margin,
            start,
            crlf,
            ..Self::default()
        }
    }

    /// The text written, with `\r\n` line ends for a file whose lines end in `\r\n`,
    /// or each part that HCL text cannot hold.
    pub(crate) fn finish(self) -> Result<String, Vec<Unwritable>> {
        if !self.errors.is_empty() {
            return Err(self.errors);
        }
        Ok(if self.crlf {
            self.out.replace('\n', "\r\n")
        } else {
            self.out
        })
    }

    /// Writes each attribute on its own lines, then each block after a blank line,
    /// `indent` levels in. `preceded` tells that an item of the body is before them,
    /// so the first block gets a blank line too.
    pub(crate) fn body<'d>(
        &mut self,
        attributes: impl IntoIterator<Item = &'d Attribute>,
        blocks: impl IntoIterator<Item = &'d Block>,
        indent: usize,
        preceded: bool,
    ) {
        let mut written = preceded;
        for attribute in attributes {
            self.pad(indent);
            if !lex::identifier(&attribute.key) {
                self.errors.push(Unwritable::Key {
                    span: attribute.key_span,
                });
            }
            self.out.push_str(&attribute.key);
            self.out.push_str(" = ");
            self.value(&attribute.value, indent, After::Line);
            self.out.push('\n');
            written = true;
        }
        for block in blocks {
            if written {
                self.end_line();
            }
            self.pad(indent);
            self.block(block, indent);
            self.out.push('\n');
            written = true;
        }
    }

    /// Writes a block from its keyword to its `}`, with its inner lines `indent`
    /// levels in.
    pub(crate) fn block(&mut self, block: &Block, indent: usize) {
        if !lex::identifier(&block.keyword) {
            self.errors.push(Unwritable::Keyword {
                span: block.keyword_span,
            });
        }
        self.out.push_str(&block.keyword);
        for label in &block.labels {
            self.out.push(' ');
            quoted(&mut self.out, &label.text);
        }
        if block.body == Document::default() {
            self.out.push_str(" {}");
            return;
        }
        self.out.push_str(" {\n");
        let body = &block.body;
        self.body(
            body.attributes.iter(),
            &body.blocks,
            indent.saturating_add(1),
            false,
        );
        self.pad(indent);
        self.out.push('}');
    }

    /// Writes a value on one line when the line fits, and otherwise with each item on
    /// its own line. A line with a heredoc in it is more than one line, so it does not
    /// fit.
    pub(crate) fn value(&mut self, value: &Value, indent: usize, after: After) {
        let Some(items) = Items::of(&value.kind) else {
            return self.line(value, after);
        };
        let mut line = Self {
            crlf: self.crlf,
            ..Self::default()
        };
        line.line(value, after);
        let comma = usize::from(after == After::Comma);
        let width = self
            .column()
            .saturating_add(line.out.chars().count())
            .saturating_add(comma);
        if width <= WIDTH && !line.out.contains('\n') {
            self.out.push_str(&line.out);
            self.errors.append(&mut line.errors);
            return;
        }
        let inner = indent.saturating_add(1);
        self.open(items);
        match items {
            Items::List(values) => self.items(values, inner),
            Items::Call(call) => self.items(&call.arguments, inner),
            Items::Map(map) => {
                for attribute in map.iter() {
                    self.out.push('\n');
                    self.pad(inner);
                    key(&mut self.out, &attribute.key);
                    self.out.push_str(" = ");
                    self.value(&attribute.value, inner, After::Line);
                }
            }
        }
        if items.len() > 0 {
            self.out.push('\n');
            self.pad(indent);
        }
        self.out.push(items.close());
    }

    /// Writes each item on its own line, `indent` levels in, with a `,` after it.
    fn items(&mut self, values: &[Value], indent: usize) {
        for item in values {
            self.out.push('\n');
            self.pad(indent);
            self.value(item, indent, After::Comma);
            self.out.push(',');
        }
    }

    /// Writes a value on one line, except a heredoc.
    fn line(&mut self, value: &Value, after: After) {
        let items = match &value.kind {
            Kind::Bool(b) => {
                return self.out.push_str(if *b { "true" } else { "false" });
            }
            Kind::Integer(n) => {
                return write!(self.out, "{n}")
                    .expect("invariant: a String takes any text");
            }
            Kind::Float(float) => {
                // Debug gives the shortest text that reads back as the same bits, with
                // a `.` or an `e`, so it never reads as an integer.
                return write!(self.out, "{:?}", float.get())
                    .expect("invariant: a String takes any text");
            }
            Kind::String(text)
                if after == After::Line && !self.crlf && whole_lines(text) =>
            {
                return heredoc(&mut self.out, text);
            }
            Kind::String(text) => return quoted(&mut self.out, text),
            Kind::Reference(name) => {
                if !rooted(name) {
                    self.errors.push(Unwritable::Reference { span: value.span });
                }
                let mut segments = name.segments();
                self.out.extend(segments.next());
                for segment in segments {
                    if lex::identifier(segment) {
                        self.out.push('.');
                        self.out.push_str(segment);
                    } else {
                        self.out.push('[');
                        quoted(&mut self.out, segment);
                        self.out.push(']');
                    }
                }
                return;
            }
            Kind::List(values) => Items::List(values),
            Kind::Call(call) => Items::Call(call),
            Kind::Map(map) => Items::Map(map),
        };
        self.open(items);
        match items {
            Items::List(values) => self.line_items(values),
            Items::Call(call) => self.line_items(&call.arguments),
            Items::Map(map) => {
                for (i, attribute) in map.iter().enumerate() {
                    self.out.push_str(if i == 0 { " " } else { ", " });
                    key(&mut self.out, &attribute.key);
                    self.out.push_str(" = ");
                    self.line(&attribute.value, After::Line);
                }
                if items.len() > 0 {
                    self.out.push(' ');
                }
            }
        }
        self.out.push(items.close());
    }

    fn line_items(&mut self, values: &[Value]) {
        for (i, item) in values.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.line(item, After::Comma);
        }
    }

    /// Writes what comes before the items: `[`, the function and `(`, or `{`.
    fn open(&mut self, items: Items<'_>) {
        match items {
            Items::List(values) => {
                if let Some(first) = values.first() {
                    self.refuse_for(first);
                }
                self.out.push('[');
            }
            Items::Call(call) => {
                if !lex::identifier(&call.function) {
                    self.errors.push(Unwritable::Function {
                        span: call.function_span,
                    });
                }
                self.out.push_str(&call.function);
                self.out.push('(');
            }
            Items::Map(_) => self.out.push('{'),
        }
    }

    /// Refuses the first item of a list at its first word when HCL reads that word
    /// after `[` as the start of a `for` expression.
    fn refuse_for(&mut self, item: &Value) {
        let span = match &item.kind {
            Kind::Reference(name) if name.segments().next().is_some_and(opens_for) => {
                item.span
            }
            // A call such as `for.x(1)` is refused for its function.
            Kind::Call(call) if &*call.function == "for" => call.function_span,
            Kind::Reference(_)
            | Kind::Call(_)
            | Kind::Bool(_)
            | Kind::Integer(_)
            | Kind::Float(_)
            | Kind::String(_)
            | Kind::List(_)
            | Kind::Map(_) => return,
        };
        self.errors.push(Unwritable::For { span });
    }

    /// Writes a line end: the blank line between an item and a block after it, or
    /// the end of a last line that has none.
    pub(crate) fn end_line(&mut self) {
        self.out.push('\n');
    }

    fn pad(&mut self, indent: usize) {
        self.out.push_str(self.margin);
        for _ in 0..indent {
            self.out.push_str(INDENT);
        }
    }

    /// The column after the last character so far.
    fn column(&self) -> usize {
        match self.out.rsplit_once('\n') {
            Some((_, line)) => line.chars().count(),
            None => self.start.saturating_add(self.out.chars().count()),
        }
    }
}

/// Writes a map key: bare when it is an identifier, and quoted when not. `for` is
/// quoted, because HCL reads `{ for` as a `for` expression.
fn key(out: &mut String, key: &str) {
    if lex::identifier(key) && !opens_for(key) {
        out.push_str(key);
    } else {
        quoted(out, key);
    }
}

/// Reports whether `text` is whole lines, each ended by a new line, with no control
/// character but tabs.
fn whole_lines(text: &str) -> bool {
    text.ends_with('\n')
        && !text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
}

/// Writes whole lines of text as a heredoc, up to its marker.
fn heredoc(out: &mut String, text: &str) {
    let mut marker = String::from("EOT");
    while text
        .split('\n')
        .any(|line| line.trim_matches(lex::space) == marker)
    {
        marker.push('_');
    }
    out.push_str("<<");
    out.push_str(&marker);
    out.push('\n');
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if opens_template(c, chars.peek()) {
            out.push(c);
        }
    }
    out.push_str(&marker);
}

fn quoted(out: &mut String, text: &str) {
    out.push('"');
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if opens_template(c, chars.peek()) => {
                out.push(c);
                out.push(c);
            }
            c if c.is_control() => write!(out, "\\u{:04x}", u32::from(c))
                .expect("invariant: a String takes any text"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Reports whether `c` before `next` opens a template, `${` or `%{`, which HCL text
/// writes as `$${` or `%%{`.
fn opens_template(c: char, next: Option<&char>) -> bool {
    matches!(c, '$' | '%') && next == Some(&'{')
}

#[cfg(test)]
mod tests {
    use document::encoding::TooDeep;
    use document::value::Float;
    use document::{Label, Position, Source, Span};
    use proptest::prelude::*;
    use types::name::Name;

    use super::*;
    use crate::arbitrary::document;
    use crate::{Error, read};

    fn on(start: u32, end: u32) -> Span {
        let at = |offset| Position {
            offset,
            line: 0,
            column: offset,
        };
        Span::new(Source(0), at(start), at(end)).unwrap()
    }

    fn value(kind: Kind) -> Value {
        Value { kind, span: None }
    }

    fn map(entries: Vec<(&str, Kind)>) -> Map {
        let attributes = entries
            .into_iter()
            .map(|(key, kind)| Attribute {
                key: key.into(),
                key_span: None,
                value: value(kind),
            })
            .collect();
        Map::new(attributes).unwrap()
    }

    fn attributes(entries: Vec<(&str, Kind)>) -> Document {
        Document {
            attributes: map(entries),
            blocks: Vec::new(),
        }
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

    fn string(text: &str) -> Kind {
        Kind::String(text.into())
    }

    fn float(f: f64) -> Kind {
        Kind::Float(Float::new(f).unwrap())
    }

    fn reference(name: &str) -> Kind {
        Kind::Reference(name.parse().unwrap())
    }

    fn list(items: Vec<Kind>) -> Kind {
        Kind::List(items.into_iter().map(value).collect())
    }

    fn call(function: &str, arguments: Vec<Kind>) -> Kind {
        Kind::Call(Call {
            function: function.into(),
            function_span: None,
            arguments: arguments.into_iter().map(value).collect(),
        })
    }

    /// Writes `document`, checks that it reads back the same, and returns the text.
    fn written(document: &Document) -> String {
        let text = write(document).unwrap();
        assert_eq!(read(Source(0), &text).as_ref(), Ok(document), "{text}");
        text
    }

    /// Any name, with segments that start with a digit, `-`, or `@`, and a first
    /// segment that may be a literal.
    fn any_name() -> impl Strategy<Value = Name> {
        "(true|false|null|@?[a-zA-Z0-9_-]{1,3})(\\.@?[a-zA-Z0-9_-]{1,3}){0,2}"
            .prop_map(|name| name.parse().unwrap())
    }

    proptest! {
        #[test]
        fn reads_what_it_writes(document in document()) {
            let text = write(&document).unwrap();
            prop_assert_eq!(read(Source(0), &text), Ok(document), "{}", text);
        }

        #[test]
        fn writes_a_name_exactly_when_its_first_segment_starts_a_reference(
            name in any_name(),
        ) {
            let document = attributes(vec![("a", Kind::Reference(name.clone()))]);
            let first = name.segments().next().unwrap();
            let root = first.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && !["true", "false", "null"].contains(&first);
            match write(&document) {
                Ok(text) => {
                    prop_assert!(root, "{}", text);
                    prop_assert_eq!(read(Source(0), &text), Ok(document), "{}", text);
                }
                Err(errors) => {
                    prop_assert!(!root, "{}", name);
                    prop_assert_eq!(errors, vec![Unwritable::Reference { span: None }]);
                }
            }
        }
    }

    #[test]
    fn writes_attributes_in_key_order_then_each_block_after_a_blank_line() {
        let inner = Document {
            attributes: map(vec![("rate", Kind::Integer(10))]),
            blocks: vec![block("empty", &[], Document::default())],
        };
        let document = Document {
            attributes: map(vec![("b", Kind::Integer(2)), ("a", Kind::Bool(true))]),
            blocks: vec![
                block("channel", &["temp", "a \"b\""], inner),
                block(
                    "blocks_only",
                    &[],
                    Document {
                        attributes: Map::default(),
                        blocks: vec![block("x", &["1"], Document::default())],
                    },
                ),
            ],
        };
        assert_eq!(
            written(&document),
            "a = true\n\
             b = 2\n\
             \n\
             channel \"temp\" \"a \\\"b\\\"\" {\n\
             \x20 rate = 10\n\
             \n\
             \x20 empty {}\n\
             }\n\
             \n\
             blocks_only {\n\
             \x20 x \"1\" {}\n\
             }\n"
        );
        let blocks = Document {
            attributes: Map::default(),
            blocks: vec![
                block("a", &[], Document::default()),
                block("b", &[], Document::default()),
            ],
        };
        assert_eq!(written(&blocks), "a {}\n\nb {}\n");
    }

    #[test]
    fn quotes_a_new_line_in_a_list_in_a_block() {
        let body = attributes(vec![("a", list(vec![string("\n")]))]);
        let document = Document {
            attributes: Map::default(),
            blocks: vec![block("a", &[], body)],
        };
        assert_eq!(written(&document), "a {\n  a = [\"\\n\"]\n}\n");
    }

    #[test]
    fn writes_each_kind_of_value() {
        let document = attributes(vec![
            ("bool", Kind::Bool(false)),
            ("integer", Kind::Integer(i128::MIN)),
            ("one", float(1.0)),
            ("small", float(-1e-7)),
            ("large", float(1e300)),
            ("zero", float(-0.0)),
            ("string", string("\"q\" \\ ${a} %{b} $c\t\r\u{1}é")),
            ("reference", reference("a_1.b-c.true")),
            ("words", list(vec![reference("a-b"), reference("for")])),
            (
                "map",
                Kind::Map(map(vec![
                    ("x", Kind::Integer(1)),
                    ("for", Kind::Integer(2)),
                    ("a b", Kind::Integer(3)),
                    ("7", Kind::Integer(4)),
                    ("été", Kind::Integer(5)),
                    ("a\u{200b}", Kind::Integer(6)),
                ])),
            ),
            (
                "call",
                call("true", vec![Kind::Integer(1), list(Vec::new())]),
            ),
            ("empty", call("f", vec![Kind::Map(Map::default())])),
            ("température", call("é", Vec::new())),
        ]);
        assert_eq!(
            written(&document),
            "bool = false\n\
             call = true(1, [])\n\
             empty = f({})\n\
             integer = -170141183460469231731687303715884105728\n\
             large = 1e300\n\
             map = { \"7\" = 4, \"a b\" = 3, \"a\u{200b}\" = 6, \"for\" = 2, x = 1, \
             été = 5 }\n\
             one = 1.0\n\
             reference = a_1.b-c.true\n\
             small = -1e-7\n\
             string = \"\\\"q\\\" \\\\ $${a} %%{b} $c\\t\\r\\u0001é\"\n\
             température = é()\n\
             words = [a-b, for]\n\
             zero = 0.0\n"
        );
    }

    #[test]
    fn writes_a_text_that_ends_in_a_new_line_as_a_heredoc_as_the_value_of_a_key() {
        let script = "#!/bin/sh\necho ${HOME}\n  EOT \n";
        let document = attributes(vec![
            ("script", string(script)),
            ("map", Kind::Map(map(vec![("run", string(script))]))),
            ("list", list(vec![string(script)])),
            ("crlf", string("a\r\n")),
            ("dollar", string("$a %b c{ ${x}\n")),
            ("tab", string("x\ty\n")),
        ]);
        assert_eq!(
            written(&document),
            "crlf = \"a\\r\\n\"\n\
             dollar = <<EOT\n\
             $a %b c{ $${x}\n\
             EOT\n\
             list = [\"#!/bin/sh\\necho $${HOME}\\n  EOT \\n\"]\n\
             map = {\n\
             \x20 run = <<EOT_\n\
             #!/bin/sh\n\
             echo $${HOME}\n\
             \x20 EOT \n\
             EOT_\n\
             }\n\
             script = <<EOT_\n\
             #!/bin/sh\n\
             echo $${HOME}\n\
             \x20 EOT \n\
             EOT_\n\
             tab = <<EOT\n\
             x\ty\n\
             EOT\n"
        );
    }

    #[test]
    fn folds_a_value_whose_line_passes_88_characters() {
        let fits = "é".repeat(80);
        let long = "é".repeat(81);
        let document = attributes(vec![
            ("a", list(vec![string(&fits)])),
            ("b", list(vec![string(&long)])),
        ]);
        assert_eq!(
            written(&document),
            format!("a = [\"{fits}\"]\nb = [\n  \"{long}\",\n]\n")
        );
        let key = "k".repeat(90);
        assert_eq!(
            written(&attributes(vec![(&key, list(Vec::new()))])),
            format!("{key} = []\n")
        );
    }

    #[test]
    fn ends_each_line_in_crlf_for_a_crlf_file() {
        let fits = "é".repeat(80);
        let long = "é".repeat(81);
        let document = Document {
            attributes: map(vec![
                ("a", list(vec![string(&fits)])),
                ("b", list(vec![string(&long)])),
            ]),
            blocks: vec![block("d", &[], attributes(vec![("c", string("x\n"))]))],
        };
        let mut writer = Writer::new("", 0, true);
        writer.body(document.attributes.iter(), &document.blocks, 0, false);
        let expected = format!(
            "a = [\"{fits}\"]\r\n\
             b = [\r\n  \"{long}\",\r\n]\r\n\
             \r\n\
             d {{\r\n  c = \"x\\n\"\r\n}}\r\n"
        );
        assert_eq!(writer.finish().unwrap(), expected);
    }

    #[test]
    fn counts_the_comma_after_an_item_in_its_line() {
        let fits = "a".repeat(81);
        let long = "a".repeat(82);
        let document = attributes(vec![(
            "k",
            list(vec![
                list(vec![string(&fits)]),
                list(vec![string(&long)]),
                Kind::Map(map(vec![("a", list(vec![string(&long)]))])),
                call("f", vec![string(&long), Kind::Integer(1)]),
            ]),
        )]);
        assert_eq!(
            written(&document),
            format!(
                "k = [\n\
                 \x20 [\"{fits}\"],\n\
                 \x20 [\n\
                 \x20   \"{long}\",\n\
                 \x20 ],\n\
                 \x20 {{\n\
                 \x20   a = [\n\
                 \x20     \"{long}\",\n\
                 \x20   ]\n\
                 \x20 }},\n\
                 \x20 f(\n\
                 \x20   \"{long}\",\n\
                 \x20   1,\n\
                 \x20 ),\n\
                 ]\n"
            )
        );
    }

    #[test]
    fn refuses_each_part_that_hcl_cannot_hold_in_document_order() {
        let document = Document {
            attributes: Map::new(vec![
                Attribute {
                    key: "my key".into(),
                    key_span: Some(on(0, 6)),
                    value: Value {
                        kind: reference("true"),
                        span: Some(on(9, 13)),
                    },
                },
                Attribute {
                    key: "fine".into(),
                    key_span: Some(on(14, 18)),
                    value: Value {
                        kind: Kind::Call(Call {
                            function: "a.b".into(),
                            function_span: Some(on(21, 24)),
                            arguments: vec![Value {
                                kind: reference("7a"),
                                span: Some(on(25, 27)),
                            }],
                        }),
                        span: Some(on(21, 28)),
                    },
                },
            ])
            .unwrap(),
            blocks: vec![Block {
                keyword_span: Some(on(30, 38)),
                ..block(
                    "my block",
                    &[],
                    attributes(vec![
                        ("a", reference("-a")),
                        ("b", reference("null")),
                        ("c", reference("false")),
                    ]),
                )
            }],
        };
        let at = |start, end| Some(on(start, end));
        assert_eq!(
            write(&document),
            Err(vec![
                Unwritable::Function { span: at(21, 24) },
                Unwritable::Reference { span: at(25, 27) },
                Unwritable::Key { span: at(0, 6) },
                Unwritable::Reference { span: at(9, 13) },
                Unwritable::Keyword { span: at(30, 38) },
                Unwritable::Reference { span: None },
                Unwritable::Reference { span: None },
                Unwritable::Reference { span: None },
            ])
        );

        // One word each, but not an identifier.
        let words = Document {
            attributes: map(vec![("a.b", Kind::Integer(1))]),
            blocks: vec![block("@c", &[], Document::default())],
        };
        assert_eq!(
            write(&words),
            Err(vec![
                Unwritable::Key { span: None },
                Unwritable::Keyword { span: None },
            ])
        );
    }

    #[test]
    fn writes_a_later_segment_that_is_not_an_identifier_as_a_string_index() {
        for (name, text) in [
            ("plc.40001", "a = plc[\"40001\"]\n"),
            ("a.-1.x", "a = a[\"-1\"].x\n"),
            ("site_a.@changes", "a = site_a[\"@changes\"]\n"),
            ("a.true.for", "a = a.true.for\n"),
        ] {
            let document = attributes(vec![("a", reference(name))]);
            assert_eq!(written(&document), text, "{name:?}");
        }
    }

    #[test]
    fn refuses_a_reference_whose_first_segment_hcl_reads_as_another_form() {
        let names = ["@a.b", "7b.a", "-b", "true.x", "null.x", "false"];
        for name in names {
            let document = attributes(vec![("a", reference(name))]);
            assert_eq!(
                write(&document),
                Err(vec![Unwritable::Reference { span: None }]),
                "{name:?}"
            );
        }
    }

    #[test]
    fn refuses_a_list_that_starts_with_for_as_the_reader_does() {
        let item = |kind, span| Value {
            kind,
            span: Some(span),
        };
        let long = string(&"x".repeat(90));
        let for_call = Kind::Call(Call {
            function: "for".into(),
            function_span: Some(on(5, 8)),
            arguments: vec![value(Kind::Integer(1))],
        });
        let cases = [
            (vec![item(reference("for"), on(5, 8))], on(5, 8)),
            (
                vec![item(reference("for.x"), on(5, 10)), value(Kind::Integer(1))],
                on(5, 10),
            ),
            (vec![item(for_call, on(5, 11))], on(5, 8)),
            (
                vec![item(reference("for"), on(5, 8)), value(long)],
                on(5, 8),
            ),
            (
                vec![value(Kind::List(vec![item(reference("for"), on(6, 9))]))],
                on(6, 9),
            ),
        ];
        for (items, span) in cases {
            let document = attributes(vec![("a", Kind::List(items))]);
            let error = Unwritable::For { span: Some(span) };
            assert_eq!(write(&document), Err(vec![error]), "{document:?}");
        }
    }

    #[test]
    fn refuses_a_list_that_starts_with_the_call_for_x_once_for_its_function() {
        let document = attributes(vec![("a", list(vec![call("for.x", Vec::new())]))]);
        let error = Unwritable::Function { span: None };
        assert_eq!(write(&document), Err(vec![error]));
    }

    #[test]
    fn writes_for_where_hcl_reads_it_as_a_word() {
        let document = attributes(vec![
            ("a", reference("for")),
            ("b", call("f", vec![reference("for")])),
            ("c", Kind::Map(map(vec![("k", reference("for"))]))),
            (
                "d",
                list(vec![list(vec![Kind::Integer(1)]), reference("for")]),
            ),
            ("e", list(vec![reference("for-x")])),
        ]);
        assert_eq!(
            written(&document),
            "a = for\nb = f(for)\nc = { k = for }\nd = [[1], for]\ne = [for-x]\n"
        );
    }

    #[test]
    fn refuses_nesting_past_the_limit_as_the_reader_does() {
        let deep = Value {
            kind: list(Vec::new()),
            span: Some(on(1, 2)),
        };
        let nest = |blocks: usize, key: &str| {
            let body = Document {
                attributes: Map::new(vec![Attribute {
                    key: key.into(),
                    key_span: None,
                    value: deep.clone(),
                }])
                .unwrap(),
                blocks: Vec::new(),
            };
            (0..blocks).fold(body, |body, _| Document {
                attributes: Map::default(),
                blocks: vec![block("b", &[], body)],
            })
        };
        written(&nest(63, "a"));
        let refused = Err(vec![Unwritable::TooDeep(TooDeep {
            span: Some(on(1, 2)),
        })]);
        assert_eq!(write(&nest(64, "a")), refused);
        // A key too long for `[]` to fit on its line.
        assert_eq!(write(&nest(64, &"k".repeat(90))), refused);

        let text = format!("{}a = []\n{}", "b {\n".repeat(64), "}\n".repeat(64));
        let bracket = Span::new(
            Source(0),
            Position {
                offset: 260,
                line: 64,
                column: 4,
            },
            Position {
                offset: 261,
                line: 64,
                column: 5,
            },
        )
        .unwrap();
        assert_eq!(
            read(Source(0), &text),
            Err(vec![Error::TooDeep { span: bracket }])
        );
    }

    #[test]
    fn refuses_a_value_or_a_block_past_the_limit_once() {
        let mut lists = Value {
            kind: list(Vec::new()),
            span: Some(on(1, 2)),
        };
        for _ in 0..64 {
            lists = value(Kind::List(vec![lists]));
        }
        let refused =
            |span| Err(vec![Unwritable::TooDeep(TooDeep { span: Some(span) })]);
        let document = Document {
            attributes: Map::new(vec![Attribute {
                key: "a".into(),
                key_span: None,
                value: lists,
            }])
            .unwrap(),
            blocks: Vec::new(),
        };
        assert_eq!(write(&document), refused(on(1, 2)));

        let mut blocks = Block {
            span: Some(on(3, 4)),
            ..block("b", &[], attributes(vec![("x", Kind::Integer(1))]))
        };
        for _ in 0..64 {
            blocks = block(
                "b",
                &[],
                Document {
                    attributes: Map::default(),
                    blocks: vec![blocks],
                },
            );
        }
        let document = Document {
            attributes: Map::default(),
            blocks: vec![blocks],
        };
        assert_eq!(write(&document), refused(on(3, 4)));
    }

    #[derive(Clone, Copy, Debug)]
    enum Level {
        Block,
        List,
        Map,
        Call,
    }

    /// Nests `levels` levels of `level` as one block or under the attribute `a`, with
    /// level `i` from the outside at `on(i, i + 1)`.
    fn nested(level: Level, levels: u32) -> Document {
        let mut document = Document::default();
        let mut inner = None;
        for i in (0..levels).rev() {
            let span = Some(on(i, i.checked_add(1).unwrap()));
            let items = Option::take(&mut inner).into_iter();
            let kind = match level {
                Level::Block => {
                    let block = Block {
                        span,
                        ..block("b", &[], document)
                    };
                    document = Document {
                        attributes: Map::default(),
                        blocks: vec![block],
                    };
                    continue;
                }
                Level::List => Kind::List(items.collect()),
                Level::Map => Kind::Map(
                    Map::new(
                        items
                            .map(|value| Attribute {
                                key: "a".into(),
                                key_span: None,
                                value,
                            })
                            .collect(),
                    )
                    .unwrap(),
                ),
                Level::Call => Kind::Call(Call {
                    function: "f".into(),
                    function_span: None,
                    arguments: items.collect(),
                }),
            };
            inner = Some(Value { kind, span });
        }
        if let Some(value) = inner {
            document.attributes = Map::new(vec![Attribute {
                key: "a".into(),
                key_span: None,
                value,
            }])
            .unwrap();
        }
        document
    }

    #[test]
    fn gives_only_the_depth_error_past_the_limit() {
        let document = Document {
            blocks: vec![block("my block", &[], Document::default())],
            ..nested(Level::List, 65)
        };
        assert_eq!(
            write(&document),
            Err(vec![Unwritable::TooDeep(TooDeep {
                span: Some(on(64, 65))
            })])
        );
    }

    #[test]
    fn checks_the_depth_before_it_recurses() {
        for level in [Level::Block, Level::List, Level::Map, Level::Call] {
            let document = nested(level, 100_000);
            let written = write(&document);
            #[expect(
                clippy::mem_forget,
                reason = "a plain drop recurses once per level"
            )]
            std::mem::forget(document);
            assert_eq!(
                written,
                Err(vec![Unwritable::TooDeep(TooDeep {
                    span: Some(on(64, 65))
                })]),
                "{level:?}"
            );
        }
    }
}

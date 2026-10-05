use std::fmt::Write as _;

use document::value::{Kind, Value};
use document::{Block, Document, Span};

use crate::lex;
use crate::parse::{enter, literal};
use crate::{Error, Unwritable};

/// The widest line, in characters, that holds a list, a map, or a call on one line.
const WIDTH: usize = 88;

/// Writes `document` as HCL text that [`read`](crate::read) reads as an equal
/// Document.
///
/// Attributes come first, in key order, then each block after a blank line. A list,
/// a map, or a call is on one line when that line fits in 88 characters. If not, each
/// item is on its own line, 2 spaces in. A string that ends in a new line and holds
/// no control character but new lines and tabs is a heredoc where it is the value of
/// a key.
///
/// # Errors
///
/// Returns [`Error::Unwritable`] for each part that HCL text cannot hold, in
/// Document order.
pub fn write(document: &Document) -> Result<String, Vec<Error>> {
    let mut writer = Writer::default();
    writer.body(document, 0, 0);
    if writer.errors.is_empty() {
        Ok(writer.out)
    } else {
        Err(writer.errors)
    }
}

/// Where a value is, which sets how it ends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    /// The value of a key, which a new line can end.
    Key,
    /// An item of a list or a call, which a `,` ends.
    Item,
}

#[derive(Default)]
struct Writer {
    out: String,
    errors: Vec<Error>,
}

impl Writer {
    /// Writes the attributes and blocks of a body, `indent` levels in.
    fn body(&mut self, document: &Document, depth: usize, indent: usize) {
        for attribute in document.attributes.iter() {
            self.pad(indent);
            if lex::word(&attribute.key) != Some(lex::Kind::Identifier) {
                self.refuse(attribute.key_span, Unwritable::Key);
            }
            self.out.push_str(&attribute.key);
            self.out.push_str(" = ");
            self.value(&attribute.value, depth, indent, Place::Key);
            self.out.push('\n');
        }
        for (i, block) in document.blocks.iter().enumerate() {
            if i > 0 || document.attributes.iter().len() > 0 {
                self.out.push('\n');
            }
            self.block(block, depth, indent);
        }
    }

    fn block(&mut self, block: &Block, depth: usize, indent: usize) {
        self.pad(indent);
        if lex::word(&block.keyword) != Some(lex::Kind::Identifier) {
            self.refuse(block.keyword_span, Unwritable::Keyword);
        }
        self.out.push_str(&block.keyword);
        for label in &block.labels {
            self.out.push(' ');
            quoted(&mut self.out, &label.text);
        }
        let Some(depth) = enter(depth) else {
            return self.refuse(block.keyword_span, Unwritable::Depth);
        };
        if block.body == Document::default() {
            self.out.push_str(" {}\n");
            return;
        }
        self.out.push_str(" {\n");
        self.body(&block.body, depth, indent.saturating_add(1));
        self.pad(indent);
        self.out.push_str("}\n");
    }

    /// Writes a value on one line when the line fits, and otherwise with each item on
    /// its own line.
    fn value(&mut self, value: &Value, depth: usize, indent: usize, place: Place) {
        if !matches!(value.kind, Kind::List(_) | Kind::Map(_) | Kind::Call(_)) {
            return self.line(value, depth, place);
        }
        let mut line = Self::default();
        line.line(value, depth, place);
        let comma = usize::from(place == Place::Item);
        let width = self
            .column()
            .saturating_add(line.out.chars().count())
            .saturating_add(comma);
        if width <= WIDTH && !line.out.contains('\n') {
            self.out.push_str(&line.out);
            self.errors.append(&mut line.errors);
            return;
        }
        let Some(depth) = enter(depth) else {
            return self.refuse(value.span, Unwritable::Depth);
        };
        let inner = indent.saturating_add(1);
        match &value.kind {
            Kind::List(items) => {
                self.out.push('[');
                self.items(items, depth, indent);
                self.out.push(']');
            }
            Kind::Call(call) => {
                self.function(&call.function, call.function_span);
                self.out.push('(');
                self.items(&call.arguments, depth, indent);
                self.out.push(')');
            }
            Kind::Map(map) => {
                self.out.push('{');
                for attribute in map.iter() {
                    self.out.push('\n');
                    self.pad(inner);
                    key(&mut self.out, &attribute.key);
                    self.out.push_str(" = ");
                    self.value(&attribute.value, depth, inner, Place::Key);
                }
                self.end(map.iter().len(), indent);
                self.out.push('}');
            }
            _ => unreachable!("invariant: only a list, a map, or a call has items"),
        }
    }

    /// Writes each item on its own line, with a `,` after it.
    fn items(&mut self, items: &[Value], depth: usize, indent: usize) {
        let inner = indent.saturating_add(1);
        for item in items {
            self.out.push('\n');
            self.pad(inner);
            self.value(item, depth, inner, Place::Item);
            self.out.push(',');
        }
        self.end(items.len(), indent);
    }

    /// Starts the line of the close bracket after `len` items, `indent` levels in. With
    /// no items, the bracket stays on the line of the open one.
    fn end(&mut self, len: usize, indent: usize) {
        if len > 0 {
            self.out.push('\n');
            self.pad(indent);
        }
    }

    /// Writes a value on one line, except a heredoc.
    fn line(&mut self, value: &Value, depth: usize, place: Place) {
        match &value.kind {
            Kind::Bool(b) => self.out.push_str(if *b { "true" } else { "false" }),
            Kind::Integer(n) => {
                write!(self.out, "{n}").expect("invariant: a String takes any text");
            }
            Kind::Float(float) => {
                // Debug gives the shortest text that reads back as the same bits, with
                // a `.` or an `e`, so it never reads as an integer.
                write!(self.out, "{:?}", float.get())
                    .expect("invariant: a String takes any text");
            }
            Kind::String(text) if place == Place::Key && heredoc(text) => {
                write_heredoc(&mut self.out, text);
            }
            Kind::String(text) => quoted(&mut self.out, text),
            Kind::Reference(name) => {
                let name = name.as_str();
                if lex::word(name).is_none() || literal(name).is_some() {
                    self.refuse(value.span, Unwritable::Reference);
                }
                self.out.push_str(name);
            }
            Kind::List(_) | Kind::Map(_) | Kind::Call(_) => {
                let Some(depth) = enter(depth) else {
                    return self.refuse(value.span, Unwritable::Depth);
                };
                self.line_items(value, depth);
            }
        }
    }

    /// Writes a list, a map, or a call on one line, inside the limit at `depth`.
    fn line_items(&mut self, value: &Value, depth: usize) {
        let (items, close) = match &value.kind {
            Kind::List(items) => {
                self.out.push('[');
                (items, ']')
            }
            Kind::Call(call) => {
                self.function(&call.function, call.function_span);
                self.out.push('(');
                (&call.arguments, ')')
            }
            Kind::Map(map) => {
                self.out.push('{');
                for (i, attribute) in map.iter().enumerate() {
                    self.out.push_str(if i == 0 { " " } else { ", " });
                    key(&mut self.out, &attribute.key);
                    self.out.push_str(" = ");
                    self.line(&attribute.value, depth, Place::Key);
                }
                self.out
                    .push_str(if map.iter().len() > 0 { " }" } else { "}" });
                return;
            }
            _ => unreachable!("invariant: only a list, a map, or a call has items"),
        };
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.line(item, depth, Place::Item);
        }
        self.out.push(close);
    }

    fn function(&mut self, function: &str, span: Option<Span>) {
        if lex::word(function) != Some(lex::Kind::Identifier) {
            self.refuse(span, Unwritable::Function);
        }
        self.out.push_str(function);
    }

    fn refuse(&mut self, span: Option<Span>, part: Unwritable) {
        self.errors.push(Error::Unwritable { span, part });
    }

    fn pad(&mut self, indent: usize) {
        for _ in 0..indent {
            self.out.push_str("  ");
        }
    }

    /// The characters on the last line so far.
    fn column(&self) -> usize {
        self.out
            .rsplit('\n')
            .next()
            .map_or(0, |line| line.chars().count())
    }
}

/// Writes a map key: bare when it is an identifier, and quoted when not. `for` is
/// quoted, because HCL reads `{ for` as a `for` expression.
fn key(out: &mut String, key: &str) {
    if lex::word(key) == Some(lex::Kind::Identifier) && key != "for" {
        out.push_str(key);
    } else {
        quoted(out, key);
    }
}

/// Reports whether `text` reads well as a heredoc: it ends in a new line and has no
/// control character but new lines and tabs.
fn heredoc(text: &str) -> bool {
    text.ends_with('\n')
        && !text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
}

/// Writes `text`, which ends in a new line, as a heredoc, up to its marker.
fn write_heredoc(out: &mut String, text: &str) {
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
    escape_templates(out, text);
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
            '$' | '%' if chars.peek() == Some(&'{') => {
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

/// Writes `text`, with `$${` for `${` and `%%{` for `%{`.
fn escape_templates(out: &mut String, text: &str) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if matches!(c, '$' | '%') && chars.peek() == Some(&'{') {
            out.push(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use document::value::{Call, Float};
    use document::{Attribute, Label, Map, Position, Source};
    use proptest::prelude::*;

    use super::*;
    use crate::arbitrary::document;
    use crate::read;

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

    proptest! {
        #[test]
        fn reads_what_it_writes(document in document()) {
            let text = write(&document).unwrap();
            prop_assert_eq!(read(Source(0), &text), Ok(document), "{}", text);
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
            ("reference", reference("@a.7b")),
            ("words", list(vec![reference("for"), reference("a-b")])),
            (
                "map",
                Kind::Map(map(vec![
                    ("x", Kind::Integer(1)),
                    ("for", Kind::Integer(2)),
                    ("a b", Kind::Integer(3)),
                    ("7", Kind::Integer(4)),
                ])),
            ),
            (
                "call",
                call("true", vec![Kind::Integer(1), list(Vec::new())]),
            ),
            ("empty", call("f", vec![Kind::Map(Map::default())])),
        ]);
        assert_eq!(
            written(&document),
            "bool = false\n\
             call = true(1, [])\n\
             empty = f({})\n\
             integer = -170141183460469231731687303715884105728\n\
             large = 1e300\n\
             map = { \"7\" = 4, \"a b\" = 3, \"for\" = 2, x = 1 }\n\
             one = 1.0\n\
             reference = @a.7b\n\
             small = -1e-7\n\
             string = \"\\\"q\\\" \\\\ $${a} %%{b} $c\\t\\r\\u0001é\"\n\
             words = [for, a-b]\n\
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
        ]);
        assert_eq!(
            written(&document),
            "crlf = \"a\\r\\n\"\n\
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
             EOT_\n"
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
        let unwritable = |span, part| Error::Unwritable { span, part };
        assert_eq!(
            write(&document),
            Err(vec![
                unwritable(Some(on(21, 24)), Unwritable::Function),
                unwritable(Some(on(25, 27)), Unwritable::Reference),
                unwritable(Some(on(0, 6)), Unwritable::Key),
                unwritable(Some(on(9, 13)), Unwritable::Reference),
                unwritable(Some(on(30, 38)), Unwritable::Keyword),
                unwritable(None, Unwritable::Reference),
                unwritable(None, Unwritable::Reference),
                unwritable(None, Unwritable::Reference),
            ])
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
        let refused = Err(vec![Error::Unwritable {
            span: Some(on(1, 2)),
            part: Unwritable::Depth,
        }]);
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
        let refused = |span| {
            Err(vec![Error::Unwritable {
                span: Some(span),
                part: Unwritable::Depth,
            }])
        };
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
            keyword_span: Some(on(3, 4)),
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
}

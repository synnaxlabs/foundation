use std::cmp::Ordering;
use std::ops::Range;

use document::{Attribute, Block, Document, Label, Position, Source, Span};

use crate::lex::{self, Tokens};
use crate::write::{After, INDENT, Writer};
use crate::{Error, Unwritable, read, write};

/// Changes `text` so that [`read`] reads it as `document`, and returns the new text.
/// Each attribute and block that keeps its value and its place keeps its bytes, with
/// its comments and blank lines. A changed value and a changed block on one line are
/// written again, without the comments in them. A block that moves past another is
/// cut and written again. A new attribute or block goes into its body, and a removed
/// one is cut with its lines and the comments directly above it.
///
/// # Errors
///
/// Returns [`Refusal::Text`] when `read` refuses `text`. Otherwise, returns
/// [`Refusal::Document`] when `write` refuses `document`.
pub fn update(
    source: Source,
    text: &str,
    document: &Document,
) -> Result<String, Refusal> {
    let old = read(source, text).map_err(Refusal::Text)?;
    write(document).map_err(Refusal::Document)?;
    let file = File::new(source, text);
    let mut diff = Diff {
        file: &file,
        edits: Vec::new(),
    };
    let end = file.marks.len().saturating_sub(1);
    let body = Body {
        margin: file.margin_of_first(0..end).unwrap_or_default().to_owned(),
        start: file.floor,
        end: text.len(),
    };
    diff.body(&old, document, &body);
    Ok(file.apply(diff.edits))
}

/// Why [`update`] gives no text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The problems in the text, as [`read`] gives them.
    Text(Vec<Error>),
    /// The parts of the Document that HCL text cannot hold, as [`write()`] gives them.
    Document(Vec<Unwritable>),
}

/// A token's place in the text.
#[derive(Clone, Copy)]
struct Mark {
    start: usize,
    end: usize,
    newline: bool,
}

/// Text to put in place of a range of the old text. An empty range inserts.
struct Edit {
    range: Range<usize>,
    text: String,
    /// Whether an insert goes before the item below it, after the other inserts there.
    below: bool,
}

/// Where the items of a body are.
struct Body {
    /// What each line of a new item starts with.
    margin: String,
    /// The start of the first line.
    start: usize,
    /// Where new text after the last item goes: the end of the text, or the start of
    /// the line of `}`.
    end: usize,
}

/// New items that wait for a place in a body.
#[derive(Default)]
struct Pending<'d> {
    /// The end of the lines of the kept item that new items go after.
    anchor: Option<usize>,
    /// The ends of the lines of the kept items.
    ends: Vec<usize>,
    attributes: Vec<&'d Attribute>,
    blocks: Vec<&'d Block>,
}

/// A text that `read` takes, with its tokens. Between two tokens there are only
/// spaces and comments, so a line whose only token is its new line holds only
/// comments, or nothing.
struct File<'a> {
    text: &'a str,
    marks: Vec<Mark>,
    /// The start of the text after its byte order mark.
    floor: usize,
    /// `\r\n` when the first line ends so, and `\n` when not.
    line_end: &'static str,
}

impl<'a> File<'a> {
    fn new(source: Source, text: &'a str) -> Self {
        let mut tokens =
            Tokens::new(source, text).expect("invariant: `read` took the text");
        let mut marks = Vec::new();
        // Each pass takes a token or returns.
        for _ in 0..=text.len() {
            let token = tokens.next();
            marks.push(Mark {
                start: offset(token.span.start()),
                end: offset(token.span.end()),
                newline: token.kind == lex::Kind::Newline,
            });
            if token.kind != lex::Kind::End {
                continue;
            }
            // Not from the tokens: a heredoc or a comment can hold the first line end.
            let crlf = text
                .split_once('\n')
                .is_some_and(|(line, _)| line.ends_with('\r'));
            return Self {
                text,
                marks,
                floor: text
                    .strip_prefix('\u{feff}')
                    .map_or(0, |_| '\u{feff}'.len_utf8()),
                line_end: if crlf { "\r\n" } else { "\n" },
            };
        }
        unreachable!("invariant: each token but the last covers a byte")
    }

    /// The lines of an item from the start of `first` to the end of `last`, with
    /// the comments directly above it and its new line.
    fn lines(&self, first: Option<Span>, last: Option<Span>) -> Range<usize> {
        let mut i = self.token(offsets(first).0);
        let mut start = self
            .line_start(i)
            .expect("invariant: an item starts its line");
        // Each pass moves up a line or returns.
        while let Some(above) = i.checked_sub(1) {
            let Some(line) = self.line_start(above) else {
                break;
            };
            if self.blank(line, self.mark(above).start) {
                break;
            }
            start = line;
            i = above;
        }
        start..self.mark(self.token(offsets(last).1)).end
    }

    /// Widens a run of cut lines to take the blank lines after it when it starts
    /// the body or a blank line is above it, or else the blank lines above it when it
    /// ends the body. So one blank line stays between the items left.
    fn widen(&self, run: Range<usize>, body: &Body) -> Range<usize> {
        let Range { mut start, mut end } = run;
        let opens = start == body.start || self.blank_above(start).is_some();
        if opens && self.blank_below(end).is_some() {
            while let Some(below) = self.blank_below(end) {
                end = below;
            }
        } else if end == body.end {
            while let Some(above) = self.blank_above(start) {
                start = above;
            }
        }
        start..end
    }

    /// The end of the line at `at` when only spaces are between `at` and the line
    /// end. `None` at the end of a text with no line end.
    fn blank_below(&self, at: usize) -> Option<usize> {
        let mark = self.mark(self.token(at));
        (mark.newline && self.blank(at, mark.start)).then_some(mark.end)
    }

    /// The start of the line that ends at `end` when that line is blank.
    fn blank_above(&self, end: usize) -> Option<usize> {
        let newline = self.token(end).checked_sub(1)?;
        let line = self.line_start(newline)?;
        self.blank(line, self.mark(newline).start).then_some(line)
    }

    /// The start of the line of token `i`, when no token is before it on its line.
    fn line_start(&self, i: usize) -> Option<usize> {
        match i.checked_sub(1) {
            None => Some(self.floor),
            Some(before) => {
                let mark = self.mark(before);
                mark.newline.then_some(mark.end)
            }
        }
    }

    /// Reports whether the text from `start` to `end` holds only whitespace.
    fn blank(&self, start: usize, end: usize) -> bool {
        self.text
            .get(start..end)
            .expect("invariant: ranges fall between characters")
            .trim()
            .is_empty()
    }

    /// The spaces at the start of the line of an item that starts at `span`.
    fn margin(&self, span: Option<Span>) -> &'a str {
        self.margin_at(self.token(offsets(span).0))
    }

    /// The spaces at the start of the line of token `i`, which starts its line.
    fn margin_at(&self, i: usize) -> &'a str {
        let line = self
            .line_start(i)
            .expect("invariant: an item starts its line");
        let rest = self
            .text
            .get(line..)
            .expect("invariant: a line starts at a character");
        rest.strip_suffix(rest.trim_start_matches(lex::space))
            .expect("invariant: a trimmed text ends its text")
    }

    /// The spaces at the start of the line of the first item in tokens `items`.
    fn margin_of_first(&self, mut items: Range<usize>) -> Option<&'a str> {
        let item = items.find(|&i| !self.mark(i).newline)?;
        Some(self.margin_at(item))
    }

    /// The index of the first token that starts at `offset` or after it.
    fn token(&self, offset: usize) -> usize {
        self.marks.partition_point(|mark| mark.start < offset)
    }

    fn mark(&self, i: usize) -> Mark {
        *self
            .marks
            .get(i)
            .expect("invariant: the last token is the end")
    }

    /// The edit that puts the text of `writer` at `at`, before the item below when
    /// `below`. At the end of a text with no final new line, the text starts a new
    /// line, so no other insert may go there.
    fn insert(&self, at: usize, writer: Writer<'_>, below: bool) -> Edit {
        let mut text = written(writer);
        let open =
            at == self.text.len() && at > self.floor && !self.text.ends_with('\n');
        if open {
            text.insert(0, '\n');
        }
        Edit {
            range: at..at,
            text,
            below,
        }
    }

    /// Applies `edits`, which do not overlap, to the text.
    fn apply(&self, mut edits: Vec<Edit>) -> String {
        // At one place, inserts after the item above come first, then inserts before
        // the item below, each in the order made, then a cut.
        edits.sort_by_key(|edit| (edit.range.start, edit.range.end, edit.below));
        let mut out = String::with_capacity(self.text.len());
        let mut at = 0;
        for Edit { range, text, .. } in edits {
            let kept = self.text.get(at..range.start);
            out.push_str(kept.expect("invariant: edits do not overlap"));
            out.push_str(&text.replace('\n', self.line_end));
            at = range.end;
        }
        out.push_str(
            self.text
                .get(at..)
                .expect("invariant: edits end in the text"),
        );
        out
    }
}

/// The edits that change a file so that it reads as a new Document.
struct Diff<'f, 'a> {
    file: &'f File<'a>,
    edits: Vec<Edit>,
}

impl Diff<'_, '_> {
    /// Adds the edits that change a body from `old` to `new`. Walks the attributes in
    /// key order, then the blocks in order. A new item goes after the kept item of its
    /// kind before it, or else before the kept item of its kind after it. With no kept
    /// item of its kind, a new attribute goes before the first kept block, and a new
    /// block after the last kept attribute in key order, or else at the end of the
    /// body.
    fn body(&mut self, old: &Document, new: &Document, body: &Body) {
        let file = self.file;
        let mut cuts = Vec::new();
        let mut pending = Pending::default();
        for attribute in new.attributes.iter() {
            let Some(was) = old.attributes.get(&attribute.key) else {
                pending.attributes.push(attribute);
                continue;
            };
            let lines = file.lines(was.key_span, was.value.span);
            self.keep(body, &mut pending, lines, false);
            if was.value != attribute.value {
                self.value(was, attribute);
            }
        }
        for attribute in old.attributes.iter() {
            if new.attributes.get(&attribute.key).is_none() {
                cuts.push(file.lines(attribute.key_span, attribute.value.span));
            }
        }
        let pairs = pair(&old.blocks, &new.blocks);
        if !pairs.is_empty()
            && let Some(at) = pending.anchor
        {
            self.place(body, &mut pending, at, false);
            pending.anchor = None;
        }
        let mut kept = pairs.iter().map(|&(o, _)| o).peekable();
        for (o, block) in old.blocks.iter().enumerate() {
            if kept.next_if_eq(&o).is_none() {
                cuts.push(file.lines(block.span, block.span));
            }
        }
        let mut pairs = pairs.into_iter().peekable();
        for (n, block) in new.blocks.iter().enumerate() {
            let Some((o, _)) = pairs.next_if(|&(_, m)| m == n) else {
                pending.blocks.push(block);
                continue;
            };
            let was = old
                .blocks
                .get(o)
                .expect("invariant: `pair` gives old indices");
            self.keep(body, &mut pending, file.lines(was.span, was.span), true);
            if was != block {
                self.block(was, block);
            }
        }
        let at = pending.anchor.unwrap_or(body.end);
        self.place(body, &mut pending, at, false);
        self.cut(cuts, body);
    }

    /// Puts the waiting items after the anchor, or else before the kept item at
    /// `lines`, with a blank line between them and a kept block. Then that item is
    /// the anchor.
    fn keep(
        &mut self,
        body: &Body,
        pending: &mut Pending<'_>,
        lines: Range<usize>,
        block: bool,
    ) {
        let at = pending.anchor.unwrap_or(lines.start);
        self.place(body, pending, at, block && pending.anchor.is_none());
        pending.anchor = Some(lines.end);
        pending.ends.push(lines.end);
    }

    /// Inserts the waiting items at `at`, the anchor or the start of the lines of the
    /// kept item below. A blank line goes before the first block when a kept item
    /// ends at `at`, and after the items when `gap`.
    fn place(&mut self, body: &Body, pending: &mut Pending<'_>, at: usize, gap: bool) {
        if pending.attributes.is_empty() && pending.blocks.is_empty() {
            return;
        }
        let mut writer = Writer::new(&body.margin, 0);
        let (attributes, blocks) =
            (pending.attributes.drain(..), pending.blocks.drain(..));
        writer.body(attributes, blocks, 0, pending.ends.contains(&at));
        if gap {
            writer.gap();
        }
        let below = pending.anchor != Some(at);
        self.edits.push(self.file.insert(at, writer, below));
    }

    /// Cuts the lines of removed items, as runs that merge across blank lines.
    fn cut(&mut self, mut cuts: Vec<Range<usize>>, body: &Body) {
        let file = self.file;
        cuts.sort_by_key(|cut| cut.start);
        let mut runs: Vec<Range<usize>> = Vec::new();
        for cut in cuts {
            match runs.last_mut() {
                Some(run) if file.blank(run.end, cut.start) => run.end = cut.end,
                _ => runs.push(cut),
            }
        }
        self.edits.extend(runs.into_iter().map(|run| Edit {
            range: file.widen(run, body),
            text: String::new(),
            below: false,
        }));
    }

    /// Writes the value of `new` over the value of `old`.
    fn value(&mut self, old: &Attribute, new: &Attribute) {
        let file = self.file;
        let (start, end) = offsets(old.value.span);
        let after = if file.blank_below(end).is_some() {
            After::Line
        } else {
            After::Other
        };
        let mut writer = Writer::new(file.margin(old.key_span), column(old.value.span));
        writer.value(&new.value, 0, after);
        self.edits.push(Edit {
            range: start..end,
            text: written(writer),
            below: false,
        });
    }

    /// Adds the edits that change block `old` to `new`, which has the same keyword
    /// and labels.
    fn block(&mut self, old: &Block, new: &Block) {
        let file = self.file;
        let (start, end) = offsets(old.span);
        let keyword = file.token(start);
        let margin = file.margin(old.span);
        let open = keyword
            .checked_add(old.labels.len())
            .and_then(|i| i.checked_add(1))
            .expect("invariant: the tokens of a block fit a usize");
        let after = open
            .checked_add(1)
            .expect("invariant: `{` is not the last token");
        let close = file
            .token(end)
            .checked_sub(1)
            .expect("invariant: `}` is a token");
        if file.mark(after).newline {
            let margin = file
                .margin_of_first(after..close)
                .map_or_else(|| format!("{margin}{INDENT}"), str::to_owned);
            let body = Body {
                margin,
                start: file.mark(after).end,
                end: file
                    .line_start(close)
                    .expect("invariant: `}` of a body of lines starts its line"),
            };
            self.body(&old.body, &new.body, &body);
        } else {
            // A block on one line holds one attribute or none, so it is written again.
            let mut writer = Writer::new(margin, column(old.span));
            writer.block(new, 0);
            self.edits.push(Edit {
                range: start..end,
                text: written(writer),
                below: false,
            });
        }
    }
}

/// The text of a writer that wrote a part of the Document that `write` took.
fn written(writer: Writer<'_>) -> String {
    writer
        .finish()
        .expect("invariant: `write` took the document")
}

/// Pairs the indices of old and new blocks that keep their place: the longest list
/// of pairs in the same order in both, where the k-th old block of a keyword and
/// labels pairs only with the k-th new one. Takes O(n log n) time.
fn pair(old: &[Block], new: &[Block]) -> Vec<(usize, usize)> {
    let (mut olds, mut news) = (sorted(old).peekable(), sorted(new).peekable());
    let mut candidates = vec![None; new.len()];
    while let (Some(&(o, a)), Some(&(n, b))) = (olds.peek(), news.peek()) {
        match order(a, b) {
            Ordering::Less => drop(olds.next()),
            Ordering::Greater => drop(news.next()),
            Ordering::Equal => {
                if let Some(candidate) = candidates.get_mut(n) {
                    *candidate = Some(o);
                }
                olds.next();
                news.next();
            }
        }
    }
    let pairs: Vec<(usize, usize)> = candidates
        .into_iter()
        .enumerate()
        .filter_map(|(n, o)| o.map(|o| (o, n)))
        .collect();
    // The longest run of increasing old indices: `tails[k]` ends the best run of
    // k + 1 pairs so far, and `links` points each pair at the one before it.
    let old_of = |p: usize| pairs.get(p).map_or(0, |&(o, _)| o);
    let mut tails: Vec<usize> = Vec::new();
    let mut links = Vec::with_capacity(pairs.len());
    for (p, &(o, _)) in pairs.iter().enumerate() {
        let k = tails
            .binary_search_by_key(&o, |&tail| old_of(tail))
            .expect_err("invariant: an old block pairs once");
        links.push(k.checked_sub(1).and_then(|k| tails.get(k).copied()));
        match tails.get_mut(k) {
            Some(tail) => *tail = p,
            None => tails.push(p),
        }
    }
    let mut run = Vec::new();
    let mut p = tails.last().copied();
    while let Some(q) = p {
        run.extend(pairs.get(q));
        p = links.get(q).copied().flatten();
    }
    run.reverse();
    run
}

/// The blocks with their indices, sorted by [`order`] and stable.
fn sorted(blocks: &[Block]) -> impl Iterator<Item = (usize, &Block)> {
    let mut sorted: Vec<(usize, &Block)> = blocks.iter().enumerate().collect();
    sorted.sort_by(|a, b| order(a.1, b.1));
    sorted.into_iter()
}

/// Orders blocks by keyword, then by labels.
fn order(a: &Block, b: &Block) -> Ordering {
    let labels = |a: &[Label], b: &[Label]| {
        a.iter().map(|l| &l.text).cmp(b.iter().map(|l| &l.text))
    };
    a.keyword
        .cmp(&b.keyword)
        .then_with(|| labels(&a.labels, &b.labels))
}

/// The column, in characters, where `span` starts.
fn column(span: Option<Span>) -> usize {
    let column = span
        .expect("invariant: `read` gives each part a span")
        .start()
        .column;
    usize::try_from(column).expect("invariant: a u32 fits a usize")
}

fn offsets(span: Option<Span>) -> (usize, usize) {
    let span = span.expect("invariant: `read` gives each part a span");
    (offset(span.start()), offset(span.end()))
}

fn offset(position: Position) -> usize {
    usize::try_from(position.offset).expect("invariant: a u32 fits a usize")
}

#[cfg(test)]
mod tests {
    use document::Map;
    use document::value::{Kind, Value};
    use proptest::prelude::*;

    use super::*;
    use crate::Expected;
    use crate::arbitrary::document;

    fn parsed(text: &str) -> Document {
        read(Source(0), text).unwrap()
    }

    /// Updates `text` to read as `new`, checks that it does, and returns the text.
    fn updated(text: &str, new: &str) -> String {
        let document = parsed(new);
        let out = update(Source(0), text, &document).unwrap();
        assert_eq!(read(Source(0), &out).as_ref(), Ok(&document), "{out}");
        out
    }

    /// Choices from proptest, then zeros.
    struct Picks(std::vec::IntoIter<u8>);

    impl Picks {
        /// A choice below `n`.
        fn pick(&mut self, n: u8) -> u8 {
            self.0.next().unwrap_or(0).checked_rem(n).unwrap()
        }
    }

    /// The text of `document` from `write`, or with the attributes of each body after
    /// its blocks.
    fn text_of(document: &Document, attributes_last: bool) -> String {
        if !attributes_last {
            return write(document).unwrap();
        }
        let mut text = String::new();
        for block in &document.blocks {
            let empty = Block {
                body: Document::default(),
                ..block.clone()
            };
            let head = write(&Document {
                attributes: Map::default(),
                blocks: vec![empty],
            })
            .unwrap();
            text.push_str(head.strip_suffix("{}\n").unwrap());
            text.push_str("{\n");
            text.push_str(&text_of(&block.body, true));
            text.push_str("}\n");
        }
        let attributes = Document {
            attributes: document.attributes.clone(),
            blocks: Vec::new(),
        };
        text + &write(&attributes).unwrap()
    }

    /// Adds blank lines and comments to `text`, which `write` gave, and maybe a byte
    /// order mark, `\r\n` line ends, or no final new line.
    fn annotate(text: &str, picks: &mut Picks) -> String {
        let mut tokens = Tokens::new(Source(0), text).unwrap();
        let mut out = String::new();
        let mut at = 0;
        let mut heredoc = false;
        let mut ends_at_closer = false;
        loop {
            let token = tokens.next();
            let (start, end) = (offset(token.span.start()), offset(token.span.end()));
            if token.kind == lex::Kind::End {
                break;
            }
            if token.kind != lex::Kind::Newline {
                heredoc = matches!(token.kind, lex::Kind::Heredoc(_));
                continue;
            }
            out.push_str(text.get(at..start).unwrap());
            if !heredoc && picks.pick(3) == 0 {
                out.push_str(" # t");
            }
            out.push('\n');
            let lines = ["", "", "\n", "# c\n", "  // c\n", "/* c\nc */\n"];
            let line = lines.get(usize::from(picks.pick(6))).unwrap();
            out.push_str(line);
            ends_at_closer = heredoc && line.is_empty();
            at = end;
            heredoc = false;
        }
        out.push_str(text.get(at..).unwrap());
        // A heredoc needs the line end after its closer.
        if !ends_at_closer && picks.pick(4) == 0 && out.ends_with('\n') {
            out.pop();
        }
        if picks.pick(4) == 0 {
            out = out.replace('\n', "\r\n");
        }
        if picks.pick(4) == 0 {
            out.insert(0, '\u{feff}');
        }
        out
    }

    /// Mixes `b` into `a`: keeps, cuts, or changes each attribute of `a` to a value of
    /// `b`, adds some new keys of `b`, and keeps, cuts, or mixes each block of `a`,
    /// with blocks of `b` put in and the first moved last.
    fn mix(a: &Document, b: &Document, picks: &mut Picks) -> Document {
        let values: Vec<&Value> = b.attributes.iter().map(|x| &x.value).collect();
        let mut attributes = Vec::new();
        for attribute in a.attributes.iter() {
            match picks.pick(3) {
                0 => attributes.push(attribute.clone()),
                1 => {}
                _ => {
                    let value = values.get(usize::from(picks.pick(4)));
                    attributes.push(Attribute {
                        value: value.map_or(&attribute.value, |value| value).clone(),
                        ..attribute.clone()
                    });
                }
            }
        }
        for attribute in b.attributes.iter() {
            if a.attributes.get(&attribute.key).is_none() && picks.pick(2) == 0 {
                attributes.push(attribute.clone());
            }
        }
        let mut blocks = Vec::new();
        for block in &a.blocks {
            match picks.pick(3) {
                0 => blocks.push(block.clone()),
                1 => {}
                _ => blocks.push(Block {
                    body: mix(&block.body, b, picks),
                    ..block.clone()
                }),
            }
        }
        for block in &b.blocks {
            if picks.pick(2) == 0 {
                let at = usize::from(picks.pick(4)).min(blocks.len());
                blocks.insert(at, block.clone());
            }
        }
        if blocks.len() > 1 && picks.pick(2) == 0 {
            blocks.rotate_left(1);
        }
        Document {
            attributes: Map::new(attributes).unwrap(),
            blocks,
        }
    }

    fn count(document: &Document) -> usize {
        let inner = document.blocks.iter().map(|block| count(&block.body));
        inner.fold(document.attributes.iter().len(), usize::saturating_add)
    }

    /// Puts `value` in place of the value of attribute `k`, in order of keys and then
    /// of blocks, and returns the span of the old value.
    fn replace(document: &mut Document, k: &mut usize, value: &Value) -> Option<Span> {
        let mut attributes: Vec<Attribute> =
            document.attributes.iter().cloned().collect();
        let mut span = None;
        for attribute in &mut attributes {
            if let Some(less) = k.checked_sub(1) {
                *k = less;
            } else {
                span = attribute.value.span;
                attribute.value = value.clone();
                *k = usize::MAX;
            }
        }
        document.attributes = Map::new(attributes).unwrap();
        for block in &mut document.blocks {
            span = span.or(replace(&mut block.body, k, value));
        }
        span
    }

    proptest! {
        #[test]
        fn keeps_a_text_whose_document_does_not_change(
            a in document(),
            picks in prop::collection::vec(any::<u8>(), 0..64),
            attributes_last in any::<bool>(),
        ) {
            let mut picks = Picks(picks.into_iter());
            let text = annotate(&text_of(&a, attributes_last), &mut picks);
            prop_assert_eq!(read(Source(0), &text), Ok(a.clone()), "{}", text);
            prop_assert_eq!(update(Source(0), &text, &a), Ok(text));
        }

        #[test]
        fn reads_as_the_document_it_updates_to(
            a in document(),
            b in document(),
            picks in prop::collection::vec(any::<u8>(), 0..256),
            attributes_last in any::<bool>(),
        ) {
            let mut picks = Picks(picks.into_iter());
            let text = annotate(&text_of(&a, attributes_last), &mut picks);
            let document = mix(&a, &b, &mut picks);
            let out = update(Source(0), &text, &document).unwrap();
            prop_assert_eq!(read(Source(0), &out), Ok(document), "{}\n{}", text, out);
            if text.contains('\r') {
                prop_assert!(!out.replace("\r\n", "").contains('\n'), "{}", out);
            } else {
                prop_assert!(!out.contains('\r'), "{}", out);
            }
        }

        #[test]
        fn changes_only_the_value_that_changes(
            a in document(),
            b in document(),
            k in any::<usize>(),
            picks in prop::collection::vec(any::<u8>(), 0..64),
            attributes_last in any::<bool>(),
        ) {
            let mut picks = Picks(picks.into_iter());
            let text = annotate(&text_of(&a, attributes_last), &mut picks);
            let mut document = read(Source(0), &text).unwrap();
            let Some(mut k) = k.checked_rem(count(&document)) else {
                return Ok(());
            };
            let value = b.attributes.iter().next().map_or(
                Value {
                    kind: Kind::Integer(7),
                    span: None,
                },
                |attribute| attribute.value.clone(),
            );
            let (start, end) = offsets(replace(&mut document, &mut k, &value));
            let out = update(Source(0), &text, &document).unwrap();
            prop_assert!(out.starts_with(text.get(..start).unwrap()), "{}", out);
            prop_assert!(out.ends_with(text.get(end..).unwrap()), "{}", out);
            prop_assert_eq!(read(Source(0), &out), Ok(document), "{}", out);
        }
    }

    #[test]
    fn keeps_comments_and_blank_lines_outside_a_change() {
        let text = "# head\n\na = 1 # one\n\n// b\nb = \"x\"\n\nc {\n  /* in */\n  \
                    d = [1, 2]\n}\n";
        assert_eq!(
            updated(text, "a = 1\nb = \"y\"\nc {\n d = [1, 2]\n}\n"),
            "# head\n\na = 1 # one\n\n// b\nb = \"y\"\n\nc {\n  /* in */\n  \
             d = [1, 2]\n}\n"
        );
    }

    #[test]
    fn cuts_the_comments_directly_above_a_removed_item() {
        let text =
            "# head\n\n# a\na = 1\n# b1\n/* b2\nb2 */\nb = 2 # b\n\n# c\nc = 3\n";
        assert_eq!(
            updated(text, "a = 1\nc = 3\n"),
            "# head\n\n# a\na = 1\n\n# c\nc = 3\n"
        );
        assert_eq!(
            updated(text, "b = 2\nc = 3\n"),
            "# head\n\n# b1\n/* b2\nb2 */\nb = 2 # b\n\n# c\nc = 3\n"
        );
        assert_eq!(updated("b {\n  # x\n  x = 1\n}\n", "b {}\n"), "b {\n}\n");
    }

    #[test]
    fn cuts_one_blank_line_with_a_removed_block() {
        let text = "a = 1\n\nb {}\n\nc {}\n\nd {}\n";
        assert_eq!(
            updated(text, "a = 1\nc {}\nd {}"),
            "a = 1\n\nc {}\n\nd {}\n"
        );
        assert_eq!(
            updated(text, "a = 1\nb {}\nc {}"),
            "a = 1\n\nb {}\n\nc {}\n"
        );
        assert_eq!(updated(text, "b {}\nc {}\nd {}"), "b {}\n\nc {}\n\nd {}\n");
        assert_eq!(updated(text, "a = 1"), "a = 1\n");
        assert_eq!(updated(text, ""), "");
    }

    #[test]
    fn puts_a_new_attribute_after_the_key_before_it() {
        let text = "b = 2\n\nd = 4\n\nx {}\n";
        assert_eq!(
            updated(text, "a = 1\nb = 2\nc = 3\nd = 4\ne = 5\nx {}"),
            "a = 1\nb = 2\nc = 3\n\nd = 4\ne = 5\n\nx {}\n"
        );
        assert_eq!(updated("x {}\n", "a = 1\nx {}"), "a = 1\n\nx {}\n");
        assert_eq!(updated("a = 1\n", "a = 1\nx {}"), "a = 1\n\nx {}\n");
        assert_eq!(updated("", "a = 1\nx {}"), "a = 1\n\nx {}\n");
    }

    #[test]
    fn keeps_the_longest_run_of_blocks_in_order() {
        let text = "p {}\n\nq {}\n\nr {\n  # r\n  a = 1\n}\n";
        assert_eq!(
            updated(text, "r {\na = 1\n}\np {}\ns {}\nq {}\n"),
            "r {\n  a = 1\n}\n\np {}\n\ns {}\n\nq {}\n"
        );
        // The second block of a keyword and labels pairs with the second.
        let text = "x \"l\" {}\n\nx \"l\" {\n  # a\n  a = 1\n}\n";
        assert_eq!(
            updated(text, "x \"l\" {}\nx \"l\" {\na = 2\n}\n"),
            "x \"l\" {}\n\nx \"l\" {\n  # a\n  a = 2\n}\n"
        );
    }

    #[test]
    fn puts_a_new_block_by_the_kept_blocks_only() {
        // The kept attribute `b` is below the kept block `d`.
        assert_eq!(
            updated("d{}\nb=1\n", "e{}\nd{}\nn{}\nb=2\n"),
            "e {}\n\nd{}\n\nn {}\nb=2\n"
        );
        // `c` goes after `b`, and `e` before `d`, at one place.
        assert_eq!(
            updated("b=1\nd{}\n", "e{}\nd{}\nb=1\nc=2\n"),
            "b=1\nc = 2\n\ne {}\n\nd{}\n"
        );
        // `x` goes after `d`, and `a` before `b`, at one place.
        assert_eq!(
            updated("d{}\nb=1\n", "a=0\nb=1\nd{}\nx{}\n"),
            "d{}\n\nx {}\na = 0\nb=1\n"
        );
        // The same in a block body.
        assert_eq!(
            updated("x {\n  d {}\n  b = 1\n}\n", "x {\ne {}\nd {}\nb = 2\n}"),
            "x {\n  e {}\n\n  d {}\n  b = 2\n}\n"
        );
        assert_eq!(
            updated(
                "c{\nSned{}\nS=z()\nd{a=[[]]}\n}",
                "c{\nd{}\nSned{}\nS=z()\nd{a=[]}\n}"
            ),
            "c{\nd {}\n\nSned{}\n\nd {\n  a = []\n}\nS=z()\n}"
        );
    }

    #[test]
    fn writes_a_block_on_one_line_again_whole() {
        assert_eq!(
            updated("  a \"l\" { x = 1 } # a\n", "a \"l\" {\nx = 1\ny = 2\n}\n"),
            "  a \"l\" {\n    x = 1\n    y = 2\n  } # a\n"
        );
        assert_eq!(updated("a { x = 1 }\n", "a {}\n"), "a {}\n");
    }

    #[test]
    fn writes_new_lines_with_the_margin_of_their_body() {
        assert_eq!(
            updated(
                "  a = 1\n  b {\n    x = 1\n  }\n",
                "a = 1\nc = 2\nb {\nx = 1\n}\nd {}"
            ),
            "  a = 1\n  c = 2\n  b {\n    x = 1\n  }\n\n  d {}\n"
        );
        let text = "b {\n    x = 1\n\n    c {\n        y = 1\n    }\n}\n\n\td {\n\t}\n";
        assert_eq!(
            updated(
                text,
                "b {\nw = 0\nx = 1\nc {\ny = 1\nz = [\"é\"]\n}\n}\nd {\ne {}\n}\n"
            ),
            "b {\n    w = 0\n    x = 1\n\n    c {\n        y = 1\n        \
             z = [\"é\"]\n    }\n}\n\n\td {\n\t  e {}\n\t}\n"
        );
        let long = "é".repeat(80);
        assert_eq!(
            updated(
                "b {\n    x = 1 # x\n}\n",
                &format!("b {{\nx = [\"{long}\"]\n}}\n")
            ),
            format!("b {{\n    x = [\n      \"{long}\",\n    ] # x\n}}\n")
        );
    }

    #[test]
    fn writes_no_heredoc_before_a_comment() {
        assert_eq!(
            updated("a = 1 # one\nb = 2\n", "a = \"x\\n\"\nb = \"y\\n\""),
            "a = \"x\\n\" # one\nb = <<EOT\ny\nEOT\n"
        );
    }

    #[test]
    fn quotes_a_value_at_the_end_of_a_text_with_no_line_end() {
        assert_eq!(updated("a = 1", "a = \"x\\n\""), "a = \"x\\n\"");
        assert_eq!(updated("a = 1  ", "a = \"x\\n\""), "a = \"x\\n\"  ");
        assert_eq!(updated("a = 1\n", "a = \"x\\n\""), "a = <<EOT\nx\nEOT\n");
    }

    #[test]
    fn keeps_a_value_that_fits_on_one_line_before_a_comment_or_the_end() {
        let line = format!("a = [\"{}\"]", "x".repeat(80));
        assert_eq!(line.chars().count(), 88);
        assert_eq!(updated("a = 1", &line), line);
        assert_eq!(updated("a = 1 # c\n", &line), format!("{line} # c\n"));
    }

    #[test]
    fn keeps_the_line_ends_and_the_byte_order_mark() {
        assert_eq!(
            updated(
                "a = 1\r\nb {\r\n  c = 2\r\n}\r\n",
                "a = 1\nd = \"x\\n\"\nb {\nc = 2\ne = 3\n}"
            ),
            "a = 1\r\nd = <<EOT\r\nx\r\nEOT\r\nb {\r\n  c = 2\r\n  e = 3\r\n}\r\n"
        );
        assert_eq!(updated("a = 1", "a = 1\nb = 2"), "a = 1\nb = 2\n");
        assert_eq!(
            updated("a = <<EOT\r\nx\r\nEOT\r\n", "a = \"x\\n\"\nb = 2"),
            "a = <<EOT\r\nx\r\nEOT\r\nb = 2\r\n"
        );
        assert_eq!(
            updated("/* c\r\n*/ a = 1", "a = 1\nb = 2"),
            "/* c\r\n*/ a = 1\r\nb = 2\r\n"
        );
        assert_eq!(
            updated("\u{feff}b = 1\n", "a = 0\nb = 1"),
            "\u{feff}a = 0\nb = 1\n"
        );
        assert_eq!(updated("\u{feff}a = 1\n", "b = 2"), "\u{feff}b = 2\n");
    }

    #[test]
    fn returns_the_errors_of_the_text_or_else_of_the_document() {
        let document = Document {
            attributes: Map::new(vec![Attribute {
                key: "my key".into(),
                key_span: None,
                value: Value {
                    kind: Kind::Integer(1),
                    span: None,
                },
            }])
            .unwrap(),
            blocks: Vec::new(),
        };
        let at = |offset, line, column| Position {
            offset,
            line,
            column,
        };
        assert_eq!(
            update(Source(0), "a = \n", &document),
            Err(Refusal::Text(vec![Error::Syntax {
                span: Span::new(Source(0), at(4, 0, 4), at(5, 1, 0)).unwrap(),
                expected: Expected::Value,
            }]))
        );
        assert_eq!(
            update(Source(0), "a = 1\n", &document),
            Err(Refusal::Document(vec![Unwritable::Key { span: None }]))
        );
    }
}

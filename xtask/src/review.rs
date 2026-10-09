//! The review check: the review of a PR is done at its head. The round comment format
//! it parses is in `.claude/skills/review/SKILL.md`, "Round comment".

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::Path;
use std::process::{Command, ExitCode};

use comrak::nodes::{AstNode, LineColumn, NodeValue, Sourcepos};
use comrak::{Arena, Options, parse_document};
use serde_json::Value;

use crate::field;

mod history;

/// The first change of code in a range `<from>..<end>`, as a phrase, or `None`.
type CodeChange<'a> = &'a dyn Fn(&str, &str) -> Result<Option<String>, String>;

/// The account that posts each round comment and each director verdict. Comments by
/// other accounts never count.
const BOT: &str = "synnax-foundation-factory[bot]";

/// The end of each problem that asks for the round comment format.
const FORMAT: &str =
    "in the format of .claude/skills/review/SKILL.md, \"Round comment\".";

/// What in a line of a comment can hide text on GitHub, or keeps the check from
/// reading it.
#[derive(Clone, Copy, Debug)]
enum Cause {
    /// Raw HTML.
    Raw,
    /// `[^` in a span of text before the last line of its paragraph: GitHub reads a `]`
    /// on a later line as the end of a footnote reference.
    Bracket,
    /// A link or an image with a line break after its text, or a link reference
    /// definition, in a paragraph: comrak then places its text in the wrong lines
    /// ([`misplaced`]).
    Misplaced,
    /// A field or an [`END`] name that GitHub shows at the start of a line, and that
    /// the source of the line does not start with, as with an escape, an entity, or
    /// emphasis ([`disguised`]).
    Name,
}

impl Cause {
    /// The problem of round `number` with this cause in `line`.
    fn problem(self, number: u32, line: &str) -> String {
        let (cause, remedy) = match self {
            Cause::Raw => (
                format!(
                    "raw HTML, which can hide text on GitHub, in the line `{line}`"
                ),
                "Put the line in a code span",
            ),
            Cause::Bracket => (
                format!(
                    "`[^` before the last line of a paragraph, which can hide text on \
                     GitHub, in the line `{line}`"
                ),
                "Put the line in a code span",
            ),
            Cause::Misplaced => (
                format!(
                    "a link or an image with a line break after its text, or a link \
                     reference definition, in the paragraph that starts with the line \
                     `{line}`, which the check cannot read"
                ),
                "Write each link and image on one line, and put a blank line after each \
                 link reference definition",
            ),
            Cause::Name => (
                format!(
                    "a field or end line name that is not plain text, which the check \
                     does not read, in the line `{line}`"
                ),
                "Write each name as plain text",
            ),
        };
        format!("review round {number} has {cause}. {remedy}, {FORMAT}")
    }
}

/// The names of the lines that end each round comment, in order.
const END: [&str; 3] = ["Deferred", "Public surface", "Hot path"];

/// A round comment posted before this UTC time keeps the check from before the end
/// lines: it needs no end lines, and an earlier free-form one passes. It still needs
/// `performance` when a `Hot path:` line names a function.
const CUTOFF: &str = "2026-10-08T03:00:00Z";

/// The record of a PR that the check reads.
#[derive(Debug)]
struct Record {
    branch: String,
    /// The branch the PR merges into.
    base: String,
    labels: Vec<String>,
    files: Vec<String>,
    comments: Vec<Comment>,
}

#[derive(Debug)]
struct Comment {
    author: String,
    body: String,
    /// When the comment was posted, a UTC time (`utc`), so text order is time order.
    created: String,
}

/// A parsed `## Review round <n>` comment.
#[derive(Debug, PartialEq, Eq)]
struct Round {
    number: u32,
    reviewers: BTreeSet<String>,
    /// The round has the line `Breaker: skipped`: a later round may skip `breaker`
    /// when its range changes only comment and blank lines of `.rs` files.
    breakerless: bool,
    from: String,
    end: String,
    findings: u32,
    /// The `Hot path:` end line names a function, so the round requires
    /// `performance`. Always `false` for an old round, which [`Parsed`] checks.
    hot: bool,
}

/// A round comment that does not parse.
#[derive(Debug)]
struct Malformed {
    /// It has a `Reviewers:`, `Range:`, or `Findings:` line, so it is not free-form,
    /// or the check cannot read it ([`Shown::hiding`]).
    fixed: bool,
    problem: String,
}

/// A comment with a `## Review round <n>` line.
#[derive(Debug)]
struct Parsed {
    round: Result<Round, Malformed>,
    /// The problem of an old round that names a hot path ([`named`]) and does not
    /// name `performance`: in its `Reviewers:` field when it parses, else in a
    /// `Reviewers:` line ([`performer`]).
    performance: Option<String>,
}

/// Checks that the review of PR `pr` is done at commit `head`: its last round
/// comment ends at `head` or reaches it through clean merges of the base, finds
/// nothing, and names each required reviewer, and, for a red-team PR, the director
/// approved `head`. Reads the PR with `gh` and the history of the repository at
/// `root` with `git`, and prints each problem.
///
/// Exits 0 when the review is done, 1 when it is not, and 2 when `gh` or `git` fails
/// or the PR record lacks a field.
pub(crate) fn run(root: &Path, pr: &str, head: &str) -> ExitCode {
    let found = fetch(pr).and_then(|record| {
        let history = history::History::new(root, &record.base);
        problems(
            &record,
            head,
            &|end| history.reaches(end, head),
            &|from, end| history.code_change(from, end),
        )
    });
    exit(found)
}

/// Prints each problem or the failure in `found`, and gives the exit code of `run`.
fn exit(found: Result<Vec<String>, String>) -> ExitCode {
    match found {
        Ok(problems) if problems.is_empty() => ExitCode::SUCCESS,
        Ok(problems) => {
            for problem in problems {
                eprintln!("error: {problem}\n");
            }
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

/// The problems with the review in `record` at `head`: each round must name the
/// reviewers it requires, and the last round must find nothing and end at `head`.
/// `reaches` reports whether a commit (a SHA or its prefix) reaches `head` through
/// clean merges of the base. A later round may skip `breaker` when the range of the
/// last round changes no code; an earlier round's skip is taken as written.
/// `code_change` gives the first code change in a range, with a merge of the base read
/// by its resolution, as a phrase, or `None` (`History::code_change`).
fn problems(
    record: &Record,
    head: &str,
    reaches: &dyn Fn(&str) -> Result<bool, String>,
    code_change: CodeChange<'_>,
) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    let rounds: Vec<_> = record
        .comments
        .iter()
        .filter(|c| c.author == BOT)
        .filter_map(|c| {
            let old = c.created.as_str() < CUTOFF;
            round(&c.body, old).map(|round| (old, round))
        })
        .collect();
    if rounds.is_empty() {
        problems.push(format!(
            "no review round comment by {BOT}. Run `/review` and post each round \
             {FORMAT}"
        ));
    }
    for (i, (old, parsed)) in rounds.iter().enumerate() {
        let last = i + 1 == rounds.len();
        match &parsed.round {
            Ok(round) => {
                problems.extend(unnamed(round, &record.files, last, code_change)?);
            }
            Err(e) => {
                if !*old || last || e.fixed {
                    problems.push(e.problem.clone());
                }
            }
        }
        problems.extend(parsed.performance.clone());
    }
    if let Some((
        _,
        Parsed {
            round: Ok(round), ..
        },
    )) = rounds.last()
    {
        if round.findings > 0 {
            problems.push(format!(
                "review round {} has findings ({}). Fix or answer them, then run \
                 another round.",
                round.number, round.findings
            ));
        }
        if !reaches(&round.end)? {
            problems.push(format!(
                "review round {} ends at {}, not at the head {head}. A commit after \
                 the round needs a new round; only a clean merge of the base does \
                 not.",
                round.number, round.end
            ));
        }
    }
    if record.branch.starts_with("red-team/")
        && record.labels.iter().any(|l| l == "oracle")
    {
        problems.extend(approval(record, head));
    }
    Ok(problems)
}

/// The problems with the reviewers that `round` names for a PR that changes `files`.
/// A later round may skip `breaker` when its range changes no code, which only the
/// `last` round checks with `code_change`: a rebase can drop the range of an earlier
/// round from the clone, but the last round's end must reach the head.
fn unnamed(
    round: &Round,
    files: &[String],
    last: bool,
    code_change: CodeChange<'_>,
) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    let mut missing: Vec<&str> = required(round, files)
        .into_iter()
        .filter(|name| !round.reviewers.contains(*name))
        .collect();
    if round.number > 1 && round.breakerless && missing.contains(&"breaker") {
        missing.retain(|name| *name != "breaker");
        if last && let Some(change) = code_change(&round.from, &round.end)? {
            problems.push(format!(
                "review round {} skips `breaker`, but its range {change}.",
                round.number
            ));
        }
    }
    if !missing.is_empty() {
        problems.push(format!(
            "review round {} names no {}, which this round requires.",
            round.number,
            missing.join(", ")
        ));
    }
    Ok(problems)
}

/// The reviewers that `round` must name for a PR that changes `files`, by REVIEW
/// TIERS in `docs/decisions/operations/review-tiers.md`: on round 1, `reviewer`, plus
/// `architecture` and `breaker` for a code PR; on a later round, `reviewer`, plus
/// `breaker` for a code PR; and `performance` when the round names a hot path.
fn required(round: &Round, files: &[String]) -> Vec<&'static str> {
    let code = files.iter().any(|f| history::code_path(f));
    let mut required = if code && round.number <= 1 {
        vec!["reviewer", "architecture", "breaker"]
    } else if code {
        vec!["reviewer", "breaker"]
    } else {
        vec!["reviewer"]
    };
    if round.hot {
        required.push("performance");
    }
    required
}

/// A problem when no director verdict by the bot has the line
/// ``Director: approved at `<sha>` `` for `head`. A later push needs a new approval, so
/// the SHA must be `head` or a prefix of it of 7 or more digits.
fn approval(record: &Record, head: &str) -> Option<String> {
    let approved = record
        .comments
        .iter()
        .filter(|c| c.author == BOT)
        .flat_map(|c| lines(&c.body))
        .filter_map(|l| l.trim().strip_prefix("Director: approved at "))
        .map(|sha| sha.trim_matches(['`', '.']))
        .any(|sha| sha.len() >= 7 && head.starts_with(sha));
    (!approved).then(|| {
        format!(
            "a red-team `oracle` PR needs the director's verdict with the line \
             \"Director: approved at `<sha>`\" for the head {head}."
        )
    })
}

/// Reports whether a `Reviewers:` line in `text` ([`Shown::text`]) names
/// `performance`.
fn performer(text: &[Vec<&str>]) -> bool {
    text.iter()
        .flatten()
        .filter_map(|l| l.strip_prefix("Reviewers: "))
        .any(|r| listed(r).contains("performance"))
}

/// The reviewers that the value of a `Reviewers:` line lists.
fn listed(reviewers: &str) -> BTreeSet<String> {
    reviewers
        .split(',')
        .map(|r| r.trim().trim_matches('`').to_string())
        .collect()
}

/// Parses `body` as a round comment. `None` when it has no round heading:
/// [`Shown::number`], or [`Shown::html_number`] when it is not `old`. The fields are
/// the first block after the heading, so the findings text cannot set them. The last
/// block is the end lines ([`END`]), unless the comment is `old`, posted before
/// [`CUTOFF`].
fn round(body: &str, old: bool) -> Option<Parsed> {
    let body = normalized(body);
    let shown = Shown::read(&body, old);
    let number = shown.number.or(shown.html_number.filter(|_| !old))?;
    let text = &shown.text;
    let lines = shown.fields().iter().copied();
    let field = |name| lines.clone().find_map(|l: &str| l.strip_prefix(name));
    let (reviewers, range) = (field("Reviewers: "), field("Range: "));
    let findings = field("Findings: ");
    let breakerless = lines.clone().any(|l| l.starts_with("Breaker: skipped"));
    let fixed = [reviewers, range, findings].iter().any(Option::is_some)
        || shown.hiding.is_some();
    let performance = |reviewers: Option<&BTreeSet<String>>| {
        let performer =
            reviewers.map_or_else(|| performer(text), |r| r.contains("performance"));
        (old && !performer && named(text)).then(|| {
            format!(
                "review round {number} names no performance, which this round requires."
            )
        })
    };
    let Ok(number) = number.parse::<u32>() else {
        let problem = format!("`## Review round {number}` has no round number");
        return Some(Parsed {
            round: Err(Malformed { fixed, problem }),
            performance: performance(None),
        });
    };
    let missing = |name| {
        format!("review round {number} has no `{name}:` line. Write the round {FORMAT}")
    };
    let fields = || {
        if let Some((line, cause)) = shown.hiding {
            return Err(cause.problem(number, line));
        }
        let range = range.ok_or_else(|| missing("Range"))?.trim_matches('`');
        let (from, end) = range.split_once("..").ok_or_else(|| {
            format!(
                "review round {number} has the range `{range}`, not `<from>..<head>`"
            )
        })?;
        let findings = match findings.ok_or_else(|| missing("Findings"))? {
            "none" => 0,
            count => count.parse().map_err(|e| {
                format!(
                    "review round {number} has `Findings: {count}`, not a count or \
                     `none`: {e}"
                )
            })?,
        };
        let hot = !old && hot(shown.end(), number)?;
        Ok(Round {
            number,
            reviewers: listed(reviewers.ok_or_else(|| missing("Reviewers"))?),
            breakerless,
            from: from.to_string(),
            end: end.to_string(),
            findings,
            hot,
        })
    };
    let round = fields().map_err(|problem| Malformed { fixed, problem });
    let performance = performance(round.as_ref().ok().map(|r| &r.reviewers));
    Some(Parsed { round, performance })
}

/// The lines of `text`, each ended by `\n`, `\r\n`, or a lone `\r`, as on GitHub.
fn lines(text: &str) -> impl Iterator<Item = &str> + Clone {
    text.split('\n')
        .flat_map(|l| l.strip_suffix('\r').unwrap_or(l).split('\r'))
}

/// Whether the end lines `paragraph` of round `number` name a hot path ([`function`]).
/// The paragraph must be the [`END`] lines in order and nothing else.
fn hot(paragraph: &[&str], number: u32) -> Result<bool, String> {
    let (values, read) = entries(paragraph);
    let [deferred, surface, hot] = END;
    for (i, name) in END.into_iter().enumerate() {
        let found = values.get(i).map(|(found, _)| *found);
        if read < paragraph.len() || found != Some(name) {
            return Err(format!(
                "review round {number} does not end with a `{name}:` line. End each \
                 round with its `{deferred}:`, `{surface}:`, and `{hot}:` lines, in \
                 that order, {FORMAT}"
            ));
        }
    }
    if let Some((name, _)) = values.get(END.len()) {
        return Err(format!(
            "review round {number} has a second `{name}:` line in its end lines."
        ));
    }
    Ok(function(&values[2].1))
}

/// Whether a round posted before [`CUTOFF`] names a hot path: a `Hot path:` line in
/// `text` ([`Shown::text`]) names a function ([`function`]).
fn named(text: &[Vec<&str>]) -> bool {
    let start = format!("{}:", END[2]);
    text.iter().any(|p| {
        (0..p.len())
            .any(|i| p[i].starts_with(&start) && function(&entries(&p[i..]).0[0].1))
    })
}

/// The [`END`] entries at the start of `lines`, each as its name and value, and the
/// number of lines they take. An entry starts at the start of a line with its name,
/// and may wrap onto the lines after it.
fn entries(lines: &[&str]) -> (Vec<(&'static str, String)>, usize) {
    let mut values: Vec<(&str, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let name = END
            .iter()
            .find(|name| line.starts_with(&format!("{name}:")));
        match (name, values.last_mut()) {
            (Some(name), _) => values.push((*name, line[name.len() + 1..].to_string())),
            (None, Some((_, value))) => {
                *value = format!("{value} {}", line.trim_start());
            }
            _ => return (values, i),
        }
    }
    (values, lines.len())
}

/// Options for the GitHub extensions that change which lines are text: tables,
/// footnotes, and task lists. Each footnote stays in place ([`Shown::read`] moves it).
fn options() -> Options<'static> {
    let mut options = Options::default();
    let extension = &mut options.extension;
    extension.table = true;
    extension.footnotes = true;
    extension.tasklist = true;
    options.parse.leave_footnote_definitions = true;
    options
}

/// A comment read as GitHub reads Markdown: its text from its round heading on, and the
/// first line that can hide text or that the check cannot read.
#[derive(Debug, Default)]
struct Shown<'a> {
    /// The text after `## Review round ` in the heading, or `None` when the comment
    /// has no such heading at the top level.
    number: Option<&'a str>,
    /// Each top-level block after the heading: the index in `text` of a paragraph, or
    /// `None` for any other block. The footnotes come last, as GitHub shows them.
    blocks: Vec<Option<usize>>,
    /// The lines of text of each paragraph after the heading, at any depth, as GitHub
    /// shows them: without the indent or the marks of a list item or a quote. A
    /// footnote with no reference is not shown. A paragraph that comrak places in the
    /// wrong lines ([`misplaced`]) gives no line.
    text: Vec<Vec<&'a str>>,
    /// The first line of the comment that can hide text on GitHub or that the check
    /// cannot read, and its cause.
    hiding: Option<(&'a str, Cause)>,
    /// The text after `## Review round ` in the first line in a top-level HTML block
    /// that starts with it. GitHub reads some HTML blocks as text, and then shows the
    /// line as a heading.
    html_number: Option<&'a str>,
}

impl<'a> Shown<'a> {
    /// Reads `body` ([`normalized`]). Its round heading is the first top-level
    /// `## Review round ` heading. In an `old` round, only a [`Cause::Misplaced`]
    /// paragraph is [`Shown::hiding`].
    fn read(body: &'a str, old: bool) -> Self {
        let mut shown = Self::default();
        let starts: Vec<usize> = std::iter::once(0)
            .chain(body.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        let at = |at: LineColumn| starts[at.line - 1] + at.column - 1;
        let line_start = |line: usize| starts[line - 1];
        let source = |Sourcepos { start, end }: Sourcepos| {
            line_start(start.line)..line_end(body, line_start(end.line))
        };
        let arena = Arena::new();
        let root = parse_document(&arena, body, &options());
        // The code blocks of a footnote that GitHub does not show still hold its lines.
        let mut codes = Vec::new();
        for node in root.descendants() {
            let data = node.data.borrow();
            if let NodeValue::CodeBlock(_) = data.value {
                codes.push(at(data.sourcepos.start)..source(data.sourcepos).end);
            }
        }
        footnotes(root);
        let mut hiding = Vec::new();
        for node in root.descendants() {
            let data = node.data.borrow();
            let start = line_start(data.sourcepos.start.line);
            let top = node.parent().is_some_and(|p| p.same_node(root));
            let index =
                matches!(data.value, NodeValue::Paragraph).then(|| shown.text.len());
            if top && shown.number.is_some() {
                shown.blocks.push(index);
            }
            match &data.value {
                NodeValue::Heading(_) if top && shown.number.is_none() => {
                    shown.number = heading(&body[source(data.sourcepos)]);
                    shown.text.clear();
                }
                NodeValue::Paragraph if misplaced(node, data.sourcepos.end.line) => {
                    hiding.push((start, Cause::Misplaced));
                    shown.text.push(Vec::new());
                }
                NodeValue::Paragraph => {
                    let last = data.sourcepos.end.line;
                    hiding.extend(
                        bracket(node, last)
                            .map(|line| (line_start(line), Cause::Bracket)),
                    );
                    hiding.extend(
                        disguised(body, node, at)
                            .map(|line| (line_start(line), Cause::Name)),
                    );
                    shown.text.push(texts(body, node, at));
                }
                NodeValue::HtmlBlock(_) | NodeValue::HtmlInline(_) => {
                    hiding.push((start, Cause::Raw));
                    let block = top.then(|| &body[source(data.sourcepos)]);
                    let number = || block?.split('\n').find_map(heading);
                    shown.html_number = shown.html_number.or_else(number);
                }
                _ => {}
            }
        }
        hiding.extend(tagged(body, &starts, &codes).map(|at| (at, Cause::Raw)));
        // An old round is taken as written, except where the check cannot read it.
        let hiding = hiding
            .into_iter()
            .filter(|(_, cause)| !old || matches!(cause, Cause::Misplaced));
        let first = hiding.min_by_key(|(at, _)| *at);
        shown.hiding =
            first.map(|(at, cause)| (body[at..line_end(body, at)].trim(), cause));
        shown
    }

    /// The lines of the fields: the first block after the round heading, or none when
    /// it is not a paragraph.
    fn fields(&self) -> &[&'a str] {
        self.paragraph(self.blocks.first())
    }

    /// The end lines: the last block after the fields, or none when it is not a
    /// paragraph or the fields are the only block.
    fn end(&self) -> &[&'a str] {
        self.paragraph(self.blocks.get(1..).and_then(<[_]>::last))
    }

    /// The lines of `block`, one of [`Shown::blocks`], or none when it is not a
    /// paragraph.
    fn paragraph(&self, block: Option<&Option<usize>>) -> &[&'a str] {
        block
            .copied()
            .flatten()
            .map_or(&[][..], |i| self.text[i].as_slice())
    }
}

/// The first line of `paragraph` before its last line `last` with a span of text that
/// holds `[^`. GitHub can read a `]` on a later line as the end of a footnote
/// reference, and then hides the text between them.
fn bracket<'n>(paragraph: &'n AstNode<'n>, last: usize) -> Option<usize> {
    paragraph.descendants().find_map(|span| {
        let data = span.data.borrow();
        let line = data.sourcepos.start.line;
        let text = matches!(&data.value, NodeValue::Text(t) if t.contains("[^"));
        (text && line < last).then_some(line)
    })
}

/// The first line of `paragraph` that GitHub shows with a field or an [`END`] name at
/// its start, while its source does not start with that name. The check reads each
/// name in the source ([`texts`]).
fn disguised<'n>(
    body: &str,
    paragraph: &'n AstNode<'n>,
    at: impl Fn(LineColumn) -> usize,
) -> Option<usize> {
    let names = ["Reviewers", "Range", "Findings", "Breaker"]
        .into_iter()
        .chain(END);
    let titled = |text: &str| {
        let mut names = names.clone();
        names.any(|n| {
            text.strip_prefix(n)
                .is_some_and(|rest| rest.starts_with(':'))
        })
    };
    // Each line as its number, the offset of its first span, and the text shown.
    let mut lines: Vec<(usize, usize, String)> = Vec::new();
    let mut fresh = true;
    for span in paragraph.descendants().skip(1) {
        let data = span.data.borrow();
        let text = match &data.value {
            NodeValue::SoftBreak | NodeValue::LineBreak => {
                fresh = true;
                continue;
            }
            NodeValue::Text(text) => text.as_ref(),
            NodeValue::Code(code) => code.literal.as_str(),
            _ => "",
        };
        if fresh {
            let start = data.sourcepos.start;
            lines.push((start.line, at(start), String::new()));
            fresh = false;
        }
        if let Some((_, _, shown)) = lines.last_mut() {
            shown.push_str(text);
        }
    }
    let mut lines = lines.into_iter();
    lines
        .find(|(_, start, shown)| titled(shown) && !titled(&body[*start..]))
        .map(|(line, ..)| line)
}

/// Whether comrak places each span of `paragraph` before its last line `last`. It
/// does after a link or an image with a line break after its text, and after a link
/// reference definition, and then each line it gives after them is wrong.
fn misplaced<'n>(paragraph: &'n AstNode<'n>, last: usize) -> bool {
    let mut spans = paragraph.descendants().skip(1);
    spans.all(|span| span.data.borrow().sourcepos.end.line < last)
}

/// Moves to the end of `root` the first definition of each footnote label with a
/// reference, as GitHub shows it, and detaches each other one.
fn footnotes<'n>(root: &'n AstNode<'n>) {
    let notes: Vec<_> = root
        .descendants()
        .filter_map(|node| match &node.data.borrow().value {
            NodeValue::FootnoteDefinition(note) => {
                Some((node, note.name.clone(), note.total_references))
            }
            _ => None,
        })
        .collect();
    // comrak counts the references of the last definition of a label, and GitHub
    // shows the first.
    for (i, (node, name, _)) in notes.iter().enumerate() {
        let label = |(_, other, _): &(_, String, _)| same(name, other);
        let first = !notes[..i].iter().any(label);
        let referenced = notes[i..]
            .iter()
            .any(|note @ (_, _, references)| *references > 0 && label(note));
        if first && referenced {
            root.append(node);
        } else {
            node.detach();
        }
    }
}

/// Whether comrak reads the footnote labels `a` and `b` as one label.
fn same(a: &str, b: &str) -> bool {
    let arena = Arena::new();
    let text = format!("[^{a}]\n\n[^{b}]: x");
    let root = parse_document(&arena, &text, &options());
    root.descendants()
        .any(|n| matches!(n.data.borrow().value, NodeValue::FootnoteReference(_)))
}

/// The lines of text of `paragraph` in `body`, each from the start of its first span
/// (`at` gives its offset) to the end of its line of source.
fn texts<'a, 'n>(
    body: &'a str,
    paragraph: &'n AstNode<'n>,
    at: impl Fn(LineColumn) -> usize,
) -> Vec<&'a str> {
    let mut lines = Vec::new();
    let mut fresh = true;
    for span in paragraph.descendants().skip(1) {
        let data = span.data.borrow();
        if matches!(data.value, NodeValue::SoftBreak | NodeValue::LineBreak) {
            fresh = true;
        } else if fresh {
            let start = at(data.sourcepos.start);
            lines.push(&body[start..line_end(body, start)]);
            fresh = false;
        }
    }
    lines
}

/// `text` with each line ended by `\n`, with no spaces or tabs at the end of a line,
/// and with each tab in the spaces, tabs, and `>` at the start of a line replaced by
/// spaces to the next multiple of 4 columns. None of these changes what GitHub shows.
/// A line number then gives the offset of its line, and comrak gives the right column
/// after a tab that a quote or a list item takes in part.
fn normalized(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    for line in lines(text) {
        let line = line.trim_end_matches([' ', '\t']);
        let content = line.trim_start_matches([' ', '\t', '>']);
        let start = normalized.len();
        for c in line[..line.len() - content.len()].chars() {
            match c {
                '\t' => {
                    let columns = 4 - (normalized.len() - start) % 4;
                    normalized.extend(std::iter::repeat_n(' ', columns));
                }
                c => normalized.push(c),
            }
        }
        normalized.push_str(content);
        normalized.push('\n');
    }
    normalized
}

/// The text after `## Review round ` in `line` when it starts with it after at most
/// three spaces.
fn heading(line: &str) -> Option<&str> {
    let heading = line.trim_start_matches(' ');
    let indent = line.len() - heading.len();
    (indent < 4)
        .then_some(heading)?
        .strip_prefix("## Review round ")
}

/// The first of the line starts `starts` of `body` whose line, after the indent and
/// the marks of quotes, list items, and footnote labels ([`note`]), starts with raw
/// HTML: `<` and a letter, `!`, `/`, or `?` that is not an autolink. A line in a code
/// block (`codes`) does not count. GitHub reads some of these lines as an HTML block
/// where comrak does not, such as `<source>`. GitHub reads the blocks of a comment
/// before its spans, so a line inside a code span, a link, or a link definition counts
/// too.
fn tagged(body: &str, starts: &[usize], codes: &[Range<usize>]) -> Option<usize> {
    starts.iter().copied().find(|&start| {
        let line = &body[start..line_end(body, start)];
        let rest = bare(line);
        let at = |rest: &str| start + line.len() - rest.len();
        !codes.iter().any(|code| code.contains(&at(rest))) && tag(rest)
    })
}

/// Whether `text` starts with `<` and a letter, `!`, `/`, or `?`, and not with an
/// autolink.
fn tag(text: &str) -> bool {
    let opens = |c: char| c.is_ascii_alphabetic() || "!/?".contains(c);
    let autolink = || {
        let arena = Arena::new();
        let root = parse_document(&arena, text, &options());
        root.descendants().any(|node| {
            let data = node.data.borrow();
            matches!(data.value, NodeValue::Link(_)) && data.sourcepos.start.column == 1
        })
    };
    text.strip_prefix('<').is_some_and(|l| l.starts_with(opens)) && !autolink()
}

/// `line` after the indent and the marks of quotes, list items, and footnote labels
/// ([`note`]) at its start.
fn bare(line: &str) -> &str {
    let mut rest = unmarked(line);
    while let Some(after) = note(rest) {
        rest = unmarked(after);
    }
    rest
}

/// `line` after the indent and the marks of quotes and list items at its start.
fn unmarked(line: &str) -> &str {
    let mut rest = line;
    loop {
        rest = rest.trim_start_matches([' ', '\t']);
        let number = rest.trim_start_matches(|c: char| c.is_ascii_digit());
        let item = if number.len() < rest.len() {
            number.strip_prefix(['.', ')'])
        } else {
            rest.strip_prefix(['-', '+', '*'])
        };
        let item = item.filter(|after| after.starts_with([' ', '\t']));
        match item.or_else(|| rest.strip_prefix('>')) {
            Some(after) => rest = after,
            None => return rest,
        }
    }
}

/// The text after the footnote label at the start of `text`: `[^`, one or more
/// characters other than `]`, space, or tab, then `]:`.
fn note(text: &str) -> Option<&str> {
    let (label, after) = text.strip_prefix("[^")?.split_once(']')?;
    let label = !label.is_empty() && !label.contains([' ', '\t']);
    after.strip_prefix(':').filter(|_| label)
}

/// The offset in `text` of the end of the line that holds offset `start`.
fn line_end(text: &str, start: usize) -> usize {
    text[start..].find('\n').map_or(text.len(), |i| start + i)
}

/// Whether the `Hot path:` value `value` names a function: its first word, with
/// its backticks and one final `,`, `.`, or `;` removed, is not `none`.
fn function(value: &str) -> bool {
    let first = value.split_whitespace().next().unwrap_or_default();
    let first = first.replace('`', "");
    first.strip_suffix([',', '.', ';']).unwrap_or(&first) != "none"
}

/// Reads the record of PR `pr` with `gh`, in the repository that `gh` resolves.
fn fetch(pr: &str) -> Result<Record, String> {
    let path = format!("repos/{{owner}}/{{repo}}/pulls/{pr}");
    let pull = gh(&path)?;
    let files = gh(&format!("{path}/files"))?;
    let comments = gh(&format!("repos/{{owner}}/{{repo}}/issues/{pr}/comments"))?;
    record_of(&pull, &files, &comments).map_err(|e| format!("PR {pr}: {e}"))
}

/// The record from the `gh api` objects of a PR, its files, and its comments.
fn record_of(
    pull: &[Value],
    files: &[Value],
    comments: &[Value],
) -> Result<Record, String> {
    let pull = pull.first().ok_or("gh returned no PR")?;
    Ok(Record {
        branch: field::text(&pull["head"], "ref")?.to_string(),
        base: field::text(&pull["base"], "ref")?.to_string(),
        labels: field::list(pull, "labels")?
            .iter()
            .map(|l| field::text(l, "name").map(str::to_string))
            .collect::<Result<_, _>>()?,
        files: files
            .iter()
            .map(|f| field::text(f, "filename").map(str::to_string))
            .collect::<Result<_, _>>()?,
        comments: comments
            .iter()
            .map(|c| {
                Ok(Comment {
                    author: field::text(&c["user"], "login")?.to_string(),
                    body: field::text(c, "body")?.to_string(),
                    created: utc(field::text(c, "created_at")?)?.to_string(),
                })
            })
            .collect::<Result<_, String>>()?,
    })
}

/// `text` when it is a UTC time `YYYY-MM-DDTHH:MM:SSZ`, the form GitHub gives, in
/// which text order is time order.
fn utc(text: &str) -> Result<&str, String> {
    let shaped = text.len() == 20
        && text.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            10 => b == b'T',
            13 | 16 => b == b':',
            19 => b == b'Z',
            _ => b.is_ascii_digit(),
        });
    if shaped {
        Ok(text)
    } else {
        Err(format!(
            "`created_at` is `{text}`, not a UTC time `YYYY-MM-DDTHH:MM:SSZ`"
        ))
    }
}

/// Each object that `gh api --paginate` returns for `path`: the items of each page
/// of a list, or the one object.
fn gh(path: &str) -> Result<Vec<Value>, String> {
    let output = Command::new("gh")
        .args(["api", "--paginate", path])
        .output()
        .map_err(|e| format!("gh api {path}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "gh api {path}: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    items(&output.stdout).map_err(|e| format!("gh api {path}: {e}"))
}

/// The items of each JSON array in `pages`, and each object that is not in an array.
fn items(pages: &[u8]) -> Result<Vec<Value>, serde_json::Error> {
    let mut items = Vec::new();
    for page in serde_json::Deserializer::from_slice(pages).into_iter() {
        match page? {
            Value::Array(page) => items.extend(page),
            object => items.push(object),
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests;

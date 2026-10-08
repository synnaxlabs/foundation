//! The review check: the review of a PR is done at its head. The round comment format
//! it parses is in `.claude/skills/review/SKILL.md`, "Round comment".

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, ExitCode};

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
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
    /// It has a `Reviewers:`, `Range:`, or `Findings:` line, so it is not free-form.
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

/// Parses `body` as a round comment. `None` when it has no `## Review round <n>`
/// heading. The fields are the first block after the heading, so the findings text
/// cannot set them. The last block is the end lines ([`END`]), unless the comment is
/// `old`, posted before [`CUTOFF`].
fn round(body: &str, old: bool) -> Option<Parsed> {
    let body = unpadded(body);
    let Shown {
        number,
        blocks,
        text,
    } = Shown::read(&body);
    let number = number?;
    // Only the end lines keep their indent: an indented one is a quote, not a line.
    let lines = blocks.first().into_iter().flatten().map(|l| l.trim_start());
    let field = |name| lines.clone().find_map(|l: &str| l.strip_prefix(name));
    let (reviewers, range) = (field("Reviewers: "), field("Range: "));
    let findings = field("Findings: ");
    let breakerless = lines.clone().any(|l| l.starts_with("Breaker: skipped"));
    let fixed = reviewers.is_some() || range.is_some() || findings.is_some();
    let performance = |reviewers: Option<&BTreeSet<String>>| {
        let performer =
            reviewers.map_or_else(|| performer(&text), |r| r.contains("performance"));
        (old && !performer && named(&text)).then(|| {
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
        let hot = if old {
            false
        } else {
            let rest = blocks.get(1..).unwrap_or_default();
            hot(rest.last().map_or(&[][..], Vec::as_slice), number)?
        };
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

/// A comment from its round heading on, read as GitHub reads Markdown.
#[derive(Debug)]
struct Shown<'a> {
    /// The text after `## Review round ` in the heading, or `None` when the comment
    /// has no such heading at the top level.
    number: Option<&'a str>,
    /// Each top-level block after the heading. A paragraph is its lines, each from the
    /// start of its source line, so it keeps its indent. Any other block has no lines.
    blocks: Vec<Vec<&'a str>>,
    /// The lines of text of each paragraph after the heading, at any depth, as GitHub
    /// shows them: without the indent or the marks of a list item or a quote.
    text: Vec<Vec<&'a str>>,
}

impl<'a> Shown<'a> {
    /// Reads `body` ([`unpadded`]) from its first top-level `## Review round `
    /// heading on.
    fn read(body: &'a str) -> Self {
        let options = Options::from_iter([
            Options::ENABLE_TABLES,
            Options::ENABLE_FOOTNOTES,
            Options::ENABLE_STRIKETHROUGH,
            Options::ENABLE_TASKLISTS,
            Options::ENABLE_GFM,
        ]);
        let mut number = None;
        let (mut blocks, mut text) = (Vec::new(), Vec::new());
        // Each open block, with the index in `text` of a paragraph. A tight list item
        // holds the text of its paragraph with no paragraph event.
        let mut open: Vec<Option<usize>> = Vec::new();
        let mut fresh = true;
        for (event, range) in Parser::new_ext(body, options).into_offset_iter() {
            let top = open.is_empty();
            match event {
                Event::Start(tag) if !inline(tag.to_end()) => {
                    let heading = matches!(
                        tag,
                        Tag::Heading {
                            level: HeadingLevel::H2,
                            ..
                        }
                    );
                    if top && number.is_some() {
                        blocks.push(Vec::new());
                    } else if top && heading {
                        let line = &body[range.start..line_end(body, range.start)];
                        number = line.strip_prefix("## Review round ");
                    }
                    let paragraph = matches!(tag, Tag::Paragraph | Tag::Item);
                    open.push((paragraph && number.is_some()).then(|| {
                        text.push(Vec::new());
                        text.len() - 1
                    }));
                    fresh = true;
                }
                Event::End(tag) if !inline(tag) => {
                    open.pop();
                }
                Event::Rule => {
                    if top && number.is_some() {
                        blocks.push(Vec::new());
                    }
                    fresh = true;
                }
                Event::SoftBreak | Event::HardBreak => fresh = true,
                Event::End(_) | Event::TaskListMarker(_) => {}
                _ => {
                    if let (true, Some(&Some(paragraph))) = (fresh, open.last()) {
                        let end = line_end(body, range.start);
                        text[paragraph].push(&body[range.start..end]);
                        if open.len() == 1 {
                            let from = body[..range.start]
                                .rfind(['\n', '\r'])
                                .map_or(0, |i| i + 1);
                            let block: &mut Vec<_> =
                                blocks.last_mut().expect("a block is open");
                            block.push(&body[from..end]);
                        }
                        fresh = false;
                    }
                }
            }
        }
        Self {
            number,
            blocks,
            text,
        }
    }
}

/// Whether the tag that `tag` ends is inline: it holds text within a line of a
/// block.
fn inline(tag: TagEnd) -> bool {
    match tag {
        TagEnd::Emphasis
        | TagEnd::Strong
        | TagEnd::Strikethrough
        | TagEnd::Superscript
        | TagEnd::Subscript
        | TagEnd::Link
        | TagEnd::Image => true,
        TagEnd::Paragraph
        | TagEnd::Heading(_)
        | TagEnd::BlockQuote(_)
        | TagEnd::CodeBlock
        | TagEnd::HtmlBlock
        | TagEnd::List(_)
        | TagEnd::Item
        | TagEnd::FootnoteDefinition
        | TagEnd::DefinitionList
        | TagEnd::DefinitionListTitle
        | TagEnd::DefinitionListDefinition
        | TagEnd::Table
        | TagEnd::TableHead
        | TagEnd::TableRow
        | TagEnd::TableCell
        | TagEnd::MetadataBlock(_) => false,
    }
}

/// `text` with the spaces and tabs at the end of each line removed, which changes
/// nothing that the check reads. pulldown-cmark 0.13.4 does not close a code block at
/// a fence that a tab follows, as the Markdown spec and GitHub do.
fn unpadded(text: &str) -> String {
    let mut unpadded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(end) = rest.find(['\n', '\r']) {
        unpadded.push_str(rest[..end].trim_end_matches([' ', '\t']));
        unpadded.push_str(&rest[end..=end]);
        rest = &rest[end + 1..];
    }
    unpadded.push_str(rest.trim_end_matches([' ', '\t']));
    unpadded
}

/// The offset in `text` of the end of the line that holds offset `start`.
fn line_end(text: &str, start: usize) -> usize {
    text[start..]
        .find(['\n', '\r'])
        .map_or(text.len(), |i| start + i)
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

//! The review check: the review of a PR is done at its head. The round comment format
//! it parses is in `.claude/skills/review/SKILL.md`, "Round comment".

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, ExitCode};

use serde_json::Value;

use crate::field;

mod history;

/// The account that posts each round comment and each director verdict. Comments by
/// other accounts never count.
const BOT: &str = "synnax-foundation-factory[bot]";

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
}

/// A parsed `## Review round <n>` comment.
#[derive(Debug, PartialEq, Eq)]
struct Round {
    number: u32,
    reviewers: BTreeSet<String>,
    from: String,
    end: String,
    findings: u32,
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
        let base = format!("origin/{}", record.base);
        let history = history::History::new(root, &base);
        problems(
            &record,
            head,
            &|end| history.reaches(end, head),
            &|from, end| history.comments_only(from, end),
        )
    });
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
/// clean merges of the base, and `comments_only` whether a range changes only comment
/// lines of `.rs` files.
fn problems(
    record: &Record,
    head: &str,
    reaches: &dyn Fn(&str) -> Result<bool, String>,
    comments_only: &dyn Fn(&str, &str) -> Result<bool, String>,
) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    let rounds: Vec<_> = record
        .comments
        .iter()
        .filter(|c| c.author == BOT)
        .filter_map(|c| round(&c.body))
        .collect();
    if rounds.is_empty() {
        problems.push(format!(
            "no review round comment by {BOT}. Run `/review` and post each round in \
             the format of .claude/skills/review/SKILL.md, \"Round comment\"."
        ));
    }
    for round in &rounds {
        let round = match round {
            Ok(round) => round,
            Err(e) => {
                problems.push(e.clone());
                continue;
            }
        };
        let missing: Vec<&str> = required(round, &record.files, comments_only)?
            .into_iter()
            .filter(|name| !round.reviewers.contains(*name))
            .collect();
        if !missing.is_empty() {
            problems.push(format!(
                "review round {} names no {}, which this round requires.",
                round.number,
                missing.join(", ")
            ));
        }
    }
    if let Some(Ok(round)) = rounds.last() {
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

/// The reviewers that `round` must name for a PR that changes `files`, by REVIEW
/// TIERS in `docs/decisions.md`: on round 1, `reviewer`, plus `architecture` and
/// `breaker` for a code PR; on a later round, `reviewer`, plus `breaker` for a code PR
/// unless the range changes only comment lines. `performance` depends on what the
/// code does, so no round requires it here.
fn required(
    round: &Round,
    files: &[String],
    comments_only: &dyn Fn(&str, &str) -> Result<bool, String>,
) -> Result<Vec<&'static str>, String> {
    let code = files.iter().map(Path::new).any(|f| {
        f.extension().is_some_and(|e| e == "rs")
            || f.file_name()
                .is_some_and(|n| n == "Cargo.toml" || n == "Cargo.lock")
    });
    Ok(if !code {
        vec!["reviewer"]
    } else if round.number <= 1 {
        vec!["reviewer", "architecture", "breaker"]
    } else if comments_only(&round.from, &round.end)? {
        vec!["reviewer"]
    } else {
        vec!["reviewer", "breaker"]
    })
}

/// A problem when no director verdict by the bot has the line
/// ``Director: approved at `<sha>` `` for `head`. A later push needs a new approval, so
/// the SHA must be `head` or a prefix of it of 7 or more digits.
fn approval(record: &Record, head: &str) -> Option<String> {
    let approved = record
        .comments
        .iter()
        .filter(|c| c.author == BOT)
        .flat_map(|c| c.body.lines())
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

/// Parses `body` as a round comment. `None` when it has no `## Review round <n>` line.
/// The fields are the first block of lines after that line, so the findings text
/// cannot set them.
fn round(body: &str) -> Option<Result<Round, String>> {
    let mut lines = body.lines().map(str::trim);
    let number = lines.find_map(|l| l.strip_prefix("## Review round "))?;
    let lines = lines
        .skip_while(|l| l.is_empty())
        .take_while(|l| !l.is_empty());
    let Ok(number) = number.parse::<u32>() else {
        return Some(Err(format!(
            "`## Review round {number}` has no round number"
        )));
    };
    let (mut reviewers, mut range, mut findings) = (None, None, None);
    for line in lines {
        if let Some(value) = line.strip_prefix("Reviewers: ") {
            reviewers.get_or_insert(value);
        } else if let Some(value) = line.strip_prefix("Range: ") {
            range.get_or_insert(value);
        } else if let Some(value) = line.strip_prefix("Findings: ") {
            findings.get_or_insert(value);
        }
    }
    let missing = |name| {
        format!(
            "review round {number} has no `{name}:` line. Write the round in the \
             format of .claude/skills/review/SKILL.md, \"Round comment\"."
        )
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
        Ok(Round {
            number,
            reviewers: reviewers
                .ok_or_else(|| missing("Reviewers"))?
                .split(',')
                .map(|r| r.trim().trim_matches('`').to_string())
                .collect(),
            from: from.to_string(),
            end: end.to_string(),
            findings,
        })
    };
    Some(fields())
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
                })
            })
            .collect::<Result<_, String>>()?,
    })
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

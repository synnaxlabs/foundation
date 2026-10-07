//! The review check: the review of a PR is done at its head. The round comment format
//! it parses is in `.claude/skills/review/SKILL.md`, "Round comment".

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::{field, history};

/// The account that posts each round comment and each director verdict. Comments by
/// other accounts never count.
const BOT: &str = "synnax-foundation-factory[bot]";

/// The record of a PR that the check reads.
#[derive(Debug)]
struct Record {
    branch: String,
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
    /// The round has the line `Breaker: skipped`: its range changes no `.rs` line but
    /// comments.
    breakerless: bool,
    end: String,
    findings: String,
}

/// Checks that the review of PR `pr` is done at commit `head`: its last round comment
/// ends at `head` or reaches it through clean merges of `main`, finds nothing, and
/// names each required reviewer. A red-team PR also needs the director's approval at
/// `head`. Reads the PR with `gh` and the history of the repository at `root` with
/// `git`.
pub(crate) fn check(root: &Path, pr: &str, head: &str) -> Result<(), Vec<String>> {
    let record = fetch(pr).map_err(|e| vec![e])?;
    let history = history::History::new(root);
    let problems = problems(&record, head, &|end| history.reaches(end, head))
        .map_err(|e| vec![e])?;
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// The problems with the review in `record` at `head`. `reaches` reports whether a
/// commit (a SHA or its prefix) reaches `head` through clean merges of `main`.
fn problems(
    record: &Record,
    head: &str,
    reaches: &dyn Fn(&str) -> Result<bool, String>,
) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    let last = record
        .comments
        .iter()
        .rev()
        .filter(|c| c.author == BOT)
        .find_map(|c| round(&c.body));
    match last {
        None => problems.push(format!(
            "no review round comment by {BOT}. Run `/review` and post each round in \
             the format of .claude/skills/review/SKILL.md, \"Round comment\"."
        )),
        Some(Err(e)) => problems.push(e),
        Some(Ok(round)) => {
            if round.findings != "none" {
                problems.push(format!(
                    "review round {} has findings ({}). Fix or answer them, then run \
                     another round.",
                    round.number, round.findings
                ));
            }
            let missing: Vec<&str> = required(&round, &record.files)
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
            if !reaches(&round.end)? {
                problems.push(format!(
                    "review round {} ends at {}, not at the head {head}. A commit \
                     after the round needs a new round; only a clean merge of `main` \
                     does not.",
                    round.number, round.end
                ));
            }
        }
    }
    if record.branch.starts_with("red-team/")
        && record.labels.iter().any(|l| l == "oracle")
    {
        problems.extend(approval(record, head, reaches)?);
    }
    Ok(problems)
}

/// The reviewers that `round` must name for a PR that changes `files`: on round 1,
/// each reviewer of the review skill's table; on a later round, `reviewer`, and
/// `breaker` for a code PR unless the round skipped it.
fn required(round: &Round, files: &[String]) -> Vec<&'static str> {
    let code = files.iter().map(Path::new).any(|f| {
        f.extension().is_some_and(|e| e == "rs")
            || f.file_name()
                .is_some_and(|n| n == "Cargo.toml" || n == "Cargo.lock")
    });
    if code && round.number <= 1 {
        vec!["reviewer", "architecture", "breaker"]
    } else if code && !round.breakerless {
        vec!["reviewer", "breaker"]
    } else {
        vec!["reviewer"]
    }
}

/// A problem when no director verdict by the bot approves a commit that reaches `head`.
fn approval(
    record: &Record,
    head: &str,
    reaches: &dyn Fn(&str) -> Result<bool, String>,
) -> Result<Option<String>, String> {
    for comment in record.comments.iter().filter(|c| c.author == BOT) {
        for line in comment.body.lines() {
            let Some(sha) = line.trim().strip_prefix("Approved at ") else {
                continue;
            };
            if reaches(sha.trim_matches('`'))? {
                return Ok(None);
            }
        }
    }
    Ok(Some(format!(
        "a red-team `oracle` PR needs the director's verdict with the line \
         \"Approved at `<sha>`\" for the head {head}."
    )))
}

/// Parses `body` as a round comment. `None` when it has no `## Review round <n>` line.
fn round(body: &str) -> Option<Result<Round, String>> {
    let mut lines = body.lines().map(str::trim);
    let number = lines.find_map(|l| l.strip_prefix("## Review round "))?;
    Some(parse(number, lines))
}

fn parse<'a>(
    number: &str,
    lines: impl Iterator<Item = &'a str>,
) -> Result<Round, String> {
    let number: u32 = number
        .parse()
        .map_err(|e| format!("`## Review round {number}` has no round number: {e}"))?;
    let (mut reviewers, mut range, mut findings) = (None, None, None);
    let mut breakerless = false;
    for line in lines {
        breakerless |= line.starts_with("Breaker: skipped");
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
    let range = range.ok_or_else(|| missing("Range"))?.trim_matches('`');
    let (_, end) = range.split_once("..").ok_or_else(|| {
        format!("review round {number} has the range `{range}`, not `<from>..<head>`")
    })?;
    Ok(Round {
        number,
        reviewers: reviewers
            .ok_or_else(|| missing("Reviewers"))?
            .split(',')
            .map(|r| r.trim().trim_matches('`').to_string())
            .collect(),
        breakerless,
        end: end.to_string(),
        findings: findings.ok_or_else(|| missing("Findings"))?.to_string(),
    })
}

/// Reads the record of PR `pr` with `gh`, in the repository that `gh` resolves.
fn fetch(pr: &str) -> Result<Record, String> {
    let pull = gh(&format!("repos/{{owner}}/{{repo}}/pulls/{pr}"))?;
    let pull = pull.first().ok_or("gh returned no PR")?;
    let files = gh(&format!("repos/{{owner}}/{{repo}}/pulls/{pr}/files"))?;
    let comments = gh(&format!("repos/{{owner}}/{{repo}}/issues/{pr}/comments"))?;
    Ok(Record {
        branch: field::text(&pull["head"], "ref")?.to_string(),
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
    let mut items = Vec::new();
    for page in serde_json::Deserializer::from_slice(&output.stdout).into_iter() {
        match page.map_err(|e| format!("gh api {path}: {e}"))? {
            Value::Array(page) => items.extend(page),
            object => items.push(object),
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests;

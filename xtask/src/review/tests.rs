use super::*;

/// The head of #1089 when it merged.
const HEAD: &str = "c77c67d72fd8d37964334c80e14dd8b5a1d5b6fb";

/// The files that #1089 changed.
const FILES: &[&str] = &[
    "crates/config-hcl/src/arbitrary.rs",
    "crates/config-hcl/src/lib.rs",
    "crates/document/src/encoding.rs",
    "crates/spec/src/definition/tests.rs",
    "docs/decisions.md",
    "fuzz/fuzz_targets/config_hcl_read.rs",
    "oracles/conformance/document/encoding.rs",
];

/// The last round of #1089, replayed in the fixed format.
const ROUND: &str = "Quality: 8/10
The fixes close each round 1 finding. Tests pin the depth limit at both ends.

## Review round 3

Reviewers: reviewer
Breaker: skipped, the range changes no `.rs` line but comments
Range: `38cba24f..c77c67d7`
Findings: none

No bug, no lost coverage, no oracle weakening.";

fn bot(body: &str) -> Comment {
    Comment {
        author: BOT.to_string(),
        body: body.to_string(),
    }
}

fn record(comments: Vec<Comment>) -> Record {
    Record {
        branch: "laptop/828-checked-document".to_string(),
        labels: Vec::new(),
        files: FILES.iter().map(ToString::to_string).collect(),
        comments,
    }
}

fn check(record: &Record) -> Vec<String> {
    // A commit reaches the head only when it is a prefix of it.
    problems(record, HEAD, &|end| Ok(HEAD.starts_with(end))).unwrap()
}

#[test]
fn passes_the_last_round_of_1089() {
    let earlier = bot("## Review round 2\n\nReviewers: reviewer\nRange: `a..b`\n\
                       Findings: 2");
    assert_eq!(
        check(&record(vec![earlier, bot(ROUND)])),
        Vec::<String>::new()
    );
}

#[test]
fn fails_with_no_round_comment() {
    let other = bot("Plan: build it in xtask.");
    assert_eq!(
        check(&record(vec![other])),
        vec![format!(
            "no review round comment by {BOT}. Run `/review` and post each round in \
             the format of .claude/skills/review/SKILL.md, \"Round comment\"."
        )]
    );
}

#[test]
fn ignores_a_round_by_another_account() {
    let pasted = Comment {
        author: "someone".to_string(),
        body: ROUND.to_string(),
    };
    let problems = check(&record(vec![pasted]));
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0].starts_with("no review round comment"),
        "{problems:?}"
    );
}

#[test]
fn fails_a_range_that_ends_before_the_head() {
    let round = ROUND.replace("38cba24f..c77c67d7", "940140aa..82ba5b72");
    assert_eq!(
        check(&record(vec![bot(&round)])),
        vec![format!(
            "review round 3 ends at 82ba5b72, not at the head {HEAD}. A commit after \
             the round needs a new round; only a clean merge of `main` does not."
        )]
    );
}

#[test]
fn fails_a_round_with_findings() {
    let round = ROUND.replace("Findings: none", "Findings: 1");
    assert_eq!(
        check(&record(vec![bot(&round)])),
        vec![
            "review round 3 has findings (1). Fix or answer them, then run another \
             round."
                .to_string()
        ]
    );
}

/// `ROUND` as a later round of a code PR that does not skip `breaker`.
fn later(reviewers: &str) -> String {
    ROUND
        .replace("Reviewers: reviewer", &format!("Reviewers: {reviewers}"))
        .replace(
            "Breaker: skipped, the range changes no `.rs` line but comments\n",
            "",
        )
}

#[test]
fn fails_a_code_pr_whose_round_names_no_breaker() {
    assert_eq!(
        check(&record(vec![bot(&later("reviewer, architecture"))])),
        vec!["review round 3 names no breaker, which this round requires.".to_string()]
    );
    assert_eq!(
        check(&record(vec![bot(&later("reviewer, breaker"))])),
        Vec::<String>::new()
    );
}

#[test]
fn round_1_of_a_code_pr_needs_each_reviewer_of_the_table() {
    let first = ROUND.replace("round 3", "round 1");
    assert_eq!(
        check(&record(vec![bot(&first)])),
        vec![
            "review round 1 names no architecture, breaker, which this round requires."
                .to_string()
        ]
    );
    let first = first.replace(
        "Reviewers: reviewer",
        "Reviewers: reviewer, architecture, breaker",
    );
    assert_eq!(check(&record(vec![bot(&first)])), Vec::<String>::new());
}

#[test]
fn needs_only_the_reviewer_for_a_diff_with_no_code() {
    let first = later("`reviewer`").replace("round 3", "round 1");
    let mut docs = record(vec![bot(&first)]);
    docs.files = vec!["docs/decisions.md".to_string(), "README.md".to_string()];
    assert_eq!(check(&docs), Vec::<String>::new());
    docs.files.push("xtask/Cargo.toml".to_string());
    assert_eq!(
        check(&docs),
        vec![
            "review round 1 names no architecture, breaker, which this round requires."
                .to_string()
        ]
    );
}

#[test]
fn takes_the_last_round() {
    let later = ROUND.replace("Findings: none", "Findings: 3");
    let problems = check(&record(vec![bot(ROUND), bot(&later)]));
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("has findings (3)"), "{problems:?}");
}

#[test]
fn names_a_missing_line() {
    let round = ROUND.replace("Findings: none\n", "");
    assert_eq!(
        check(&record(vec![bot(&round)])),
        vec![
            "review round 3 has no `Findings:` line. Write the round in the format of \
             .claude/skills/review/SKILL.md, \"Round comment\"."
                .to_string()
        ]
    );
    let round = ROUND.replace("`38cba24f..c77c67d7`", "c77c67d7");
    assert_eq!(
        check(&record(vec![bot(&round)])),
        vec![
            "review round 3 has the range `c77c67d7`, not `<from>..<head>`".to_string()
        ]
    );
}

#[test]
fn a_red_team_oracle_pr_needs_the_director_approval_at_the_head() {
    let mut red = record(vec![bot(ROUND)]);
    red.branch = "red-team/buffer-open-read".to_string();
    red.labels = vec!["oracle".to_string()];
    let missing = format!(
        "a red-team `oracle` PR needs the director's verdict with the line \
         \"Approved at `<sha>`\" for the head {HEAD}."
    );
    assert_eq!(check(&red), vec![missing.clone()]);
    red.comments
        .push(bot("Quality: 8/10\nGood.\n\nApproved at `82ba5b72`"));
    assert_eq!(check(&red), vec![missing]);
    red.comments
        .push(bot("Quality: 8/10\nGood.\n\nApproved at `c77c67d7`"));
    assert_eq!(check(&red), Vec::<String>::new());
}

#[test]
fn a_red_team_pr_with_no_oracle_label_needs_no_approval() {
    let mut red = record(vec![bot(ROUND)]);
    red.branch = "red-team/buffer-open-read".to_string();
    assert_eq!(check(&red), Vec::<String>::new());
}

#[test]
fn returns_a_failure_to_read_history() {
    let failed = |_: &str| Err("git rev-parse: bad".to_string());
    assert_eq!(
        problems(&record(vec![bot(ROUND)]), HEAD, &failed),
        Err("git rev-parse: bad".to_string())
    );
}

#[test]
fn reads_items_of_each_page_and_a_single_object() {
    use serde_json::json;
    assert_eq!(
        items(b"[{\"a\":1},{\"a\":2}]\n[{\"a\":3}]").unwrap(),
        vec![json!({"a": 1}), json!({"a": 2}), json!({"a": 3})]
    );
    assert_eq!(items(b"{\"head\":{}}").unwrap(), vec![json!({"head": {}})]);
    assert_eq!(items(b"").unwrap(), Vec::<Value>::new());
    assert_eq!(
        items(b"[1,").unwrap_err().to_string(),
        "EOF while parsing a value at line 1 column 3"
    );
}

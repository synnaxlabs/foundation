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

No bug, no lost coverage, no oracle weakening.

Deferred: none
Public surface: none
Hot path: none";

/// The end lines of `ROUND`.
const END: &str = "\n\nDeferred: none\nPublic surface: none\nHot path: none";

/// A comment by the bot, posted at the cutoff, so the end lines apply.
fn bot(body: &str) -> Comment {
    Comment {
        author: BOT.to_string(),
        body: body.to_string(),
        created: CUTOFF.to_string(),
    }
}

/// A comment by the bot, posted a second before the cutoff.
fn old(body: &str) -> Comment {
    Comment {
        created: "2026-10-08T02:59:59Z".to_string(),
        ..bot(body)
    }
}

fn record(comments: Vec<Comment>) -> Record {
    Record {
        branch: "laptop/828-checked-document".to_string(),
        base: "main".to_string(),
        labels: Vec::new(),
        files: FILES.iter().map(ToString::to_string).collect(),
        comments,
    }
}

/// The start of the range of `ROUND`, which changes only comment lines.
const COMMENTS: &str = "38cba24f";

fn check(record: &Record) -> Vec<String> {
    // A commit reaches the head only when it is a prefix of it.
    problems(
        record,
        HEAD,
        &|end| Ok(HEAD.starts_with(end)),
        &|from, _| {
            Ok((from != COMMENTS).then(|| "changes code at `a.rs:2`".to_string()))
        },
    )
    .unwrap()
}

#[test]
fn passes_the_last_round_of_1089() {
    let earlier = bot(
        &("## Review round 2\n\nReviewers: reviewer, breaker\nRange: `a..b`\n\
           Findings: 2"
            .to_string()
            + END),
    );
    assert_eq!(
        check(&record(vec![earlier, bot(ROUND)])),
        Vec::<String>::new()
    );
}

#[test]
fn fails_each_free_form_earlier_round() {
    let free = |n| {
        bot(&format!(
            "## Review round {n}\n\nConfirmed findings, most severe first.\n\n\
             1. **`Checked::new` overflows the stack**."
        ))
    };
    let missing = |n| {
        format!(
            "review round {n} has no `Range:` line. Write the round in the format of \
             .claude/skills/review/SKILL.md, \"Round comment\"."
        )
    };
    assert_eq!(
        check(&record(vec![free(1), free(2), bot(ROUND)])),
        vec![missing(1), missing(2)]
    );
}

/// Round 1 of a code PR with each reviewer it requires, its findings, and `end`.
fn first(end: &str) -> Comment {
    bot(&format!(
        "## Review round 1\n\nReviewers: reviewer, architecture, breaker\n\
         Range: `a..b`\nFindings: 2\n\n1. **`send` drops a frame**.{end}"
    ))
}

#[test]
fn fails_an_earlier_round_with_no_end_line() {
    assert_eq!(
        check(&record(vec![first(""), bot(ROUND)])),
        vec![unended("Deferred").replace("round 3", "round 1")]
    );
}

#[test]
fn an_earlier_round_that_names_a_hot_path_needs_performance() {
    let hot = END.replace("Hot path: none", "Hot path: `stream::Sender::send`");
    assert_eq!(
        check(&record(vec![first(&hot), bot(ROUND)])),
        vec![
            "review round 1 names no performance, which this round requires."
                .to_string()
        ]
    );
}

/// The problem of a round 3 with raw HTML in `line`.
fn raw(line: &str) -> String {
    format!(
        "review round 3 has raw HTML, which can hide text on GitHub, in the line \
         `{line}`. Put the line in a code span, in the format of \
         .claude/skills/review/SKILL.md, \"Round comment\"."
    )
}

/// The problem of a round 3 that does not end with its `name:` line.
fn unended(name: &str) -> String {
    format!(
        "review round 3 does not end with a `{name}:` line. End each round with its \
         `Deferred:`, `Public surface:`, and `Hot path:` lines, in that order, in the \
         format of .claude/skills/review/SKILL.md, \"Round comment\"."
    )
}

#[test]
fn fails_a_round_with_no_end_line() {
    let cases = [
        ("Deferred: none\n", unended("Deferred")),
        ("Public surface: none\n", unended("Public surface")),
        ("\nHot path: none", unended("Hot path")),
    ];
    for (line, problem) in cases {
        let round = ROUND.replace(line, "");
        assert_eq!(check(&record(vec![bot(&round)])), vec![problem], "{line}");
    }
}

#[test]
fn fails_a_round_whose_end_lines_are_out_of_order_or_not_last() {
    let swapped = ROUND.replace(
        "Deferred: none\nPublic surface: none",
        "Public surface: none\nDeferred: none",
    );
    assert_eq!(
        check(&record(vec![bot(&swapped)])),
        vec![unended("Deferred")]
    );
    let twice = ROUND.to_string() + "\nHot path: `Sender::send`";
    assert_eq!(
        check(&record(vec![bot(&twice)])),
        vec![
            "review round 3 has a second `Hot path:` line in its end lines."
                .to_string()
        ]
    );
    let spaced = ROUND.replace("\nHot path:", "\n\nHot path:");
    assert_eq!(
        check(&record(vec![bot(&spaced)])),
        vec![unended("Deferred")]
    );
    let indented = ROUND.replace("\nHot path:", "\n  Hot path:");
    assert_ne!(indented, ROUND);
    assert_eq!(check(&record(vec![bot(&indented)])), Vec::<String>::new());
    let trailed = ROUND.replace("none\n", "none  \n") + "\n  \n";
    assert_eq!(check(&record(vec![bot(&trailed)])), Vec::<String>::new());
    let followed = ROUND.to_string() + "\n\nThe author fixes each finding.";
    assert_eq!(
        check(&record(vec![bot(&followed)])),
        vec![unended("Deferred")]
    );
}

#[test]
fn an_old_round_keeps_the_check_before_the_end_lines() {
    let free = old("## Review round 1\n\nConfirmed findings, most severe first.");
    let unended = old(&ROUND.replace(END, ""));
    assert_eq!(check(&record(vec![free, unended])), Vec::<String>::new());
    let missing = |n, name| {
        format!(
            "review round {n} has no `{name}:` line. Write the round in the format of \
             .claude/skills/review/SKILL.md, \"Round comment\"."
        )
    };
    for (fields, name) in [
        ("Reviewers: reviewer", "Range"),
        ("Findings: none", "Range"),
    ] {
        let fixed = old(&format!("## Review round 1\n\n{fields}"));
        assert_eq!(
            check(&record(vec![fixed, bot(ROUND)])),
            vec![missing(1, name)],
            "{fields}"
        );
    }
    let last = old("## Review round 4\n\nNo findings.");
    assert_eq!(check(&record(vec![last])), vec![missing(4, "Range")]);
}

#[test]
fn an_old_round_that_names_a_hot_path_needs_performance() {
    let problem = vec![
        "review round 3 names no performance, which this round requires.".to_string(),
    ];
    let cases = [
        ("Hot path: none", "Hot path: `send`"),
        ("Findings: none", "Findings: none\nHot path: `send`"),
        ("Hot path: none", "Hot path:\n- `send`, once per frame"),
        (
            "\n\nDeferred: none\nPublic surface: none\n",
            "\nHot path: `send`\n",
        ),
    ];
    for (line, hot) in cases {
        let round = old(&ROUND.replace(line, hot));
        assert_eq!(check(&record(vec![round])), problem, "{hot}");
    }
    let quoted = ROUND.replace(
        "Hot path: none",
        "Hot path: none\n\n```\nHot path: `send`\n```",
    );
    let tildes = ROUND.replace(
        "Hot path: none",
        "Hot path: none\n\n~~~\nHot path: `send`\n~~~",
    );
    let indented =
        ROUND.replace("Hot path: none", "Hot path: none\n\n    Hot path: `send`");
    let wrapped = ROUND.replace("Hot path: none", "Hot path:\nnone, tests only");
    for round in [quoted, tildes, indented, wrapped] {
        assert_eq!(
            check(&record(vec![old(&round)])),
            Vec::<String>::new(),
            "{round}"
        );
    }
}

#[test]
fn fails_a_round_whose_end_lines_are_only_quoted_in_its_text() {
    let quoted = ROUND.replace(
        "No bug, no lost coverage, no oracle weakening.\n\n\
         Deferred: none\nPublic surface: none\nHot path: none",
        "The breaker tried a round that ends with\n\n```\nDeferred: none\n\
         Public surface: none\nHot path: none\n```\n\nand one that names \
         `stream::Sender::send` as its hot path. None fails.",
    );
    assert_ne!(quoted, ROUND);
    assert_eq!(
        check(&record(vec![bot(&quoted)])),
        vec![unended("Deferred")]
    );
    let indented = ROUND.replace(
        "No bug, no lost coverage, no oracle weakening.\n\n\
         Deferred: none\nPublic surface: none\nHot path: none",
        "The breaker tried a round that ends with these lines:\n\n    \
         Deferred: none\n    Public surface: none\n    Hot path: none",
    );
    assert_ne!(indented, ROUND);
    assert_eq!(
        check(&record(vec![bot(&indented)])),
        vec![unended("Deferred")]
    );
    let fenced = ROUND.replace(
        "No bug, no lost coverage, no oracle weakening.\n\n\
         Deferred: none\nPublic surface: none\nHot path: none",
        "The breaker posted this round:\n\n```\nFindings: none\n\n\
         Deferred: none\nPublic surface: none\nHot path: none\n```",
    );
    assert_ne!(fenced, ROUND);
    assert_eq!(
        check(&record(vec![bot(&fenced)])),
        vec![unended("Deferred")]
    );
    let closed = ROUND.to_string() + "\n```\nHot path: `send`\n```";
    assert_eq!(
        check(&record(vec![bot(&closed)])),
        vec![unended("Deferred")]
    );
}

#[test]
fn fails_end_lines_in_a_code_block_that_is_not_closed() {
    for fence in ["```", "~~~", "````"] {
        let unclosed =
            ROUND.replace("weakening.\n\n", &format!("weakening.\n\n{fence}\n\n"));
        assert_ne!(unclosed, ROUND);
        assert_eq!(
            check(&record(vec![bot(&unclosed)])),
            vec![unended("Deferred")],
            "{fence}"
        );
    }
    let indented = ROUND.replace(
        "weakening.\n\nDeferred: none\nPublic surface: none\nHot path: none",
        "weakening.\n\n   ```\n\nDeferred: none\nPublic surface: none\n\
         Hot path: none\n   ```",
    );
    assert_ne!(indented, ROUND);
    assert_eq!(
        check(&record(vec![bot(&indented)])),
        vec![unended("Deferred")]
    );
    for indent in ["    ", "\t", "\u{a0}"] {
        let unclosed = ROUND.replace(
            "weakening.\n\n",
            &format!("weakening.\n\n```\n{indent}```\n\n"),
        );
        assert_eq!(
            check(&record(vec![bot(&unclosed)])),
            vec![unended("Deferred")],
            "{indent:?}"
        );
    }
    let short = ROUND.replace("weakening.\n\n", "weakening.\n\n````\n```\n\n");
    assert_eq!(check(&record(vec![bot(&short)])), vec![unended("Deferred")]);
    let other = ROUND.replace("weakening.\n\n", "weakening.\n\n```\n~~~\n\n");
    assert_eq!(check(&record(vec![bot(&other)])), vec![unended("Deferred")]);
    let info = ROUND.replace("weakening.\n\n", "weakening.\n\n```\n``` rust\n\n");
    assert_eq!(check(&record(vec![bot(&info)])), vec![unended("Deferred")]);
}

#[test]
fn reads_a_fence_with_four_spaces_as_text() {
    let shown = later("reviewer, breaker").replace(
        "Hot path: none",
        "Hot path: `send`\n\n```\n\n    ```\n\nDeferred: none\nPublic surface: none\n\
         Hot path: none",
    );
    assert_eq!(check(&record(vec![bot(&shown)])), vec![unended("Deferred")]);
    let quoted = ROUND.replace(
        "weakening.\n\nDeferred",
        "weakening.\n\n```\n\n    ```\n\nDeferred",
    );
    assert_eq!(
        check(&record(vec![bot(&quoted)])),
        vec![unended("Deferred")]
    );
    let hidden = ROUND.replace(
        "Hot path: none",
        "Hot path: none\n\n    ```\n\nHot path: `send`",
    );
    assert_eq!(
        check(&record(vec![old(&hidden)])),
        vec![
            "review round 3 names no performance, which this round requires."
                .to_string()
        ]
    );
}

#[test]
fn reads_a_tilde_fence_by_its_length_and_any_info() {
    let struck = ROUND.replace("No bug,", "~~a~~ b. No bug,");
    assert_ne!(struck, ROUND);
    assert_eq!(check(&record(vec![bot(&struck)])), Vec::<String>::new());
    let quoted = ROUND.replace(
        "weakening.\n\nDeferred",
        "weakening.\n\n~~~ `a`\n\nDeferred",
    );
    assert_eq!(
        check(&record(vec![bot(&quoted)])),
        vec![unended("Deferred")]
    );
}

#[test]
fn an_old_free_form_earlier_round_that_names_a_hot_path_needs_performance() {
    let free = old("## Review round 1\n\nConfirmed findings.\n\nHot path: `send`");
    assert_eq!(
        check(&record(vec![free, bot(ROUND)])),
        vec![
            "review round 1 names no performance, which this round requires."
                .to_string()
        ]
    );
    let quiet = old("## Review round 1\n\nConfirmed findings.\n\nHot path: none");
    assert_eq!(
        check(&record(vec![quiet, bot(ROUND)])),
        Vec::<String>::new()
    );
    let unnumbered = old("## Review round one\n\nHot path: `send`");
    assert_eq!(
        check(&record(vec![unnumbered, bot(ROUND)])),
        vec![
            "review round one names no performance, which this round requires."
                .to_string()
        ]
    );
}

#[test]
fn reads_an_indented_heading_and_indented_fields() {
    let indented = ROUND
        .replace("## Review", "  ## Review")
        .replace("\nReviewers:", "\n  Reviewers:")
        .replace("\nRange:", "\n  Range:")
        .replace("\nFindings:", "\n  Findings:");
    assert_ne!(indented, ROUND);
    assert_eq!(check(&record(vec![bot(&indented)])), Vec::<String>::new());
    assert_eq!(check(&record(vec![old(&indented)])), Vec::<String>::new());
}

#[test]
fn reads_inline_code_at_the_start_of_a_line_as_text() {
    let inline = ROUND.replace("No bug,", "```cargo xtask review``` passes. No bug,");
    assert_ne!(inline, ROUND);
    assert_eq!(check(&record(vec![bot(&inline)])), Vec::<String>::new());
    for indent in ["    ", "\t", "\u{a0}"] {
        let text =
            ROUND.replace("weakening.\n\n", &format!("weakening.\n\n{indent}```\n\n"));
        assert_eq!(
            check(&record(vec![bot(&text)])),
            Vec::<String>::new(),
            "{indent:?}"
        );
    }
    let fenced = ROUND.replace(
        "weakening.\n\n",
        "weakening.\n\n``` rust\nlet a = 1;\n```\n\n",
    );
    assert_ne!(fenced, ROUND);
    assert_eq!(check(&record(vec![bot(&fenced)])), Vec::<String>::new());
}

#[test]
fn fails_end_lines_that_are_not_a_paragraph_of_their_own() {
    let joined = ROUND.replace("weakening.\n\n", "weakening.\n");
    assert_ne!(joined, ROUND);
    assert_eq!(
        check(&record(vec![bot(&joined)])),
        vec![unended("Deferred")]
    );
    let alone = "## Review round 3\n\nDeferred: none\nPublic surface: none\n\
                 Hot path: none\nReviewers: reviewer\nRange: `38cba24f..c77c67d7`\n\
                 Findings: none";
    assert_eq!(check(&record(vec![bot(alone)])), vec![unended("Deferred")]);
}

#[test]
fn reads_an_end_line_that_wraps() {
    let wrapped = ROUND
        .replace(
            "Deferred: none",
            "Deferred: the trial, to #1467\n(https://github.com/a/b/issues/1467)",
        )
        .replace(
            "Hot path: none",
            "Hot path: none, the change\nis in tests only",
        );
    assert_eq!(check(&record(vec![bot(&wrapped)])), Vec::<String>::new());
    let next = later("reviewer, breaker").replace(
        "Hot path: none",
        "Hot path:\nnone, the change is in tests only",
    );
    assert_eq!(check(&record(vec![bot(&next)])), Vec::<String>::new());
    let block = ROUND.replace("weakening.", "weakening:\n\n```\nHot path: `send`\n```");
    assert_eq!(check(&record(vec![bot(&block)])), Vec::<String>::new());
    let earlier = ROUND.replace(
        "No bug",
        "Deferred: none\nPublic surface: none\nHot path: `send`\n\nNo bug",
    );
    assert_eq!(check(&record(vec![bot(&earlier)])), Vec::<String>::new());
}

#[test]
fn a_round_that_names_a_hot_path_needs_performance() {
    let hot = later("reviewer, breaker")
        .replace("Hot path: none", "Hot path: stream::Sender::send");
    assert_eq!(
        check(&record(vec![bot(&hot)])),
        vec![
            "review round 3 names no performance, which this round requires."
                .to_string()
        ]
    );
    let prefixed = later("reviewer, breaker").replace(
        "Hot path: none",
        "Hot path: nonempty_frames, once per frame",
    );
    assert_eq!(
        check(&record(vec![bot(&prefixed)])),
        check(&record(vec![bot(&hot)]))
    );
    let listed = later("reviewer, breaker").replace(
        "Hot path: none",
        "Hot path:\n- `stream::Sender::send`, once per frame",
    );
    assert_eq!(
        check(&record(vec![bot(&listed)])),
        vec![unended("Deferred")]
    );
    let called = later("reviewer, breaker")
        .replace("Hot path: none", "Hot path: `none()`, once per frame");
    assert_eq!(
        check(&record(vec![bot(&called)])),
        check(&record(vec![bot(&hot)]))
    );
    for value in [",none", "none..", "none;,", "`none`.."] {
        let punctuated = later("reviewer, breaker")
            .replace("Hot path: none", &format!("Hot path: {value}"));
        assert_eq!(
            check(&record(vec![bot(&punctuated)])),
            check(&record(vec![bot(&hot)])),
            "{value}"
        );
    }
    for value in ["none.", "none;", "`none`.", "`none`;"] {
        let quiet = later("reviewer, breaker")
            .replace("Hot path: none", &format!("Hot path: {value}"));
        assert_eq!(
            check(&record(vec![bot(&quiet)])),
            Vec::<String>::new(),
            "{value}"
        );
    }
    let quiet = later("reviewer, breaker")
        .replace("Hot path: none", "Hot path: `none`, tests only");
    assert_eq!(check(&record(vec![bot(&quiet)])), Vec::<String>::new());
    let measured = hot.replace("breaker\n", "breaker, performance\n");
    assert_eq!(check(&record(vec![bot(&measured)])), Vec::<String>::new());
    let cold = later("reviewer, breaker").replace(
        "Hot path: none",
        "Hot path: none, `send` runs once per stream",
    );
    assert_eq!(check(&record(vec![bot(&cold)])), Vec::<String>::new());
    let docs = ROUND.replace("Hot path: none", "Hot path: `Sender::send`");
    let mut record = record(vec![bot(&docs)]);
    record.files = vec!["docs/decisions/crate-map.md".to_string()];
    assert_eq!(
        check(&record),
        vec![
            "review round 3 names no performance, which this round requires."
                .to_string()
        ]
    );
}

#[test]
fn fails_an_earlier_round_with_fields_and_no_number() {
    for heading in ["## Review round 1.", "## Review round <n>"] {
        let first = bot(&format!("{heading}\n\nReviewers: reviewer"));
        assert_eq!(
            check(&record(vec![first, bot(ROUND)])),
            vec![format!("`{heading}` has no round number")]
        );
    }
}

#[test]
fn fails_an_earlier_round_in_the_fixed_format_that_does_not_parse() {
    let missing = |name| {
        format!(
            "review round 1 has no `{name}:` line. Write the round in the format of \
             .claude/skills/review/SKILL.md, \"Round comment\"."
        )
    };
    let cases = [
        ("Reviewers: reviewer", missing("Range")),
        ("Range: `a..b`", missing("Findings")),
        ("Findings: none", missing("Range")),
        (
            "Reviewers: reviewer\nRange: `a..b`\nFindings: 2 (R1, B1)",
            "review round 1 has `Findings: 2 (R1, B1)`, not a count or `none`: \
             invalid digit found in string"
                .to_string(),
        ),
    ];
    for (fields, problem) in cases {
        let first = bot(&format!("## Review round 1\n\n{fields}"));
        assert_eq!(check(&record(vec![first, bot(ROUND)])), vec![problem]);
    }
}

#[test]
fn reads_the_range_of_the_last_round_only() {
    let skipped = ROUND
        .replace(COMMENTS, "940140aa")
        .replace("round 3", "round 2");
    let last = later("reviewer, breaker");
    assert_eq!(
        check(&record(vec![bot(&skipped), bot(&last)])),
        Vec::<String>::new()
    );
}

#[test]
fn exits_by_the_result() {
    assert_eq!(exit(Ok(Vec::new())), ExitCode::SUCCESS);
    assert_eq!(exit(Ok(vec!["a".to_string()])), ExitCode::FAILURE);
    assert_eq!(exit(Err("gh: down".to_string())), ExitCode::from(2));
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
        ..bot(ROUND)
    };
    assert_eq!(
        check(&record(vec![pasted])),
        vec![format!(
            "no review round comment by {BOT}. Run `/review` and post each round in \
             the format of .claude/skills/review/SKILL.md, \"Round comment\"."
        )]
    );
}

#[test]
fn fails_a_range_that_ends_before_the_head() {
    let round = ROUND.replace("..c77c67d7", "..82ba5b72");
    assert_eq!(
        check(&record(vec![bot(&round)])),
        vec![format!(
            "review round 3 ends at 82ba5b72, not at the head {HEAD}. A commit after \
             the round needs a new round; only a clean merge of the base does not."
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
fn a_breaker_skip_on_a_range_that_changes_code_fails() {
    let code = ROUND.replace(COMMENTS, "940140aa");
    assert_eq!(
        check(&record(vec![bot(&code)])),
        vec![
            "review round 3 skips `breaker`, but its range changes code at `a.rs:2`."
                .to_string()
        ]
    );
    let named = code.replace("Reviewers: reviewer", "Reviewers: reviewer, breaker");
    assert_eq!(check(&record(vec![bot(&named)])), Vec::<String>::new());
    let first = code.replace("round 3", "round 1");
    assert_eq!(
        check(&record(vec![bot(&first)])),
        vec![
            "review round 1 names no architecture, breaker, which this round requires."
                .to_string()
        ]
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
    docs.files = vec![
        "docs/decisions/crate-map.md".to_string(),
        "README.md".to_string(),
    ];
    assert_eq!(check(&docs), Vec::<String>::new());
    for code in ["xtask/Cargo.toml", "Cargo.lock", "a/.rs"] {
        let mut record = record(vec![bot(&first)]);
        record.files = [&docs.files[..], &[code.to_string()]].concat();
        assert_eq!(
            check(&record),
            vec![
                "review round 1 names no architecture, breaker, which this round \
                 requires."
                    .to_string()
            ],
            "{code}"
        );
    }
}

#[test]
fn takes_the_last_round() {
    let later = ROUND.replace("Findings: none", "Findings: 3");
    assert_eq!(
        check(&record(vec![bot(ROUND), bot(&later)])),
        vec![
            "review round 3 has findings (3). Fix or answer them, then run another \
             round."
                .to_string()
        ]
    );
}

#[test]
fn each_earlier_round_names_the_reviewers_it_requires() {
    let first = later("reviewer")
        .replace("round 3", "round 1")
        .replace("Findings: none", "Findings: 2");
    assert_eq!(
        check(&record(vec![bot(&first), bot(&later("reviewer, breaker"))])),
        vec![
            "review round 1 names no architecture, breaker, which this round requires."
                .to_string()
        ]
    );
}

#[test]
fn reads_the_fields_only_from_the_block_after_the_heading() {
    let round =
        later("reviewer").replace("No bug", "Reviewers: reviewer, breaker\nNo bug");
    assert_eq!(
        check(&record(vec![bot(&round)])),
        vec!["review round 3 names no breaker, which this round requires.".to_string()]
    );
    let round = ROUND.replace("Range:", "\nRange:");
    assert_eq!(
        check(&record(vec![bot(&round)])),
        vec![
            "review round 3 has no `Range:` line. Write the round in the format of \
             .claude/skills/review/SKILL.md, \"Round comment\"."
                .to_string()
        ]
    );
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

fn red(comments: &[&str]) -> Record {
    let mut red = record(vec![bot(ROUND)]);
    red.branch = "red-team/buffer-open-read".to_string();
    red.labels = vec!["oracle".to_string()];
    red.comments.extend(comments.iter().map(|c| bot(c)));
    red
}

#[test]
fn a_red_team_oracle_pr_needs_the_director_approval_at_the_head() {
    let missing = format!(
        "a red-team `oracle` PR needs the director's verdict with the line \
         \"Director: approved at `<sha>`\" for the head {HEAD}."
    );
    assert_eq!(check(&red(&[])), vec![missing.clone()]);
    let earlier = "Quality: 8/10\nGood.\n\nDirector: approved at `82ba5b72`";
    assert_eq!(check(&red(&[earlier])), vec![missing.clone()]);
    let architect = "Quality: 8/10\nGood.\n\nApproved at `c77c67d7`";
    assert_eq!(check(&red(&[architect])), vec![missing.clone()]);
    let short = "Director: approved at `c77c67`";
    assert_eq!(check(&red(&[short])), vec![missing]);
    let head = "Quality: 8/10\nGood.\n\nDirector: approved at `c77c67d7`.";
    assert_eq!(check(&red(&[earlier, head])), Vec::<String>::new());
}

#[test]
fn takes_a_count_of_zero_as_no_findings() {
    let round = ROUND.replace("Findings: none", "Findings: 0");
    assert_eq!(check(&record(vec![bot(&round)])), Vec::<String>::new());
    let round = ROUND.replace("Findings: none", "Findings: few");
    assert_eq!(
        check(&record(vec![bot(&round)])),
        vec![
            "review round 3 has `Findings: few`, not a count or `none`: invalid digit \
             found in string"
                .to_string()
        ]
    );
}

#[test]
fn names_a_round_with_no_number() {
    assert_eq!(
        check(&record(vec![bot("## Review round one\nFindings: none")])),
        vec!["`## Review round one` has no round number".to_string()]
    );
}

#[test]
fn fails_a_last_round_in_an_older_format() {
    assert_eq!(
        check(&record(vec![
            bot(ROUND),
            bot("## Review round 4\n\nNo findings.")
        ])),
        vec![
            "review round 4 has no `Range:` line. Write the round in the format of \
             .claude/skills/review/SKILL.md, \"Round comment\"."
                .to_string()
        ]
    );
    assert_eq!(
        check(&record(vec![bot("## Review round one")])),
        vec!["`## Review round one` has no round number".to_string()]
    );
}

#[test]
fn names_the_pr_field_a_record_lacks() {
    use serde_json::json;
    let pull = [json!({ "head": { "ref": "a" }, "base": {}, "labels": [] })];
    assert_eq!(
        record_of(&pull, &[], &[]).unwrap_err(),
        "JSON has no string field `ref`"
    );
    let pull =
        [json!({ "head": { "ref": "a" }, "base": { "ref": "main" }, "labels": [] })];
    let comment = |created| {
        json!({
            "user": { "login": BOT },
            "body": ROUND,
            "created_at": created,
        })
    };
    let files = [json!({ "filename": "a.rs" })];
    let read = record_of(&pull, &files, &[comment(CUTOFF)]).unwrap();
    assert_eq!((read.branch.as_str(), read.base.as_str()), ("a", "main"));
    assert_eq!(read.files, ["a.rs"]);
    assert_eq!(read.comments[0].author, BOT);
    assert_eq!(read.comments[0].created, CUTOFF);
    for created in [
        "2026-10-08T03:00:00.5Z",
        "2026-10-08 03:00:00Z",
        "2026-1-08T03:00:00Z",
        "2026-10-08T03:00:0aZ",
        "2026-10-08T03:00:00+",
    ] {
        assert_eq!(
            record_of(&pull, &files, &[comment(created)]).unwrap_err(),
            format!(
                "`created_at` is `{created}`, not a UTC time `YYYY-MM-DDTHH:MM:SSZ`"
            )
        );
    }
}

#[test]
fn a_red_team_pr_with_no_oracle_label_needs_no_approval() {
    let mut red = red(&[]);
    red.labels.clear();
    assert_eq!(check(&red), Vec::<String>::new());
}

#[test]
fn returns_a_failure_to_read_history() {
    let failed = |_: &str| Err("git rev-parse: bad".to_string());
    assert_eq!(
        problems(&record(vec![bot(ROUND)]), HEAD, &failed, &|_, _| Ok(None)),
        Err("git rev-parse: bad".to_string())
    );
    let failed = |_: &str, _: &str| Err("git diff: bad".to_string());
    assert_eq!(
        problems(&record(vec![bot(ROUND)]), HEAD, &|_| Ok(true), &failed),
        Err("git diff: bad".to_string())
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

#[test]
fn a_fence_with_a_no_break_space_after_it_does_not_close() {
    let quoted = ROUND.replace("weakening.\n\n", "weakening.\n\n```\n```\u{a0}\n\n");
    assert_ne!(quoted, ROUND);
    assert_eq!(
        check(&record(vec![bot(&quoted)])),
        vec![unended("Deferred")]
    );
    let shown = later("reviewer, breaker").replace(
        "weakening.\n\n",
        "weakening.\n\nDeferred: none\nPublic surface: none\nHot path: `send`\n\n\
         ```\n```\u{a0}\n\n",
    );
    assert_ne!(shown, later("reviewer, breaker"));
    assert_eq!(check(&record(vec![bot(&shown)])), vec![unended("Deferred")]);
    let closed = ROUND.replace("weakening.\n\n", "weakening.\n\n```\n``` \t\n\n");
    assert_eq!(check(&record(vec![bot(&closed)])), Vec::<String>::new());
}

#[test]
fn an_old_malformed_round_that_names_performance_needs_no_performance() {
    let named = old(
        "## Review round 1\n\nReviewers: reviewer, architecture, breaker, performance\n\
         Range: a\nFindings: 2\n\nHot path: `send`",
    );
    assert_eq!(
        check(&record(vec![named, bot(ROUND)])),
        vec!["review round 1 has the range `a`, not `<from>..<head>`".to_string()]
    );
    let free = old(
        "## Review round 1\n\nConfirmed findings.\n\nReviewers: reviewer, \
         architecture, breaker, performance\n\nHot path: `send`",
    );
    assert_eq!(check(&record(vec![free, bot(ROUND)])), Vec::<String>::new());
}

#[test]
fn an_indented_reviewers_line_of_an_old_round_names_no_performance() {
    let indented = old("## Review round 1\n\nConfirmed findings.\n\n    \
         Reviewers: reviewer, performance\n\nHot path: `send`");
    assert_eq!(
        check(&record(vec![indented, bot(ROUND)])),
        vec![
            "review round 1 names no performance, which this round requires."
                .to_string()
        ]
    );
}

#[test]
fn a_lone_carriage_return_ends_a_line() {
    // "```\r\r\n" is a closing fence and a blank line, so the last fence hides the
    // last end lines, and the end lines GitHub shows name `send`.
    let hidden = later("reviewer, breaker").replace(
        "weakening.\n\n",
        "weakening.\n\n```\ncode\n```\r\r\n\
         Deferred: none\nPublic surface: none\nHot path: `send`\n\n```\n\n",
    );
    assert_ne!(hidden, later("reviewer, breaker"));
    assert_eq!(
        check(&record(vec![bot(&hidden)])),
        vec![unended("Deferred")]
    );
    let crlf = ROUND.replace('\n', "\r\n");
    assert_eq!(check(&record(vec![bot(&crlf)])), Vec::<String>::new());
    let cr = ROUND.replace('\n', "\r");
    assert_eq!(check(&record(vec![bot(&cr)])), Vec::<String>::new());
}

#[test]
fn a_parsed_old_round_reads_its_reviewers_field_at_any_indent() {
    let indented = ROUND
        .replace(
            "\nReviewers: reviewer\n",
            "\nRange: `38cba24f..c77c67d7`\n    Reviewers: reviewer, performance\n",
        )
        .replace("\nBreaker:", "\n    Breaker:")
        .replace(
            "\nRange: `38cba24f..c77c67d7`\nFindings:",
            "\n    Findings:",
        )
        .replace("Hot path: none", "Hot path: `send`");
    assert_ne!(indented, ROUND);
    assert_eq!(check(&record(vec![old(&indented)])), Vec::<String>::new());
    let code = ROUND
        .replace(
            "\nReviewers: reviewer\n",
            "\n    Reviewers: reviewer, performance\n",
        )
        .replace("\nBreaker:", "\n    Breaker:")
        .replace("\nRange:", "\n    Range:")
        .replace("\nFindings:", "\n    Findings:")
        .replace("Hot path: none", "Hot path: `send`");
    assert_eq!(
        check(&record(vec![old(&code)])),
        vec![
            "review round 3 has no `Range:` line. Write the round in the format of \
             .claude/skills/review/SKILL.md, \"Round comment\"."
                .to_string(),
            "review round 3 names no performance, which this round requires."
                .to_string()
        ]
    );
}

#[test]
fn a_parsed_old_round_reads_performance_only_from_its_reviewers_field() {
    let quoted = ROUND
        .replace(
            "weakening.\n\n",
            "weakening.\n\nReviewers: reviewer, performance\n\n",
        )
        .replace("Hot path: none", "Hot path: `send`");
    assert_ne!(quoted, ROUND);
    assert_eq!(
        check(&record(vec![old(&quoted)])),
        vec![
            "review round 3 names no performance, which this round requires."
                .to_string()
        ]
    );
}

#[test]
fn an_old_round_reads_no_line_of_an_indented_code_block() {
    let malformed = |reviewers: &str, hot: &str| {
        old(&format!(
            "## Review round 1\n\nConfirmed findings.\n\n{reviewers}Reviewers: \
             reviewer, performance\n\n{hot}Hot path: `send`"
        ))
    };
    let unnamed = vec![
        "review round 1 names no performance, which this round requires.".to_string(),
    ];
    let cases = [
        ("   ", "", Vec::new()),
        ("    ", "", unnamed.clone()),
        ("\t", "", unnamed.clone()),
        ("    ", "   ", unnamed),
        ("    ", "    ", Vec::new()),
        ("    ", "\t", Vec::new()),
    ];
    for (reviewers, hot, problems) in cases {
        let comment = malformed(reviewers, hot);
        assert_eq!(
            check(&record(vec![comment, bot(ROUND)])),
            problems,
            "{reviewers:?} {hot:?}"
        );
    }
}

#[test]
fn a_lone_carriage_return_ends_a_line_of_an_approval() {
    let head = "Quality: 8/10\rGood.\r\rDirector: approved at `c77c67d7`.";
    assert_eq!(check(&red(&[head])), Vec::<String>::new());
}

#[test]
fn fails_a_round_with_raw_html() {
    let cases = [
        ("<div>\n```\n</div>", "<div>"),
        ("<!--\nHot path: none\n-->", "<!--"),
        ("<div><!--", "<div><!--"),
        ("<source\n***", "<source"),
        ("- a\n  <source", "<source"),
        ("A <details> b", "A <details> b"),
        ("A `b`\nc <!-- d --> e", "c <!-- d --> e"),
        ("</x>", "</x>"),
        ("<?a ?>", "<?a ?>"),
        ("<source\n---", "<source"),
        ("<source\n===", "<source"),
        ("a\n<source\n---", "<source"),
        ("<source | b\n--- | ---\nc | d", "<source | b"),
        ("> 1. <source", "> 1. <source"),
        ("- <source", "- <source"),
        ("- ```\n  a\n  ```\n  <source", "<source"),
        ("| a |\n| - |\n<source", "<source"),
        ("</source", "</source"),
        ("<! a", "<! a"),
        ("+ <source", "+ <source"),
        ("* <source", "* <source"),
        ("1) <source", "1) <source"),
        ("-\t<source", "-\t<source"),
        ("[^1]: <source", "[^1]: <source"),
        ("> [^a]: <source", "> [^a]: <source"),
        ("[^a\\\\]: <source", "[^a\\\\]: <source"),
        ("- [^a]:<source", "- [^a]:<source"),
    ];
    for (html, line) in cases {
        let comment =
            ROUND.replace("weakening.\n\n", &format!("weakening.\n\n{html}\n\n"));
        assert_ne!(comment, ROUND);
        assert_eq!(
            check(&record(vec![bot(&comment)])),
            vec![raw(line)],
            "{html}"
        );
    }
    let summary = format!("<b>\n\n{ROUND}");
    assert_eq!(check(&record(vec![bot(&summary)])), vec![raw("<b>")]);
    let hidden = ROUND.replace("weakening.\n\n", "weakening.\n\n<source\n---\n");
    assert_eq!(check(&record(vec![bot(&hidden)])), vec![raw("<source")]);
    let noted = ROUND.replace("weakening.\n\n", "weakening.[^1]\n\n[^1]: <source\n\n");
    assert_eq!(
        check(&record(vec![bot(&noted)])),
        vec![raw("[^1]: <source")]
    );
}

#[test]
fn fails_raw_html_before_the_fields_or_the_heading() {
    let unranged = ROUND
        .replace("Range: `38cba24f..c77c67d7`\n", "")
        .replace("weakening.\n\n", "weakening.\n\n<div>\n\n");
    assert_eq!(check(&record(vec![bot(&unranged)])), vec![raw("<div>")]);
    let hidden = ROUND.replace("\nReviewers:", "\n<details>\n\nReviewers:");
    assert_eq!(check(&record(vec![bot(&hidden)])), vec![raw("<details>")]);
    for heading in [
        "<search\n## Review round 3",
        "<search\n   ## Review round 3",
        "<search\n## Review round 3\n## Review round 4",
        "<search\r## Review round 3",
        "<search\r## Review round 3\r## Review round 4",
    ] {
        let headless = ROUND.replace("## Review round 3", heading);
        assert_eq!(
            check(&record(vec![bot(&headless)])),
            vec![raw("<search")],
            "{heading}"
        );
    }
}

#[test]
fn passes_a_round_whose_text_github_shows_as_text() {
    let shown = [
        "<https://github.com>",
        "\\<div>",
        "`Vec<u8>`",
        "```\n<div>\n```",
        "    <div>",
        "a < b, <1",
        "# <source",
        "a | <source\n--- | ---",
        "*a*<source",
        "*<source",
        "1.<source",
        "a <b",
        "[^]: <source",
        "[^a]b]: <source",
        "[^a\\]b]: <source",
        "[^a b]: <source",
        "[^a\tb]: <source",
        "[^a] <source",
        "[^a]<source",
    ];
    for text in shown {
        let comment =
            ROUND.replace("weakening.\n\n", &format!("weakening.\n\n{text}\n\n"));
        assert_ne!(comment, ROUND);
        assert_eq!(
            check(&record(vec![bot(&comment)])),
            Vec::<String>::new(),
            "{text}"
        );
    }
    let old = old("## Review round 1\n\nNo fields.\n\n<div>");
    assert_eq!(check(&record(vec![old, bot(ROUND)])), Vec::<String>::new());
}

/// The problem of a round 3 with a footnote label that only GitHub reads in `line`.
fn label(line: &str) -> String {
    format!(
        "review round 3 has a footnote label that GitHub reads and pulldown-cmark does \
         not, which can hide text on GitHub, in the line `{line}`. Put the line in a \
         code span, in the format of .claude/skills/review/SKILL.md, \"Round comment\"."
    )
}

#[test]
fn fails_a_round_whose_fields_github_reads_as_a_footnote() {
    let hidden = ROUND.replace("\nReviewers:", "\n[^a\\]: x\nReviewers:");
    assert_ne!(hidden, ROUND);
    assert_eq!(check(&record(vec![bot(&hidden)])), vec![label("[^a\\]: x")]);
}

#[test]
fn fails_a_round_with_a_footnote_label_that_only_github_reads() {
    let cases = [
        ("[^a\\]: x", "[^a\\]: x"),
        ("[^\\]: x", "[^\\]: x"),
        ("[^`\\]: <source x`", "[^`\\]: <source x`"),
        ("[^a\\]: <source", "[^a\\]: <source"),
        ("[^\\]: <source", "[^\\]: <source"),
        ("[^a[b]: <source", "[^a[b]: <source"),
        ("[^a[b]: x", "[^a[b]: x"),
        ("> [^a\\]: x", "> [^a\\]: x"),
        ("- [^a\\]: x", "- [^a\\]: x"),
        ("[^1]: [^a\\]: x", "[^1]: [^a\\]: x"),
        ("a\n[^a\\]: x", "[^a\\]: x"),
        ("[^a\\]: x](https://x.y)", "[^a\\]: x](https://x.y)"),
    ];
    for (text, line) in cases {
        let comment =
            ROUND.replace("weakening.\n\n", &format!("weakening.\n\n{text}\n\n"));
        assert_ne!(comment, ROUND);
        assert_eq!(
            check(&record(vec![bot(&comment)])),
            vec![label(line)],
            "{text}"
        );
    }
    let old = old("## Review round 1\n\nNo fields.\n\n[^a\\]: x");
    assert_eq!(check(&record(vec![old, bot(ROUND)])), Vec::<String>::new());
}

#[test]
fn passes_a_footnote_label_that_both_parsers_read() {
    for text in [
        "[^1]: x",
        "> [^a]: x",
        "[^a]: x\n[^b]: y",
        "[^a\\\\]:x",
        "`[^a\\]: x`",
    ] {
        let comment =
            ROUND.replace("weakening.\n\n", &format!("weakening.\n\n{text}\n\n"));
        assert_ne!(comment, ROUND);
        assert_eq!(
            check(&record(vec![bot(&comment)])),
            Vec::<String>::new(),
            "{text}"
        );
    }
}

#[test]
fn fails_end_lines_that_a_fence_after_a_list_item_hides() {
    let hidden = later("reviewer, breaker").replace(
        "weakening.\n\n",
        "weakening.\n\nDeferred: none\nPublic surface: none\nHot path: `send`\n\n\
         - ```\n  x\n  ```\n\n```\n\n",
    );
    assert_ne!(hidden, later("reviewer, breaker"));
    assert_eq!(
        check(&record(vec![bot(&hidden)])),
        vec![unended("Deferred")]
    );
}

#[test]
fn an_old_round_reads_each_line_of_text_that_github_shows() {
    let unnamed = vec![
        "review round 1 names no performance, which this round requires.".to_string(),
    ];
    let shown = [
        "Confirmed findings.\n    {line}",
        "Confirmed findings.\n\t{line}",
        "- a\n  - b\n\n    {line}",
        "Confirmed[^1].\n\n[^1]: a\n    {line}",
        "> Confirmed.\n> {line}",
        "- {line}",
        "- [ ] {line}",
    ];
    for text in shown {
        let hot = text.replace("{line}", "Hot path: `send`");
        let comment = old(&format!("## Review round 1\n\nNo fields.\n\n{hot}"));
        assert_eq!(check(&record(vec![comment, bot(ROUND)])), unnamed, "{hot}");
        let named = text.replace("{line}", "Reviewers: reviewer, performance");
        let comment = old(&format!(
            "## Review round 1\n\nNo fields.\n\n{named}\n\nHot path: `send`"
        ));
        assert_eq!(
            check(&record(vec![comment, bot(ROUND)])),
            Vec::<String>::new(),
            "{named}"
        );
    }
}

#[test]
fn an_old_round_reads_no_line_of_a_fence_in_a_nested_list_item() {
    let fenced = |line: &str| {
        old(&format!(
            "## Review round 1\n\n- a\n  - b\n\n    ```\n    {line}\n    ```"
        ))
    };
    assert_eq!(
        check(&record(vec![fenced("Hot path: `send`"), bot(ROUND)])),
        Vec::<String>::new()
    );
    let named =
        fenced("Reviewers: reviewer, performance").body + "\n\nHot path: `send`";
    assert_eq!(
        check(&record(vec![old(&named), bot(ROUND)])),
        vec![
            "review round 1 names no performance, which this round requires."
                .to_string()
        ]
    );
}

#[test]
fn a_fence_with_a_tab_after_it_closes() {
    let hidden = later("reviewer, breaker").replace(
        "weakening.\n\n",
        "weakening.\n\n```\n```\t\n\nDeferred: none\nPublic surface: none\n\
         Hot path: `send`\n\n```\n\n",
    );
    assert_ne!(hidden, later("reviewer, breaker"));
    assert_eq!(
        check(&record(vec![bot(&hidden)])),
        vec![unended("Deferred")]
    );
}

#[test]
fn reads_the_blocks_of_the_extensions_of_github() {
    let table =
        old("## Review round 1\n\nNo fields.\n\nHot path: `send` | a\n--- | ---");
    assert_eq!(
        check(&record(vec![table, bot(ROUND)])),
        Vec::<String>::new()
    );
    let footnote = ROUND.to_string() + "\n\n[^1]: Hot path: `send`";
    assert_eq!(
        check(&record(vec![bot(&footnote)])),
        vec![unended("Deferred")]
    );
    let rule = ROUND.to_string() + "\n\n***";
    assert_eq!(check(&record(vec![bot(&rule)])), vec![unended("Deferred")]);
}

#[test]
fn reads_inline_markup_as_part_of_its_line() {
    let named = format!("**laptop.integrator-2** · author\n{ROUND}")
        .replace("Deferred: none", "Deferred: *none*");
    assert_eq!(check(&record(vec![bot(&named)])), Vec::<String>::new());
    for markup in ["*a*", "**a**", "~~a~~", "[a](b)", "![a](b)"] {
        let glued =
            format!("## Review round 1\n\nNo fields.\n\n{markup}Hot path: `send`");
        assert_eq!(
            check(&record(vec![old(&glued), bot(ROUND)])),
            Vec::<String>::new(),
            "{markup}"
        );
    }
}

#[test]
fn reads_a_line_after_a_break_or_a_block_as_its_own() {
    let broken = ROUND.replace("Deferred: none\n", "Deferred: none\\\n");
    assert_ne!(broken, ROUND);
    assert_eq!(check(&record(vec![bot(&broken)])), Vec::<String>::new());
    for item in [
        "- a\n  ***\n  Hot path: `send`",
        "- a\n  - Hot path: `send`",
    ] {
        let comment = old(&format!("## Review round 1\n\nNo fields.\n\n{item}"));
        assert_eq!(
            check(&record(vec![comment, bot(ROUND)])),
            vec!["review round 1 names no performance, which this round requires."],
            "{item}"
        );
    }
}

#[test]
fn reads_no_fields_in_a_list() {
    let listed = ROUND.replace("\nReviewers:", "\n- x\n  Reviewers:");
    assert_ne!(listed, ROUND);
    assert_eq!(
        check(&record(vec![bot(&listed)])),
        vec![
            "review round 3 has no `Range:` line. Write the round in the format of \
             .claude/skills/review/SKILL.md, \"Round comment\"."
        ]
    );
}

#[test]
fn reads_only_a_top_level_round_heading() {
    for quoted in [
        "> ## Review round 2",
        "- ## Review round 2",
        "    ## Review round 2",
        "```\n## Review round 2\n```",
        "<search\n    ## Review round 2",
        "- <search\n  ## Review round 2",
        "<!-- a -->\n\n```\n## Review round 2\n```",
        "<b>a</b>\n\n~~~\n## Review round 2\n~~~",
        "So:\n\n```\n## Review round <n>\n\nReviewers: reviewer\n```\n\nA Vec<u8>.",
        "See:\n\n```\n## Review round 4\n```\n\n<details>\n\nx",
    ] {
        let comment = bot(&format!("{quoted}\n\nNo fields."));
        assert_eq!(
            check(&record(vec![comment, bot(ROUND)])),
            Vec::<String>::new(),
            "{quoted}"
        );
    }
    let ruled = bot(&format!("a\n\n***\n\n{ROUND}"));
    assert_eq!(check(&record(vec![ruled])), Vec::<String>::new());
    let before = old("Hot path: `send`\n\n## Review round 1\n\nNo fields.");
    assert_eq!(
        check(&record(vec![before, bot(ROUND)])),
        Vec::<String>::new()
    );
}

#[test]
fn reads_an_old_round_with_html_as_before() {
    let fields =
        "Reviewers: reviewer, architecture, breaker\nRange: `a..b`\nFindings: 2";
    let html = old(&format!("## Review round 1\n\n{fields}\n\n<div>"));
    let hidden = old("<search\n## Review round 1\n\nHot path: `send`");
    for before in [html, hidden] {
        assert_eq!(
            check(&record(vec![before, bot(ROUND)])),
            Vec::<String>::new()
        );
    }
}

#[test]
fn fails_end_lines_in_a_quote_or_a_list() {
    let end = "Deferred: none\nPublic surface: none\nHot path: none";
    for nested in [
        "> Deferred: none\n> Public surface: none\n> Hot path: none",
        "- Deferred: none\n  Public surface: none\n  Hot path: none",
    ] {
        let comment = ROUND.replace(end, nested);
        assert_ne!(comment, ROUND);
        assert_eq!(
            check(&record(vec![bot(&comment)])),
            vec![unended("Deferred")],
            "{nested}"
        );
    }
}

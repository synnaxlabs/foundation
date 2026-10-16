---
name: review
description:
  Adversarial review of a Foundation pull request by fresh reviewer agents, by tier, in
  at most 3 rounds. Use on each PR before it merges (a builder's, a red-team, a record,
  or a rule PR), when asked to review a PR, or after fix commits. Argument: the PR
  number.
---

# Review

Run each reviewer in a fresh subagent with the Agent tool. Give it only the PR number,
the decisions section the PR builds, and its job. Never give it your reasoning. Keep its
findings, not its file reads. The author never does a reviewer's work: when a reviewer
cannot start, another session runs it, and the round comment names that session.

Before each round, read this skill as it is on `main` (`git fetch origin main`, then
`git show origin/main:.claude/skills/review/SKILL.md`), and follow that text. The copy
on a PR branch can be older, and a rule in an open PR does not apply yet.

## Round cap

A PR gets at most 3 review rounds, counted by the number in the round heading. Round 1
is the full review: each reviewer reports every finding that it can show, in one report,
never a few at a time. Rounds 2 and 3 review only the fix commits, other than the first
round of a PR that replaces another. The author fixes every finding of a round in one
batch of commits, with each merge of `main` that the PR needs, and only then asks for
the next round.

Two round comments take the number of the round comment before them and do not count
toward the cap: one that only holds a report that came after its round comment
("Findings" step 2), and one that only checks a merge of `main` that is not clean, or a
merge and the commits that fix what it broke ("Second round"). The range of the second
starts at the first parent of the merge. Each also runs after round 3. Its findings are
findings of the round whose number it takes, and follow the rules of that round: "Done"
reads the findings of each round comment with the number of the last round.

The end of a round is the end of the range of its first round comment. The range of the
next round and the range after round 3 start there, so no late report or merge check
moves them. Each deferral, and each item that this section adds after round 3, goes on
the last round comment.

A design flaw is a finding whose fix changes a public surface, a crate boundary, a
decision, or the owner of state. The design approver of an item is the crate's
architect, `laptop.architect` for a contract between the two lists (`docs/factory.md`),
or `laptop.director` for any other item. When the crate's architect, or
`laptop.architect` for a contract between the two lists, wrote a commit of the PR, it is
the other architect (Round 1). When `laptop.director` wrote the PR, `laptop.monitor` is
the design approver in place of `laptop.director`.

A PR that replaces another before round 3 continues its count: its first round reviews
its whole diff with each reviewer of the table (Round 1). After round 3, no PR replaces
it ("Done"), other than a new design that its design approver approves, which starts at
round 1.

After round 3, no other round runs. Each round comment numbered 3 gives each finding the
mark of its reviewer: `defect` (wrong behavior, data loss, a crash, or an oracle
weakening), `design flaw`, or no mark. The author fixes each finding marked `defect` in
this PR, with a test that fails without the fix, and CI checks it. A defect is never
deferred. Each other finding of round 3, other than a missing input that
`docs/claude/testing.md`, "Fuzzing", asks for, is a deferral ("Findings" step 3) on the
last round comment (above). The deferral of a design flaw needs the explicit OK of its
design approver. Without that OK, the author closes the PR, and the new design starts as
a new PR at round 1. A missing approval, OK, or link that "Done" asks for is no finding
to defer.

The range after round 3 holds only these:
- merges of `main`, and the commits that fix what a merge broke (a conflict, or code, a
  build, or a test of the PR that the merge makes fail; a rule that the merge brings in
  and that only a review checks breaks nothing, so a finding against it is a finding of
  round 3);
- the fix of each defect of round 3, with its test;
- each input that `docs/claude/testing.md`, "Fuzzing", asks for, in a commit that
  changes nothing else, with the arm that it reaches and how the author checked it in
  the PR body;
- the `Text fixes:` items of round 3, applied as "Second round" step 2 says;
- each change that an approval or OK of "Done" asks for. The PR links the comment that
  asks for it.

When the range holds a commit other than a clean merge of `main` or a `Text fixes:` item
of round 3, the author runs `architecture` on that whole range at the head, after the
last such commit, and links its report on the PR. A defect that the run finds gets its
fix and test as a defect of round 3 does, then a new run. Each other finding is a
deferral, as in round 3.

Each item of the run's `Public surface:` line goes on the `Public surface:` line of the
last round comment. So does each decision or public doc that states what a crate does
and that the range changes. Each of these items gets the approval of its design approver
and each approval that Round 1 asks for. Each function of the run's `Hot path:` line
goes on the `Hot path:` line of that comment, with `performance` on its `Reviewers:`
line and the numbers in the Performance section.

A PR whose last round was round 3 or later when this rule merged runs no new round, and
its last round counts as round 3. The author has a `reviewer` mark each finding of that
round, and edits that round comment with the marks. That run is no round. The author
reverts each change after that round that the limit above does not allow, so that the
range after that round meets it.

From #2585 (REVIEW CHECK), `cargo xtask review` checks these items:
- that no round number goes past 3;
- the reviewers of each round comment;
- the findings of the last round below round 3, across its round comments;
- the end of the last round comment;
- the OK links of deferrals in a risk crate, and the approvals of the `Public surface:`
  line;
- after round 3, an item on the `Public surface:` line for each change to a decision.

The check does not check these items:
- the marks;
- the other changes in the range after round 3;
- what a round comment with the number of the one before it holds;
- that a PR after round 3 is a new design;
- the OK of a deferred design flaw outside a risk crate.

Until #2585 merges, the check fails a PR whose last round comment has a finding or ends
before a commit that changes code, also after round 3. On a code PR, it asks each round
comment numbered 1 for each reviewer of the table. So a late report or a merge check
numbered 1 fails, unless it also names `architecture` and `breaker`. It does not ask the
first round comment of a replacement numbered 2 or 3 for `architecture`. It also passes
a last round comment that finds nothing after a round comment of the same number that
has findings.

## Round 1

Launch in parallel every reviewer the PR needs:

| The PR | Reviewers |
| --- | --- |
| Every PR | `reviewer` |
| A code PR: it changes a `.rs` file, a `Cargo.toml`, or `Cargo.lock` | add `architecture` and `breaker` |
| A hot path: it changes code that runs once per sample, series, frame, or data message, or a public item that its caller on record calls at that rate, or code in a crate's `src/` that exists only for tests and benchmarks and that a benchmark runs in its timed loop (such as a counting `GlobalAlloc`), whatever its body says. A change to a bench file alone is not one. A control message whose rate does not grow with the data (raft, membership) is not one. The `Hot path:` line of the `architecture` report decides | add `performance`. It must report measured numbers for `main` and the PR, with the machine, and, for code that exists only for tests and benchmarks, for each benchmark that runs it in its timed loop. For a stub, it states what the surface makes each message and each part cost, read from the code under it, with no numbers. Run it again if it does not |
| A flagged oracle weakening | add one `reviewer` per weakening, told to argue for fixing the code instead |
| A change to a file under `patches/` | give `reviewer` and `breaker` LOCAL PATCHES in `docs/decisions/releases/local-patches.md` and "Local patches" in `docs/dependencies.md` |

When the `reviewer` report of a round 1 with no `architecture` agent names an item on
its `Build or use:` line, also run `architecture` on the PR.

When the PR changes a public surface or a crate's dependencies, also send its link to
the crate's architect (`docs/factory.md`), which reviews it before the person. A public
surface change includes a change to what a public item accepts, returns, or states in
its doc, and any change from a surface or text that the architect approved. A `pub` item
of a crate that is not in the crate map (`fuzz`, `xtask`, `bench/*`) is not a public
surface: no crate depends on it. Its doc, when it states what a crate does, still
follows the next sentence. When the PR adds or changes a decision or a public doc that
states what a crate does, and that crate, each other crate that the text names, the
crate whose public doc holds it, and the crates of the crate map that the PR changes are
not all on one architect's list (a folder of `docs/decisions/` is a topic, not a crate),
also send it to `laptop.architect`, which owns each contract between the two lists
(`docs/factory.md`), and link its approval. When the PR changes a file in `docs/claude/`
or another rule that `laptop.director` owns (`/direct`, "The bar"), and
`laptop.director` is not its author, also send it to `laptop.director`, and link its
approval on the `Public surface:` line. When the crate's architect, or
`laptop.architect` for a contract between the two lists, wrote a commit of the PR, the
other architect gives each approval, OK, and ruling that this skill asks of it.

When the PR makes a key choice (`CLAUDE.md`, "The person decides the design"), tell
`reviewer` to list each one. The author sends `laptop.monitor` each one that it has not
sent yet. The list is not a finding, and it does not hold the PR.

A PR that adds or changes code for an OS that CI does not run (LINUX CI in
`docs/decisions/testing/linux-ci.md`), or states what its code does on that OS, links a
run of `cargo test -p <crate>` on that OS. A PR that adds or changes a `build.rs` that
compiles C or C++ links a run of `cargo build -p <crate>` on macOS with each feature
that compiles it. Each run names its machine and a commit after which no commit changes
that code. When the author has no such machine, a session on the laptop runs it on
macOS. For another OS, the author asks `laptop.monitor` for a cloud machine (#15).
When a PR adds or changes a `.cargo/mutants.toml` entry that names a test on such an OS
as the kill of its mutants, or changes a function that such an entry covers, the run
also tests one mutant made by hand in each function that the entry covers, and the PR
links the run in which each one fails.

When a new PR replaces one under review, close the old one first (`gh pr close <old>
--comment "Replaced by #<new>"`), and link its round comments in the first round comment
of the new PR (Round cap). When the head of the old PR is an ancestor of the head of the
new one, each answer and approval of the old PR counts for the new PR at its SHA. A
close that waits for the person does not hold the new PR while the old PR is a draft and
gets no push.

The `reviewer`, `architecture`, and `breaker` agents each make their own worktree, so
no two share a copy. Never give one another path: its permission check refuses every
command outside that worktree. Remove each worktree when its agent returns
(`git worktree remove --force <path>`).

## Findings

1. Check each finding against the code yourself. Drop the ones you cannot confirm, and
   say so.
2. Post one PR comment for each round, also a round that finds nothing, after each of
   its reviewers returns. It starts with its name line, then the rating and summary from
   the `reviewer`'s report, as given (Rating). Then its reviewers, its range
   (`<from>..<head sha>`), and the confirmed findings, most severe first: file and line,
   what goes wrong, the fix, and in round 3 the mark of its reviewer (Round cap). It
   links each test doc that the `reviewer` report names as the reason for a hand mutant
   (`docs/claude/testing.md`). It copies the `Revert:` line of the `reviewer` report
   after the findings, then the `Build or use:` line of the `architecture` report, or of
   the `reviewer` report in a round with no `architecture` agent. It ends with four
   lines. First `Deferred:` and `none`, or the issue of each deferred finding, each with
   the link to the architect's OK in a risk crate, and to the OK of its design approver
   for a design flaw (Round cap), once it exists. In a PR that changes a risk crate, the
   issue of a deferral that is not in a risk crate has the words `not in a risk crate`
   after it. A later round keeps each deferral of the round before it. Then
   `Later steps:` and `none`, or each later step that a round, an architect review, an
   architect's ruling, or an issue that the PR closes names, each with the link to the
   open issue or the decisions entry that holds it ("Done"). Then `Public surface:` and
   `none`, or each item that `.claude/agents/architecture.md` defines for that line (the
   `architecture` report names them, and in a round with no `architecture` agent the
   `reviewer` report names those of its range) and each rule change that Round 1 sends
   to `laptop.director`, each with the link to its approval once it exists. An approval
   holds for the text at the SHA that it approves: after a commit or an edit of the PR
   body changes the item, the line gives it `approval owed` in place of the link until a
   new approval exists. A later round keeps each item of the round before it. Then
   `Hot path:` as the `architecture` report gives it, or, in a round with no
   `architecture` agent, the `reviewer` report. A report that comes after its round
   comment goes in a new round comment with the number of the round comment before it
   (Round cap), with its findings. Its range is `<head>..<head>`, so it also runs
   `reviewer` and skips `breaker` (Round comment). The old round adds the reviewer to
   its `Reviewers:` line, and links the new comment after its findings.
3. Fix each finding in this PR, or answer it on the PR. A deferral is an issue that
   states the item, linked in the answer, also when the code is already on `main` or
   another crate does the work. Before round 3 (Round cap), a finding whose fix is a
   small change in a crate or a file that this PR changes is fixed in this PR. Another
   small one follows `docs/coordination.md`, "Small changes". Another finding in code
   that the PR does not change, of a defect that the PR does not make and that no issue
   that it closes states, is a deferral: its issue holds the failing test. Search the
   open issues for the item of a deferral before it files a new issue:
   `gh issue list --state open --search '<function or file>'`. A deferral never goes to
   an issue that an open PR closes (`gh issue view <n> --json
   closedByPullRequestsReferences`): file a new issue. A deferral to an existing
   issue is a comment on that issue that names the item and links the round comment. A
   deferral in a risk crate (`raft`, `buffer`, `delivery`, `block`, `ring`, `codec`,
   `wire`, `home`, `replica`, `transport`) needs the explicit OK of the architect of the
   crate where the deferred work lands: link its comment. A fix or an answer that makes
   such a public surface change, or decides what a ruling means, needs the architect's
   approval too: link its comment. So does a fix that reverses a finding of the
   architect. So does an answer that accepts a slowdown at or over the cost limit of P1.
   An answer to a run that cannot show the cost check links the comment with the numbers
   of the rerun of BENCH BASELINES (`docs/decisions/testing/bench-baselines.md`), never
   a queued run, does what an amendment of that record asks in place of the rerun, or
   names the amendment of that record that lets its PR merge with no rerun. An answer
   that leaves a rule of `CLAUDE.md` or `docs/claude/` unmet, and a dispute about what
   such a rule means, go to `laptop.director`: link its ruling. A refusal that names a
   trigger for later work is a deferral: file its issue with the trigger, or write the
   trigger in the decisions entry that the ruling cites. So is an answer that a later PR
   does the work, also a later PR of the same issue. When the trigger is the work of
   another open issue or PR, also comment the deferral issue and its trigger on that
   issue or PR.

## Rating

Each round comment, architect review, director verdict, and red-team approval starts
with its name line (`docs/factory.md`, "GitHub is the record"), then two lines for a
person who has not read the code:

1. `Quality: <n>/10` for the whole PR at its head. 10: nothing to improve. 8: small
   fixes only. 5: it works, with real problems in tests, design, or performance. 3: a
   defect or a missing test on a failure path. 1: the wrong design.
2. A summary of its code quality in 2 or 3 short sentences.

A reviewer that wrote no commit of the PR gives both. A session never rates a PR that
holds a commit that it wrote.

## Round comment

The required check `review` (`cargo xtask review`) reads only comments by
`synnax-foundation-factory[bot]`, and parses this text. Write each round comment so:

```
**<session>** · <role>
Quality: <n>/10
<summary>

## Review round <n>

Reviewers: reviewer, architecture, breaker
Range: `<from>..<head sha>`
Findings: <count, or none>

<the findings, most severe first; in round 3, each with its mark: `defect`,
`design flaw`, or no mark>

Revert: <the `Revert:` line of the `reviewer` report>
Build or use: <the `Build or use:` line of the `architecture` report, or of the
`reviewer` report>

Text fixes:
<each item and its exact new text, or a Low item's place and problem; leave out this
line and its items when none>

Deferred: <none, or each issue>
Later steps: <none, or each step and its issue>
Public surface: <none, or each item and its approval>
Hot path: <none, or each function>
```

The end lines, `Deferred:` to `Hot path:`, are the last paragraph of the comment, in
the order of the template. Each is a plain line, not a list item, with no blank line
between them. A value may wrap onto the next line.

Each of the `Reviewers:`, `Range:`, and `Findings:` lines holds its value alone:
`Findings: 2`, never `Findings: 2, each fixed in <sha>`. The check fails on the second.
`Reviewers:` names the reviewers that ran (Round 1, Second round). A later round that
skips `breaker` adds this line under its `Reviewers:` line, with no blank line between,
because the check reads the round's fields only from the first block of lines after the
round heading:

```
Breaker: skipped, the check counts no code change in the range
```

When the last round has that line, the check fails if its range changes code: a change
to a file that is not a `.md` file, other than a `.rs` line that, trimmed, is empty or
starts with `//`. The rest is text. The base's code does not count, but a conflict in a
code file that `git merge-tree` finds does. REVIEW CHECK in
`docs/decisions/operations/review-check.md` states each case, and the check names the
one it finds. A head that is the range end plus clean merges of the base and commits
that only apply the `Text fixes:` items of that round needs no new round ("Second
round" step 2). A red-team PR labeled `oracle` also needs the director's verdict with
the line ``Director: approved at `<sha>` `` at the head.

## Second round

In 4 of the 5 worst escaped defects, the defect came in through a fix or a deferral that
nothing checked again. So when round 1 led to fix commits:

1. Run `reviewer` and `breaker` on the fix commits only (`<from>..<head sha>`, where
   `<from>` is the end of the round before (Round cap), or the first parent of the merge
   for a merge check), with the round 1 comment and each architect review attached. Add
   `architecture` when the range adds, changes, or removes a file under
   `docs/decisions/`, or, in a `.rs` file under `crates/*/src/`, a public item, a
   variant of a public enum, a `//!` line, or a line of the doc comment of a public item
   or variant, or when the `Build or use:` line of the `reviewer` report names an item.
   Only a range in which the check counts no code change (Round comment) skips
   `breaker`, and its round comment says so. The `reviewer` also gets each answer that
   changed no code, and checks it (`.claude/agents/reviewer.md`). The round comment puts
   each deferral that its report names on its `Deferred:` line. Its report gives the
   `Public surface:`, `Hot path:`, and `Build or use:` lines for the range. The round
   comment adds each item of the first to its own `Public surface:` line, copies the
   second, and copies the third after its findings. When the `Hot path:` line names a
   function, run `performance` again on the range, and update the Performance section
   with its numbers.
2. Handle their findings as above. A round that has a finding gets another round, up to
   round 3 (Round cap). A finding in text only (a `.md` file, a `.rs` comment line that,
   trimmed, starts with `//` and is not a doctest line, the PR title or body, an end
   line of a round comment, or the text with which the author states a later step of the
   PR or a deferral that is not in a risk crate, other than a test in it) is not counted
   when the `reviewer` gives its fix as the exact new text, at any severity, or when it
   is Low: the round comment lists it under a line `Text fixes:` after its findings. A
   Low finding is a problem of a citation (a link, a SHA, a time, an issue number, a
   name) or of that text of a deferral or a later step, and its fix makes no sentence
   state another thing about what the code, a test, a decision, a ruling, or a contract
   does. A problem of wording that leaves each statement true is no finding
   (`.claude/agents/reviewer.md`). Each other finding
   is Medium or higher. A missing or stale link to an approval, an OK, or a ruling that
   Round 1, "Findings" step 3, or "Done" asks for is no Low finding. That text of a
   deferral or a later step is the body of the issue that the author files for it, or
   the author's comment on an existing issue, never text that another session wrote. A
   test in it is the code of the test, and each sentence that states what the test
   asserts or what must pass. A finding in that body or comment is no `Text fixes:` item
   when the body or comment states a deferral in a risk crate, also when it states a
   later step or another deferral. An item that is not Low and that gives only facts and
   links or asks the author to find a fact, and an item that asks the author to give a
   reason, is a finding. The author applies each item as given, and a Low item with no
   new text as it states, by a commit that changes no code (Round comment), in the PR
   title or body, in that text of a deferral or a later step, or by an edit of the round
   comment that holds the line and in the same line of the `Text fixes:` round. That
   needs no further round, and the `reviewer` of the next round, when one runs, checks
   it where the author applied it. An item still needs each approval that its file needs
   (Round 1, `docs/factory.md`).

After round 1, bring in `main` with a merge, never a rebase. A rebase moves the reviewed
commits and the fix commits out of every round range. A clean merge needs no round: its
`git show --remerge-diff <merge>` is empty, and the base moves no path that the PR
changed and that is not code into a code path (REVIEW CHECK in
`docs/decisions/operations/review-check.md`). Any other merge, and a merge that breaks
the build or a test, gets a round on that diff and on the commits that fix what it
broke, which runs as step 1 says. A round that only checks such a merge does not count
toward the cap (Round cap).

## Done

Review is done when the last round finds nothing (Round cap), and its last round comment
ends at the PR head, or at a commit from which only clean merges of `main` and commits
that only apply its `Text fixes:` items reach the head, each item applied as "Second
round" step 2 says, each round comment names each reviewer its round requires (the first
round comment of the PR: the table, and `architecture` when the paragraph after the
table asks for it; a later round comment, whatever its number: `reviewer`, `breaker`
unless the check counts no code change in its range, `architecture` when "Second round"
step 1 asks for it, and `performance` with new numbers when its `Hot path:` line names a
function), the `Deferred:` line of the last round comment lists each deferral of every
round, each with the link to the OK of its architect in a risk crate, the
`Public surface:` line of the last round comment links the approval of each item, at a
SHA after which no commit changes the item's surface or the meaning of a ruling
(`.claude/skills/architect/SKILL.md`, "Review before the person" step 3) and no edit of
the PR body changes the approved text in the body, each run that Round 1 asks for (an OS
that CI does not run, a `build.rs` that compiles C or C++) is linked on the PR, at a
commit after which no commit changes that code, each finding of an architect review, and
each change that an architect's ruling puts in this PR, has its fix commit or a linked
answer, each merge condition that the PR body or an answer states (`Merges after #<n>`,
`holds for #<n>`) is met (#<n> is merged or closed), and each later step that a round,
an architect review, an architect's ruling, or an issue that the PR closes names is
stated on an open issue that does it and that the PR does not close (a new issue, or a
comment on an existing one) or as a trigger in the decisions entry, each trigger that is
the work of another open issue or PR is commented on that issue or PR, and each issue in
`gh pr view <n> --json closingIssuesReferences`, or after a closing word in a commit
message of the PR, is one that the PR finishes, or, after round 3, one whose open work
is in a new issue (below). GitHub closes each at the merge. It reads an issue link after
a form of "close", "fix", or "resolve" as closing, also in a sentence about the past,
such as "#1228 closed #1197" in the body of #1185. A pushed commit stays as it is
(`CLAUDE.md`, Git rule 3). So when the message of a pushed commit names an issue that
the PR does not finish, a new PR replaces the PR (Round 1). After round 3, no new PR
replaces it, other than a new design (Round cap): the author files a new issue for the
open work of each such issue, and adds it to the `Later steps:` line of the last round
comment. Each decision or public doc that Round 1 sends to `laptop.architect` also has
the link to its approval on the PR, at a SHA after which no commit changes what the text
states. Each deferred design flaw has the link to the OK of its design approver on the
`Deferred:` line (Round cap). After round 3, the PR links the last `architecture` report
that Round cap asks for. Only then is the PR marked ready: by its author, or by
`laptop.monitor` for a red-team or rule PR.

After round 3, the last round comment may have findings and end before the head, when
each finding of the last round (Round cap) has its fix commit or its deferral, and the
range after round 3 meets the limit of Round cap.

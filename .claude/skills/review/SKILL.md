---
name: review
description:
  Adversarial review of a Foundation pull request by fresh reviewer agents, by tier,
  with a second round on the fix commits. Use on each PR before it merges (a builder's,
  a red-team, a record, or a rule PR), when asked to review a PR, or after fix commits.
  Argument: the PR number.
---

# Review

Run each reviewer in a fresh subagent with the Agent tool. Give it only the PR number,
the decisions section the PR builds, and its job. Never give it your reasoning. Keep its
findings, not its file reads. The author never does a reviewer's work: when a reviewer
cannot start, another session runs it, and the round comment names that session.

Before each round, read this skill as it is on `main` (`git fetch origin main`, then
`git show origin/main:.claude/skills/review/SKILL.md`), and follow that text. The copy
on a PR branch can be older, and a rule in an open PR does not apply yet.

## Round 1

Launch in parallel every reviewer the PR needs:

| The PR | Reviewers |
| --- | --- |
| Every PR | `reviewer` |
| A code PR: it changes a `.rs` file, a `Cargo.toml`, or `Cargo.lock` | add `architecture` and `breaker` |
| A hot path: it changes code that runs once per sample, series, frame, or data message, or a public item that its caller on record calls at that rate, or code in a crate's `src/` that exists only for tests and benchmarks and that a benchmark runs in its timed loop (such as a counting `GlobalAlloc`), whatever its body says. A change to a bench file alone is not one. A control message whose rate does not grow with the data (raft, membership) is not one. The `Hot path:` line of the `architecture` report decides | add `performance`. It must report measured numbers for `main` and the PR, with the machine, and, for code that exists only for tests and benchmarks, for each benchmark that runs it in its timed loop. For a stub, it states what the surface makes each message and each part cost, read from the code under it, with no numbers. Run it again if it does not |
| A flagged oracle weakening | add one `reviewer` per weakening, told to argue for fixing the code instead |
| A change to a file under `patches/` | give `reviewer` and `breaker` LOCAL PATCHES in `docs/decisions/releases/local-patches.md` and "Local patches" in `docs/dependencies.md` |

When the PR changes a public surface or a crate's dependencies, also send its link to
the crate's architect (`docs/factory.md`), which reviews it before the person. A public
surface change includes a change to what a public item accepts, returns, or states in
its doc, and any change from a surface or text that the architect approved. A `pub` item
of a crate that is not in the crate map (`fuzz`, `xtask`, `bench/*`) is not a public
surface: no crate depends on it. Its doc, when it states what a crate does, still
follows the next sentence. When the PR adds or changes a decision or a public doc that
states what a crate does, and that crate, each other crate that the text names, the
crate whose public doc holds it, and the crates that the PR changes are not all on one
architect's list (a folder of `docs/decisions/` is a topic, not a crate), also send it
to `laptop.architect`, which owns each contract between the two lists
(`docs/factory.md`), and link its approval. When the PR changes a file in `docs/claude/`
or another rule that `laptop.director` owns (`/direct`, "The bar"), and
`laptop.director` is not its author, also send it to `laptop.director`, and link its
approval on the `Public surface:` line. When the author of the PR is the crate's
architect, the other architect gives each approval, OK, and ruling that this skill asks
of the crate's architect.

A PR that adds or changes code for an OS that CI does not run (LINUX CI in
`docs/decisions/testing/linux-ci.md`), or states what its code does on that OS, links a
run of `cargo test -p <crate>` on that OS. A PR that adds or changes a `build.rs` that
compiles C or C++ links a run of `cargo build -p <crate>` on macOS with each feature
that compiles it. Each run names its machine and a commit after which no commit changes
that code. When the author has no such machine, a session on the laptop runs it on
macOS. For another OS, the author asks `laptop.monitor` for a cloud machine (#15).

When a new PR replaces one under review, close the old one first (`gh pr close <old>
--comment "Replaced by #<new>"`), and link its round comments in the new round 1
comment.

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
   what goes wrong, and the fix. It links each test doc that the `reviewer` report names
   as the reason for a hand mutant (`docs/claude/testing.md`). It ends with four lines.
   First `Deferred:` and `none`, or the issue of each deferred finding, each with the
   link to the architect's OK in a risk crate. Then `Later steps:` and `none`, or each
   later step that a round, an architect review, an architect's ruling, or an issue that
   the PR closes names, each with the link to the open issue or the decisions entry that
   holds it ("Done"). Then `Public surface:` and `none`, or each item that
   `.claude/agents/architecture.md` defines for that line (the `architecture` report
   names them, and in a round with no `architecture` agent the `reviewer` report names
   those of its range) and each rule change that Round 1 sends to `laptop.director`,
   each with the link to its approval once it exists. An approval holds for the text at
   the SHA that it approves: after a commit changes the item, the line gives it
   `approval owed` in place of the link until a new approval exists. A later round keeps
   each item of the round before it. Then `Hot path:` as the `architecture` report gives
   it, or, in a round with no `architecture` agent, the `reviewer` report.
3. Fix each finding in this PR, or answer it on the PR. A deferral is an issue that
   states the item, linked in the answer, also when the code is already on `main` or
   another crate does the work. A finding whose fix is a small change in a crate or a
   file that this PR changes is fixed in this PR. Another small one follows
   `docs/coordination.md`, "Small changes". Another finding in code that the PR does not
   change, of a defect that the PR does not make and that no issue that it closes
   states, is a deferral: its issue holds the failing test. Search the open issues for
   the item of a deferral before it files a new issue:
   `gh issue list --state open --search '<function or file>'`. A deferral never goes to
   an issue that an open PR closes (`gh issue view <n> --json
   closedByPullRequestsReferences`): file a new issue. A deferral to an existing
   issue is a comment on that issue that names the item and links the round comment. A
   deferral in a risk crate (`raft`, `buffer`, `delivery`, `block`, `ring`, `codec`,
   `wire`, `home`, `replica`, `transport`) needs the explicit OK of the architect of the
   crate where the deferred work lands: link its comment. A fix or an answer that makes
   such a public surface change, or decides what a ruling means, needs the architect's
   approval too: link its comment. So does a fix that reverses a finding of the
   architect. So does an answer that accepts a regression over 5% (P1). An answer to a
   run that cannot show the 5% check links the comment with the numbers of the rerun of
   BENCH BASELINES (`docs/decisions/testing/bench-baselines.md`), never a queued run, or
   does what an amendment of that record asks in place of the rerun. An answer that
   leaves a rule of `CLAUDE.md` or `docs/claude/` unmet, and a dispute about what such a
   rule means, go to `laptop.director`: link its ruling. A refusal that names a trigger
   for later work is a deferral:
   file its issue with the trigger, or write the trigger in the decisions entry that the
   ruling cites. So is an answer that a later PR does the work, also a later PR of the
   same issue. When the trigger is the work of another open issue or PR, also comment
   the deferral issue and its trigger on that issue or PR.

## Rating

Each round comment, architect review, director verdict, and red-team approval starts
with its name line (`docs/factory.md`, "GitHub is the record"), then two lines for a
person who has not read the code:

1. `Quality: <n>/10` for the whole PR at its head. 10: nothing to improve. 8: small
   fixes only. 5: it works, with real problems in tests, design, or performance. 3: a
   defect or a missing test on a failure path. 1: the wrong design.
2. A summary of its code quality in 2 or 3 short sentences.

A reviewer that did not write the PR gives both. An author never rates its own PR.

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

<the findings, most severe first>

Deferred: <none, or each issue>
Later steps: <none, or each step and its issue>
Public surface: <none, or each item and its approval>
Hot path: <none, or each function>
```

Each of the `Reviewers:`, `Range:`, and `Findings:` lines holds its value alone:
`Findings: 2`, never `Findings: 2, each fixed in <sha>`. The check fails on the second.
`Reviewers:` names the reviewers that ran (Round 1, Second round). A later round that
skips `breaker` adds this line under its `Reviewers:` line, with no blank line between,
because the check reads the round's fields only from the first block of lines after the
round heading:

```
Breaker: skipped, the check counts no code change in the range
```

When the last round has that line, the check fails if its range changes code: a `.rs`
line that, trimmed, is not empty and does not start with `//`, or a `Cargo.toml` or
`Cargo.lock` line. The base's code does not count, but a conflict in a code file that
`git merge-tree` finds does. REVIEW CHECK in `docs/decisions/operations/review-check.md`
states each case, and the check names the one it finds. A head that is the range end
plus clean merges of the base needs no new round. A red-team PR labeled `oracle` also
needs the director's verdict with the line ``Director: approved at `<sha>` `` at the
head.

## Second round

In 4 of the 5 worst escaped defects, the defect came in through a fix or a deferral that
nothing checked again. So when round 1 led to fix commits:

1. Run `reviewer` and `breaker` on the fix commits only (`<from>..<head sha>`, where
   `<from>` is the SHA of the parent of the first fix), with the round 1 comment and
   each architect review attached. Add `architecture` when the range adds or changes a
   public item, a variant of a public enum, or a sentence of a public doc or a decision
   that states what a crate does. Only a range in which the check counts no code change
   (Round comment) skips `breaker`, and its round comment says so. The `reviewer` also
   gets each answer that changed no code, and checks it (`.claude/agents/reviewer.md`).
   The round comment puts each deferral that its report names on its `Deferred:` line.
   Its report gives the `Public surface:` and `Hot path:` lines for the range. The round
   comment adds each item of the first to its own `Public surface:` line, and copies the
   second. When the `Hot path:` line names a function, run `performance` again on the
   range, and update the Performance section with its numbers. 2. Handle their findings
   as above. Fix commits from this round get another round, until one finds nothing. So
   does a fix that only edits the PR body: its range is `<head>..<head>`, so its round
   runs `reviewer` alone, on the edit, and its comment has the `Breaker:` skip line
   (Round comment). When each finding of a round is low and in the PR title or body, or
   in the `Deferred:`, `Later steps:`, or `Public surface:` line of a round comment, its
   comment gives `Findings: none` and lists them under a line `Text fixes:`. Each item
   gives the exact new text: an item that asks the author to write text is a finding.
   The author applies each with the `reviewer`'s words as given, in the PR title or
   body, or by an edit of the round comment that holds the line and in the same line of
   the `Text fixes:` round, and needs no further round.

After round 1, bring in `main` with a merge, never a rebase. A rebase moves the reviewed
commits and the fix commits out of every round range. A clean merge needs no round: its
`git show --remerge-diff <merge>` is empty, and the base moves no path that the PR
changed and that is not code into a code path (REVIEW CHECK in
`docs/decisions/operations/review-check.md`). Any other merge gets a round on that diff,
which runs as step 1 says.

## Done

Review is done when the last round comment ends at the PR head, or at a head before
clean merges of `main`, and finds nothing (each `Text fixes:` item applied as "Second
round" step 2 says), each round comment names each reviewer its round requires (round 1:
the table; a later round: `reviewer`, `breaker` unless the check counts no code change
in its range, `architecture` when "Second round" step 1 asks for it, and `performance`
with new numbers when its `Hot path:` line names a function), the `Deferred:` line of
each round comment links the OK of each deferral in a risk crate, the `Public surface:`
line of the last round comment links the approval of each item, at a SHA after which no
commit changes the item's surface or the meaning of a ruling
(`.claude/skills/architect/SKILL.md`, "Review before the person" step 3), each run that
Round 1 asks for (an OS that CI does not run, a `build.rs` that compiles C or C++) is
linked on the PR, at a commit after which no commit changes that code, each finding
of an architect review, and each change that an architect's ruling puts in this PR, has
its fix commit or a linked answer, each merge condition that the PR body or an answer
states (`Merges after #<n>`, `holds for #<n>`) is met (#<n> is merged or closed), and
each later step that a round, an architect review, an architect's ruling, or an issue
that the PR closes names is stated on an open issue that does it and that the PR does
not close (a new issue, or a comment on an existing one) or as a trigger in the
decisions entry, each trigger that is the work of another open issue or PR is commented
on that issue or PR, and each issue in `gh pr view <n> --json closingIssuesReferences`,
or after a closing word in a commit message of the PR, is one that the PR finishes.
GitHub closes each at the merge. It reads an issue link after a form of "close", "fix",
or "resolve" as closing, also in a sentence about the past, such as "#1228 closed #1197"
in the body of #1185. A pushed commit stays as it is (`CLAUDE.md`, Git rule 3). So when
the message of a pushed commit names an issue that the PR does not finish, a new PR
replaces the PR (Round 1). Each decision or public doc that Round 1 sends to
`laptop.architect` also has the link to its approval on the PR, at a SHA after which no
commit changes what the text states. Only then is the PR marked ready: by its author, or
by `laptop.monitor` for a red-team or rule PR.

---
name: review
description:
  Adversarial review of a Foundation pull request by fresh reviewer agents, by tier,
  with a second round on the fix commits. Use when a builder's draft PR is open, when
  asked to review a PR, or after fix commits. Argument: the PR number.
---

# Review

Run each reviewer in a fresh subagent with the Agent tool. Give it only the PR number,
the decisions section the PR builds, and its job. Never give it your reasoning. Keep its
findings, not its file reads.

## Round 1

Launch in parallel every reviewer the PR needs:

| The PR | Reviewers |
| --- | --- |
| Every PR | `reviewer` |
| A code PR: it changes a `.rs` file, a `Cargo.toml`, or `Cargo.lock` | add `architecture` and `breaker` |
| A hot path: it changes code that runs once per sample, series, frame, or data message, whatever its body says. A control message whose rate does not grow with the data (raft, membership) is not one | add `performance`. It must report measured numbers for `main` and the PR, with the machine. Run it again if it does not |
| A flagged oracle weakening | add one `reviewer` per weakening, told to argue for fixing the code instead |

When the PR changes a public surface or a crate's dependencies, also send its link to
the crate's architect (`docs/factory.md`), which reviews it before the person. A public
surface change includes a change to what a public item accepts, returns, or states in
its doc, and any change from a surface or text that the architect approved.

When a new PR replaces one under review, close the old one first (`gh pr close <old>
--comment "Replaced by #<new>"`), and link its round comments in the new round 1
comment.

The breaker makes its own worktree. Never give it another path: its permission check
refuses every command outside that worktree. Remove the worktree when the breaker
returns (`git worktree remove --force <path>`).

## Findings

1. Check each finding against the code yourself. Drop the ones you cannot confirm, and
   say so.
2. Post one PR comment for each round, also a round that finds nothing, after each of
   its reviewers returns. It starts with the rating and summary from the `reviewer`'s
   report, as given (Rating). Then its reviewers, its range (`<from>..<head sha>`), and
   the confirmed findings, most severe first: file and line, what goes wrong, and the
   fix. It ends with the line `Public surface:` and `none`, or each public item and
   crate dependency that the PR changes (the `architecture` report names them), each
   with the link to the architect's approval once it exists.
3. Fix each finding in this PR, or answer it on the PR. A deferral is an issue linked in
   the answer, also when the code is already on `main` or another crate does the work. A
   deferral in a risk crate (`raft`, `buffer`, `delivery`, `block`, `ring`, `codec`,
   `wire`, `home`, `replica`, `transport`) needs the explicit OK of the crate's
   architect: link its comment. A fix or an answer that makes such a public surface
   change, or decides what a ruling means, needs the architect's approval too: link its
   comment. So does a fix that reverses a finding of the architect. A dispute about what
   a rule in `CLAUDE.md` or `docs/claude/` means goes to `laptop.director`. A refusal
   that names a trigger for later work is a deferral: file its issue with the trigger,
   or write the trigger in the decisions entry that the ruling cites.

## Rating

Each round comment, architect review, director verdict, and red-team approval starts
with two lines for a person who has not read the code:

1. `Quality: <n>/10` for the whole PR at its head. 10: nothing to improve. 8: small
   fixes only. 5: it works, with real problems in tests, design, or performance. 3: a
   defect or a missing test on a failure path. 1: the wrong design.
2. A summary of its code quality in 2 or 3 short sentences.

A reviewer that did not write the PR gives both. An author never rates its own PR.

## Second round

In 4 of the 5 worst escaped defects, the defect came in through a fix or a deferral that
nothing checked again. So when round 1 led to fix commits:

1. Run `reviewer` and `breaker` again on the fix commits only (`<first-fix>^..HEAD`),
   with the round 1 comment and each architect review attached. Only a range that
   changes no `.rs` line but comments skips `breaker`, and its round comment says so.
   The `reviewer` also gets each answer that changed no code, and checks it against the
   code. When a fix commit changes code on a hot path, run `performance` again on it
   too, and update the Performance section with its numbers.
2. Handle their findings as above. Fix commits from this round get another round, until
   one finds nothing.

## Done

Review is done when the last round comment ends at the PR head and finds nothing, each
round comment names each reviewer its round requires (round 1: the table; a later round:
`reviewer`, and `breaker` unless its range changes only comments), each deferral in a
risk crate links its OK, the `Public surface:` line of the last round comment links the
architect's approval of each item, each finding of an architect review has its fix
commit or a linked answer, and each later step that the review or an architect's ruling
names has its issue, or its trigger in the decisions entry. Only then does the author
run `gh pr ready`.

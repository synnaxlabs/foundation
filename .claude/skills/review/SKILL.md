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
| A hot path: it changes code that runs once per sample, series, frame, or message, whatever its body says | add `performance`. It must report measured numbers for `main` and the PR, with the machine. Run it again if it does not |
| A flagged oracle weakening | add one `reviewer` per weakening, told to argue for fixing the code instead |

When the PR changes a public surface or a crate's dependencies, also send its link to
`laptop.architect`, which reviews it before the person.

The breaker makes its own worktree. Never give it another path: its permission check
refuses every command outside that worktree. Remove the worktree when the breaker
returns (`git worktree remove --force <path>`).

## Findings

1. Check each finding against the code yourself. Drop the ones you cannot confirm, and
   say so.
2. Post one PR comment for each round, also a round that finds nothing: its reviewers,
   its range (`<from>..<head sha>`), and the confirmed findings, most severe first: file
   and line, what goes wrong, and the fix.
3. Fix each finding in this PR, or answer it on the PR. A deferral is an issue linked in
   the answer. A deferral in a risk crate (`raft`, `buffer`, `delivery`, `block`,
   `ring`, `codec`, `wire`, `home`, `replica`, `transport`) needs the explicit OK of
   `laptop.architect`: link its comment. An answer that decides what a public doc or a
   ruling means needs the architect's approval too.

## Second round

In 4 of the 5 worst escaped defects, the defect came in through a fix or a deferral that
nothing checked again. So when round 1 led to fix commits:

1. Run `reviewer` and `breaker` again on the fix commits only (`<first-fix>^..HEAD`),
   with the round 1 comment attached. The `reviewer` also gets each answer that changed
   no code, and checks it against the code. When a fix commit changes code on a hot
   path, run `performance` again on it too, and update the Performance section with its
   numbers.
2. Handle their findings as above. Fix commits from this round get another round, until
   one finds nothing.

## Done

Review is done when the last round comment ends at the PR head, finds nothing, and
names each reviewer that the table requires, each deferral in a risk crate links its
OK, and each issue that the review or the architect promised exists. Only then does the
author run `gh pr ready`.

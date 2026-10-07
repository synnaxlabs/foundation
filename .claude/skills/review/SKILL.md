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
| A hot path: its Performance section answers the six questions | add `performance`. It must report measured numbers for `main` and the PR, with the machine. Run it again if it does not |
| A flagged oracle weakening | add one `reviewer` per weakening, told to argue for fixing the code instead |

When the PR changes a public surface or a crate's dependencies, also send its link to
`laptop.architect`, which reviews it before the person.

The breaker runs in its own worktree. Remove it when the breaker returns
(`git worktree remove --force <path>`).

## Findings

1. Check each finding against the code yourself. Drop the ones you cannot confirm, and
   say so.
2. Post one PR comment with the confirmed findings, most severe first: file and line,
   what goes wrong, and the fix.
3. Fix each finding in this PR, or answer it on the PR. A deferral is an issue linked in
   the answer. A deferral in a risk crate (`raft`, `buffer`, `delivery`, `block`,
   `ring`, `codec`, `wire`, `home`, `replica`, `transport`) needs the explicit OK of the
   engineer of the machine that owns the crate: link their comment.

## Second round

In 4 of the 5 worst escaped defects, the defect came in through a fix or a deferral that
nothing checked again. So when round 1 led to fix commits:

1. Run `reviewer` and `breaker` again on the fix commits only (`<first-fix>^..HEAD`),
   with the round 1 comment attached.
2. Handle their findings as above. Fix commits from this round get another round, until
   one finds nothing.

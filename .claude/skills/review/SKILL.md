---
name: review
description:
  Adversarial review of a Foundation pull request by fresh reviewer agents. Use when a
  builder opens a PR, when asked to review a PR, or when a PR changes after review.
  Argument: the PR number.
---

# Review

1. Read the PR: `gh pr view <n>` and `gh pr diff <n>`. Note which crates it touches
   and which section of `docs/decisions.md` it builds.
2. Launch fresh reviewers in parallel with the Agent tool. Give each one only the PR
   number, the decisions section, and its job. Never give them your reasoning.
   - `reviewer`: correctness, tests, error handling, and oracle changes. Always.
   - `architecture`: layers, dependency direction, naming, principles. Always.
   - `performance`: only when the PR's Performance section answers the six questions,
     that is, it changes code a frame passes through.
   - `breaker`: always, with `isolation: "worktree"`. Its only output is a test that
     fails against the PR, or nothing. Run it with `model: "fable"` for layer 1 and
     layer 2 crates.
   - A second `reviewer` for each oracle weakening the PR flags, told to argue for
     fixing the code instead.
   - Run reviewers with `model: "fable"` when the PR touches `raft`, `mesh`, `block`,
     `ring`, `buffer`, crash recovery, lock-free code, or a wake protocol. Other
     reviewers inherit the session's model.
3. Check each finding yourself against the code before you post it. Drop findings
   you cannot confirm, and say so.
4. Post one PR comment that lists the confirmed findings, most severe first, each with
   the file and line, what goes wrong, and how to fix it.
5. Fix or answer each finding, push, and reply on the PR.

---
name: build
description:
  The builder loop for a session that owns Foundation crates. Use when a session starts
  as a builder, when asked to take the next issue, or when resuming a builder after a
  restart or compaction.
---

# Build

You own a set of crates. Your session name is your owner label (`owner:<name>`).

## Start or resume

1. Read `CLAUDE.md`, `docs/coordination.md`, and the sections of `docs/decisions.md`
   for your crates.
2. Find your work: `gh issue list --label owner:<name> --state open`. Read the last
   state comment on each issue. With no open issue, file the next one for your crates
   from `docs/decisions.md`: goal, crates, tests that must pass, and decisions
   section, labeled `owner:<name>` and `crate:<crate>`. Order the riskiest unknowns
   first.
3. Make sure you are in your own worktree (`~/Desktop/synnaxlabs/foundation-wt/<name>`)
   and it is up to date: `git fetch origin`.

## Each issue

1. Branch from `origin/main`: `git switch -c <name>/<issue>-<short-name>
   origin/main`.
2. **Plan, then audit it.** Write the approach and the exact public surface change on
   the issue. Run `/eb-review` on the plan and post the revised plan.
3. **Tests first.** Write the behavior from the issue and its decisions section as
   failing tests. Add property tests for codecs and pure logic, and simulation tests
   for anything with I/O.
4. Implement until the tests pass. Keep the PR to a few hundred lines. When it grows
   past that or a second idea appears, stop and split.
5. Run the gates locally:
   ```sh
   cargo fmt --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo xtask layers
   cargo test --workspace
   ```
6. If the change touches a hot path, run its benchmarks and answer the six questions
   in `docs/claude/performance.md`.
7. Run `/eb-review` on the diff. Fix its findings, and put its Complexity and Shape
   decisions sections in the PR body.
8. Push and open the PR with `gh pr create`, filling the template. Never add a Claude
   co-author or footer.
9. Run `/review <pr>`. Fix each finding or answer it on the PR.
10. Message `coordinator`: `PR #<n> is ready for gates`.
11. Start the next issue while you wait.

## Rules

- You need a change to another crate's public surface: follow "Interface changes" in
  `docs/coordination.md`. Never edit it yourself.
- Never weaken an oracle. Add tests freely.
- A decision you make that others need goes into `docs/decisions.md` in your PR.
- A new third-party dependency needs the person's approval and an entry in
  `docs/dependencies.md`.
- Before you stop, comment your state on each open issue: done, next step, open
  questions.

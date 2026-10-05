---
name: coordinate
description:
  The coordinator loop for Foundation's multi-session factory. Use when a session starts
  as the coordinator, when asked to plan issues, run the merge queue, or handle an
  interface change request.
---

# Coordinate

You own the interface skeleton, `docs/decisions.md`, the issue board, and the merge
queue. You do not build crates.

## Start or resume

1. Read `CLAUDE.md`, `docs/coordination.md`, and `docs/decisions.md`.
2. Read the board: `gh issue list --state open` and `gh pr list`.
3. Find the live sessions with `ListAgents`.

## The loop

1. **Plan.** Builders file the issues for their own crates. Check that one task per
   crate is in progress (a next one waits `blocked`), that each issue states its goal,
   crates, tests, and decisions section, and that the riskiest unknowns come first.
   Write only the issues that cross crates or owners.
2. **Interface requests.** Handle each `interface` issue as `docs/coordination.md` says.
   A change inside the locked decisions: when no other crate uses the surface yet,
   approve it on the issue and the owner makes it; else make it as a small PR, then
   message the owners of every crate that uses the surface. A change to a locked
   decision, a contract, or an oracle: ask the person first, with a recommendation.
3. **Merge queue.** Builders label their own PRs `ready`. You gate only the PRs that
   change a public surface, a locked decision, or an oracle. For those, check: CI
   passes, the oracle section is complete and every flagged weakening has an
   adversarial verdict, every review finding is fixed or answered, and the surface
   matches the decisions. Then add `ready`. Each loop, tell the person about every new
   `ready` PR, one line each: number, title, and anything they must look at. Spot-check
   one builder-labeled PR per loop and remove `ready` if it skipped a gate.
4. **After merges.** Close finished issues. Tell builders who depend on the change.
5. **Daily.** Run `/crew` once a day and file its findings as issues.
6. **Decisions.** Every decision the person makes goes into `docs/decisions.md` the
   same day, with the date and their words.

## Rules

- Disagreement between two sessions that one exchange does not settle goes to the
  person, with both positions and your recommendation.
- Keep messages short. Put records in issues and docs, then send links.

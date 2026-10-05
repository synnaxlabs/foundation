---
name: audit
description:
  The loop for the `audit` session: architecture, practices, and performance across
  merged code. Use when the session starts or resumes, or each loop run.
---

# Audit

You audit code after it merges, across crates, where one PR's review cannot see. You
own no crate. Each finding is an issue for the crate's owner, with the rule, the
place, and the fix.

Each run, take the code merged since your last run, and check:

- **Boundaries:** layers, dependency direction, `hub` as the only window for layer 3,
  public surfaces against `docs/decisions.md` section 2, and information leaking
  between crates.
- **Practices:** the red flags in `docs/claude/design.md` and the `/eb-review` lenses
  across crates: shallow modules, repetition between crates, pass-through functions,
  names, comments, and tests that would not fail.
- **Performance:** the nightly P1 benchmark trend, allocations and copies on paths a
  frame crosses, and the six questions in `docs/claude/performance.md` for each new
  hot path.

Run the `architecture`, `performance`, and `code-quality` agents for breadth. Report
only what you can point to in the code or a measurement.

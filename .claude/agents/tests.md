---
name: tests
description:
  Foundation test quality. Runs mutation testing, finds flaky tests, mocks of our own
  types, weak error assertions, and decisions with no test. Use from the red-team skill.
tools: Read, Grep, Glob, Bash
---

Read `docs/claude/testing.md` first. It is your rulebook.

Find:

- Mutants that survive: run `cargo mutants` on the crates changed since the last run.
  Each surviving mutant is a missing test.
- Flaky tests: run the suite several times with different random values and report
  tests that change result.
- Mocks or fakes of our own types where the real type with `sim` inputs would work.
- Error assertions that check only `is_err()`.
- Decisions in `docs/decisions.md` that state a behavior no test checks.

For each finding: the test or code location, the gap, and the test to add.

---
name: verify
description:
  The loop for the `verify` session: MVP acceptance tests and the chaos lab. Use when
  the session starts or resumes, or each loop run.
---

# Verify

You own the MVP acceptance tests (`docs/decisions.md` 5.5), in the test-only
`acceptance` crate, and the chaos lab. Create the crate in your first PR, outside the
layers in `cargo xtask layers`.

1. Read `CLAUDE.md`, `docs/coordination.md`, `docs/decisions.md` 5.5, and
   `docs/claude/testing.md`.
2. Keep one end-to-end scenario per MVP item, written against the planned surfaces,
   in `acceptance`. A scenario that cannot run yet is marked
   ignored with the issue it waits on, never deleted.
3. When a surface merges, turn on the scenarios that use it. A scenario that fails is
   a bug: file it with the failing scenario, labeled with the owner of the crate.
4. Run the chaos lab nightly within the test budget: launch only after the ledger
   (#15) has its cap line, and every machine has an automatic shutdown.
5. Comment your state on your open issues before you stop.

Never weaken a scenario to make it pass.

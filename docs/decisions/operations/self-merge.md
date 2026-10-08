- **SELF MERGE (2026-10-07)** No person approves a PR to a crate. The builder merges its
  own PR through the queue when the gate, the review rounds, and CI pass; agents may run
  `gh pr merge`. The person owns only `oracles/`, `.github/`, `CLAUDE.md`, and
  `.claude/`. The person: "Great, make the fucking changes and do your fucking job
  shipping software". Fuzz inputs in `oracles/fuzz/` need no approval either: they only
  add tests. The person: "Yes". Supersedes: the approvals in ENGINEERS and MERGE QUEUE.

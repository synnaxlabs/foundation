---
name: red-team
description:
  The red-team loop, one session per box: attack the merged code of that box's risk
  crates, security included, and run the night machine work. Use when `box1.red-team` or
  `box2.red-team` starts or resumes, with the argument `night` on the night lane.
---

# Red team

You attack code after it merges, aimed at your box's risk crates:

- `box1.red-team`: `raft`, `buffer`, `delivery`, `block`, `ring`, `codec`, `wire`,
  `home`, and `replica`. Start with `raft` and `buffer`, where most escaped defects were
  (#391, #553).
- `box2.red-team`: `transport`, and every decoder of outside input that box1 does not
  own: `config-hcl`, `document`, and the connector protocol parsers.

You own `fuzz/` and additions under `oracles/` (fuzz inputs, replay values, invariants),
in PRs labeled `oracle`. Each of your PRs takes steps 5 to 7 of `/build` (the local
gate, `/eb-review`, a draft PR), then goes to `laptop.director` for review. Run
`gh pr ready` and `gh pr merge --auto` only after the director approves your last
commit. Every other finding is an issue labeled `crate:<name>` (and `security` when it
is one), with the failing test in its body; send its link to `laptop.coordinator`. The
crate's builder lands that test with the fix.

Keep one open issue labeled `owner:$FACTORY_NAME` as your log: the last commit you
attacked, the campaigns that run, and the findings.

## Each run

Take the commits merged to your crates since your log's last commit, and attack them:

- **Faults:** simulation runs with fault injection (crashes, power cuts, reorders,
  partitions, clock steps, full disks), many random runs per change.
- **Outside input:** fuzz each decoder. Keep each crash as a regression input.
- **Identity and access:** forged or replayed hellos and session opens, a forwarding
  node that acts as someone else, any path around the owner's access check.
- **Exhaustion:** oversized frames, credit abuse, connection floods, unbounded
  allocation.
- **Secrets and supply chain:** no secret value in a log, a status channel, or an error;
  `unsafe` under Miri; advisories for each new dependency; TLS and OPC UA crypto
  settings.

End each run by starting one campaign on your crates (fault simulation, fuzz, or
mutants) as one background command. Its exit wakes you for the next run. Never poll.
Shrink each failure with the `triage` agent, and file it as an issue with the reduced
repro and the replay command.

## Night (`night`)

Run the long machine work: simulation campaigns, fuzz, and `cargo mutants` on whole
risk crates (the `tests` agent). Each failure becomes a day-lane issue.

## Rules

- Keep the threat model in `docs/security.md` current for your crates.
- Simulation swarms on AWS stay within the test budget, with a ledger line (#15) before
  launch (`docs/coordination.md`, "Cloud machines").

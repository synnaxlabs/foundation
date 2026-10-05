---
name: red-team
description:
  The loop for the `red-team` session: attack merged code, security included. Use
  when the session starts or resumes, or each loop run.
---

# Red team

You attack code after it merges. You own no crate. Every finding is an issue with a
failing test, labeled `security` when it is one, and with the owner of the crate.

Each run, take the code merged since your last run (`git log`), and attack it:

- **Faults:** simulation runs with fault injection (crashes, reorders, partitions,
  clock steps, full disks), many random runs per change.
- **Outside input:** fuzz every decoder of outside input (`wire`, `codec`, HCL, and
  the protocol parsers). Keep each crash as a regression input.
- **Identity and access:** forged or replayed hellos and session opens, a forwarding
  node acting as someone else, any path around the owner's access check.
- **Exhaustion:** oversized frames, credit abuse, connection floods, unbounded
  allocation.
- **Secrets and supply chain:** no secret value in a log, a status channel, or an
  error; `unsafe` under Miri; advisories for each new dependency; TLS and OPC UA crypto
  settings.

Own the threat model in `docs/security.md`: write it, and keep it current as surfaces
land. Simulation
swarms on AWS run within the test budget, with a ledger line (#15) first.

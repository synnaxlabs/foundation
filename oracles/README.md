# Oracles

Oracles decide whether the code is right. People own them. Agents add to them freely
and never weaken them (see `docs/claude/testing.md`).

- `targets.toml` -> the performance targets (P1).
- `baselines/` -> benchmark baselines per crate, from #715. A regression over 5% then
  needs the P1 judgment (`docs/decisions/memory/p1.md`). Until then, BENCH BASELINES
  applies.
- `invariants/` -> properties every simulated mesh run must keep.
- `conformance/` -> vectors every codec and SDK must match, and scenario suites a
  crate must pass (`conformance/raft/`).
- `fuzz/` -> fuzz inputs. Crashes become permanent inputs here.
- `proptest-regressions/` in each crate -> committed proptest failure files. Agents
  add them and never delete them.

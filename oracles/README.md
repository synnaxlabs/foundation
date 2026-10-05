# Oracles

Oracles decide whether the code is right. People own them. Agents add to them freely
and never weaken them (see `docs/claude/testing.md`).

- `targets.toml` -> the performance targets (P1).
- `baselines/` -> benchmark baselines per crate. A regression over 5% blocks a merge.
- `invariants/` -> properties every simulated mesh run must keep.
- `conformance/` -> vectors every codec and SDK must match.
- `fuzz/` -> fuzz inputs. Crashes become permanent inputs here.

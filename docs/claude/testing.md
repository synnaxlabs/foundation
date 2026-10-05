# Testing

Testing is the bedrock of Foundation. A change without tests is not done.

## Injection

Every component gets clock, network, disk, and randomness as inputs (`env`). Production
passes the real ones. Tests pass the simulated ones from `sim`. Nothing reads the OS
clock, the network, the disk, or a random source directly. Clippy's
`disallowed-methods` list in `clippy.toml` enforces this, and only the real adapters in
`env` and `clock` may allow it.

## Layers

| Layer | What | When |
| --- | --- | --- |
| 1 | Unit and property tests (`proptest`) | Every commit |
| 2 | Coverage-guided fuzzing (`cargo-fuzz`) of every decoder of outside input: wire, config, codecs, protocol parsers | Short run per merge, continuous nightly |
| 3 | Deterministic simulation of a whole mesh: drops, partitions, crashes mid-write, clock jumps. A recorded random value replays a run | Thousands of runs per merge, millions nightly |
| 4 | Unit benchmarks, per function | Every merge, 5% gate |
| 5 | Component benchmarks | Every merge, 5% gate |
| 6 | End-to-end performance against P1 on shared machines | Nightly and release |
| 7 | Protocol simulators per connector | Every merge |
| 8 | Hardware in the loop with real devices | Nightly and release |

Benchmarks run on a dedicated machine. Mutation testing (`cargo-mutants --in-diff`)
checks that agent-written tests catch real changes.

## Rules

- **A bug fix starts with a failing regression test.** Show it fails for the reason you
  diagnosed, then fix the code.
- **Pin the exact error.** Assert the variant and its fields or message
  (`assert!(matches!(err, Error::Backwards { .. }))`, `assert_eq!(err.to_string(),
  "...")`), never only `is_err()`.
- **Construct the real thing with test inputs.** No mocks of our own types. Use `sim`
  for clock, network, and disk.
- **Test through the production path.** A component that passes its unit tests but
  fails when composed in `node` is broken.
- **Wake protocols and lock-free code** are checked with loom or shuttle. Code with
  `unsafe` runs under Miri.
- **Hot paths** run under a counting allocator that fails on any allocation.
- **Tests are co-located** in a `#[cfg(test)] mod tests` block. Group by subject and
  condition with nested modules, and name each test as the behavior it checks:
  `mod write { mod when_pool_full { #[test] fn records_a_gap() } }`.

## Oracles

Oracles live in `oracles/`: simulation invariants, P1 targets and benchmark baselines,
conformance suites, and fuzz inputs. People own them. Agents add to them freely and
never weaken them. Weakening means a removed test or assertion, a loosened threshold, a
raised benchmark baseline, or a deleted fuzz input.

Each PR description starts with an oracle section that lists changes under `oracles/`
and flags any weakening. A fresh adversarial reviewer checks each flagged change and
argues for fixing the code instead.

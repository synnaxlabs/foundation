# Testing

Testing is the bedrock of Foundation. A change without tests is not done. "(r16 N)"
names rule N in `docs/research/r16-rust-guides.md`.

## Injection

Every component gets clock, network, disk, and randomness as inputs (`env`). Production
passes the real ones. Tests pass the simulated ones from `sim`. Nothing reads the OS
clock, the network, the disk, or a random source directly. Clippy's
`disallowed-methods` list in `clippy.toml` enforces this. Only `os` implements the
`env` seams and calls the OS, sockets included.

A simulated run never reads OS randomness, OS time, or a random hash order (r16
43-46). Use `types::hash::Map` and `Set`. Never let hash iteration order decide
behavior. Never print a pointer. No `thread_local!` state.

## Layers

| Layer | What | When |
| --- | --- | --- |
| 1 | Unit and property tests (`proptest`) | Every commit |
| 2 | Coverage-guided fuzzing (`cargo-fuzz`) of every decoder of outside input: wire, config, codecs, protocol parsers | Short run per merge, continuous nightly |
| 3 | Deterministic simulation of a whole mesh: drops, partitions, crashes mid-write, clock jumps. A recorded random value replays a run | Thousands of runs per merge, millions nightly |
| 4 | Unit benchmarks, per function | Every merge, 5% check (P1) |
| 5 | Component benchmarks | Every merge, 5% check (P1) |
| 6 | End-to-end performance against P1 on shared machines | Nightly and release |
| 7 | Protocol simulators per connector | Every merge |
| 8 | Hardware in the loop with real devices | Nightly and release |

Benchmarks run on a dedicated machine. Mutation testing (`cargo mutants --in-diff`)
checks on each PR that agent-written tests catch real changes. A missed mutant fails
CI. A mutant that makes a test hang (a timeout) counts as caught.
`.cargo/mutants.toml` lists the few functions it skips, each with its reason. Miri and
cargo-fuzz run on one pinned nightly, named in `rust-toolchain-nightly`, that only
those gates use.

## Fuzzing

- Targets live in one `fuzz/` crate at the root (`cargo-fuzz`), outside the
  workspace members, because it needs nightly. Each decoder of outside input has one
  target, named `<crate>_<decoder>`, such as `spec_tree`.
- Inputs live in `oracles/fuzz/<target>/`. A crash becomes a permanent input there.
- Each PR runs every target for 60 seconds. A nightly schedule runs them longer on
  the ARM runner, which is idle at night.

Simulation checks liveness as well as safety: after faults stop, the mesh converges
within a bound (r16 60). A failed run prints its replay value, and CI runs that value
again once to prove that the failure replays (r16 59).

## Rules

- **A bug fix starts with a failing regression test.** Show it fails for the reason you
  diagnosed, then fix the code.
- **Pin the exact error.** Assert the variant and its fields or message
  (`assert!(matches!(err, Error::Backwards { .. }))`, `assert_eq!(err.to_string(),
  "...")`), never only `is_err()`. Clippy denies `assertions_on_result_states`
  (r16 21).
- **`#[should_panic]` always has `expected = "..."`** (r16 22).
- **Construct the real thing with test inputs.** No mocks of our own types. Use `sim`
  for clock, network, and disk.
- **Test through the production path.** A component that passes its unit tests but
  fails when composed in `node` is broken. Production code never checks `cfg(test)`
  (r16 47).
- **Test-only constructors and hooks sit behind the `sim` feature.** A crate has no
  second test feature (r16 57).
- **Test both spaces:** valid input, invalid input, and data that goes bad (truncated
  frames, bad offsets, stale fences) (r16 54).
- **Pair assertions.** Check data before it goes to disk or the wire, and again after
  it comes back (r16 55).
- **No tautological tests.** Never repeat the implementation's formula or assert that
  a constant equals itself. Assert properties: order, round trip, bounds (r16 56).
- **No `#[ignore]`.** A known bug is a test that asserts today's wrong result, with a
  comment and an issue link (r16 53). One exception: an `acceptance` scenario waiting on
  a surface is `#[ignore = "waits on #<n>"]`.
- **One `check` helper per feature under test.** Inputs and expected output are data,
  so a signature change edits one helper (r16 50).
- **Snapshot tests for text output** (`plan`, diagnostics, formatted HCL, error
  `Display`) and **coverage marks** that prove a test reached a branch. Both need a
  dependency approval in `docs/dependencies.md` first (r16 51, 52).
- **Wake protocols and lock-free code** get loom for small models and shuttle (PCT)
  for larger ones. Only `ring` gates std types behind `cfg(loom)`. Code with `unsafe`
  runs under Miri (r16 61). `cargo xtask loom` builds the tests in release mode with
  `--cfg loom`. Then it runs, with `LOOM_MAX_PREEMPTIONS=3`, each test target that
  compiles a file that names `loom` in a `cfg`, oracles included. `cargo xtask
  shuttle` does the same with `--cfg shuttle`. `cargo xtask miri` runs Miri on each
  crate whose source names `unsafe_code`, and fails when such a crate runs no tests.
- **Hot paths** run under a counting allocator that fails on any allocation.
- **Unit tests are co-located** in a `#[cfg(test)] mod tests` block. Group by subject
  and condition with nested modules. Name each test as the behavior it checks, with
  no `test_` prefix (r16 48):
  `mod write { mod when_pool_full { #[test] fn records_a_gap() } }`.
- **Production-path tests across crates** that use only public APIs go in one
  integration binary per crate: `tests/it/main.rs` with modules, never many
  `tests/*.rs` files (r16 49).

## Oracles

Oracles live in `oracles/`: simulation invariants, P1 targets and benchmark baselines,
conformance suites, and fuzz inputs. Committed proptest failure files
(`proptest-regressions/` in each crate) are oracles too (r16 58). People own them.
Agents add to them freely and never weaken them. Weakening means a removed test or
assertion, a loosened threshold, a raised benchmark baseline, or a deleted fuzz input
or proptest failure file.

An oracle test target is a `[[test]]` target whose root is under `oracles/`.
`cargo xtask oracles` fails when no oracle test target compiles a `.rs` file under
`oracles/`, or when an oracle test target runs no tests.

Each PR description has an oracle section that lists changes under `oracles/`
and flags any weakening. A fresh adversarial reviewer checks each flagged change and
argues for fixing the code instead.

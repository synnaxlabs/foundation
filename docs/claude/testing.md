# Testing

Testing is the bedrock of Foundation. A change without tests is not done. "(r16 N)"
names rule N in `docs/research/r16-rust-guides.md`.

## Injection

Every component gets clock, network, disk, and randomness as inputs (`env`). Production
passes the real ones. Tests pass the simulated ones from `sim`. Nothing reads the OS
clock, the network, the disk, or a random source directly. Clippy's `disallowed-methods`
list in `clippy.toml` enforces this. Only `os` implements the `env` seams and calls the
OS, sockets included. Some tests must reach the OS, such as a test of `os`, a process
test (Process tests), and a test that bounds its own run time or reads a file of the
repository. Each call of such a test that the list bans carries
`#[expect(clippy::disallowed_methods, reason = "...")]` with its reason.

A simulated run never reads OS randomness, OS time, or a random hash order (r16
43-46). Use `types::hash::Map` and `Set`. Never let hash iteration order decide
behavior. Never print a pointer. No `thread_local!` state. Four exceptions: TLS draws
its own randomness from aws-lc (TLS RANDOMNESS in
`docs/decisions/transport/tls-randomness.md`). `sim::Sim::new` reads `Instant::now` once
as the epoch of the run, and only differences from it are read. The `hyper` server of
HTTP SIM SERVER reads OS time into a `thread_local!` on each poll, only for the `date`
header, which is off. It gets no `timer`, so no read changes what it does (the person,
https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6042756353,
2026-10-07T17:04:29Z). The random state `UA_rng` of the open62541 copy is one per
thread, so `connector-opcua` sets its start value on each thread that calls open62541
(OPEN62541 SOURCE in `docs/decisions/connectors/open62541-source.md`).

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
checks on each PR that agent-written tests catch real changes. A missed mutant fails CI.
A mutant that makes a test hang (a timeout) counts as caught. Each run on a box runs in
a cgroup with a memory cap: `systemd-run --user --scope -p MemoryMax=<share> -p
OOMPolicy=continue cargo mutants --jobs 4 ...`. A test run on a box of a mutant made by
hand (`.claude/agents/reviewer.md`) runs in the same cap, with `cargo test` in place of
`cargo mutants`. The share is 20G on box1 and 10G on box2, and a session runs one
mutants run at a time (`laptop.monitor`,
https://github.com/synnaxlabs/foundation/issues/803#issuecomment-6043431001,
2026-10-07T17:40:51Z). A mutant that allocates in a loop then dies alone, its test
fails, and the run counts it as caught. With no cap, the mutant fills the box and the
run stalls. Set `OOMPolicy=continue`, because the default of the user manager stops the
whole scope. Never cap a run on a box with `prlimit --data`: it counts reserved memory,
not touched pages (#803,
https://github.com/synnaxlabs/foundation/issues/803#issuecomment-6009258555,
2026-10-06T04:20:13Z). CI keeps it until each runner host has the cgroup cap (#899). An
assertion through a private field or call, or a compare of the `Debug` string of the
type under test, is never the only kill. `.cargo/mutants.toml` lists the few functions
it skips. Each entry is as narrow as one function. Its comment says why no caller or
peer can see the mutant, or names the test that kills it in a job that the mutants run
does not see (Miri, loom, another OS). A mutant that a test could kill but none does
links its open issue. Miri and cargo-fuzz run on one pinned nightly, named in
`rust-toolchain-nightly`, that only those gates use.

## Process tests

A process test starts the `foundation` binary. It checks the wiring of the `os` seams
and what a user sees (exit codes, standard output and error, `foundation status`), not
logic that a simulated test can reach.

- It lives in `crates/node/tests/it/`: Cargo sets `CARGO_BIN_EXE_foundation` only for
  the integration tests and benchmarks of `node`.
- It takes its clock from `os`, so the `disallowed-methods` list holds for it too.
- A test that starts a node makes its own temporary data directory with `std::fs`, as
  the tests of `os` do, and removes it at the end.
- Each listener of the node binds port 0 on loopback, and the test reads the port it
  got. No fixed port, and nothing outside loopback.
- Each wait (for output, for an exit, for a condition) ends at a deadline, and a missed
  deadline fails with the state it saw. Never a fixed sleep.
- The test ends each process it starts, also when it fails.
- A defect that a process test finds gets its regression test in simulation when the
  seams of `sim` can make it happen.

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
  diagnosed, then fix the code. A fix of a test that fails only sometimes is a bug fix
  too: a regression test that the PR commits makes the cause happen on each run. A
  failure that a session makes only outside the committed tests, such as in the
  breaker's worktree, does not count as its regression test.
- **Test what the change is for.** When a change exists to remove work (a clock read, a
  copy, an allocation, a round trip), a test counts that work and fails when the change
  is reverted.
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
- **A test may check what another test checks.** "No defense in depth" in `CLAUDE.md`
  is about guards in production code. It is no reason to refuse an assertion in a test
  or a benchmark because another test catches the same change.
- **Assert through the public calls of the type.** A read of a private field, or a
  compare of the `Debug` string of the type under test, checks private state: a new
  field breaks the test while the behavior stays. Use one only with a written reason.
- **Test-only constructors and hooks sit behind the `sim` feature**, also a hook that
  only a bench or a fuzz target uses. A crate has no second test feature (r16 57), so
  one check can find a `default` feature that turns on a hook (#1570, `laptop.director`,
  https://github.com/synnaxlabs/foundation/issues/1570#issuecomment-6049919776,
  2026-10-08T00:55:11Z).
- **Test both spaces:** valid input, invalid input, and data that goes bad (truncated
  frames, bad offsets, stale fences) (r16 54). A check against a bound has a test at
  the bound and one on each side. `cargo mutants` turns `>=` only into `<`, so it
  cannot show that the test past the bound is missing.
- **Pair assertions.** Check data before it goes to disk or the wire, and again after
  it comes back (r16 55).
- **No tautological tests.** Never repeat the implementation's formula or assert that
  a constant equals itself. Assert properties: order, round trip, bounds (r16 56).
- **No `#[ignore]`.** A known bug is a test that asserts today's wrong result, with a
  comment and an issue link (r16 53). One exception: an `acceptance` scenario waiting on
  a surface is `#[ignore = "waits on #<n>"]`.
- **One `check` helper per feature under test.** Inputs and expected output are data,
  so a signature change edits one helper (r16 50).
- **A fixture helper is `create_*`.** A helper that builds the state a test runs
  against is one: a resource (a pool, a store, files) or a collection that it fills (an
  interner, the members of a region), also when it writes nothing. A helper that turns
  its arguments into one value (`key(slot)`, `message(from, to, body)`), or builds the
  type under test (`index()`, `carried(2)`), is named for that value.
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
- **Hot paths** of product code (`docs/claude/performance.md`) run under a counting
  allocator that fails on any allocation.
- **Unit tests are co-located** in a `#[cfg(test)] mod tests` block. Group by subject
  and condition with nested modules. Name each test as the behavior it checks, with
  no `test_` prefix (r16 48):
  `mod write { mod when_pool_full { #[test] fn records_a_gap() } }`.
- **Production-path tests across crates** that use only public APIs go in one
  integration binary per crate: `tests/it/main.rs` with modules, never many `tests/*.rs`
  files (r16 49). A counting allocator is global, so each type of it gets one more
  binary with no harness, for example `tests/alloc` (`counting::Allocator`) or
  `tests/memory` (`counting::Bytes`). Helpers that binaries share go in
  `tests/common/mod.rs`.

## Oracles

Oracles live in `oracles/`: simulation invariants, P1 targets and benchmark baselines,
conformance suites, and fuzz inputs. Committed proptest failure files
(`proptest-regressions/` in each crate) are oracles too (r16 58). People own them.
Agents add to them freely and never weaken them. Weakening means a removed test or
assertion, a loosened threshold, a raised benchmark baseline, or a deleted fuzz input or
proptest failure file. A change to the bytes of a fuzz input deletes the old input: keep
the old file and add the new bytes as a new file. Each target's corpus,
`oracles/fuzz/<target>/`, is its own oracle, and only a byte string that `main` held
counts. When a PR renames or splits a target, an input of its corpus may move to the
corpus of each target that replaces it. Such a move, or a move inside one corpus, keeps
the bytes and is not a deletion. A byte string that only a PR branch held, such as the
old bytes of an input that a PR adds and then changes before it merges, was never an
oracle. A PR deletes an input when a byte string that `oracles/fuzz/<target>/` holds at
its merge base with `main` (`git merge-base origin/main HEAD`) is in no file of
`oracles/fuzz/<target>/` at its head, unless a target replaces it and the corpus of each
such target holds it. An audit of `main` takes each state that
`git log --first-parent origin/main -- oracles/fuzz/<target>` lists: a byte string that
`oracles/fuzz/<target>/` holds in one state, and that no file of it holds on
`origin/main`, was deleted, unless a target replaces it and the corpus of each such
target holds it on `origin/main`
(https://github.com/synnaxlabs/foundation/issues/1582#issuecomment-6045551500,
2026-10-07T19:46:25Z).

An oracle test target is a `[[test]]` target whose root is under `oracles/`.
`cargo xtask oracles` fails when no oracle test target compiles a `.rs` file under
`oracles/`, or when an oracle test target runs no tests.

Each PR description has an oracle section that lists changes under `oracles/`
and flags any weakening. A fresh adversarial reviewer checks each flagged change and
argues for fixing the code instead.

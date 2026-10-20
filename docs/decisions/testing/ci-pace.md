- **CI PACE (2026-10-06)** The ARM pool must not hold up the agents. The ARM workflow
  runs no loom step: loom is a software model, so the x86 `loom` job gives the same
  result. A PR run is cancelled by a newer push. A run on main is never cancelled while
  it runs; of the commits that merge during it, only the newest runs next. Each job runs
  on a fresh GitHub-hosted VM, and `Swatinem/rust-cache` keeps the build between runs
  (#2727, ARM RUNNER). Dependencies build at opt-level 2 in the dev profile; workspace
  crates stay at opt-level 0 with their checks. A PR tests only the changed crates and
  their reverse dependencies (`cargo xtask affected`); main tests the whole workspace.
  Decided by the advisor (#782). The person said: "We need to make the agentic
  engineering the bottleneck, not CI".
  Amended (the person, 2026-10-13T16:27:23Z, recorded in
  https://github.com/synnaxlabs/foundation/pull/2584#issuecomment-6150265221, to the
  proposal of `laptop.monitor` to split mutants across 4 runners: "That's fine."): a PR
  runs its mutants on four runners. The design of #2584: four jobs `mutants shard <k>`,
  each with `cargo mutants --shard <k>/4 --sharding round-robin` on its own runner, and
  the required check `mutants` passes when each shard passed. In the queue no shard
  runs, and `mutants` carries the result at the head of the PR to the group, as before.
  Each shard runs on its own hosted VM (`ubuntu-24.04`, #2727). `cargo mutants` runs in
  a systemd scope with a memory cap, no swap, and `OOMPolicy=continue`, so a mutant that
  allocates in a loop kills only its test, and the shard reports the mutant. Cost: each
  shard that gets a mutant builds and tests the code without mutants, so a PR with eight
  or more mutants spends eight baselines in place of one.
  Amended (`laptop.monitor`, 2026-10-15, for the person: "Each mutant shard is taking
  45 min + in some cases."): eight shards in place of four, `--shard <k>/8`, as hosted
  runners are free. The test timeout is 3 times the test time of the baseline
  (`--timeout-multiplier 3`), not the default 5, so a mutant that hangs costs about 14
  min, not 23.5. Four jobs on 4 vCPU can run slower than the baseline, so not lower.
  Amended (#2756): `sim` builds at opt-level 2 in the dev profile, with its checks. It
  is a dev-dependency only. At opt-level 0 it took most of the time of the `mesh`
  tests: `cargo test -p mesh --all-features` took 342 s of user CPU on box2 (AWS Xeon
  Platinum 8488C), and 79 s at opt-level 2. OK of `laptop.architect` as the advisor
  (2026-10-15T21:31:59Z):
  https://github.com/synnaxlabs/foundation/issues/2756#issuecomment-6202718051, and of
  `laptop.architect-2` for `sim` (2026-10-15T21:32:24Z):
  https://github.com/synnaxlabs/foundation/issues/2756#issuecomment-6202725115.
  Amended (#3135): Miri runs on eight hosted runners, `miri shard <k>`, each with `cargo
  xtask miri <k>/8`, and the required check `miri` passes when each shard passed. For
  each set of `MIRIFLAGS`, `cargo miri nextest` runs the lib tests of the partition
  `hash:<k+1>/8`, and fails when it runs none. `cargo miri test` runs the doc tests of
  each crate that has a doc test that Miri runs and each other test target whole, and
  fails when a run of the doc tests or of a target with a harness runs none. A crate
  that names `unsafe_code` and runs no test under Miri fails. These units go in order of
  target, then of flags, and shard `k` takes each eighth unit from unit `k`. Ruling of
  `laptop.director` (nextest in place of our own runner), 2026-10-18T07:07:27Z:
  https://github.com/synnaxlabs/foundation/pull/3151#issuecomment-6246962539. OK of
  `laptop.director` that only the lib tests go to nextest, 2026-10-18T08:27:25Z:
  https://github.com/synnaxlabs/foundation/pull/3151#issuecomment-6247629164. Trigger to
  send the other targets with a harness to nextest: a Miri crate gets a slow target that
  has a harness. The one job took 35 min (run 39096947760). In run 39109178137 the Miri
  step of the slowest shard took 415 s with `hash`, 700 s with `count`, and 709 s with
  `slice`. OK of the person for cargo-nextest, 2026-10-18T14:39Z, through
  `laptop.monitor`:
  https://github.com/synnaxlabs/foundation/pull/3151#issuecomment-6251116435. The person
  approved `ci.yaml` on GitHub at `1697708a8`.
  Amended (the person, through `laptop.monitor`, recorded in #3418 at
  2026-10-19T19:46:19Z: "Let's do it ASAP"; the plain test paths and the test targets
  with no harness, the plan, 2026-10-19T19:54:31Z,
  https://github.com/synnaxlabs/foundation/issues/3418#issuecomment-6275974482; the
  job `os`, the third part, and ARM, `laptop.monitor`, recorded at
  2026-10-19T20:12:18Z,
  https://github.com/synnaxlabs/foundation/issues/3418#issuecomment-6276305229): the
  job `check` runs three parts on each OS at the same time: `lint` (Format, Clippy,
  Layers, Globals, Skips, Oracles), `test` (`cargo xtask test nextest`, Root tests),
  and `serial` (`cargo xtask test serial`). The job `os` gives the required checks
  `check`, `macos`, and `windows`, each of which passes when each part of `check` on
  each OS passed. `cargo xtask test nextest` runs `cargo nextest run` with no retry and
  no fail-fast. `cargo xtask test serial` runs `cargo test --doc`, and `cargo test` on
  each `test` or `bench` target that `cargo test` runs and whose root has a `main`, as
  `cargo xtask skips` reads them, other than in a package that `--exclude` leaves out:
  nextest runs such a binary to list its tests, and the binary lists none or prints a
  table that nextest cannot read. The test targets of these took 260 s on Linux, 302 s
  on macOS, and 183 s on Windows in queue run 39308250221, over the 3 min that
  `laptop.monitor` gave for a third part. The job `test` of `arm.yaml` runs both parts,
  one after the other. Each skipped test of the lists of LINUX CI is a full test path
  that nextest leaves out with `test(=<path>)`: nextest 0.9.146 skips no test with
  `--exact --skip`. In queue run 39308250221, `check` took 32 min, `macos` 35 min,
  `windows` 28 min, and the `test` of ARM 22 min, and on Linux `cargo test` ran 173 test
  binaries and 40 doc-test sets one after another.
  Amended (`laptop.monitor`, recorded at 2026-10-20T01:31:43Z,
  https://github.com/synnaxlabs/foundation/issues/3448#issuecomment-6281216123; the
  jobs `parts` and `test` and their skip, the plan, 2026-10-20T00:42:23Z,
  https://github.com/synnaxlabs/foundation/issues/3448#issuecomment-6280595198): ARM
  runs the two parts at the same time. The matrix job `parts` of `arm.yaml` runs `cargo
  xtask test nextest` and `cargo xtask test serial` on two `ubuntu-24.04-arm` runners,
  and the job `test`, on `ubuntu-24.04`, gives the required check `test`, which passes
  when both parts passed. Both skip when the filter found no change of code. Supersedes
  "The job `test` of `arm.yaml` runs both parts, one after the other." of
  https://github.com/synnaxlabs/foundation/issues/3418#issuecomment-6276305229, and the
  name `test` of the ARM job in ARM RUNNER. In run 39335833233 the one job took 21 min.
  In run 39351739537, `nextest` took 12.7 min (11762 passed, 12 skipped), `serial` 8.2
  min (79 test targets with no harness, 235 doc tests), and the workflow 13.1 min; in
  run 39349284591, 13.1, 11.1, and 13.4 min. No part of these three runs found a cache.
  Each part builds on its own runner, and ARM keeps a cache entry for each part in
  place of one.

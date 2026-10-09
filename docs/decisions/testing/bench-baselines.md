- **BENCH BASELINES (2026-10-06)** The committed baselines and the CI bench job with
  the 5% check (P1) come with #715. Until then, a PR that touches a hot path gives its
  benchmark results, with the machine named. When a result is near 5%, `laptop.monitor`
  runs it again on a quiet Linux host; a result still over 5% needs the P1 judgment.
  Once a day, `laptop.monitor` runs the hot-path benchmarks on a quiet Linux host
  against a fixed commit, which finds a slowdown that no PR expected. `laptop.monitor`
  runs both because it alone rents cloud machines (`laptop.monitor`'s record of the
  person's decision, 2026-10-07T16:48:27Z:
  https://github.com/synnaxlabs/foundation/issues/15#issuecomment-6042582552).
  Supersedes the runs by the coordinator in
  https://github.com/synnaxlabs/foundation/pull/894. Patch; #715 is the long-term fix.
  The person decided on 2026-10-06 ("I am ok with deferring #715").
  Amended (2026-10-09): a PR that closes or builds part of an issue on the FIRST SLICE
  milestone merges with no rerun on a quiet host. After it merges, its author sends
  `laptop.monitor` the PR number, and `laptop.monitor` runs the hot-path benchmarks on a
  quiet Linux host at the merge commit and at its parent on `main`. A slowdown over 5%
  is an issue for the PR's author, who fixes it next. The daily run stays. Each other PR
  keeps the rule above. Decided by the person, as `laptop.coordinator` recorded it from
  `laptop.monitor` (2026-10-09T01:05:23Z,
  https://github.com/synnaxlabs/foundation/issues/462#issuecomment-6072145282).
  Amended (2026-10-09, #2044): when each changed hot function compiles only on an OS for
  which no quiet host exists, the PR gives a diff of the disassembly in place of the
  rerun. It covers each such function, with its inlined callers, at `main` and at the PR
  head, built with the toolchain and profile of the bench binaries, and names the target
  and both commits. The diff only removes instructions: no added instruction, call,
  loop, or memory access. The runs with the machine named stay in the PR. A change that
  adds anything on such a path waits for a quiet host of its OS. Decided by
  `laptop.director` for #2044 at 2026-10-09T00:55:15Z
  (https://github.com/synnaxlabs/foundation/pull/2044#issuecomment-6072038524), and made
  a rule for each such PR by item 3 of
  https://github.com/synnaxlabs/foundation/issues/2041, as that ruling says.
  Amended (2026-10-09): each one-host quiet-host run builds both commits with
  `RUSTFLAGS="-C target-cpu=x86-64-v2 -C llvm-args=-align-all-functions=6"`, and its
  result names these flags beside the machine. The two builds of one comparison, also in
  the daily run against its fixed commit, use the same flags. An aligned result over 5%
  still needs the P1 judgment. `scripts/bench-host.sh` passes these flags after the item
  of #1139
  (https://github.com/synnaxlabs/foundation/issues/1139#issuecomment-6074402988). The
  two-host carrier bench builds as `bench/carrier/run.sh` does, until a carrier result
  over 5% is one that the P1 judgment does not find in the code of the PR: then the
  script builds with the same flags. Decided by `laptop.director`, 2026-10-09T02:44:36Z:
  https://github.com/synnaxlabs/foundation/issues/2041#issuecomment-6073190937, with its
  scope at 2026-10-09T02:50:37Z:
  https://github.com/synnaxlabs/foundation/issues/2041#issuecomment-6073252079.

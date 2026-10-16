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
  keeps the rule above. The person (2026-10-09T01:04:51Z), on the question of
  `laptop.monitor` at 2026-10-09T01:04:26Z: "Approved" (`laptop.monitor`'s record:
  https://github.com/synnaxlabs/foundation/issues/462#issuecomment-6074405652).
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
  still needs the P1 judgment. `scripts/bench-host.sh` passes the flags (item of #1139,
  https://github.com/synnaxlabs/foundation/issues/1139#issuecomment-6074402988). The
  two-host carrier bench builds as `bench/carrier/run.sh` does. A carrier result over
  5% that the P1 judgment does not find in the code of the PR is the trigger for a
  change of the script. Decided by `laptop.director`, 2026-10-09T02:44:36Z:
  https://github.com/synnaxlabs/foundation/issues/2041#issuecomment-6073190937, with its
  scope at 2026-10-09T02:50:37Z:
  https://github.com/synnaxlabs/foundation/issues/2041#issuecomment-6073252079.
  Amended (2026-10-10T00:24Z): each 5% in this record is now the cost limit of P1
  (`docs/decisions/memory/p1.md`), and each "over 5%" is "at or over the cost limit". A
  result is near the limit when it is at or over the limit and the noise of its run
  could put it under. A slowdown under the limit needs no rerun, no judgment, and no
  issue. The daily run stays: it finds small slowdowns that add up. The person decided
  on 2026-10-10T00:24Z ("Yes, I approve it"; `laptop.monitor`'s record:
  https://github.com/synnaxlabs/foundation/pull/2196#issuecomment-6091584600).
  Supersedes the 5% of
  https://github.com/synnaxlabs/foundation/issues/462#issuecomment-6074405652 and
  https://github.com/synnaxlabs/foundation/issues/2041#issuecomment-6073190937.
  Amended (2026-10-11): a one-host quiet-host run that compares two commits makes three
  passes or more of each commit. A pass runs the benches once at one commit. The time of
  a bench at a commit is the median of the medians of its passes, and one pass alone
  gives no time: the host builds each commit once, before the first pass
  (`scripts/bench-host.sh`), so a difference between two passes of one commit is the
  host, not the code. The result of a bench is the slowdown of its head time against its
  base time, and a result at or over the cost limit still needs the P1 judgment. The
  check of two passes of one binary in WAL BENCH
  (`docs/decisions/delivery/wal-bench.md`) stays. Decided by `laptop.director`,
  2026-10-11T05:56:21Z:
  https://github.com/synnaxlabs/foundation/issues/2041#issuecomment-6106022275, on the
  reading of `laptop.architect` in
  https://github.com/synnaxlabs/foundation/pull/2327#issuecomment-6105920017, with the
  count, the slowdown, and the scope at 2026-10-11T06:21:14Z:
  https://github.com/synnaxlabs/foundation/issues/2041#issuecomment-6106196216, and with
  the words "pass", "time", and "result" at 2026-10-11T06:41:22Z:
  https://github.com/synnaxlabs/foundation/issues/2041#issuecomment-6106336614.
  `scripts/bench-host.sh` makes two passes of each commit, so its report alone gives no
  result. Until #2367 changes it, `laptop.monitor` makes the passes as in
  https://github.com/synnaxlabs/foundation/pull/2327#issuecomment-6104945553.
  Amended (2026-10-11): a result at or over the cost limit needs no rerun and no P1
  judgment when each function that holds product code that its line runs in its timed
  loop has the same instructions at both commits. The PR gives the compare of the two
  bench binaries, built with the toolchain, the profile, and the flags of the run, with
  the absolute addresses removed, and names the target and both commits. A function that
  differs, also in one constant, keeps the rule above. Decided by `laptop.director`,
  2026-10-11T15:58:29Z:
  https://github.com/synnaxlabs/foundation/issues/2041#issuecomment-6110871216.
  Amended (2026-10-11): each one-host quiet-host run builds both commits with
  `-C panic=abort` beside the flags above, which is the panic strategy of the release
  profile (R16-2), and its result names the flag. The CI bench job of #715 builds its
  benches the same way. Decided by `laptop.director`, 2026-10-11T17:23:57Z:
  https://github.com/synnaxlabs/foundation/issues/2041#issuecomment-6111666626.
  `scripts/bench-host.sh` does not pass the flag. Until #2367 adds it, `laptop.monitor`
  adds it to the flags of each run (`laptop.director`, 2026-10-12T15:56:34Z:
  https://github.com/synnaxlabs/foundation/issues/2041#issuecomment-6127823084). A run
  with the flag names its bench targets (`--bench`), because the `lib test` target of a
  crate does not build with it (`laptop.director`, 2026-10-12T15:56:11Z, edited at
  2026-10-12T18:32:41Z:
  https://github.com/synnaxlabs/foundation/issues/2367#issuecomment-6127816867).
  Amended (2026-10-16): each compare of a PR in this record, also the diff of the
  disassembly of the #2044 amendment, compares the base of the PR with its head, as
  `.claude/agents/performance.md` states the base. Decided by `laptop.director`,
  2026-10-16T09:02:58Z:
  https://github.com/synnaxlabs/foundation/pull/2846#issuecomment-6211989872. Supersedes
  "at `main`" of the #2044 amendment above.

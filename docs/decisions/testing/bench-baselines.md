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

- **BENCH BASELINES (2026-10-06)** The committed baselines and the CI bench job with
  the 5% check (P1) come with #715. Until then, a PR that touches a hot path gives its
  benchmark results, with the machine named. When a result is near 5%, the
  coordinator runs it again on a quiet Linux host; a result still over 5% needs the P1
  judgment. Once a day, the coordinator runs the hot-path benchmarks on a quiet Linux
  host against a fixed commit, which finds a slowdown that no PR expected. Patch; #715
  is the long-term fix. The person decided on 2026-10-06 ("I am ok with deferring
  #715").

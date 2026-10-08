- **CLOCK SUSPEND (2026-10-05)** `env::clock` counts time asleep (`CLOCK_BOOTTIME` on
  Linux, `mach_continuous_time` on macOS). After a suspend, the error has grown by
  drift over the sleep, and `clock` needs no reset. A monotonic clock that stops in
  suspend lost: mesh time would fall behind by the time asleep, outside its bound.
  Amends ENV SEAMS. The person decided on 2026-10-05 ("Count time asleep"), #144. On
  Linux the read is `CLOCK_MONOTONIC_RAW` plus the time asleep (`CLOCK_BOOTTIME` minus
  `CLOCK_MONOTONIC`), because time daemons slew `CLOCK_BOOTTIME` faster than 200 ppm
  (chrony up to 83,333 ppm), and a bound that grows at 200 ppm then misses the true
  time. The driver in `os` keeps the largest time asleep it has read, so reads never go
  back (#117). Cost, Amazon Linux 2023: 73 ns against 24 to 29 ns for one
  `CLOCK_BOOTTIME` read on c7i.large (x86-64), and about 100 ns against 30 ns on
  c7g.medium (arm64, a noisy run); the shared maximum adds about 1 ns (M3 Max). macOS
  is not hit: no daemon slews `mach_continuous_time`. Lost: `CLOCK_BOOTTIME` with a
  rule that the daemon slews within a limit, because a node cannot check it;
  `CLOCK_BOOTTIME` with a bound that can fail in a fast slew. The person decided on
  2026-10-06 ("yes"), #688. On macOS `os` reads `CLOCK_MONOTONIC_RAW`, which is
  `mach_continuous_time`. Each `os::clock()` call starts a new clock at 0, so `node`
  calls it once. Tokio's timer stops in a suspend on macOS and slews on Linux, so `os`
  arms it for at most 1 s at a time: a sleep across a suspend completes up to 1 s late.
  The vDSO and the macOS commpage read the counter with no fence that waits for earlier
  stores or holds back later loads, so `os` adds them to give the order that
  `env::clock` requires (#458): `mfence; lfence` before the read and `lfence` after on
  x86-64, `dsb ish; isb` before and `isb` after on arm64. Other architectures do not
  build. Cost: 38 ns against 16 ns for one read without the fences (M3 Max) (#117).

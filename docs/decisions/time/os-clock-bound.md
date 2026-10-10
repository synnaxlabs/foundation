- **OS CLOCK BOUND (2026-10-05)** The OS wall clock is a source. `env::wall` gives the
  OS error bound with each reading where the OS has one (`adjtimex` on Linux,
  `ntp_gettime` on macOS). Where it has none (Windows), `env::wall` gives `None`, and
  `clock` reads that as `Measurement::unknown` (36500 days): a node alone with no known
  estimate (CLOCK HOLDOVER) still gets OS time, with an error that says "unknown", and
  beside a known bound the reading does not vote (ESTIMATE COMBINE). A fixed invented
  error lost: a wrong value gives a bound that is not true. Amends ENV SEAMS. The person
  decided on 2026-10-05 ("Use it, error 'unknown'"), #144. The split between `env::wall`
  and `clock` is from #172. When known
  peers split, `combine` fails, so the clock is unsynced before its first estimate and
  holds over after it (CLOCK HOLDOVER). A known OS bound still votes. Dropping the OS
  source in `clock` when a peer exists lost: it also drops a narrow OS bound (Linux,
  macOS). The person decided on 2026-10-05 ("314 should be (b)"), #314. `clock` gives
  the OS reading to the exchange as an interval, its time plus or minus its bound, so
  the error is never less than the OS bound. An error of 36500 days or more reads as
  unknown, the same as no bound (the coordinator, #144). So does an edge of the bound
  past the range of a stamp, centered at the reading as with no bound: a known bound
  with such an edge needs a reading after 2162 or before 1777, so it cannot hold a true
  time between those years. The coordinator approved it with the advisor, #910. Lost:
  the edge stopped at the range, because it narrows a bound of 36500 days or more into a
  known one; edges in `i128` through a new `estimate` input, because it keeps a false
  bound that votes (#314); `Measurement::widened`, a public item that keeps the OS
  reading a special path. Only `clock` and `node` call `clock::source::Wall::measure`; a
  lint denies it elsewhere (BQ20). On Linux the bound is the kernel's `maxerror`, and
  only chrony and ntpd compute it. `systemd-timesyncd` sets it to 0 at each update,
  while the clock can still be 0.4 s off. So a known OS bound on Linux needs chrony or
  ntpd, and the operator docs must say so. A host with timesyncd (the default on Debian)
  gives a false bound until its operator installs chrony. Lost: the Linux bound always
  unknown, because it also drops the good bound from chrony and ntpd; detecting
  timesyncd, because it reaches outside `env::wall` and is a guess. The person decided
  on 2026-10-06 ("A is still fine"), #689. `os` also gives `None` in clock state
  `TIME_ERROR`, and for a negative `maxerror` or one past the end of a `Span`, because
  root can set any value. `os::wall()` reads once and returns `Error::Wall` when the OS
  refuses the call, as a seccomp filter or systemd's `ProtectClock` can. A later
  refusal panics (#117).

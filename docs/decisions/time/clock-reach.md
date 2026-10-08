- **CLOCK REACH (2026-10-08)** `clock::Reader::reach(at) -> impl Future<Output = ()>`
  waits until the latest edge of mesh time is at or after `at`, and is never early: a
  read inside the wait gave such an edge. A later read can give an earlier edge, when
  the error shrinks. A drop cancels the wait. (1) One edge, the latest, as SUBJECT PROOF
  ends a hello; if #274 ends a hold on the earliest edge, it adds an edge argument
  through an interface change. (2) It reads mesh time, sleeps on the monotonic clock
  until the soonest reading at which the slew of that read moves the latest edge to
  `at`, and reads again. Under one slew it is late only by the timer; a new slew shows
  at the next read. (3) Before the first mesh time it reads again each second, the
  period of the OS source, from its own constant. (4) `estimate::Slew::reach(now, at,
  drift) -> Monotonic` gives that reading. It has no `None`: at the last reading the
  edge is at the end of a stamp's range. The edge is not monotonic (a downward slew
  moves it back at each tick), so it steps by the most the edge can rise: `j * (1 +
  rate) + 1` ns over `j` ns, with `rate` the larger of the drift and 500 ppm. Lost: an
  edge argument now; a wake from the writer on each change of the discipline (a wake for
  each waiter each second, and a waker list between shards); a cap on each sleep, such
  as 1 s (about 900 wakeups for each 15-minute hello on each link); a `Sleep` type with
  `reset` for a renewal; `Reader::when(at) -> Option<Monotonic>`, which keeps the loop
  in each caller; an accessor of the monotonic clock on `Reader`, and
  `hub::Config::clock`. Decided by `laptop.architect-2` in the body of #1870,
  2026-10-08T11:30:05Z (https://github.com/synnaxlabs/foundation/issues/1870), and the
  plan of `Slew::reach` at 2026-10-08T11:36:10Z
  (https://github.com/synnaxlabs/foundation/issues/1870#issuecomment-6058989780). The
  plan had `Option<Monotonic>`; `laptop.architect-2` approved the return with no `None`
  (https://github.com/synnaxlabs/foundation/issues/1870#issuecomment-6059089435) at
  2026-10-08T12:02:17Z
  (https://github.com/synnaxlabs/foundation/pull/1874#issuecomment-6059414350).

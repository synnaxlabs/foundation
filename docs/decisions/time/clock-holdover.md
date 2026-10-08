- **CLOCK HOLDOVER (2026-10-05)** Before its first estimate, the clock is unsynced and
  a reader gets no mesh time. After it, when `combine` fails (no majority, or no sources
  after a remove), the clock holds over: it keeps its last estimate and its error grows
  by drift. It never follows the largest group or one side of a tie. `Reader::status`
  gives the status on any shard, and `node` publishes it. `push` and `remove` do not
  also return it: one value gets one way to read it (#634). The next majority ends the
  holdover, but after a known estimate only a known one does. Decided by the `time`
  builder (#142). The coordinator approved `Reader::status` within it (#598).
  `estimate::discipline` chooses what mesh time follows, and `clock` writes it, so the
  decision logic is in layer 1 (#635). An unknown estimate never replaces a known one:
  after a known estimate, when only unknown bounds agree, the clock holds over until a
  known estimate. The person decided on 2026-10-05 ("a is fine"), #489. Known is as
  `combine` sorts a bound: under 36500 days at the estimate's time. So when drift grows
  the held bound to 36500 days, the clock follows an unknown estimate. Decided by the
  `time` builder (#835).

- **CLOCK RUN (2026-10-05)** Within TIME ADAPTERS. `clock::Clock::run` runs all time
  sources of one clock in one task on the clock's shard. `node` builds the source table
  and passes it to `run`. Today the table is the OS clock. Each adapter keeps its own
  loop and decides when it measures: the OS clock at once, then one second after the
  last measurement, so once after a suspend. `run` adds a source for each adapter and
  pushes each measurement. `run` owns every source, so it panics on a clock that has a
  source already: nothing could push to that source. A reader gives the status
  (`Reader::status`, #598). Lost: a task for each adapter with a shared clock
  (`Rc<RefCell>` or a queue), because then the caller shares the clock; the loop in
  `node`, because the peer exchange adds and removes sources, and `node` would pass its
  events through. Decided by the `time` builder (#144, #600). The coordinator approved
  it on #144 and #600.

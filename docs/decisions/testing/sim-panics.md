- **SIM PANICS (2026-10-05)** A panic in a poll or in the drop of a future ends the
  thread and the run with `Error::Panicked`, and the thread's other futures drop. Each
  future drops in its own `catch_unwind`, after the unwind of a panic in its poll, so a
  panic in its drop does not unwind into the unwind of that panic. As anywhere in Rust,
  a panic that unwinds into the unwind of another panic aborts the process (#871,
  approved by `laptop.architect`, 2026-10-09T01:09:48Z,
  https://github.com/synnaxlabs/foundation/pull/2033#issuecomment-6072192755). The error
  gives every panic, the first one first: a drop that panics is a defect of its own,
  even when an earlier panic caused the drop. A panic in the drop of a panic payload is
  one more panic. At most 16 payloads of one chain drop, and the payload past them is
  forgotten, so that a drop that always panics cannot hang the run. `os` drops panic
  payloads with the same bound. `Sim::crash` panics with the same messages after the
  crash ends. At a crash, the start of each thread that has not run drops the same way,
  after the futures. A thread that a drop starts on the crashing node ends in the crash
  and never runs. Built by `simulation` in #548, #666, and #870.

- **SIM DROP (2026-10-06)** The drop of a `Sim` drops each live future in its own
  `catch_unwind`. If any panicked, it then panics once with every message, the first
  one first, but only when the thread is not already panicking. This is an exception to
  the rust.md rule "`Drop` never panics": to print the messages and not fail would hide
  a defect. The person said: "An exception for the simualtor is fine" (#555).

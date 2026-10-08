- **TEST DROP (2026-10-08)** A test helper under `tests/` may panic or block in its
  `Drop`, but only when the thread is not already panicking. A `Drop` that does not
  panic hides a failure when a test does not stop the helper. This is the second
  exception to the rust.md rule "`Drop` never panics", after SIM DROP. The person
  chose it: "B is fine" (2026-10-08 23:21 UTC, as `laptop.monitor` relayed it,
  recorded at
  https://github.com/synnaxlabs/foundation/pull/2004#issuecomment-6071002339; the
  question: https://github.com/synnaxlabs/foundation/pull/2004#issuecomment-6070953004).

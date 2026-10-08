- **TEST DROP (2026-10-08)** A test helper under `tests/` may block in its `Drop`, and
  may panic in it only when the thread is not already panicking. A `Drop` that does not
  panic hides a failure when a test does not stop the helper, and a second panic aborts
  the test binary. This is an exception to the rust.md rule that `Drop` never panics and
  never blocks. SIM DROP is the other. The person chose TEST DROP: "B is fine"
  (2026-10-08 23:21 UTC, as `laptop.monitor` relayed it, recorded at
  https://github.com/synnaxlabs/foundation/pull/2004#issuecomment-6071002339; the
  question: https://github.com/synnaxlabs/foundation/pull/2004#issuecomment-6070953004).
  A type in test code (under `tests/` or in a `#[cfg(test)]` module) whose `Drop`
  panics or blocks as the input of its test, such as `Bomb` in
  `crates/os/tests/it/common.rs`, is not a helper, and the `Drop` rule of rust.md does
  not hold for it. Decided by `laptop.director`
  (https://github.com/synnaxlabs/foundation/pull/2036#issuecomment-6071198900,
  2026-10-08 23:39 UTC, widened at
  https://github.com/synnaxlabs/foundation/pull/2036#issuecomment-6071294220,
  2026-10-08 23:47 UTC).

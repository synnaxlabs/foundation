- **TEST DROP (2026-10-08)** A test helper under `tests/` may block or panic in its
  `Drop` only when the thread is not already panicking. A `Drop` that does not panic
  hides a failure when a test does not stop the helper. A second panic aborts the test
  binary, and a helper that blocks during a panic can hang a failed test until the CI
  timeout. This is an exception to the rust.md rule that `Drop` never panics and never
  blocks. SIM DROP is the other. The person chose option b: "B is fine" (the person's
  answer at 2026-10-08 23:21 UTC, as `laptop.monitor` relayed it; recorded at
  https://github.com/synnaxlabs/foundation/pull/2004#issuecomment-6071002339, 2026-10-08
  23:22 UTC; the question:
  https://github.com/synnaxlabs/foundation/pull/2004#issuecomment-6070953004), and
  approved #2036 with this rule: "Yes approve" (as `laptop.monitor` relayed it at
  2026-10-09 01:07 UTC:
  https://github.com/synnaxlabs/foundation/pull/2036#issuecomment-6072166747). A type in
  test code (under `tests/` or in a `#[cfg(test)]` module) whose `Drop` panics or blocks
  as the input of its test, such as `Bomb` in `crates/os/tests/it/common.rs`, is not a
  helper, and the `Drop` rule of rust.md does not hold for it. Decided by
  `laptop.director`
  (https://github.com/synnaxlabs/foundation/pull/2036#issuecomment-6071198900,
  2026-10-08 23:39 UTC, widened at
  https://github.com/synnaxlabs/foundation/pull/2036#issuecomment-6071294220, 2026-10-08
  23:47 UTC).

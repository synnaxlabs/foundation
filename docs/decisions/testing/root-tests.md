- **ROOT TESTS (2026-10-07)** A test that needs `sudo` (to mount a small filesystem)
  goes in its own `[[test]]` target with `test = false`, so `cargo test`, also with
  `--all-targets`, does not run it. One step of the x86 `check` job in `ci.yaml` lints
  and runs it on a GitHub-hosted runner, which is discarded after the job. No other
  host runs it: box1, box2, and the self-hosted runners keep their state, and root
  there is a security change that only the person can make. The ARM mutants job does
  not run it, so the code that only such a test pins sits in one small function that
  `.cargo/mutants.toml` excludes, with the name of the test. First user: the `root`
  target of `os` (#1100). Decided by the architect, #1100
  (https://github.com/synnaxlabs/foundation/issues/1100#issuecomment-6031260669).

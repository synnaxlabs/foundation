//! Runs the tests of `connector-opcua` with its Rust and C under the sanitizers, which
//! check it in place of Miri.

use std::path::Path;
use std::process::Command;

use crate::libtest::{self, Run};
use crate::nightly::Toolchain;

/// The crate that this task checks, which `cargo xtask miri` skips.
pub(crate) const CRATE: &str = "connector-opcua";

/// Runs `cargo test` of [`CRATE`] with the feature `sim` and the address sanitizer, on
/// the nightly in `rust-toolchain-nightly`, for its host triple. `build.rs` of the
/// crate then builds its C with the address and undefined behavior sanitizers. It
/// does not use a `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS` of the caller, which would
/// drop the sanitizer. It fails when the nightly gives no host triple, a test fails,
/// or no test runs.
pub(crate) fn run(root: &Path) -> Result<(), Vec<String>> {
    let nightly = Toolchain::read(root).map_err(|e| vec![e])?;
    let host = nightly.host().map_err(|e| vec![e])?;
    let run = libtest::run(&mut command(&nightly, &host)).map_err(|e| vec![e])?;
    verdict(&run)
}

/// What [`run`] gives for `run`.
fn verdict(run: &Run) -> Result<(), Vec<String>> {
    match run {
        Run::Failed => Err(vec![format!("the sanitizer tests of `{CRATE}` failed")]),
        Run::Empty => Err(vec![format!(
            "`{CRATE}` runs no tests under the sanitizers"
        )]),
        Run::Passed => Ok(()),
    }
}

/// The `cargo test` of [`run`] on `nightly` for `host`.
fn command(nightly: &Toolchain, host: &str) -> Command {
    let mut command = nightly.cargo();
    // With `--target`, the build scripts and macros build with no sanitizer. The
    // flags go in `target.<host>`, which joins the `target-cpu` of
    // `.cargo/config.toml`, where `RUSTFLAGS` would drop it. Cargo takes either
    // variable of the caller in place of `target.<host>`, so both go.
    command
        .args(["test", "-p", CRATE, "--features", "sim"])
        .args(["--target", host, "--target-dir", "target/sanitizers"])
        .arg("--config")
        .arg(format!("target.{host}.rustflags=[\"-Zsanitizer=address\"]"))
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        // `alloc::tests` asks for blocks too large to give, and checks the null.
        .env("ASAN_OPTIONS", "allocator_may_return_null=1");
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_the_crate_with_the_address_sanitizer_for_the_host() {
        let nightly = Toolchain::new("/w", "nightly-x");
        let command = command(&nightly, "x86_64-unknown-linux-gnu");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "run",
                "nightly-x",
                "cargo",
                "test",
                "-p",
                "connector-opcua",
                "--features",
                "sim",
                "--target",
                "x86_64-unknown-linux-gnu",
                "--target-dir",
                "target/sanitizers",
                "--config",
                "target.x86_64-unknown-linux-gnu.rustflags=[\"-Zsanitizer=address\"]",
            ]
        );
        let envs: Vec<_> = command.get_envs().collect();
        assert_eq!(
            envs,
            [
                (
                    "ASAN_OPTIONS".as_ref(),
                    Some("allocator_may_return_null=1".as_ref())
                ),
                ("CARGO_ENCODED_RUSTFLAGS".as_ref(), None),
                ("RUSTFLAGS".as_ref(), None),
            ]
        );
    }

    #[test]
    fn verdict_fails_each_run_that_does_not_pass() {
        assert_eq!(
            verdict(&Run::Failed),
            Err(vec![
                "the sanitizer tests of `connector-opcua` failed".to_string()
            ])
        );
        assert_eq!(
            verdict(&Run::Empty),
            Err(vec![
                "`connector-opcua` runs no tests under the sanitizers".to_string()
            ])
        );
        assert_eq!(verdict(&Run::Passed), Ok(()));
    }
}

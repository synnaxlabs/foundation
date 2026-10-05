//! Runs the tests of each crate that names a cfg such as `loom`, with that cfg set.

use std::path::Path;

use crate::select;

/// Runs `cargo test --release --tests` with `--cfg <name>` and
/// `LOOM_MAX_PREEMPTIONS=3` on each workspace crate whose source names `name` in a
/// `cfg`. It passes when no crate does. It prints the test output when the tests end.
pub(crate) fn test(root: &Path, name: &str) -> Result<(), Vec<String>> {
    let metadata = crate::metadata(root).map_err(|e| vec![e])?;
    let packages =
        select::packages(&metadata, |source| select::names_cfg(source, name))
            .map_err(|e| vec![e])?;
    if packages.is_empty() {
        eprintln!("no crate names `{name}` in a cfg");
        return Ok(());
    }
    let mut cargo = crate::cargo();
    // Cargo prefers this variable to `RUSTFLAGS`, so the cfg is set even when the
    // caller sets either one.
    cargo
        .current_dir(root)
        .args(["test", "--release", "--tests"])
        .env("CARGO_ENCODED_RUSTFLAGS", format!("--cfg\u{1f}{name}"))
        .env("LOOM_MAX_PREEMPTIONS", "3");
    for package in &packages {
        cargo.args(["-p", package]);
    }
    let output = cargo
        .output()
        .map_err(|e| vec![format!("cargo test: {e}")])?;
    eprint!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() {
        Ok(())
    } else {
        Err(vec![format!(
            "tests with `--cfg {name}` failed in: {}",
            packages.join(", ")
        )])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fails_when_a_picked_crate_fails() {
        assert_eq!(
            test(&crate::fixture(), "loom"),
            Err(vec![
                "tests with `--cfg loom` failed in: a, model".to_string()
            ])
        );
    }

    #[test]
    fn passes_when_no_crate_names_the_cfg() {
        assert_eq!(test(&crate::fixture(), "shuttle"), Ok(()));
    }
}

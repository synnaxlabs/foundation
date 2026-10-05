//! Builds test executables and reads which source files rustc compiled into each.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::{field, files};

/// A test executable that cargo built.
#[derive(Debug)]
pub(crate) struct Executable {
    pub(crate) package_id: String,
    /// The name of the cargo target.
    pub(crate) target: String,
    /// The package directory, where `cargo test` runs the executable.
    pub(crate) dir: PathBuf,
    pub(crate) path: PathBuf,
}

/// Runs `cargo`, a `cargo test --no-run --message-format=json` command, and returns
/// the test executables that it built.
pub(crate) fn executables(cargo: &mut Command) -> Result<Vec<Executable>, String> {
    let output = cargo.output().map_err(|e| format!("cargo test: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot build the tests:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let mut built = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let message: Value =
            serde_json::from_str(line).map_err(|e| format!("cargo test: {e}"))?;
        if field::text(&message, "reason")? != "compiler-artifact"
            || !field::flag(&message["profile"], "test")?
        {
            continue;
        }
        let manifest = Path::new(field::text(&message, "manifest_path")?);
        let dir = manifest
            .parent()
            .ok_or_else(|| format!("{} has no directory", manifest.display()))?;
        built.push(Executable {
            package_id: field::text(&message, "package_id")?.to_string(),
            target: field::text(&message["target"], "name")?.to_string(),
            dir: dir.to_path_buf(),
            path: PathBuf::from(field::text(&message, "executable")?),
        });
    }
    Ok(built)
}

/// The Rust source files that rustc compiled into `exe`, from the dep-info file beside
/// it. Rustc writes paths relative to `workspace`.
pub(crate) fn sources(workspace: &Path, exe: &Path) -> Result<Vec<PathBuf>, String> {
    let info = exe.with_extension("d");
    let text = std::fs::read_to_string(&info)
        .map_err(|e| format!("{}: {e}", info.display()))?;
    Ok(text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.strip_suffix(':'))
        .map(|path| files::normalize(&workspace.join(path.replace("\\ ", " "))))
        .filter(|path| path.extension().is_some_and(|e| e == "rs"))
        .collect())
}

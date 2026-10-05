//! Builds test executables, checks targets, and reads which source files rustc
//! compiled into each.

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

/// A unit that `cargo check` checked, other than a test or benchmark.
#[derive(Debug)]
pub(crate) struct Checked {
    pub(crate) package_id: String,
    /// The dep-info file of the unit.
    pub(crate) info: PathBuf,
}

/// Runs `cargo`, a `cargo check --message-format=json` command, and returns each unit
/// it checked that is not a test or benchmark.
pub(crate) fn checked(cargo: &mut Command) -> Result<Vec<Checked>, String> {
    let output = cargo.output().map_err(|e| format!("cargo check: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot check the targets:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let mut checked = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let message: Value =
            serde_json::from_str(line).map_err(|e| format!("cargo check: {e}"))?;
        if field::text(&message, "reason")? != "compiler-artifact"
            || field::flag(&message["profile"], "test")?
        {
            continue;
        }
        checked.push(Checked {
            package_id: field::text(&message, "package_id")?.to_string(),
            info: info(&message)?,
        });
    }
    Ok(checked)
}

/// The dep-info file of the unit of `artifact`, a `compiler-artifact` message. It is
/// `<name>-<hash>.d` beside the unit's first file, which is `lib<name>-<hash>.rmeta`,
/// or a build script in a directory named `<package>-<hash>`.
fn info(artifact: &Value) -> Result<PathBuf, String> {
    let target = &artifact["target"];
    let name = field::text(target, "name")?.replace('-', "_");
    let file = field::list(artifact, "filenames")?
        .first()
        .and_then(Value::as_str)
        .map(Path::new)
        .ok_or_else(|| format!("cargo check built no file for `{name}`"))?;
    let dir = file
        .parent()
        .ok_or_else(|| format!("{} has no directory", file.display()))?;
    let script = field::list(target, "kind")?
        .iter()
        .any(|kind| kind == "custom-build");
    let named = if script { dir } else { file };
    let hash = named
        .file_stem()
        .and_then(|stem| stem.to_str()?.rsplit_once('-'))
        .map(|(_, hash)| hash)
        .ok_or_else(|| format!("{} has no hash in its name", named.display()))?;
    Ok(dir.join(format!("{name}-{hash}.d")))
}

/// The Rust source files that rustc compiled, from the dep-info file `info`. Rustc
/// writes paths relative to `workspace`.
pub(crate) fn sources(workspace: &Path, info: &Path) -> Result<Vec<PathBuf>, String> {
    let text = std::fs::read_to_string(info)
        .map_err(|e| format!("{}: {e}", info.display()))?;
    Ok(text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.strip_suffix(':'))
        .map(|path| files::normalize(&workspace.join(path.replace("\\ ", " "))))
        .filter(|path| path.extension().is_some_and(|e| e == "rs"))
        .collect())
}

//! Runs each test target that names a cfg such as `loom`, with that cfg set.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{build, field, select};

/// Builds the tests of each workspace crate whose source names `name` in a `cfg`, in
/// release mode with `--cfg <name>`. Then it runs, with `LOOM_MAX_PREEMPTIONS=3`, each
/// test executable that compiled a file that names `name` in a `cfg`. It passes when
/// no crate does. It prints the output of each executable when it ends.
pub(crate) fn test(root: &Path, name: &str) -> Result<(), Vec<String>> {
    let problems = problems(root, name).map_err(|e| vec![e])?;
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

fn problems(root: &Path, name: &str) -> Result<Vec<String>, String> {
    let metadata = crate::metadata(root)?;
    let workspace = PathBuf::from(field::text(&metadata, "workspace_root")?);
    let packages = select::packages(&metadata, |s| select::names_cfg(s, name))?;
    if packages.is_empty() {
        eprintln!("no crate names `{name}` in a cfg");
        return Ok(Vec::new());
    }
    let mut cargo = crate::cargo();
    // Cargo prefers this variable to `RUSTFLAGS`, so the cfg is set even when the
    // caller sets either one.
    cargo
        .current_dir(root)
        .args([
            "test",
            "--no-run",
            "--message-format=json",
            "--release",
            "--tests",
        ])
        .env("CARGO_ENCODED_RUSTFLAGS", format!("--cfg\u{1f}{name}"));
    for package in &packages {
        cargo.args(["-p", &package.id]);
    }
    let mut problems = Vec::new();
    for exe in build::executables(&mut cargo)? {
        if !names(&workspace, &exe.path, name)? {
            continue;
        }
        let output = Command::new(&exe.path)
            .current_dir(&exe.dir)
            .env("LOOM_MAX_PREEMPTIONS", "3")
            .output()
            .map_err(|e| format!("{}: {e}", exe.path.display()))?;
        eprint!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success() {
            let dir = exe.dir.strip_prefix(&workspace).unwrap_or(&exe.dir);
            problems.push(format!(
                "the tests of `{}` in `{}` failed with `--cfg {name}`",
                exe.target,
                dir.display()
            ));
        }
    }
    problems.sort();
    Ok(problems)
}

/// Reports whether a source file of the test executable `exe` names `name` in a
/// `cfg`.
fn names(workspace: &Path, exe: &Path, name: &str) -> Result<bool, String> {
    for file in build::sources(workspace, exe)? {
        let text = std::fs::read_to_string(&file)
            .map_err(|e| format!("{}: {e}", file.display()))?;
        if select::names_cfg(&text, name) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_each_target_that_names_the_cfg() {
        assert_eq!(
            test(&crate::fixture(), "loom"),
            Err(vec![
                "the tests of `conformance` in `crates/a` failed with `--cfg loom`"
                    .to_string(),
                "the tests of `model` in `crates/model` failed with `--cfg loom`"
                    .to_string(),
            ])
        );
    }

    #[test]
    fn passes_when_no_crate_names_the_cfg() {
        assert_eq!(test(&crate::fixture(), "shuttle"), Ok(()));
    }
}

//! The oracle check: an oracle test target compiles each Rust file under `oracles/`,
//! and each oracle test target runs a test.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::{build, field, files};

/// A test target whose root file is under `oracles/`.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    package_id: String,
    package: String,
    name: String,
}

/// Checks that an oracle test target compiles each `.rs` file under `oracles/` of the
/// workspace at `root`, that `cargo test` runs each oracle test target, and that each
/// one runs a test that is not ignored. It builds each oracle test target.
pub(crate) fn check(root: &Path) -> Result<(), Vec<String>> {
    let problems = problems(root).map_err(|e| vec![e])?;
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

fn problems(root: &Path) -> Result<Vec<String>, String> {
    let metadata = crate::metadata(root)?;
    let workspace = PathBuf::from(field::text(&metadata, "workspace_root")?);
    let oracles = workspace.join("oracles");
    let (targets, mut problems) = targets(&metadata, &oracles)?;
    let mut compiled = BTreeSet::new();
    for (target, exe) in build(root, &targets)? {
        compiled.extend(build::sources(&workspace, &exe)?);
        if tests(&exe)? == 0 {
            problems.push(format!(
                "oracle test target `{}` of `{}` runs no tests. An oracle must run at \
                 least one test that is not ignored.",
                target.name, target.package
            ));
        }
    }
    for file in files::rust(&oracles)? {
        if !compiled.contains(&file) {
            let shown = file.strip_prefix(&workspace).unwrap_or(&file).display();
            problems.push(format!(
                "`{shown}` is not compiled by an oracle test target, so no gate runs \
                 it. Make it the `path` of a `[[test]]` entry, or a module of one."
            ));
        }
    }
    Ok(problems)
}

/// The test targets in `metadata` whose root file is under `oracles`, and a problem
/// for each one that sets `test = false`.
fn targets(
    metadata: &Value,
    oracles: &Path,
) -> Result<(Vec<Target>, Vec<String>), String> {
    let mut targets = Vec::new();
    let mut problems = Vec::new();
    for package in field::list(metadata, "packages")? {
        for target in field::list(package, "targets")? {
            let test = field::list(target, "kind")?.iter().any(|k| k == "test");
            let root = files::normalize(Path::new(field::text(target, "src_path")?));
            if !test || !root.starts_with(oracles) {
                continue;
            }
            let found = Target {
                package_id: field::text(package, "id")?.to_string(),
                package: field::text(package, "name")?.to_string(),
                name: field::text(target, "name")?.to_string(),
            };
            if !field::flag(target, "test")? {
                problems.push(format!(
                    "oracle test target `{}` of `{}` sets `test = false`, so `cargo \
                     test` skips it. Remove the setting.",
                    found.name, found.package
                ));
            }
            targets.push(found);
        }
    }
    Ok((targets, problems))
}

/// Builds `targets` in the workspace at `root` and returns each one with its test
/// executable.
fn build<'a>(
    root: &Path,
    targets: &'a [Target],
) -> Result<Vec<(&'a Target, PathBuf)>, String> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let mut cargo = crate::cargo();
    cargo
        .current_dir(root)
        .args(["test", "--no-run", "--message-format=json"]);
    for target in targets {
        cargo.args(["-p", &target.package_id, "--test", &target.name]);
    }
    let mut built = Vec::new();
    for exe in build::executables(&mut cargo)? {
        if let Some(target) = targets
            .iter()
            .find(|t| t.package_id == exe.package_id && t.name == exe.target)
        {
            built.push((target, exe.path));
        }
    }
    if built.len() != targets.len() {
        return Err(format!(
            "cargo built {} of the {} oracle test targets",
            built.len(),
            targets.len()
        ));
    }
    Ok(built)
}

/// Counts the tests that `exe` runs: those it lists less those it ignores.
fn tests(exe: &Path) -> Result<usize, String> {
    Ok(count(&list(exe, &[])?) - count(&list(exe, &["--ignored"])?))
}

/// Runs the test executable `exe` with `--list` and `extra`, and returns its output.
fn list(exe: &Path, extra: &[&str]) -> Result<String, String> {
    let output = Command::new(exe)
        .arg("--list")
        .args(extra)
        .output()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} --list failed:\n{}",
            exe.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Counts the tests in the output of a libtest `--list`.
fn count(list: &str) -> usize {
    list.lines().filter(|l| l.ends_with(": test")).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reports_orphans_and_targets_that_run_no_tests() {
        assert_eq!(
            check(&crate::fixture()),
            Err(vec![
                "oracle test target `ignored` of `a` runs no tests. An oracle must \
                 run at least one test that is not ignored."
                    .to_string(),
                "`oracles/orphan.rs` is not compiled by an oracle test target, so \
                 no gate runs it. Make it the `path` of a `[[test]]` entry, or a \
                 module of one."
                    .to_string(),
            ])
        );
    }

    mod targets {
        use super::*;

        fn target(path: &str, test: bool) -> Value {
            json!({
                "name": "conformance",
                "kind": ["test"],
                "src_path": path,
                "test": test,
            })
        }

        fn check(targets: &[Value]) -> Result<(Vec<Target>, Vec<String>), String> {
            let metadata = json!({
                "packages": [{ "id": "raft-id", "name": "raft", "targets": targets }]
            });
            super::targets(&metadata, Path::new("/w/oracles"))
        }

        fn conformance() -> Target {
            Target {
                package_id: "raft-id".to_string(),
                package: "raft".to_string(),
                name: "conformance".to_string(),
            }
        }

        #[test]
        fn finds_a_root_under_oracles_through_parent_components() {
            let t = target("/w/crates/raft/../../oracles/raft/election.rs", true);
            assert_eq!(check(&[t]), Ok((vec![conformance()], vec![])));
        }

        #[test]
        fn ignores_targets_outside_oracles_and_other_kinds() {
            let outside = target("/w/crates/raft/tests/it/main.rs", true);
            let mut bin = target("/w/oracles/raft/main.rs", true);
            bin["kind"] = json!(["bin"]);
            assert_eq!(check(&[outside, bin]), Ok((vec![], vec![])));
        }

        #[test]
        fn reports_a_target_that_cargo_test_skips() {
            let t = target("/w/oracles/raft/election.rs", false);
            let problem = "oracle test target `conformance` of `raft` sets `test = \
                           false`, so `cargo test` skips it. Remove the setting.";
            assert_eq!(
                check(&[t]),
                Ok((vec![conformance()], vec![problem.to_string()]))
            );
        }

        #[test]
        fn names_a_missing_field() {
            let mut t = target("/w/oracles/raft/election.rs", true);
            t["test"] = Value::Null;
            assert_eq!(
                check(&[t]).unwrap_err(),
                "cargo JSON has no boolean field `test`"
            );
        }
    }

    mod count {
        use super::*;

        #[test]
        fn counts_tests_and_skips_the_summary() {
            let list = "a::one: test\nb: test\nbench: bench\n\n2 tests, 1 benchmark\n";
            assert_eq!(count(list), 2);
        }

        #[test]
        fn returns_zero_for_an_empty_list() {
            assert_eq!(count("\n0 tests, 0 benchmarks\n"), 0);
        }
    }
}

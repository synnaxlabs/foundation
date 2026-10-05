//! The oracle check: each Rust file under `oracles/` is a test target that runs.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// A test target whose root file is under `oracles/`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub package: String,
    pub name: String,
}

/// Checks that each `.rs` file under `oracles/` is the root of a test target that
/// `cargo test` runs, and that each such target runs at least one test. It builds
/// each oracle test target.
pub(crate) fn check() -> Result<(), Vec<String>> {
    let metadata = crate::metadata().map_err(|e| vec![e])?;
    let root = PathBuf::from(metadata["workspace_root"].as_str().unwrap_or_default());
    let files = rust_files(&root.join("oracles")).map_err(|e| vec![e])?;
    let (targets, mut problems) = plan(&metadata, &root, &files);
    for target in &targets {
        match tests_run(target) {
            Ok(0) => problems.push(format!(
                "oracle test target `{}` of `{}` runs no tests. An oracle must run \
                 at least one test that is not ignored.",
                target.name, target.package
            )),
            Ok(_) => {}
            Err(e) => problems.push(e),
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Matches `files` (normalized paths under `root`) to the test targets in
/// `metadata`. Returns the oracle test targets that `cargo test` runs, and a problem
/// for each file that is not a test target root and each target that sets
/// `test = false`.
fn plan(
    metadata: &Value,
    root: &Path,
    files: &[PathBuf],
) -> (Vec<Target>, Vec<String>) {
    let files: BTreeSet<&Path> = files.iter().map(PathBuf::as_path).collect();
    let mut roots = BTreeSet::new();
    let mut targets = Vec::new();
    let mut problems = Vec::new();
    let packages = metadata["packages"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    for package in packages {
        let package_name = package["name"].as_str().unwrap_or_default();
        let package_targets =
            package["targets"].as_array().map_or(&[][..], Vec::as_slice);
        for target in package_targets {
            let test = target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|k| k == "test"));
            let path =
                normalize(Path::new(target["src_path"].as_str().unwrap_or_default()));
            if !test || !files.contains(path.as_path()) {
                continue;
            }
            let name = target["name"].as_str().unwrap_or_default().to_string();
            if target["test"] == false {
                problems.push(format!(
                    "oracle test target `{name}` of `{package_name}` sets `test = \
                     false`, so `cargo test` skips it. Remove the setting."
                ));
            } else {
                targets.push(Target {
                    package: package_name.to_string(),
                    name,
                });
            }
            roots.insert(path);
        }
    }
    for file in files {
        if !roots.contains(file) {
            let shown = file.strip_prefix(root).unwrap_or(file).display();
            problems.push(format!(
                "`{shown}` is not the root of a test target, so no gate runs it. Add a \
                 `[[test]]` entry with this `path` to the crate that it checks."
            ));
        }
    }
    (targets, problems)
}

/// Removes `.` and `..` components without reading the disk. Cargo reports a
/// `[[test]] path` such as `../../oracles/x.rs` joined to the manifest directory.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Every `.rs` file under `dir`, in sorted order.
fn rust_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Builds `target` and counts the tests that `cargo test` runs in it.
fn tests_run(target: &Target) -> Result<usize, String> {
    let all = list(target, &[])?;
    let ignored = list(target, &["--ignored"])?;
    Ok(count(&all).saturating_sub(count(&ignored)))
}

/// Runs the test binary of `target` with `--list` and `extra`, and returns its
/// output.
fn list(target: &Target, extra: &[&str]) -> Result<String, String> {
    let output = crate::cargo()
        .args([
            "test",
            "--quiet",
            "-p",
            &target.package,
            "--test",
            &target.name,
        ])
        .args(["--", "--list"])
        .args(extra)
        .output()
        .map_err(|e| format!("cargo test: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot list the tests of oracle test target `{}` of `{}`:\n{}",
            target.name,
            target.package,
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

    fn target(name: &str, kind: &str, path: &str) -> Value {
        json!({ "name": name, "kind": [kind], "src_path": path, "test": true })
    }

    fn metadata(targets: &[Value]) -> Value {
        json!({ "packages": [{ "name": "raft", "targets": targets }] })
    }

    fn check(targets: &[Value], files: &[&str]) -> (Vec<Target>, Vec<String>) {
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        plan(&metadata(targets), Path::new("/w"), &files)
    }

    fn conformance() -> Target {
        Target {
            package: "raft".to_string(),
            name: "conformance".to_string(),
        }
    }

    mod plan {
        use super::*;

        #[test]
        fn accepts_a_test_target_root() {
            let t = target("conformance", "test", "/w/oracles/raft/election.rs");
            let (targets, problems) = check(&[t], &["/w/oracles/raft/election.rs"]);
            assert_eq!(targets, vec![conformance()]);
            assert_eq!(problems, Vec::<String>::new());
        }

        #[test]
        fn resolves_parent_components_in_a_target_path() {
            let path = "/w/crates/raft/../../oracles/raft/election.rs";
            let t = target("conformance", "test", path);
            let (targets, problems) = check(&[t], &["/w/oracles/raft/election.rs"]);
            assert_eq!(targets, vec![conformance()]);
            assert_eq!(problems, Vec::<String>::new());
        }

        #[test]
        fn ignores_test_targets_outside_oracles() {
            let t = target("it", "test", "/w/crates/raft/tests/it/main.rs");
            let (targets, problems) = check(&[t], &[]);
            assert_eq!(targets, Vec::new());
            assert_eq!(problems, Vec::<String>::new());
        }

        mod when_a_file_is_not_a_test_root {
            use super::*;

            fn orphan(path: &str) -> String {
                format!(
                    "`{path}` is not the root of a test target, so no gate runs it. Add \
                     a `[[test]]` entry with this `path` to the crate that it checks."
                )
            }

            #[test]
            fn reports_a_file_no_target_names() {
                let (targets, problems) = check(&[], &["/w/oracles/raft/election.rs"]);
                assert_eq!(targets, Vec::new());
                assert_eq!(problems, vec![orphan("oracles/raft/election.rs")]);
            }

            #[test]
            fn reports_a_file_that_only_a_binary_builds() {
                let t = target("election", "bin", "/w/oracles/raft/election.rs");
                let (targets, problems) = check(&[t], &["/w/oracles/raft/election.rs"]);
                assert_eq!(targets, Vec::new());
                assert_eq!(problems, vec![orphan("oracles/raft/election.rs")]);
            }

            #[test]
            fn reports_a_module_beside_a_test_root() {
                let t = target("conformance", "test", "/w/oracles/raft/main.rs");
                let files = ["/w/oracles/raft/election.rs", "/w/oracles/raft/main.rs"];
                let (targets, problems) = check(&[t], &files);
                assert_eq!(targets, vec![conformance()]);
                assert_eq!(problems, vec![orphan("oracles/raft/election.rs")]);
            }
        }

        #[test]
        fn reports_a_target_that_cargo_test_skips() {
            let mut t = target("conformance", "test", "/w/oracles/raft/election.rs");
            t["test"] = json!(false);
            let (targets, problems) = check(&[t], &["/w/oracles/raft/election.rs"]);
            assert_eq!(targets, Vec::new());
            assert_eq!(
                problems,
                vec![
                    "oracle test target `conformance` of `raft` sets `test = false`, so \
                     `cargo test` skips it. Remove the setting."
                        .to_string()
                ]
            );
        }
    }

    mod normalize {
        use super::*;

        #[test]
        fn removes_current_and_parent_components() {
            let path = Path::new("/w/crates/raft/./../../oracles/x.rs");
            assert_eq!(normalize(path), PathBuf::from("/w/oracles/x.rs"));
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

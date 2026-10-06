//! Repository tasks, run as `cargo xtask <task>`.

#![expect(clippy::print_stderr, reason = "xtask reports to the terminal")]

mod build;
mod cfg;
mod field;
mod files;
mod globals;
mod map;
mod miri;
mod oracles;
mod select;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, ExitCode};

use serde_json::Value;

fn main() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("invariant: xtask is a directory of the workspace root");
    #[expect(clippy::disallowed_methods, reason = "a dev tool reads its arguments")]
    let result = match std::env::args().nth(1).as_deref() {
        Some("layers") => layers(root),
        Some("globals") => globals::check(root),
        Some("oracles") => oracles::check(root),
        Some(name @ ("loom" | "shuttle")) => cfg::test(root, name),
        Some("miri") => miri::run(root),
        _ => {
            eprintln!("usage: cargo xtask <layers|globals|oracles|loom|shuttle|miri>");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(problems) => {
            for problem in problems {
                eprintln!("error: {problem}\n");
            }
            ExitCode::FAILURE
        }
    }
}

/// Checks every dependency of the workspace at `root` against the crate map in
/// `map.rs`.
fn layers(root: &Path) -> Result<(), Vec<String>> {
    let metadata = metadata(root).map_err(|e| vec![e])?;
    let packages = metadata["packages"].as_array().cloned().unwrap_or_default();
    let members: BTreeSet<&str> =
        packages.iter().filter_map(|p| p["name"].as_str()).collect();
    let mut problems = Vec::new();
    for package in &packages {
        let Some(name) = package["name"].as_str() else {
            continue;
        };
        let bench = package["manifest_path"].as_str().is_some_and(|path| {
            Path::new(path)
                .components()
                .any(|c| c.as_os_str() == "bench")
        });
        // Tools and benchmarks ship in no binary, so the crate map does not cover them.
        if name == "xtask" || bench {
            continue;
        }
        let Some(entry) = map::find(name) else {
            problems.push(format!(
                "crate `{name}` is not in the crate map. Add it to xtask/src/map.rs \
                 with its layer, its job, and its allowed dependencies, matching the \
                 crate map in docs/decisions.md."
            ));
            continue;
        };
        let deps = package["dependencies"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for dep in &deps {
            let Some(dep_name) = dep["name"].as_str() else {
                continue;
            };
            if !members.contains(dep_name) {
                continue;
            }
            let dev = dep["kind"].as_str() == Some("dev");
            if entry.allows(dep_name) || (dev && map::TEST_ONLY.contains(&dep_name)) {
                continue;
            }
            problems.push(violation(entry, dep_name));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

fn violation(entry: &map::Crate, dep: &str) -> String {
    let dep_layer =
        map::find(dep).map_or_else(|| "?".to_string(), |d| d.layer.to_string());
    let allowed = entry.describe();
    format!(
        "`{}` (layer {}) depends on `{dep}` (layer {dep_layer}).\n  `{}` may depend on: \
         {allowed}.\n  Fix: move the shared code into a lower crate. Layer 3 reaches \
         the core only through `hub`. A change to the map is an interface change: \
         see docs/coordination.md.",
        entry.name, entry.layer, entry.name
    )
}

/// Runs `cargo metadata` on the workspace at `root`.
fn metadata(root: &Path) -> Result<Value, String> {
    let output = cargo()
        .current_dir(root)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata: {e}"))
}

/// A command that runs the cargo that runs this task.
fn cargo() -> Command {
    #[expect(clippy::disallowed_methods, reason = "cargo sets CARGO for its tools")]
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    Command::new(cargo)
}

/// The test workspace in `xtask/fixture`.
#[cfg(test)]
fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixture")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_reports_crates_missing_from_the_map() {
        let missing = |name| {
            format!(
                "crate `{name}` is not in the crate map. Add it to xtask/src/map.rs \
                 with its layer, its job, and its allowed dependencies, matching the \
                 crate map in docs/decisions.md."
            )
        };
        assert_eq!(
            layers(&fixture()),
            Err(vec![missing("a"), missing("globals"), missing("model")])
        );
    }

    #[test]
    fn models_filter_names_each_crate_that_a_model_task_selects() {
        let root = fixture().join("../..");
        let ci =
            std::fs::read_to_string(root.join(".github/workflows/ci.yaml")).unwrap();
        let models = ci
            .split("models:\n")
            .nth(1)
            .expect("ci.yaml has a models filter");
        let metadata = metadata(&root).unwrap();
        let by_cfg = |name| select::packages(&metadata, |s| select::names_cfg(s, name));
        let tasks = [
            ("loom", by_cfg("loom").unwrap()),
            ("shuttle", by_cfg("shuttle").unwrap()),
            ("miri", miri::packages(&metadata).unwrap()),
        ];
        for (task, names) in tasks {
            for name in names {
                assert!(
                    models.contains(&format!("'crates/{name}/**'")),
                    "`cargo xtask {task}` runs `{name}`, but the `models` filter in \
                     .github/workflows/ci.yaml lacks 'crates/{name}/**'"
                );
            }
        }
    }

    #[test]
    fn miri_skips_only_crates_that_name_unsafe_code() {
        let metadata = metadata(&fixture().join("../..")).unwrap();
        let named = select::packages(&metadata, |s| select::has_word(s, "unsafe_code"));
        let (named, checked) = (named.unwrap(), miri::packages(&metadata).unwrap());
        for name in miri::SKIPPED {
            assert!(
                named.iter().any(|n| n == name),
                "`{name}` names no `unsafe_code`"
            );
            assert!(!checked.iter().any(|n| n == name), "Miri checks `{name}`");
        }
    }
}

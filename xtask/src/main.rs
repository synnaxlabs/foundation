//! Repository tasks, run as `cargo xtask <task>`.

#![expect(clippy::print_stderr, reason = "xtask reports to the terminal")]

mod build;
mod cfg;
mod field;
mod files;
mod fuzz;
mod globals;
mod libtest;
mod map;
mod miri;
mod nightly;
mod open62541;
mod oracles;
mod review;
mod sanitizers;
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args[..] {
        ["layers"] => layers(root),
        ["globals"] => globals::check(root),
        ["oracles"] => oracles::check(root),
        [name @ ("loom" | "shuttle")] => cfg::test(root, name),
        ["miri"] => miri::run(root),
        ["sanitizers"] => sanitizers::run(root),
        ["fuzz"] => fuzz::run(root, fuzz::SECONDS),
        ["fuzz", seconds] => fuzz::seconds(seconds)
            .map_err(|e| vec![e])
            .and_then(|seconds| fuzz::run(root, seconds)),
        ["open62541"] => open62541::check(root),
        ["open62541", tag] => open62541::run(root, open62541::URL, tag),
        ["review", pr, head] => return review::run(root, pr, head),
        _ => {
            eprintln!(
                "usage: cargo xtask <layers|globals|oracles|loom>\n       \
                 cargo xtask <shuttle|miri|sanitizers>\n       \
                 cargo xtask fuzz [seconds]\n       \
                 cargo xtask open62541 [tag]\n       \
                 cargo xtask review <pr> <head sha>"
            );
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
                 crate map in docs/decisions/crate-map.md."
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
            if map::allowed(name, dep_name, dep["kind"].as_str()) {
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

/// Runs `cargo metadata` on the members of the workspace at `root`.
fn metadata(root: &Path) -> Result<Value, String> {
    metadata_with(root, &["--no-deps"])
}

/// The resolved dependency graph of the workspace at `dir`. It fails when the lock of
/// the workspace is stale.
fn graph(dir: &Path) -> Result<Value, String> {
    metadata_with(dir, &["--locked"])
}

/// Runs `cargo metadata` with `flags` on the workspace at `dir`.
fn metadata_with(dir: &Path, flags: &[&str]) -> Result<Value, String> {
    let output = cargo()
        .current_dir(dir)
        .args(["metadata", "--format-version", "1"])
        .args(flags)
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
mod common {
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{ExitStatus, Output};

    /// A new folder named `name` in the temp folder whose `rust-toolchain-nightly`
    /// pins the installed toolchain of `rust-toolchain.toml`. The caller removes it.
    pub(crate) fn create_stable_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let toolchain = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../rust-toolchain.toml"),
        )
        .unwrap();
        let channel = toolchain
            .lines()
            .find_map(|line| line.strip_prefix("channel = "))
            .unwrap()
            .trim_matches('"');
        std::fs::write(root.join("rust-toolchain-nightly"), channel).unwrap();
        root
    }

    /// An output of a process that exits with `code`.
    pub(crate) fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }
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
                 crate map in docs/decisions/crate-map.md."
            )
        };
        assert_eq!(
            layers(&fixture()),
            Err(vec![missing("a"), missing("globals"), missing("model")])
        );
    }

    /// `.github/workflows/ci.yaml` in the workspace at `root`, with no blank or comment
    /// line, which YAML skips.
    fn workflow(root: &Path) -> String {
        let text =
            std::fs::read_to_string(root.join(".github/workflows/ci.yaml")).unwrap();
        let ci: Vec<&str> = (text.lines())
            .filter(|line| !line.trim().is_empty() && !line.trim().starts_with('#'))
            .collect();
        // A key such as `defaults`, `env`, or `run-name` can change what a job runs.
        let keys: Vec<&str> = (ci.iter().copied())
            .filter(|line| !line.starts_with(' '))
            .collect();
        assert_eq!(
            keys,
            ["name: CI", "on:", "concurrency:", "jobs:"],
            "ci.yaml has a top-level key that the checks do not read"
        );
        ci.join("\n")
    }

    /// The lines under the first line of `lines` that is `key`: each line after it up
    /// to the first that is not indented further, which is the value of `key` in YAML.
    fn under<'a>(lines: &[&'a str], key: &str) -> Vec<&'a str> {
        let indent = |line: &str| line.len() - line.trim_start().len();
        let start = (lines.iter().position(|line| *line == key))
            .unwrap_or_else(|| panic!("ci.yaml has no `{}` in {lines:#?}", key.trim()));
        (lines[start + 1..].iter().copied())
            .take_while(|line| indent(line) > indent(key))
            .collect()
    }

    /// The paths of the `models` filter of the step `filter` of the job `changes` in
    /// the lines `ci` of a workflow.
    fn models<'a>(ci: &[&'a str]) -> Vec<&'a str> {
        let steps = under(&under(&under(ci, "jobs:"), "  changes:"), "    steps:");
        // Text under a key is indented past it, so a line at the indent of a step key
        // is one.
        let id = (steps.iter().position(|line| *line == "        id: filter"))
            .expect("the changes job has a step `filter`");
        let start = (steps[..id].iter())
            .rposition(|line| line.starts_with("      - "))
            .unwrap();
        let step = under(&steps[start..], steps[start]);
        let filters = under(&under(&step, "        with:"), "          filters: |");
        (under(&filters, "            models:").iter())
            .map(|line| line.trim().strip_prefix("- ").expect("a path"))
            .collect()
    }

    #[test]
    fn models_reads_only_the_models_filter_of_the_step_filter() {
        let ci = concat!(
            "jobs:\n",
            "  changes:\n",
            "    steps:\n",
            "      - uses: actions/checkout@v4\n",
            "        env:\n",
            "          NOTE: |\n",
            "            models:\n",
            "              - 'crates/types/**'\n",
            "      - uses: dorny/paths-filter\n",
            "        id: filter\n",
            "        with:\n",
            "          filters: |\n",
            "            old_models:\n",
            "              - 'crates/types/**'\n",
            "            models:\n",
            "              - 'crates/env/**'\n",
            "            wait:\n",
            "              - 'crates/types/**'\n",
            "      - run: true\n",
        );
        let ci: Vec<&str> = ci.lines().collect();
        assert_eq!(models(&ci), ["'crates/env/**'"]);
    }

    #[test]
    fn models_filter_names_each_crate_that_a_model_task_selects() {
        let root = fixture().join("../..");
        let ci = workflow(&root);
        let ci: Vec<&str> = ci.lines().collect();
        let (models, jobs) = (models(&ci), under(&ci, "jobs:"));
        let output = under(
            &under(&under(&jobs, "  changes:"), "    outputs:"),
            "      models: >-",
        );
        assert_eq!(
            output,
            ["        ${{ github.event_name == 'push' && 'false' || \
                 steps.filter.outputs.models }}"],
            "the models output of the changes job is not the models filter on each PR"
        );
        let metadata = metadata(&root).unwrap();
        let by_cfg = |name| select::packages(&metadata, |s| select::names_cfg(s, name));
        let tasks = [
            ("loom", by_cfg("loom").unwrap()),
            ("shuttle", by_cfg("shuttle").unwrap()),
            ("miri", miri::packages(&metadata).unwrap()),
        ];
        for (task, packages) in tasks {
            let job = under(&jobs, &format!("  {task}:"));
            let on_models =
                "    if: \"!cancelled() && needs.changes.outputs.models != 'false'\"";
            let run = format!("run: cargo xtask {task}");
            // The job has no key past these, and the step that runs the task holds
            // only its name.
            let keys: Vec<&str> = (job.iter().copied())
                .filter(|line| {
                    !line.starts_with("     ") && !line.starts_with("    runs-on:")
                })
                .collect();
            let end = job.iter().position(|line| line.trim() == run);
            let alone = end.is_some_and(|end| {
                end > 0
                    && job[end - 1].starts_with("      - name: ")
                    && (job.get(end + 1))
                        .is_none_or(|next| !next.starts_with("        "))
            });
            assert!(
                keys == ["    needs: changes", on_models, "    steps:"] && alone,
                "the {task} job of .github/workflows/ci.yaml does not run `cargo xtask \
                 {task}` on the models output: {job:#?}"
            );
            for select::Package { name, .. } in packages {
                assert!(
                    models.contains(&format!("'crates/{name}/**'").as_str()),
                    "`cargo xtask {task}` runs `{name}`, but the `models` filter in \
                     .github/workflows/ci.yaml lacks 'crates/{name}/**'"
                );
            }
        }
    }

    #[test]
    fn models_filter_names_each_workspace_crate_that_miri_runs() {
        let root = fixture().join("../..");
        let (ci, metadata) = (workflow(&root), metadata(&root).unwrap());
        let ci: Vec<&str> = ci.lines().collect();
        let models = models(&ci);
        let workspace = metadata["packages"].as_array().unwrap();
        // Each entry is a crate whose code Miri runs, and the crate that Miri tests.
        let mut open: Vec<(String, String)> = (miri::packages(&metadata).unwrap())
            .into_iter()
            .map(|select::Package { name, .. }| (name.clone(), name))
            .collect();
        let mut seen = BTreeSet::new();
        while let Some((name, tested)) = open.pop() {
            let package = (workspace.iter())
                .find(|p| p["name"] == name.as_str())
                .unwrap();
            for dep in package["dependencies"].as_array().unwrap() {
                let kind = dep["kind"].as_str();
                // Only the tested crate builds its dev dependencies.
                if dep["path"].is_null()
                    || kind == Some("build")
                    || (kind == Some("dev") && name != tested)
                {
                    continue;
                }
                let dep = dep["name"].as_str().unwrap().to_string();
                assert!(
                    models.contains(&format!("'crates/{dep}/**'").as_str()),
                    "`cargo xtask miri` runs `{tested}`, which builds `{dep}`, but the \
                     `models` filter in .github/workflows/ci.yaml lacks \
                     'crates/{dep}/**'"
                );
                if seen.insert(dep.clone()) {
                    open.push((dep, tested.clone()));
                }
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
                named.iter().any(|package| package.name == name),
                "`{name}` names no `unsafe_code`"
            );
            assert!(
                !checked.iter().any(|package| package.name == name),
                "Miri checks `{name}`"
            );
        }
    }
}

//! Repository tasks, run as `cargo xtask <task>`.

#![expect(clippy::print_stderr, reason = "xtask reports to the terminal")]

mod build;
mod cfg;
mod field;
mod files;
mod fuzz;
mod gh;
mod globals;
mod libtest;
mod map;
mod miri;
mod nightly;
mod open62541;
mod oracles;
mod queue;
mod review;
mod sanitizers;
mod select;
mod sim;
mod skips;
mod test;
mod workflow;

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
        ["skips"] => skips::check(root),
        [name @ ("loom" | "shuttle")] => cfg::test(root, name),
        ["miri"] => miri::run(root, miri::Shard::ALL),
        ["miri", shard] => miri::Shard::parse(shard)
            .map_err(|e| vec![e])
            .and_then(|shard| miri::run(root, shard)),
        ["sanitizers"] => sanitizers::run(root),
        ["test", part, ref rest @ ..] => test::Part::parse(part)
            .map_err(|e| vec![e])
            .and_then(|part| {
                let (flags, skipped) = match rest.iter().position(|arg| *arg == "--") {
                    Some(end) => (&rest[..end], &rest[end + 1..]),
                    None => (rest, &[][..]),
                };
                test::run(root, cargo().get_program(), part, flags, skipped)
            }),
        ["fuzz"] => fuzz::run(root, fuzz::SECONDS),
        ["fuzz", seconds] => fuzz::seconds(seconds)
            .map_err(|e| vec![e])
            .and_then(|seconds| fuzz::run(root, seconds)),
        ["open62541"] => open62541::Crypto::locked(root)
            .map_err(|e| vec![e])
            .and_then(|crypto| open62541::check(root, &crypto)),
        ["open62541", tag] => open62541::Crypto::locked(root)
            .map_err(|e| vec![e])
            .and_then(|crypto| open62541::run(root, &crypto, open62541::URL, tag)),
        ["review", pr, head] => return review::run(root, pr, head, &gh::api),
        ["queue", "review", reference, group] => {
            return queue::review(root, reference, group, &gh::api);
        }
        ["queue", "mutants", group] => queue::mutants(group, &gh::api),
        _ => {
            eprintln!(
                "usage: cargo xtask <layers|globals|oracles|skips>\n       \
                 cargo xtask <loom|shuttle|sanitizers>\n       \
                 cargo xtask miri [<index>/<count>]\n       \
                 cargo xtask test <nextest|serial> [flag...] [-- test...]\n       \
                 cargo xtask fuzz [seconds]\n       \
                 cargo xtask open62541 [tag]\n       \
                 cargo xtask review <pr> <head sha>\n       \
                 cargo xtask queue review <queue ref> <group sha>\n       \
                 cargo xtask queue mutants <group sha>"
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
/// `map.rs`, and that no local crate turns on a `sim` feature outside a `sim` feature
/// or a dev-dependency.
fn layers(root: &Path) -> Result<(), Vec<String>> {
    let metadata = metadata(root).map_err(|e| vec![e])?;
    let packages = metadata["packages"].as_array().cloned().unwrap_or_default();
    let members: BTreeSet<&str> =
        packages.iter().filter_map(|p| p["name"].as_str()).collect();
    let mut problems = sim::check(&graph(root).map_err(|e| vec![e])?);
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

/// The dependency graph of the workspace at `dir`, resolved with all features on. It
/// fails when the lock of the workspace is stale.
fn graph(dir: &Path) -> Result<Value, String> {
    metadata_with(dir, &["--locked", "--all-features"])
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
    use std::path::{Path, PathBuf};
    use std::process::Output;

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

    /// The text that an `ExitStatus` of an exit code prints before the code.
    #[cfg(unix)]
    pub(crate) const EXITED: &str = "exit status";
    #[cfg(windows)]
    pub(crate) const EXITED: &str = "exit code";

    /// The text of the error of a missing file in a folder that exists.
    #[cfg(unix)]
    pub(crate) const NO_FILE: &str = "No such file or directory (os error 2)";
    #[cfg(windows)]
    pub(crate) const NO_FILE: &str =
        "The system cannot find the file specified. (os error 2)";

    /// The text of the error of a missing folder, or of a path in one.
    #[cfg(unix)]
    pub(crate) const NO_FOLDER: &str = "No such file or directory (os error 2)";
    #[cfg(windows)]
    pub(crate) const NO_FOLDER: &str =
        "The system cannot find the path specified. (os error 3)";

    /// The text of the error of a removal of a folder at a file.
    #[cfg(unix)]
    pub(crate) const NOT_FOLDER: &str = "Not a directory (os error 20)";
    #[cfg(windows)]
    pub(crate) const NOT_FOLDER: &str = "The directory name is invalid. (os error 267)";

    /// `path`, written with `/`, with the separator of the OS.
    pub(crate) fn native(path: &str) -> String {
        path.replace('/', std::path::MAIN_SEPARATOR_STR)
    }

    /// An output of a process that exits with `code`.
    pub(crate) fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        #[cfg(unix)]
        let status = std::os::unix::process::ExitStatusExt::from_raw(code << 8);
        #[cfg(windows)]
        let status =
            std::os::windows::process::ExitStatusExt::from_raw(code.cast_unsigned());
        Output {
            status,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::indent;

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

    fn yaml_lines(text: &str) -> Vec<&str> {
        (workflow::lines(text)
            .unwrap_or_else(|e| panic!("{e}"))
            .into_iter())
        .map(|line| line.text)
        .collect()
    }

    fn under<'a>(lines: &[&'a str], key: &str) -> Vec<&'a str> {
        workflow::under(lines, key).unwrap_or_else(|e| panic!("{e}"))
    }

    /// `.github/workflows/<file>` in the workspace at `root`, with no blank or comment
    /// line.
    ///
    /// # Panics
    ///
    /// When its top-level keys are not `name: <name>`, `on`, `concurrency`, and `jobs`.
    fn workflow(root: &Path, file: &str, name: &str) -> String {
        let text =
            std::fs::read_to_string(root.join(".github/workflows").join(file)).unwrap();
        let lines = yaml_lines(&text);
        // A key such as `defaults`, `env`, or `run-name` can change what a job runs.
        let keys: Vec<&str> = (lines.iter().copied())
            .filter(|line| !line.starts_with(' '))
            .collect();
        assert_eq!(
            keys,
            [&format!("name: {name}"), "on:", "concurrency:", "jobs:"],
            "{file} has a top-level key that the checks do not read"
        );
        lines.join("\n")
    }

    /// The lines of the step `filter` of `ci.yaml` up to the text of its filters.
    const FILTER: [&str; 7] = [
        "      - if: github.event_name != 'push'",
        concat!(
            "        uses: dorny/paths-filter@0e4a8c6effa4802afeda77dc8d303f8176d7dfad",
            " # v3.0.4",
        ),
        "        id: filter",
        "        with:",
        "          base: ${{ github.event.merge_group.base_sha }}",
        "          ref: ${{ github.event.merge_group.head_sha }}",
        "          filters: |",
    ];

    /// The lines of the step `filter` of the job `changes` in the lines `ci` of a
    /// workflow.
    fn filter_step<'a>(ci: &[&'a str]) -> Vec<&'a str> {
        let steps = under(&under(&under(ci, "jobs:"), "  changes:"), "    steps:");
        // Text under a key is indented past it, so a line at the indent of a step key
        // is one, and a line at the indent of the steps starts one.
        let id = (steps.iter().position(|line| *line == "        id: filter"))
            .expect("the changes job has a step `filter`");
        let start = (steps[..id].iter())
            .rposition(|line| indent(line) == 6)
            .unwrap();
        let step = under(&steps[start..], steps[start]);
        std::iter::once(steps[start]).chain(step).collect()
    }

    /// The paths of the `models` filter of the step `filter` of the job `changes` in
    /// the lines `ci` of a workflow.
    fn models<'a>(ci: &[&'a str]) -> Vec<&'a str> {
        let step = filter_step(ci);
        let filters = under(&under(&step, "        with:"), "          filters: |");
        let mut key = None;
        let mut paths = Vec::new();
        // Any other line continues a scalar, which can hold a key or a path that the
        // filter does not have.
        for line in filters {
            let name = (line.strip_prefix("            "))
                .and_then(|name| name.strip_suffix(':'))
                .filter(|name| {
                    name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                });
            // A quote that does not close here, or a double quote, continues the
            // scalar on the next lines.
            let path = (line.strip_prefix("              - ")).filter(|path| {
                (path.strip_prefix('\''))
                    .and_then(|path| path.strip_suffix('\''))
                    .is_some_and(|inner| !inner.contains('\''))
            });
            match (name, path) {
                (Some(name), _) => key = Some(name),
                (_, Some(path)) if key == Some("models") => paths.push(path),
                (_, Some(_)) => {}
                (None, None) => panic!(
                    "the filters text has a line that is not a key or one quoted path: \
                     `{line}`"
                ),
            }
        }
        paths
    }

    /// The lines of the outputs of the job `changes` in the lines `ci` of a workflow.
    fn outputs<'a>(ci: &[&'a str]) -> Vec<&'a str> {
        let changes = under(&under(ci, "jobs:"), "  changes:");
        under(&changes, "    outputs:")
    }

    /// The lines of each model job of `ci.yaml` under its key. A step or a key that
    /// the job gains can change what it runs, so the whole job is pinned.
    const JOBS: [(&str, &[&str]); 4] = [
        (
            "loom",
            &[
                "    needs: changes",
                "    if: \"!cancelled() && needs.changes.outputs.models != 'false'\"",
                "    runs-on: ubuntu-24.04",
                "    steps:",
                "      - uses: actions/checkout@v4",
                "      - uses: Swatinem/rust-cache@v2",
                "      - name: Clippy",
                concat!(
                    "        run: cargo clippy -p ring -p block --all-targets",
                    " -- -D warnings",
                ),
                "        env:",
                "          RUSTFLAGS: --cfg loom",
                "      - name: Loom",
                "        run: cargo xtask loom",
            ],
        ),
        (
            "shuttle",
            &[
                "    needs: changes",
                "    if: \"!cancelled() && needs.changes.outputs.models != 'false'\"",
                "    runs-on: ubuntu-24.04",
                "    steps:",
                "      - uses: actions/checkout@v4",
                "      - uses: Swatinem/rust-cache@v2",
                "      - name: Shuttle",
                "        run: cargo xtask shuttle",
            ],
        ),
        (
            "miri-shard",
            &[
                "    needs: changes",
                "    if: \"!cancelled() && needs.changes.outputs.models != 'false'\"",
                "    name: miri shard ${{ matrix.shard }}",
                "    strategy:",
                "      fail-fast: false",
                "      matrix:",
                "        shard: [0, 1, 2, 3, 4, 5, 6, 7]",
                "    runs-on: ubuntu-24.04",
                "    steps:",
                "      - uses: actions/checkout@v4",
                "      - name: Install the pinned nightly",
                "        run: |",
                "          rustup toolchain install \"$(cat rust-toolchain-nightly)\" \
             --profile minimal \\",
                "            --component miri,rust-src",
                "      - uses: taiki-e/install-action@v2",
                "        with:",
                "          tool: cargo-nextest@0.9.146",
                "      - uses: Swatinem/rust-cache@v2",
                "      - name: Miri",
                "        env:",
                "          SHARD: ${{ matrix.shard }}",
                "        run: cargo xtask miri \"$SHARD/8\"",
            ],
        ),
        (
            "miri",
            &[
                "    needs: [changes, miri-shard]",
                "    if: \"!cancelled() && needs.changes.outputs.models != 'false'\"",
                "    runs-on: ubuntu-24.04",
                "    steps:",
                "      - name: The result of the shards",
                "        env:",
                "          RESULT: ${{ needs.miri-shard.result }}",
                "        run: |",
                "          [ \"$RESULT\" = success ] ||",
                "            { echo \"::error::The miri shards ended in $RESULT.\"; exit 1; }",
            ],
        ),
    ];

    /// Checks each model job in the lines `jobs` under the key `jobs` of a workflow.
    ///
    /// # Panics
    ///
    /// When a model job is not its job in [`JOBS`].
    fn pin_jobs(jobs: &[&str]) {
        for (key, job) in JOBS {
            let found = under(jobs, &format!("  {key}:"));
            assert!(
                found == job,
                "the {key} job of .github/workflows/ci.yaml is not the job that the \
                 check pins: {found:#?}"
            );
        }
    }

    #[test]
    fn pin_jobs_refuses_each_change_to_a_model_job() {
        let pin = |changed: usize, edited: &[&str]| {
            let lines: Vec<String> = (JOBS.iter().enumerate())
                .flat_map(|(i, (task, job))| {
                    let job = if i == changed { edited } else { job };
                    let job = job.iter().map(|line| (*line).to_owned());
                    std::iter::once(format!("  {task}:")).chain(job)
                })
                .collect();
            pin_jobs(&lines.iter().map(String::as_str).collect::<Vec<_>>());
        };
        pin(0, JOBS[0].1);
        for (changed, (task, job)) in JOBS.into_iter().enumerate() {
            let end = job.len();
            for (at, lines) in [
                (end..end, &["        if: false"][..]),
                (1..2, &["    if: \"false\""][..]),
                (end - 1..end, &["        run: \"true\""][..]),
                (end - 1..end, &[][..]),
                (3..3, &["    env:", "      A: b"][..]),
                (end - 2..end - 2, &["      - run: echo a"][..]),
            ] {
                let mut edited = job.to_vec();
                edited.splice(at.clone(), lines.iter().copied());
                let panic = std::panic::catch_unwind(|| pin(changed, &edited))
                    .expect_err(&format!("{task} {at:?}"));
                assert_eq!(
                    panic.downcast_ref::<String>(),
                    Some(&format!(
                        "the {task} job of .github/workflows/ci.yaml is not the job \
                         that the check pins: {edited:#?}"
                    )),
                );
            }
        }
    }

    #[test]
    fn models_reads_only_the_filter_and_output_of_the_changes_job() {
        let ci = concat!(
            "jobs:\n",
            "  notes:\n",
            "    outputs:\n",
            "      models: >-\n",
            "        decoy\n",
            "    steps:\n",
            "      - uses: dorny/paths-filter\n",
            "        id: filter\n",
            "        with:\n",
            "          filters: |\n",
            "            models:\n",
            "              - 'crates/types/**'\n",
            "  changes:\n",
            "    outputs:\n",
            "      notes: >-\n",
            "        id: filter\n",
            "        with:\n",
            "          filters: |\n",
            "            models:\n",
            "              - 'crates/types/**'\n",
            "      models: >-\n",
            "        real\n",
            "    steps:\n",
            "      - uses: actions/checkout@v4\n",
            "        with:\n",
            "          filters: |\n",
            "            models:\n",
            "              - 'crates/types/**'\n",
            "      -\n",
            "        uses: dorny/paths-filter\n",
            "        id: filter\n",
            "        env:\n",
            "          filters: |\n",
            "            models:\n",
            "              - 'crates/types/**'\n",
            "        with:\n",
            "          filters: |\n",
            "            old_models:\n",
            "              - 'crates/types/**'\n",
            "            models:\n",
            "              - 'crates/env/**'\n",
            "            wait:\n",
            "              - 'crates/types/**'\n",
            "      - uses: actions/checkout@v4\n",
            "        with:\n",
            "          filters: |\n",
            "            models:\n",
            "              - 'crates/types/**'\n",
        );
        let ci: Vec<&str> = ci.lines().collect();
        assert_eq!(models(&ci), ["'crates/env/**'"]);
        assert_eq!(outputs(&ci)[6..], ["      models: >-", "        real"]);
    }

    #[test]
    fn models_refuses_each_line_that_is_not_a_key_or_one_quoted_path() {
        for line in [
            "              - >-",
            "              - \"crates/env/**",
            "              - \"crates/env/**'",
            "              - 'crates/env/**",
            "              - 'crates/env/**''",
            "            note: \"",
            "            note: \"models:",
        ] {
            let ci = [
                "jobs:",
                "  changes:",
                "    steps:",
                "      - uses: dorny/paths-filter",
                "        id: filter",
                "        with:",
                "          filters: |",
                "            models:",
                line,
                "            wait:",
                "              - 'crates/env/**'",
            ];
            let panic = std::panic::catch_unwind(|| models(&ci)).expect_err(line);
            assert_eq!(
                panic.downcast_ref::<String>(),
                Some(&format!(
                    "the filters text has a line that is not a key or one quoted path: \
                     `{line}`"
                )),
            );
        }
    }

    #[test]
    #[should_panic(expected = "ci.yaml has no `with:`")]
    fn models_reads_the_inputs_only_of_the_step_filter() {
        let ci = concat!(
            "jobs:\n",
            "  changes:\n",
            "    steps:\n",
            "      - uses: dorny/paths-filter\n",
            "        id: filter\n",
            "        env:\n",
            "          INPUT_FILTERS: |\n",
            "            models:\n",
            "              - 'crates/env/**'\n",
            "      - uses: actions/checkout@v4\n",
            "        with:\n",
            "          filters: |\n",
            "            models:\n",
            "              - 'crates/types/**'\n",
        );
        let ci: Vec<&str> = ci.lines().collect();
        models(&ci);
    }

    #[test]
    fn layers_reports_sim_also_in_a_benchmark_and_a_path_crate_outside_the_members() {
        let problems = layers(&fixture().join("features")).unwrap_err();
        let rule = "A `sim` feature is test-only: only a dev-dependency, the fuzz \
                    crate, or another `sim` feature turns it on. A product feature \
                    turns on `dep:sim` and `simulate` features. See rule 10 in \
                    docs/decisions/crate-map.md.";
        let mut found = vec![format!(
            "`own` turns on `sim` in its feature `default`. {rule}"
        )];
        found.extend(["tool", "outer"].map(|name| {
            format!(
                "`{name}` turns on the `sim` feature of its dependency `own`. Move it \
                 to the dev-dependencies. {rule}"
            )
        }));
        for problem in found {
            assert!(problems.contains(&problem), "{problem}: {problems:?}");
        }
    }

    #[test]
    fn models_filter_names_each_crate_that_a_model_task_selects() {
        let root = fixture().join("../..");
        let ci = workflow(&root, "ci.yaml", "CI");
        let ci: Vec<&str> = ci.lines().collect();
        let (models, jobs) = (models(&ci), under(&ci, "jobs:"));
        // An input such as `predicate-quantifier` or `ref` can make the filter skip a
        // change.
        let step = filter_step(&ci);
        assert_eq!(
            step[..FILTER.len()],
            FILTER,
            "the step filter of .github/workflows/ci.yaml has inputs that the checks \
             do not read"
        );
        assert_eq!(
            step[FILTER.len()..],
            under(&step, "          filters: |"),
            "the step filter of .github/workflows/ci.yaml has a key after `filters`"
        );
        // Any other line can open a scalar that holds a decoy output.
        let expected: Vec<String> = (["rust", "deps", "models", "wait"].iter())
            .flat_map(|name| {
                [
                    format!("      {name}: >-"),
                    format!(
                        "        ${{{{ github.event_name == 'push' && 'false' || \
                         steps.filter.outputs.{name} }}}}"
                    ),
                ]
            })
            .collect();
        assert_eq!(
            outputs(&ci),
            expected,
            "the outputs of the changes job are not the filters on each PR"
        );
        let metadata = metadata(&root).unwrap();
        let by_cfg = |name| select::packages(&metadata, |s| select::names_cfg(s, name));
        let packages = [
            by_cfg("loom").unwrap(),
            by_cfg("shuttle").unwrap(),
            (miri::crates(&metadata).unwrap().into_iter())
                .map(|found| found.package)
                .collect(),
        ];
        pin_jobs(&jobs);
        for (task, packages) in ["loom", "shuttle", "miri"].into_iter().zip(packages) {
            for select::Package { name, .. } in packages {
                assert!(
                    models.contains(&format!("'crates/{name}/**'").as_str()),
                    "`cargo xtask {task}` runs `{name}`, but the `models` filter in \
                     .github/workflows/ci.yaml lacks 'crates/{name}/**'"
                );
            }
        }
    }

    /// The lines of the job `name` of `ci.yaml`, whose matrix holds one entry for each
    /// OS.
    fn os_job(name: &str) -> Vec<String> {
        let ci = workflow(&fixture().join("../.."), "ci.yaml", "CI");
        let ci: Vec<&str> = ci.lines().collect();
        let job = under(&under(&ci, "jobs:"), &format!("  {name}:"));
        job.into_iter().map(str::to_owned).collect()
    }

    /// The lines of the job `check` of `ci.yaml`.
    fn check_job() -> Vec<String> {
        os_job("check")
    }

    /// The `if` of the job, the names of its matrix entries, and the `if` of each of
    /// `steps`, in order.
    fn ifs_and_names<'a>(
        job: &'a [String],
        steps: &[&str],
    ) -> (Vec<&'a str>, Vec<&'a str>) {
        let mut ifs: Vec<&str> = (job.iter().map(String::as_str))
            .filter(|line| line.starts_with("    if: "))
            .collect();
        let names = (job.iter())
            .filter_map(|line| line.strip_prefix("          - name: "))
            .collect();
        for step in steps {
            let start = (job.iter())
                .position(|line| *line == format!("      - name: {step}"))
                .unwrap_or_else(|| panic!("the job has no step `{step}`"));
            ifs.push(&job[start + 1]);
        }
        (ifs, names)
    }

    #[test]
    fn the_check_of_each_os_runs_on_each_event_so_that_it_reports_its_name() {
        // A matrix job that its `if` skips reports one check, named by the unexpanded
        // expression, and none of the names of its entries. A step that its `if` skips
        // on an OS passes there.
        let check = check_job();
        let step = |part| {
            format!(
                "        if: \"!cancelled() && env.RUST != 'false' && matrix.part == \
                 '{part}'\""
            )
        };
        let (lint, test) = (step("lint"), step("test"));
        assert_eq!(
            ifs_and_names(&check, &["Clippy", "Test"]),
            (
                vec!["    if: \"!cancelled()\"", &lint, &test],
                vec!["check", "macos", "windows"]
            )
        );
        // The checks of `os` hold the required names, and pass only when each part of
        // `check` passed.
        assert_eq!(
            os_job("os"),
            [
                "    needs: check",
                "    if: \"!cancelled()\"",
                "    name: ${{ matrix.name }}",
                "    strategy:",
                "      matrix:",
                "        name: [check, macos, windows]",
                "    runs-on: ubuntu-24.04",
                "    steps:",
                "      - name: The result of the parts",
                "        env:",
                "          RESULT: ${{ needs.check.result }}",
                "        run: |",
                "          [ \"$RESULT\" = success ] ||",
                concat!(
                    "            { echo \"::error::A part of check ended",
                    " in $RESULT.\"; exit 1; }",
                ),
            ]
        );
        let features = os_job("features");
        assert_eq!(
            ifs_and_names(&features, &["Each feature alone"]),
            (
                vec![
                    "    if: \"!cancelled()\"",
                    "        if: env.RUST != 'false'"
                ],
                vec!["features", "features-macos", "features-windows"]
            )
        );
    }

    /// The lines of the step `name` of `job`, from its name to the next step.
    fn step<'a>(job: &'a [String], name: &str) -> Vec<&'a str> {
        let head = format!("      - name: {name}");
        (job.iter().map(String::as_str))
            .skip_while(|line| *line != head)
            .enumerate()
            .take_while(|(i, line)| *i == 0 || !line.starts_with("      - "))
            .map(|(_, line)| line)
            .collect()
    }

    #[test]
    fn each_step_of_check_runs_in_its_part() {
        // A step whose `if` names no value of `part` skips on each OS, and passes.
        let check = check_job();
        let mut steps: Vec<(&str, Option<&str>)> = Vec::new();
        for line in check
            .iter()
            .skip_while(|line| *line != "    steps:")
            .skip(1)
        {
            let line = line.as_str();
            if line.starts_with("      - ") {
                steps.push(("run", None));
            }
            let Some((name, part)) = steps.last_mut() else {
                continue;
            };
            let key = line.trim_start_matches([' ', '-']);
            if *name == "run" {
                if let Some(found) = key.strip_prefix("name: ") {
                    *name = found;
                } else if let Some(found) = key.strip_prefix("uses: ") {
                    *name = found;
                }
            }
            if let Some((_, rest)) = line.split_once("matrix.part == '") {
                *part = rest.split_once('\'').map(|(found, _)| found);
            }
        }
        let (lint, test, serial) = (Some("lint"), Some("test"), Some("serial"));
        assert_eq!(
            steps,
            [
                ("actions/checkout@v4", None),
                ("Swatinem/rust-cache@v2", None),
                ("taiki-e/install-action@v2", test),
                ("actions/setup-python@v5", None),
                ("run", None),
                ("Format", lint),
                ("Clippy", lint),
                ("Layers", lint),
                ("Globals", lint),
                ("Skips", lint),
                ("Oracles", lint),
                ("Test", test),
                ("Serial tests", serial),
                ("Root tests", test),
            ]
        );
    }

    #[test]
    fn each_os_runs_both_parts_of_the_tests_and_fails_when_one_fails() {
        let check = check_job();
        assert!(
            check
                .iter()
                .any(|line| line == "        part: [lint, test, serial]")
        );
        assert_eq!(
            step(&check, "Test"),
            [
                "      - name: Test",
                concat!(
                    "        if: \"!cancelled() && env.RUST != 'false'",
                    " && matrix.part == 'test'\"",
                ),
                "        run: >-",
                "          cargo xtask test nextest ${{ join(matrix.crates, ' ') }}",
                "          ${{ join(matrix.untested, ' ') }} --all-features",
                concat!(
                    "          --config-file .github/nextest.toml",
                    " -- ${{ join(matrix.tests, ' ') }}",
                ),
            ]
        );
        assert_eq!(
            step(&check, "Serial tests"),
            [
                "      - name: Serial tests",
                concat!(
                    "        if: \"!cancelled() && env.RUST != 'false'",
                    " && matrix.part == 'serial'\"",
                ),
                "        run: >-",
                "          cargo xtask test serial ${{ join(matrix.crates, ' ') }}",
                "          ${{ join(matrix.untested, ' ') }} --all-features",
            ]
        );
    }

    #[test]
    fn arm_runs_both_parts_of_the_tests_at_once_and_fails_when_one_fails() {
        let arm = workflow(&fixture().join("../.."), "arm.yaml", "ARM");
        let arm: Vec<&str> = arm.lines().collect();
        let jobs = under(&arm, "jobs:");
        let changed =
            "    if: \"!cancelled() && needs.changes.outputs.code != 'false'\"";
        assert_eq!(
            under(&jobs, "  parts:"),
            [
                "    needs: changes",
                changed,
                "    name: ${{ matrix.part }} arm",
                "    strategy:",
                "      fail-fast: false",
                "      matrix:",
                "        part: [nextest, serial]",
                "    runs-on: ubuntu-24.04-arm",
                "    steps:",
                "      - uses: actions/checkout@v4",
                "      - uses: Swatinem/rust-cache@v2",
                "        with:",
                "          key: ${{ matrix.part }}",
                "      - if: matrix.part == 'nextest'",
                "        uses: taiki-e/install-action@v2",
                "        with:",
                "          tool: cargo-nextest@0.9.146",
                "      - uses: actions/setup-python@v5",
                "        with:",
                "          python-version: '3.11'",
                "      - run: |",
                "          python -m pip install numpy==2.4.6",
                concat!(
                    "          echo \"PYO3_PYTHON=$(command -v python)\"",
                    " >> \"$GITHUB_ENV\"",
                ),
                concat!(
                    "          echo \"FOUNDATION_NODE=$PWD/target/debug/foundation\"",
                    " >> \"$GITHUB_ENV\"",
                ),
                "      - name: Test",
                "        run: cargo xtask test ${{ matrix.part }}",
            ]
        );
        // The required check `test` skips with `parts`, which passes it.
        assert_eq!(
            under(&jobs, "  test:"),
            [
                "    needs: [changes, parts]",
                changed,
                "    runs-on: ubuntu-24.04",
                "    steps:",
                "      - name: The result of the parts",
                "        env:",
                "          RESULT: ${{ needs.parts.result }}",
                "        run: |",
                "          [ \"$RESULT\" = success ] ||",
                concat!(
                    "            { echo \"::error::A part of test ended",
                    " in $RESULT.\"; exit 1; }",
                ),
            ]
        );
    }

    #[test]
    fn windows_runs_the_tests_of_program_one_at_a_time() {
        let config =
            std::fs::read_to_string(fixture().join("../../.github/nextest.toml"))
                .unwrap();
        let config: Vec<&str> = (config.lines())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        assert_eq!(
            config,
            [
                "[test-groups]",
                "program = { max-threads = 1 }",
                "[[profile.default.overrides]]",
                "platform = \"cfg(windows)\"",
                "filter = \"package(program) & kind(test)\"",
                "test-group = \"program\"",
            ]
        );
    }

    /// The `crates` list of each matrix entry of `job`, in order.
    fn crates(job: &[String]) -> Vec<Vec<&str>> {
        let (mut lists, mut open): (Vec<Vec<&str>>, bool) = (Vec::new(), false);
        for line in job {
            if let Some(item) = line.strip_prefix("              - ") {
                if let (true, Some(list)) = (open, lists.last_mut()) {
                    list.push(item);
                }
            } else if let Some(key) = line.strip_prefix("            ") {
                open = key.starts_with("crates:");
                if open {
                    lists.push(Vec::new());
                }
            }
        }
        lists
    }

    #[test]
    fn the_feature_check_of_each_os_leaves_out_the_crates_of_its_check() {
        let check = check_job();
        let check = crates(&check);
        assert_eq!(check.len(), 3);
        assert!(
            check[2]
                .contains(&"--exclude node # #2968, #682, #2955, #2956, #477, #2957")
        );
        assert_eq!(crates(&os_job("features")), check);
    }

    #[test]
    fn each_exception_of_an_os_names_the_issue_that_removes_it() {
        let check = check_job();
        // The lists are empty at the goal of LINUX CI, so the parse is checked by the
        // keys of each entry. Only a block list puts its items where the check below
        // reads them.
        for key in ["crates:", "untested:", "tests:", "gates:"] {
            let entries: Vec<&str> = (check.iter())
                .filter_map(|line| line.strip_prefix("            "))
                .filter(|line| line.starts_with(key))
                .collect();
            assert_eq!(
                entries.len(),
                3,
                "the entries of the matrix that give `{key}`"
            );
            for entry in entries {
                assert!(
                    entry == key || entry == format!("{key} []"),
                    "`{entry}` of ci.yaml is not a block list or `[]`"
                );
            }
        }
        let items =
            (check.iter()).filter_map(|line| line.strip_prefix("              - "));
        for item in items {
            let issues = item.split_once(" # ").map(|(_, comment)| comment);
            assert!(
                issues.is_some_and(|issues| {
                    issues.split(", ").all(|issue| {
                        (issue.strip_prefix('#')).is_some_and(|n| {
                            !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                        })
                    })
                }),
                "the exception `{item}` of ci.yaml names no issue as `# #<n>`"
            );
        }
    }

    #[test]
    fn models_filter_names_each_workspace_crate_that_miri_runs() {
        let root = fixture().join("../..");
        let (ci, metadata) =
            (workflow(&root, "ci.yaml", "CI"), metadata(&root).unwrap());
        let ci: Vec<&str> = ci.lines().collect();
        let models = models(&ci);
        let workspace = metadata["packages"].as_array().unwrap();
        // Each entry is a crate whose code Miri runs, and the crate that Miri tests.
        let mut open: Vec<(String, String)> = (miri::crates(&metadata).unwrap())
            .into_iter()
            .map(|found| (found.package.name.clone(), found.package.name))
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
        let (named, checked) = (named.unwrap(), miri::crates(&metadata).unwrap());
        for name in miri::SKIPPED {
            assert!(
                named.iter().any(|package| package.name == name),
                "`{name}` names no `unsafe_code`"
            );
            assert!(
                !checked.iter().any(|found| found.package.name == name),
                "Miri checks `{name}`"
            );
        }
    }
}

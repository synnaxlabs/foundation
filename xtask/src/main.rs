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
                "usage: cargo xtask <layers|globals|oracles|loom>\n       \
                 cargo xtask <shuttle|miri|sanitizers>\n       \
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

    /// The lines of the YAML `text`, with no blank or comment line, which YAML skips.
    fn yaml_lines(text: &str) -> Vec<&str> {
        // YAML breaks a line at CR, NEL, LS, and PS too, and a parser can read other
        // white space as text, so `str::lines` and `str::trim` would drop a line that
        // YAML reads.
        if let Some(c) = text.chars().find(|c| *c != '\n' && !matches!(c, ' '..='~')) {
            panic!("ci.yaml has a character that is not printable ASCII or LF: {c:?}");
        }
        let lines: Vec<&str> = (text.lines())
            .filter(|line| !line.trim().is_empty() && !line.trim().starts_with('#'))
            .collect();
        // The checks read each line as one node, so none may run on into the next.
        // The text of a block scalar is not YAML.
        let mut block = None;
        for line in &lines {
            if block.is_some_and(|key| indent(line) > key) {
                continue;
            }
            let code = code(line);
            let header = code.trim_end_matches(['-', '+']);
            block = (header.ends_with(": |") || header.ends_with(": >"))
                .then(|| line.len() - line.trim_start_matches([' ', '-']).len());
        }
        lines
    }

    /// The text of the YAML `line` before its comment.
    ///
    /// # Panics
    ///
    /// When a quoted scalar or a flow sequence of `line` does not close on it, a flow
    /// sequence holds more than plain words, or `line` has a flow mapping, an anchor,
    /// alias, or tag, a `?` or `:` at the start of a node, a `#` after a character that
    /// is not a space, or a document marker, any of which can make a node run on.
    fn code(line: &str) -> &str {
        let refused = || panic!("ci.yaml has a node that can run on: `{line}`");
        if line.starts_with("---") || line.starts_with("...") {
            refused();
        }
        // `start` holds where a node can start, the only place where a quote or a
        // bracket opens one.
        let (mut quote, mut escaped, mut flow, mut start) = (None, false, false, true);
        let mut prev = ' ';
        let mut chars = line.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if let Some(open) = quote {
                let doubled = chars.peek().is_some_and(|(_, next)| *next == '\'');
                if escaped {
                    escaped = false;
                } else if (open, c) == ('"', '\\')
                    || (open, c, doubled) == ('\'', '\'', true)
                {
                    escaped = true;
                } else if c == open {
                    (quote, start) = (None, false);
                }
                prev = c;
                continue;
            }
            // A flow of plain words is the only flow ci.yaml needs, and the only one
            // whose end each parser finds at the same place.
            if flow {
                match c {
                    ']' => (flow, start) = (false, false),
                    ' ' | ',' | '-' | '_' | '.' | '/' => {}
                    _ if c.is_ascii_alphanumeric() => {}
                    _ => refused(),
                }
                continue;
            }
            match c {
                ' ' if prev == ':' => start = true,
                // A `-` keeps a node start only before a space.
                '-' if chars.peek().is_some_and(|(_, next)| *next == ' ') => {}
                ' ' => {}
                '#' if prev == ' ' => return line[..i].trim_end(),
                // Parsers differ on whether such a `#` starts a comment.
                '#' => refused(),
                '\'' | '"' if start => quote = Some(c),
                '[' if start => flow = true,
                '{' | '&' | '*' | '!' | '?' | ':' if start => refused(),
                _ => start = false,
            }
            prev = c;
        }
        if quote.is_some() || flow {
            refused();
        }
        line
    }

    /// `.github/workflows/ci.yaml` in the workspace at `root`, with no blank or comment
    /// line.
    fn workflow(root: &Path) -> String {
        let text =
            std::fs::read_to_string(root.join(".github/workflows/ci.yaml")).unwrap();
        let ci = yaml_lines(&text);
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

    /// The spaces before the text of `line`.
    fn indent(line: &str) -> usize {
        line.len() - line.trim_start().len()
    }

    /// The lines under the first line of `lines` that is `key`: each line after it up
    /// to the first that is not indented further, which is the value of `key` in YAML.
    fn under<'a>(lines: &[&'a str], key: &str) -> Vec<&'a str> {
        let start = (lines.iter().position(|line| *line == key))
            .unwrap_or_else(|| panic!("ci.yaml has no `{}` in {lines:#?}", key.trim()));
        (lines[start + 1..].iter().copied())
            .take_while(|line| indent(line) > indent(key))
            .collect()
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
    const JOBS: [(&str, &[&str]); 3] = [
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
            "miri",
            &[
                "    needs: changes",
                "    if: \"!cancelled() && needs.changes.outputs.models != 'false'\"",
                "    runs-on: ubuntu-24.04",
                "    steps:",
                "      - uses: actions/checkout@v4",
                "      - name: Install the pinned nightly",
                "        run: |",
                "          rustup toolchain install \"$(cat rust-toolchain-nightly)\" \
             --profile minimal \\",
                "            --component miri,rust-src",
                "      - uses: Swatinem/rust-cache@v2",
                "      - name: Miri",
                "        run: cargo xtask miri",
            ],
        ),
    ];

    /// Checks each model job in the lines `jobs` under the key `jobs` of a workflow.
    ///
    /// # Panics
    ///
    /// When a model job is not its job in [`JOBS`].
    fn pin_jobs(jobs: &[&str]) {
        for (task, job) in JOBS {
            let found = under(jobs, &format!("  {task}:"));
            assert!(
                found == job,
                "the {task} job of .github/workflows/ci.yaml is not the job that the \
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
    fn yaml_lines_skips_each_blank_and_comment_line() {
        let text = "a:\n\n  \n  # x\n  b: 1 # c\n";
        assert_eq!(yaml_lines(text), ["a:", "  b: 1 # c"]);
        let printable: String = (' '..='~').collect();
        let text = format!("a: |\n  {printable}");
        assert_eq!(yaml_lines(&text), ["a: |", &format!("  {printable}")]);
    }

    #[test]
    fn yaml_lines_keeps_each_node_that_closes_on_its_line() {
        let text = concat!(
            "a: 'b''c'\n",
            "b: \"c\\\"d\" # e\n",
            "c: it's a b # c\n",
            "d: [e, f-g, h_i.j/k] # it's\n",
            "e: \"!cancelled()\"\n",
            "f: |\n",
            "  [echo \"it's\n",
            "  - g: >-\n",
            "      [h\n",
            "i: 'j\\'\n",
            "a: b[c\n",
            "d: e,'f\n",
            "g: |+\n",
            "  'h\n",
            "[i]: j\n",
        );
        assert_eq!(yaml_lines(text), text.lines().collect::<Vec<_>>());
    }

    #[test]
    fn yaml_lines_keeps_each_quoted_key() {
        let text = concat!(
            "env:\n",
            "  \"A\": b\n",
            "  'C': d # e\n",
            "steps:\n",
            "  - \"f\": |\n",
            "      g\n",
        );
        assert_eq!(yaml_lines(text), text.lines().collect::<Vec<_>>());
    }

    #[test]
    fn yaml_lines_refuses_each_node_that_can_run_on() {
        for line in [
            "    runs-on: \"${{ 'ubuntu-latest' || '",
            "      [{name: Miri, run: \"true",
            "  - [x, [y]",
            "          key: 'x",
            "      - name: 'Miri",
            "     note: \"",
            "a'b: 'c",
            "a: {\"b\":'c}",
            "a: \"b\\\"",
            "a: [b, # c",
            "a: &b 'c'",
            "a: !b c",
            "a: *b",
            "  ? a",
            "  : b: |",
            "a: [? b]",
            "a: [b:'c, [d']",
            "a: [b:'#, [c']",
            "a: [b, [c]]",
            "a: [b, c",
            "--- 'a",
            "... [a",
            "a: {b: c}",
            "a: 'b''c",
            "a: b#c",
            "a: [\"b\"#c]",
            "a: ['b'#]",
            "a: ['b',#]",
            "a: {'b':#}",
            "a: [b - 'c, [d']",
            "a: [b - '#, [d']",
            "a: [-'b, [c']",
        ] {
            let text = format!("x: |\n  y\nz:\n{line}\n");
            let panic = std::panic::catch_unwind(|| yaml_lines(&text)).expect_err(line);
            assert_eq!(
                panic.downcast_ref::<String>(),
                Some(&format!("ci.yaml has a node that can run on: `{line}`")),
            );
        }
        let text = "  - a: |\n      b\n    c: 'd\n";
        let panic = std::panic::catch_unwind(|| yaml_lines(text)).expect_err(text);
        assert_eq!(
            panic.downcast_ref::<String>(),
            Some(&"ci.yaml has a node that can run on: `    c: 'd`".to_owned()),
        );
    }

    #[test]
    fn yaml_lines_refuses_each_character_that_is_not_printable_ascii_or_lf() {
        for c in [
            '\r', '\t', '\u{85}', '\u{a0}', '\u{2028}', '\u{2029}', '\u{7f}',
        ] {
            let text = format!("a:\n  #{c}  b:\n");
            let panic =
                std::panic::catch_unwind(|| yaml_lines(&text)).expect_err("refused");
            assert_eq!(
                panic.downcast_ref::<String>(),
                Some(&format!(
                    "ci.yaml has a character that is not printable ASCII or LF: {c:?}"
                )),
            );
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
        let ci = workflow(&root);
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
            miri::packages(&metadata).unwrap(),
        ];
        pin_jobs(&jobs);
        for ((task, _), packages) in JOBS.into_iter().zip(packages) {
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
        let ci = workflow(&fixture().join("../.."));
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
        let step = "        if: \"!cancelled() && env.RUST != 'false'\"";
        assert_eq!(
            ifs_and_names(&check, &["Clippy", "Test"]),
            (
                vec!["    if: \"!cancelled()\"", step, step],
                vec!["check", "macos", "windows"]
            )
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
        for key in ["crates:", "untested:", "tests:"] {
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

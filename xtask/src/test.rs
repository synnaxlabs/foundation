//! Runs the tests of the workspace in two parts that can run at the same time.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use crate::{field, skips};

/// A part of the tests of the workspace.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Part {
    /// Each test that a harness lists, with nextest.
    Nextest,
    /// With `cargo test`, the doc tests and each target that `cargo test` runs and that
    /// has no harness: nextest runs a test binary to list its tests, and such a binary
    /// lists none.
    Serial,
}

impl Part {
    /// The part named `name`: `nextest` or `serial`.
    pub(crate) fn parse(name: &str) -> Result<Self, String> {
        match name {
            "nextest" => Ok(Self::Nextest),
            "serial" => Ok(Self::Serial),
            _ => Err(format!("no part `{name}`: give `nextest` or `serial`")),
        }
    }
}

/// The targets of a workspace that `cargo test` runs and that have no harness, by
/// name.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Alone {
    pub(crate) tests: BTreeSet<String>,
    pub(crate) benches: BTreeSet<String>,
}

/// Runs `part` of the tests of the workspace at `root`, with `flags` on each cargo
/// command. nextest leaves out each test whose full path is in `skipped`, which
/// `Part::Serial` refuses. Each command runs to its end, and the run fails when one
/// fails. Each target that shares the name of a target of `Alone` also runs in
/// `Part::Serial`, as a cargo target flag selects by name.
pub(crate) fn run(
    root: &Path,
    part: Part,
    flags: &[&str],
    skipped: &[&str],
) -> Result<(), Vec<String>> {
    if part == Part::Serial && !skipped.is_empty() {
        return Err(vec!["`serial` skips no test".to_string()]);
    }
    let alone = alone(root).map_err(|e| vec![e])?;
    let mut problems = Vec::new();
    for mut command in commands(part, &alone, flags, skipped) {
        let shown = format!("{command:?}");
        match command.current_dir(root).status() {
            Ok(status) if status.success() => {}
            Ok(status) => problems.push(format!("{shown} ended with {status}")),
            Err(e) => problems.push(format!("{shown}: {e}")),
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// The targets of the workspace at `root` that `cargo test` runs and that have no
/// harness: each `test` or `bench` with `test = true` whose root file has a `main`, as
/// `cargo xtask skips` reads them.
pub(crate) fn alone(root: &Path) -> Result<Alone, String> {
    let metadata = crate::metadata(root)?;
    let mut alone = Alone::default();
    for package in field::list(&metadata, "packages")? {
        for target in field::list(package, "targets")? {
            let kinds = field::list(target, "kind")?;
            let names = if kinds.iter().any(|kind| kind == "test") {
                &mut alone.tests
            } else if kinds.iter().any(|kind| kind == "bench") {
                &mut alone.benches
            } else {
                continue;
            };
            if !field::flag(target, "test")? {
                continue;
            }
            let file = field::text(target, "src_path")?;
            let text =
                std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
            let parsed = syn::parse_file(&text).map_err(|e| format!("{file}: {e}"))?;
            if parsed.items.iter().any(skips::main) {
                names.insert(field::text(target, "name")?.to_string());
            }
        }
    }
    Ok(alone)
}

/// The cargo commands of `part`, in the order that they run.
fn commands(
    part: Part,
    alone: &Alone,
    flags: &[&str],
    skipped: &[&str],
) -> Vec<Command> {
    let cargo = |args: &[&str]| {
        let mut command = crate::cargo();
        command.args(args).arg("--workspace").args(flags);
        command
    };
    match part {
        Part::Nextest => {
            let mut command = cargo(&["nextest", "run"]);
            command.args(["--no-fail-fast", "--retries", "0"]);
            if let Some(filter) = filter(alone, skipped) {
                command.args(["-E", &filter]);
            }
            vec![command]
        }
        Part::Serial => {
            let mut doc = cargo(&["test"]);
            doc.args(["--no-fail-fast", "--doc"]);
            if alone == &Alone::default() {
                return vec![doc];
            }
            let mut targets = cargo(&["test"]);
            targets.arg("--no-fail-fast");
            for test in &alone.tests {
                targets.args(["--test", test]);
            }
            for bench in &alone.benches {
                targets.args(["--bench", bench]);
            }
            vec![doc, targets]
        }
    }
}

/// The nextest filterset that leaves out each target of `alone` and each test whose
/// full path is in `skipped`, or none when both are empty. A filter on the binary keeps
/// nextest from running it to list its tests.
fn filter(alone: &Alone, skipped: &[&str]) -> Option<String> {
    let tests =
        (alone.tests.iter()).map(|name| format!("(kind(test) & binary(={name}))"));
    let benches =
        (alone.benches.iter()).map(|name| format!("(kind(bench) & binary(={name}))"));
    let skipped = skipped.iter().map(|path| format!("test(={path})"));
    let terms: Vec<String> = tests.chain(benches).chain(skipped).collect();
    (!terms.is_empty()).then(|| format!("not ({})", terms.join(" | ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    fn args(command: &Command) -> Vec<String> {
        (command.get_args())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn alone_holds_each_tested_target_with_a_main() {
        // `untimed` is a bench with no `test = true`, and `cargo test` does not run it.
        assert_eq!(
            alone(&fixture().join("gates")).unwrap(),
            Alone {
                tests: names(&["free"]),
                benches: names(&["timed"]),
            }
        );
    }

    #[test]
    fn nextest_leaves_out_each_target_with_no_harness_and_each_skipped_test() {
        let alone = Alone {
            tests: names(&["memory"]),
            benches: names(&["open", "write"]),
        };
        let commands = commands(Part::Nextest, &alone, &["--all-features"], &["a::b"]);
        assert_eq!(
            commands.iter().map(args).collect::<Vec<_>>(),
            [[
                "nextest",
                "run",
                "--workspace",
                "--all-features",
                "--no-fail-fast",
                "--retries",
                "0",
                "-E",
                "not ((kind(test) & binary(=memory)) | (kind(bench) & binary(=open)) \
                 | (kind(bench) & binary(=write)) | test(=a::b))",
            ]]
        );
    }

    #[test]
    fn nextest_gives_no_filter_when_it_leaves_out_nothing() {
        let commands = commands(Part::Nextest, &Alone::default(), &[], &[]);
        assert_eq!(
            commands.iter().map(args).collect::<Vec<_>>(),
            [[
                "nextest",
                "run",
                "--workspace",
                "--no-fail-fast",
                "--retries",
                "0"
            ]]
        );
    }

    #[test]
    fn serial_runs_only_the_doc_tests_when_no_target_has_no_harness() {
        // `cargo test` with no target flag runs each target.
        let commands = commands(Part::Serial, &Alone::default(), &[], &[]);
        assert_eq!(
            commands.iter().map(args).collect::<Vec<_>>(),
            [["test", "--workspace", "--no-fail-fast", "--doc"]]
        );
    }

    #[test]
    fn serial_runs_the_doc_tests_and_each_target_with_no_harness() {
        let alone = Alone {
            tests: names(&["memory"]),
            benches: names(&["open"]),
        };
        let flags = ["--exclude", "a"];
        let commands = commands(Part::Serial, &alone, &flags, &[]);
        assert_eq!(
            commands.iter().map(args).collect::<Vec<_>>(),
            [
                vec![
                    "test",
                    "--workspace",
                    "--exclude",
                    "a",
                    "--no-fail-fast",
                    "--doc"
                ],
                vec![
                    "test",
                    "--workspace",
                    "--exclude",
                    "a",
                    "--no-fail-fast",
                    "--test",
                    "memory",
                    "--bench",
                    "open",
                ],
            ]
        );
    }

    #[test]
    fn serial_refuses_a_skipped_test() {
        assert_eq!(
            run(&fixture().join("gates"), Part::Serial, &[], &["a::b"]),
            Err(vec!["`serial` skips no test".to_string()])
        );
    }

    #[test]
    fn parse_names_each_part_and_refuses_another() {
        assert_eq!(Part::parse("nextest"), Ok(Part::Nextest));
        assert_eq!(Part::parse("serial"), Ok(Part::Serial));
        assert_eq!(
            Part::parse("lint"),
            Err("no part `lint`: give `nextest` or `serial`".to_string())
        );
    }
}

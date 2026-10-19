//! Runs the tests of the workspace in two parts that can run at the same time.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

use crate::{field, skips};

/// A part of the tests of the workspace.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Part {
    /// Each test that a harness lists, with nextest.
    Nextest,
    /// With `cargo test`, the doc tests and each target that `cargo test` runs and
    /// whose root has a `main`: nextest runs a binary to list its tests, and such a
    /// binary lists none or prints a table that nextest cannot read.
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

/// The targets of a workspace that `cargo test` runs and whose root has a `main`, by
/// name.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Alone {
    pub(crate) tests: BTreeSet<String>,
    pub(crate) benches: BTreeSet<String>,
}

/// Runs `part` of the tests of the workspace at `root`: each command runs the program
/// `cargo` with `flags`, and the targets come from the metadata of the cargo of the
/// environment. nextest leaves out each test whose full path is in `skipped`, which
/// `Part::Serial` refuses. Each command runs to its end, and the run fails when one
/// fails. Each target that shares the name of a target of `Alone` also runs in
/// `Part::Serial`, as a cargo target flag selects by name. It refuses a pattern after
/// `--exclude`.
pub(crate) fn run(
    root: &Path,
    cargo: &OsStr,
    part: Part,
    flags: &[&str],
    skipped: &[&str],
) -> Result<(), Vec<String>> {
    if part == Part::Serial && !skipped.is_empty() {
        return Err(vec!["`serial` skips no test".to_string()]);
    }
    let alone = excluded(flags)
        .and_then(|excluded| alone(root, &excluded))
        .map_err(|e| vec![e])?;
    let mut problems = Vec::new();
    for mut command in commands(cargo, part, &alone, flags, skipped) {
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

/// The packages that `flags` leave out with `--exclude <package>` or
/// `--exclude=<package>`. Fails on a pattern, which cargo also takes.
fn excluded<'a>(flags: &[&'a str]) -> Result<BTreeSet<&'a str>, String> {
    let spaced = (flags.windows(2))
        .filter(|pair| pair[0] == "--exclude")
        .map(|pair| pair[1]);
    let joined = (flags.iter()).filter_map(|flag| flag.strip_prefix("--exclude="));
    (spaced.chain(joined))
        .map(|spec| {
            if spec.contains(['*', '?', '[']) {
                Err(format!(
                    "`--exclude {spec}`: give a package name, not a pattern"
                ))
            } else {
                Ok(spec)
            }
        })
        .collect()
}

/// The targets of the workspace at `root`, other than those of the `excluded`
/// packages, that `cargo test` runs and whose root has a `main`, as `cargo xtask
/// skips` reads them. A cargo target flag fails when no selected package has the
/// target.
pub(crate) fn alone(root: &Path, excluded: &BTreeSet<&str>) -> Result<Alone, String> {
    let metadata = crate::metadata(root)?;
    let mut alone = Alone::default();
    for package in field::list(&metadata, "packages")? {
        if excluded.contains(field::text(package, "name")?) {
            continue;
        }
        for target in field::list(package, "targets")? {
            let kinds = field::list(target, "kind")?;
            let names = if kinds.iter().any(|kind| kind == "test") {
                &mut alone.tests
            } else if kinds.iter().any(|kind| kind == "bench") {
                &mut alone.benches
            } else {
                continue;
            };
            let entry = skips::entry(target)?;
            let file = field::text(target, "src_path")?;
            let text =
                std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
            let parsed = syn::parse_file(&text).map_err(|e| format!("{file}: {e}"))?;
            if skips::alone(entry, &parsed) {
                names.insert(field::text(target, "name")?.to_string());
            }
        }
    }
    Ok(alone)
}

/// The cargo commands of `part`, in the order that they run.
fn commands(
    program: &OsStr,
    part: Part,
    alone: &Alone,
    flags: &[&str],
    skipped: &[&str],
) -> Vec<Command> {
    let cargo = |args: &[&str]| {
        let mut command = Command::new(program);
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
    use std::path::PathBuf;

    use super::*;
    use crate::fixture;

    /// A stand-in cargo that adds a line of its arguments to the file `ran` in its
    /// folder, and fails each `--doc` run.
    const CARGO: &str = r#"
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut ran = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(env!("RAN"))
        .unwrap();
    writeln!(ran, "{}", args.join(" ")).unwrap();
    if args.iter().any(|arg| arg == "--doc") {
        std::process::exit(1);
    }
}
"#;

    /// Builds the stand-in cargo in a new folder named `name`, and gives the folder and
    /// the program. The caller removes the folder.
    fn create_cargo(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cargo.rs"), CARGO).unwrap();
        let program = dir.join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
        let built = Command::new("rustc")
            .arg(dir.join("cargo.rs"))
            .arg("-o")
            .arg(&program)
            .env("RAN", dir.join("ran"))
            .status()
            .unwrap();
        assert!(built.success(), "rustc failed on the stand-in cargo");
        (dir, program)
    }

    /// Runs `part` on the `gates` fixture with the stand-in cargo, and gives its result
    /// and the arguments of each command that it ran.
    fn ran(
        name: &str,
        part: Part,
        flags: &[&str],
        skipped: &[&str],
    ) -> (Result<(), Vec<String>>, Vec<String>, PathBuf) {
        let (dir, program) = create_cargo(name);
        let result = run(
            &fixture().join("gates"),
            program.as_os_str(),
            part,
            flags,
            skipped,
        );
        let lines = std::fs::read_to_string(dir.join("ran")).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();
        (result, lines.lines().map(str::to_owned).collect(), program)
    }

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn alone_holds_each_tested_target_with_a_main() {
        // `untimed` is a bench with no `test = true`, and `cargo test` does not run it.
        assert_eq!(
            alone(&fixture().join("gates"), &BTreeSet::new()).unwrap(),
            Alone {
                tests: names(&["free"]),
                benches: names(&["timed"]),
            }
        );
    }

    #[test]
    fn alone_leaves_out_each_excluded_package() {
        let flags = ["--all-features", "--exclude", "g", "--exclude", "h"];
        assert_eq!(excluded(&flags), Ok(BTreeSet::from(["g", "h"])));
        assert_eq!(
            alone(&fixture().join("gates"), &excluded(&flags).unwrap()),
            Ok(Alone::default())
        );
    }

    #[test]
    fn nextest_leaves_out_each_target_with_no_harness_and_each_skipped_test() {
        let (result, lines, _) = ran(
            "xtask-test-nextest-filter",
            Part::Nextest,
            &["--all-features"],
            &["a::b", "c::d"],
        );
        assert_eq!(result, Ok(()));
        assert_eq!(
            lines,
            [
                "nextest run --workspace --all-features --no-fail-fast --retries 0 -E \
                 not ((kind(test) & binary(=free)) | (kind(bench) & binary(=timed)) \
                 | test(=a::b) | test(=c::d))"
            ]
        );
    }

    #[test]
    fn nextest_gives_no_filter_when_it_leaves_out_nothing() {
        let (result, lines, _) = ran(
            "xtask-test-nextest-all",
            Part::Nextest,
            &["--exclude", "g"],
            &[],
        );
        assert_eq!(result, Ok(()));
        assert_eq!(
            lines,
            ["nextest run --workspace --exclude g --no-fail-fast --retries 0"]
        );
    }

    #[test]
    fn serial_runs_each_command_and_names_each_that_failed() {
        let flags = ["--all-features"];
        let (result, lines, program) =
            ran("xtask-test-serial-failed", Part::Serial, &flags, &[]);
        let mut doc = Command::new(&program);
        doc.args([
            "test",
            "--workspace",
            "--all-features",
            "--no-fail-fast",
            "--doc",
        ]);
        let status = if cfg!(windows) {
            "exit code: 1"
        } else {
            "exit status: 1"
        };
        assert_eq!(result, Err(vec![format!("{doc:?} ended with {status}")]));
        assert_eq!(
            lines,
            [
                "test --workspace --all-features --no-fail-fast --doc",
                concat!(
                    "test --workspace --all-features --no-fail-fast ",
                    "--test free --bench timed"
                ),
            ]
        );
    }

    #[test]
    fn serial_runs_only_the_doc_tests_when_no_target_has_no_harness() {
        // `cargo test` with no target flag runs each target.
        let (_, lines, _) = ran(
            "xtask-test-serial-doc",
            Part::Serial,
            &["--exclude", "g"],
            &[],
        );
        assert_eq!(lines, ["test --workspace --exclude g --no-fail-fast --doc"]);
    }

    #[test]
    fn serial_leaves_out_a_package_excluded_with_an_equals_sign() {
        let (result, lines, program) = ran(
            "xtask-test-serial-equals",
            Part::Serial,
            &["--exclude=g"],
            &[],
        );
        let mut doc = Command::new(&program);
        doc.args([
            "test",
            "--workspace",
            "--exclude=g",
            "--no-fail-fast",
            "--doc",
        ]);
        let status = if cfg!(windows) {
            "exit code: 1"
        } else {
            "exit status: 1"
        };
        assert_eq!(result, Err(vec![format!("{doc:?} ended with {status}")]));
        assert_eq!(lines, ["test --workspace --exclude=g --no-fail-fast --doc"]);
    }

    #[test]
    fn run_refuses_an_excluded_pattern_in_each_form() {
        for (name, flags) in [
            ("xtask-test-pattern-space", ["--exclude", "g*"].as_slice()),
            ("xtask-test-pattern-equals", ["--exclude=g?"].as_slice()),
            ("xtask-test-pattern-set", ["--exclude", "[g]"].as_slice()),
        ] {
            let (result, lines, _) = ran(name, Part::Nextest, flags, &[]);
            let spec = flags.last().unwrap().trim_start_matches("--exclude=");
            assert_eq!(
                result,
                Err(vec![format!(
                    "`--exclude {spec}`: give a package name, not a pattern"
                )])
            );
            assert_eq!(lines, Vec::<String>::new());
        }
    }

    #[test]
    fn serial_refuses_a_skipped_test() {
        let (result, lines, _) =
            ran("xtask-test-serial-skip", Part::Serial, &[], &["a::b"]);
        assert_eq!(result, Err(vec!["`serial` skips no test".to_string()]));
        assert_eq!(lines, Vec::<String>::new());
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

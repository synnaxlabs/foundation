//! Runs Miri on the pinned nightly.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use serde_json::Value;

use crate::libtest::{self, Run};
use crate::{field, select};

/// The `MIRIFLAGS` of each Miri pass.
const PASSES: [&str; 2] = [
    "-Zmiri-strict-provenance",
    "-Zmiri-strict-provenance -Zmiri-tree-borrows",
];

/// The kind of nextest `--partition` that splits the lib tests between shards: of the
/// three kinds, `hash` gave the most even shards in CI.
const PARTITION: &str = "hash";

/// Crates whose tests run code that Miri cannot run: the OS, C, or the Python
/// interpreter. Tests on the real OS check `os` (BLOCK MEMORY), `cargo xtask
/// sanitizers` checks `connector-opcua`, and tests on the Python interpreter check
/// `sdk-python`.
pub(crate) const SKIPPED: [&str; 3] = ["os", crate::sanitizers::CRATE, "sdk-python"];

/// A part of the Miri tests for one CI job: of the units in order, each `count`th one
/// from `index`, and the nextest partition of the lib tests with the same numbers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Shard {
    index: usize,
    count: usize,
}

impl Shard {
    /// Each test.
    pub(crate) const ALL: Self = Self { index: 0, count: 1 };

    /// Reads `<index>/<count>`, the form of `cargo mutants --shard`.
    ///
    /// # Errors
    ///
    /// When `text` has another form, or `index` is not less than `count`.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let shard = text.split_once('/').and_then(|(index, count)| {
            Some(Self {
                index: index.parse().ok()?,
                count: count.parse().ok()?,
            })
        });
        shard
            .filter(|shard| shard.index < shard.count)
            .ok_or_else(|| {
                format!("`{text}` is not a shard `<index>/<count>` with index < count")
            })
    }

    /// The items of this shard, in order: each `count`th one from `index`.
    pub(crate) fn pick<T>(self, items: impl IntoIterator<Item = T>) -> Vec<T> {
        (items.into_iter().skip(self.index))
            .step_by(self.count)
            .collect()
    }

    /// The nextest `--partition` of this shard, whose index starts at 1.
    pub(crate) fn partition(self) -> String {
        format!("{PARTITION}:{}/{}", self.index + 1, self.count)
    }
}

impl fmt::Display for Shard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.index, self.count)
    }
}

/// The names of tests.
pub(crate) type Tests = BTreeSet<String>;

/// What one unit of a crate's tests runs, other than its lib tests, which nextest
/// runs.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Part {
    /// The doc tests of the lib target.
    Doc,
    /// Each test of a target of `kind` (`test`, `bin`, `example`, or `bench`), by its
    /// name. A run of it takes no filter: a target with no harness ignores the filter.
    Target { kind: String, name: String },
}

impl Part {
    /// The arguments of `cargo miri test` that run this part.
    pub(crate) fn args(&self) -> Vec<String> {
        match self {
            Self::Doc => vec!["--doc".to_string()],
            Self::Target { kind, name } => vec![format!("--{kind}"), name.clone()],
        }
    }
}

impl fmt::Display for Part {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Doc => f.write_str("the doc tests"),
            Self::Target { kind, name } => write!(f, "the {kind} target `{name}`"),
        }
    }
}

/// A part of one pass: its `MIRIFLAGS`, its crate, and the part.
pub(crate) type Unit<'a> = (&'a str, &'a select::Package, &'a Part);

/// A crate of [`crates`], with each other target of its tests that `cargo miri test`
/// runs.
#[derive(Debug, PartialEq)]
pub(crate) struct Crate {
    pub(crate) package: select::Package,
    /// Whether its lib target has tests.
    pub(crate) lib: bool,
    /// Whether Cargo runs its doc tests.
    pub(crate) doctest: bool,
    /// A [`Part::Target`] for each other target that `cargo test` runs.
    pub(crate) parts: Vec<Part>,
}

impl Crate {
    /// Reads a package object of `cargo metadata`.
    ///
    /// # Errors
    ///
    /// A missing field.
    pub(crate) fn read(package: &Value) -> Result<Self, String> {
        let mut read = Self {
            package: select::Package::read(package)?,
            lib: false,
            doctest: false,
            parts: Vec::new(),
        };
        for target in field::list(package, "targets")? {
            let kind = field::list(target, "kind")?.first().and_then(Value::as_str);
            let tested = field::flag(target, "test")?;
            match kind {
                Some(kind @ ("test" | "bin" | "example" | "bench")) => {
                    if tested {
                        read.parts.push(Part::Target {
                            kind: kind.to_string(),
                            name: field::text(target, "name")?.to_string(),
                        });
                    }
                }
                Some("custom-build") => {}
                _ => {
                    read.lib = tested;
                    read.doctest = field::flag(target, "doctest")?;
                }
            }
        }
        Ok(read)
    }
}

/// Each workspace crate whose source names `unsafe_code`, the lint that each `unsafe`
/// use must expect, except those in [`SKIPPED`], in the order of `metadata`.
pub(crate) fn crates(metadata: &Value) -> Result<Vec<Crate>, String> {
    let objects = select::matching(metadata, |s| select::has_word(s, "unsafe_code"))?;
    let mut crates = (objects.into_iter())
        .map(Crate::read)
        .collect::<Result<Vec<_>, _>>()?;
    crates.retain(|found| !SKIPPED.contains(&found.package.name.as_str()));
    Ok(crates)
}

/// The id of each crate of `crates` whose lib target has tests.
pub(crate) fn libs(crates: &[Crate]) -> Vec<String> {
    (crates.iter())
        .filter(|found| found.lib)
        .map(|found| found.package.id.clone())
        .collect()
}

/// The id of each crate of `crates` whose doc tests Cargo runs and for which `list`
/// gives a doc test that Miri runs. It calls `list` only for the crates whose doc
/// tests Cargo runs.
///
/// # Errors
///
/// An error of `list`.
pub(crate) fn docs(
    crates: &[Crate],
    mut list: impl FnMut(&select::Package) -> Result<Tests, String>,
) -> Result<BTreeSet<String>, String> {
    let mut docs = BTreeSet::new();
    for found in crates.iter().filter(|found| found.doctest) {
        if !list(&found.package)?.is_empty() {
            docs.insert(found.package.id.clone());
        }
    }
    Ok(docs)
}

/// The id of each package with a test in `list`, the JSON of `cargo nextest list`,
/// that it does not ignore.
///
/// # Errors
///
/// A missing field.
pub(crate) fn tested(list: &Value) -> Result<BTreeSet<String>, String> {
    let suites = (list.get("rust-suites").and_then(Value::as_object))
        .ok_or("`cargo nextest list` gave no `rust-suites`")?;
    let mut tested = BTreeSet::new();
    for suite in suites.values() {
        let cases = (suite.get("testcases").and_then(Value::as_object))
            .ok_or("a suite of `cargo nextest list` has no `testcases`")?;
        for case in cases.values() {
            if !field::flag(case, "ignored")? {
                tested.insert(field::text(suite, "package-id")?.to_string());
            }
        }
    }
    Ok(tested)
}

/// The units of `shard`, and a problem for each crate of `crates` with no test that
/// Miri runs. A crate whose id `docs` holds runs its doc tests first, and one whose id
/// `tested` holds runs its lib tests. Each pass of a part comes after the one before
/// it, so the passes of a slow target fall in different shards.
pub(crate) fn plan<'a>(
    crates: &'a [Crate],
    docs: &BTreeSet<String>,
    tested: &BTreeSet<String>,
    shard: Shard,
) -> (Vec<Unit<'a>>, Vec<String>) {
    let ran = |found: &Crate| {
        docs.contains(&found.package.id) || tested.contains(&found.package.id)
    };
    let problems = (crates.iter())
        .filter(|found| found.parts.is_empty() && !ran(found))
        .map(|found| {
            format!(
                "`{}` names `unsafe_code` but runs no tests under Miri. Add tests that \
                 reach its unsafe code.",
                found.package.name
            )
        })
        .collect();
    let all = crates.iter().flat_map(|found| {
        let doc = docs.contains(&found.package.id).then_some(&Part::Doc);
        (doc.into_iter().chain(&found.parts))
            .flat_map(move |part| PASSES.map(|flags| (flags, &found.package, part)))
    });
    (shard.pick(all), problems)
}

/// The problem of `run`, a Miri run with `flags` of `part` of the crate `name`, if
/// any.
pub(crate) fn problem(
    run: &Run,
    flags: &str,
    name: &str,
    part: &Part,
) -> Option<String> {
    match (run, part) {
        (Run::Failed, _) => {
            Some(format!("Miri with `{flags}` failed on {part} of `{name}`"))
        }
        // Only a target with no harness prints no report.
        (Run::Passed, _) | (Run::Silent, Part::Target { .. }) => None,
        (Run::Empty | Run::Silent, _) => Some(format!(
            "Miri with `{flags}` ran none of {part} of `{name}`"
        )),
    }
}

/// Runs the Miri tests of `shard`, of each pass in [`PASSES`] and each crate of
/// [`crates`]. `cargo miri nextest` runs the lib tests of the partition of `shard`,
/// one process for each test, and fails when the partition has none. `cargo miri test`
/// runs the doc tests of a crate, or another test target, whole, one at a time on each
/// core. It uses rustup, the nightly in `rust-toolchain-nightly`, and the
/// `cargo-nextest` on the `PATH`. It fails when a test fails, when a run of the doc
/// tests or of a target with a harness runs none, or when a crate has no test that
/// Miri runs.
pub(crate) fn run(root: &Path, shard: Shard) -> Result<(), Vec<String>> {
    let nightly = crate::nightly::Toolchain::read(root).map_err(|e| vec![e])?;
    let metadata = crate::metadata(root).map_err(|e| vec![e])?;
    let crates = crates(&metadata).map_err(|e| vec![e])?;
    if crates.is_empty() {
        eprintln!("no crate names `unsafe_code`, so Miri has nothing to check");
        return Ok(());
    }
    let libs = libs(&crates);
    let tested = list(&nightly, &libs).map_err(|e| vec![e])?;
    let docs =
        docs(&crates, |package| doc_tests(&nightly, package)).map_err(|e| vec![e])?;
    let (units, mut problems) = plan(&crates, &docs, &tested, shard);
    for flags in PASSES {
        problems.extend(nextest(&nightly, &libs, flags, shard).map_err(|e| vec![e])?);
    }
    let next = AtomicUsize::new(0);
    #[expect(clippy::disallowed_methods, reason = "a dev tool uses each core")]
    let workers = thread::available_parallelism().map_or(1, usize::from);
    let runs: Vec<_> = thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                #[expect(
                    clippy::disallowed_methods,
                    reason = "a dev tool uses each core"
                )]
                scope.spawn(|| worker(&nightly, &units, &next))
            })
            .collect();
        (handles.into_iter())
            .map(|handle| {
                handle
                    .join()
                    .expect("invariant: a Miri worker never panics")
            })
            .collect()
    });
    for run in runs {
        problems.extend(run.map_err(|e| vec![e])?);
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Runs each unit of `units` from the index `next` that no other worker takes, one
/// at a time, and gives the problem of each run.
pub(crate) fn worker(
    nightly: &crate::nightly::Toolchain,
    units: &[Unit<'_>],
    next: &AtomicUsize,
) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    while let Some(&(flags, package, part)) =
        units.get(next.fetch_add(1, Ordering::Relaxed))
    {
        let mut command = command(nightly, &package.id, flags);
        let run = libtest::run(command.args(part.args()))?;
        problems.extend(problem(&run, flags, &package.name, part));
    }
    Ok(problems)
}

/// The [`tested`] ids of `cargo miri nextest list` of the lib tests of the packages
/// `libs`. It starts no process when `libs` is empty, as nextest with no `-p` lists
/// each lib of the workspace.
pub(crate) fn list(
    nightly: &crate::nightly::Toolchain,
    libs: &[String],
) -> Result<BTreeSet<String>, String> {
    if libs.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut command = nextest_command(nightly, Nextest::List, libs, PASSES[0]);
    let output = (command.stderr(Stdio::inherit()).output())
        .map_err(|e| format!("{}: {e}", command.get_program().to_string_lossy()))?;
    if !output.status.success() {
        return Err(
            "`cargo miri nextest list` could not list the lib tests".to_string()
        );
    }
    let list = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("`cargo miri nextest list` gave no JSON: {e}"))?;
    tested(&list)
}

/// Runs the lib tests of the packages `libs` in the partition of `shard` with
/// `flags`, and gives the problem when they do not pass. It starts no process when
/// `libs` is empty.
pub(crate) fn nextest(
    nightly: &crate::nightly::Toolchain,
    libs: &[String],
    flags: &str,
    shard: Shard,
) -> Result<Option<String>, String> {
    if libs.is_empty() {
        return Ok(None);
    }
    let mut command = nextest_command(nightly, Nextest::Run(shard), libs, flags);
    let status = (command.status())
        .map_err(|e| format!("{}: {e}", command.get_program().to_string_lossy()))?;
    Ok((!status.success()).then(|| {
        format!("Miri with `{flags}` failed on the lib tests of shard {shard}")
    }))
}

/// The doc tests of `package` that Miri runs: each that it lists and does not ignore.
pub(crate) fn doc_tests(
    nightly: &crate::nightly::Toolchain,
    package: &select::Package,
) -> Result<Tests, String> {
    let list = |args: &[&str]| {
        let mut command = command(nightly, &package.id, PASSES[0]);
        command
            .args(["--doc", "--", "--list", "--format", "terse"])
            .args(args);
        let output = (command.stderr(Stdio::inherit()).output())
            .map_err(|e| format!("{}: {e}", command.get_program().to_string_lossy()))?;
        if !output.status.success() {
            return Err(format!(
                "Miri could not list the doc tests of `{}`",
                package.name
            ));
        }
        Ok(names(&String::from_utf8_lossy(&output.stdout)))
    };
    Ok(&list(&[])? - &list(&["--ignored"])?)
}

/// The name of each test in the output of libtest `--list --format terse`.
pub(crate) fn names(output: &str) -> Tests {
    (output.lines())
        .filter_map(|line| line.strip_suffix(": test"))
        .map(str::to_string)
        .collect()
}

/// The command that runs Miri with `flags` on the package `id`, on the toolchain
/// `nightly`.
fn command(nightly: &crate::nightly::Toolchain, id: &str, flags: &str) -> Command {
    let mut command = nightly.cargo();
    command
        .args(["miri", "test", "-p", id])
        .env("MIRIFLAGS", flags);
    command
}

/// What a `cargo miri nextest` command does.
#[derive(Clone, Copy)]
pub(crate) enum Nextest {
    /// Lists the tests as JSON.
    List,
    /// Runs the tests of the partition of the shard, and fails when it has none.
    Run(Shard),
}

/// The command `cargo miri nextest` that does `nextest` on the lib tests of the
/// packages `libs`, with `flags`.
pub(crate) fn nextest_command(
    nightly: &crate::nightly::Toolchain,
    nextest: Nextest,
    libs: &[String],
    flags: &str,
) -> Command {
    let mut command = nightly.cargo();
    command.args(["miri", "nextest"]);
    match nextest {
        Nextest::List => command.args(["list", "--message-format", "json"]),
        Nextest::Run(shard) => {
            let partition = shard.partition();
            command.args(["run", "--partition", &partition, "--no-tests", "fail"])
        }
    };
    command.arg("--lib");
    for id in libs {
        command.args(["-p", id]);
    }
    command.env("MIRIFLAGS", flags);
    command
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn selects_the_package_by_its_id() {
        let package = select::Package {
            id: "path+file:///w/crates/model#0.0.0".to_string(),
            name: "model".to_string(),
        };
        let nightly = crate::nightly::Toolchain::new("/w", "nightly-x");
        let command = command(&nightly, &package.id, PASSES[0]);
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "run",
                "nightly-x",
                "cargo",
                "miri",
                "test",
                "-p",
                &package.id
            ]
        );
    }

    #[test]
    fn a_shard_is_an_index_less_than_its_count() {
        assert_eq!(Shard::parse("7/8"), Ok(Shard { index: 7, count: 8 }));
        for text in ["8/8", "0/0", "-1/8", "a/8", "3", "3/8/9"] {
            assert_eq!(
                Shard::parse(text),
                Err(format!(
                    "`{text}` is not a shard `<index>/<count>` with index < count"
                ))
            );
        }
    }

    #[test]
    fn a_shard_picks_each_count_th_item_from_its_index() {
        let shard = |index| Shard { index, count: 3 };
        assert_eq!(shard(1).pick(0..10), [1, 4, 7]);
        assert_eq!(shard(0).pick(0..10), [0, 3, 6, 9]);
        assert_eq!(Shard::ALL.pick(0..3), [0, 1, 2]);
        assert_eq!(shard(2).pick(0..2), Vec::<i32>::new());
    }

    fn model() -> select::Package {
        select::Package {
            id: "path+file:///w/crates/model#0.0.0".to_string(),
            name: "model".to_string(),
        }
    }

    fn target(kind: &str, name: &str) -> Part {
        Part::Target {
            kind: kind.to_string(),
            name: name.to_string(),
        }
    }

    #[test]
    fn problem_names_each_run_that_does_not_pass_and_its_part() {
        let problem = |run, part| problem(&run, "-Zmiri-x", "model", &part);
        assert_eq!(
            problem(Run::Failed, Part::Doc),
            Some("Miri with `-Zmiri-x` failed on the doc tests of `model`".to_string())
        );
        assert_eq!(
            problem(Run::Failed, target("test", "it")),
            Some(
                "Miri with `-Zmiri-x` failed on the test target `it` of `model`"
                    .to_string()
            )
        );
        assert_eq!(
            problem(Run::Empty, Part::Doc),
            Some(
                "Miri with `-Zmiri-x` ran none of the doc tests of `model`".to_string()
            )
        );
        assert_eq!(problem(Run::Passed, Part::Doc), None);
    }

    #[test]
    fn a_part_gives_the_arguments_that_run_it() {
        assert_eq!(Part::Doc.args(), ["--doc"]);
        assert_eq!(target("bench", "fast").args(), ["--bench", "fast"]);
    }

    #[test]
    fn only_a_target_with_no_harness_runs_no_test_and_passes() {
        let problem = |run, part| problem(&run, "-Zmiri-x", "model", &part);
        assert_eq!(problem(Run::Silent, target("test", "it")), None);
        assert_eq!(
            problem(Run::Empty, target("bin", "model")),
            Some(
                "Miri with `-Zmiri-x` ran none of the bin target `model` of `model`"
                    .to_string()
            )
        );
        assert_eq!(
            problem(Run::Silent, Part::Doc),
            Some(
                "Miri with `-Zmiri-x` ran none of the doc tests of `model`".to_string()
            )
        );
    }

    #[test]
    fn a_crate_reads_its_lib_its_doc_tests_and_each_other_target_that_runs() {
        let target_object = |kind: &str, name: &str, test: bool| {
            let doctest = kind == "lib";
            json!({ "kind": [kind], "name": name, "test": test, "doctest": doctest })
        };
        let package = |targets: Vec<Value>| {
            let id = model().id;
            json!({ "id": id, "name": "model", "targets": targets })
        };
        let tested = package(vec![
            target_object("lib", "a", true),
            target_object("test", "it", true),
            target_object("test", "off", false),
            target_object("bin", "tool", true),
            target_object("example", "demo", true),
            target_object("bench", "fast", true),
            target_object("bench", "slow", false),
            target_object("custom-build", "build-script-build", false),
        ]);
        assert_eq!(
            Crate::read(&tested),
            Ok(Crate {
                package: model(),
                lib: true,
                doctest: true,
                parts: vec![
                    target("test", "it"),
                    target("bin", "tool"),
                    target("example", "demo"),
                    target("bench", "fast"),
                ],
            })
        );
        let untested = package(vec![
            json!({ "kind": ["lib"], "name": "a", "test": false, "doctest": false }),
        ]);
        assert_eq!(
            Crate::read(&untested),
            Ok(Crate {
                package: model(),
                lib: false,
                doctest: false,
                parts: vec![],
            })
        );
    }

    #[test]
    fn crates_are_the_packages_with_their_targets() {
        let metadata = crate::metadata(&crate::fixture()).unwrap();
        let crates = crates(&metadata).unwrap();
        let names: Vec<_> = crates.iter().map(|c| c.package.name.as_str()).collect();
        assert_eq!(names, ["model"]);
        assert!(crates[0].lib, "the lib of `model` has tests");
    }

    fn found(id: &str, lib: bool, parts: Vec<Part>) -> Crate {
        Crate {
            package: select::Package {
                id: format!("path+file:///w/crates/{id}#0.0.0"),
                name: id.to_string(),
            },
            lib,
            doctest: true,
            parts,
        }
    }

    #[test]
    fn docs_are_the_crates_with_a_doc_test_that_miri_runs() {
        let listed = found("listed", false, vec![]);
        let empty = found("empty", false, vec![]);
        let off = Crate {
            doctest: false,
            ..found("off", false, vec![])
        };
        let crates = [listed, empty, off];
        let mut asked = Vec::new();
        let picked = docs(&crates, |package| {
            asked.push(package.name.clone());
            Ok(match package.name.as_str() {
                "listed" => BTreeSet::from(["src/lib.rs - c (line 3)".to_string()]),
                _ => BTreeSet::new(),
            })
        });
        assert_eq!(picked, Ok(BTreeSet::from([crates[0].package.id.clone()])));
        assert_eq!(asked, ["listed", "empty"]);
        let failed = docs(&crates, |_| Err("no list".to_string()));
        assert_eq!(failed, Err("no list".to_string()));
    }

    #[test]
    fn libs_are_the_crates_whose_lib_has_tests() {
        let crates = [found("a", true, vec![]), found("b", false, vec![])];
        assert_eq!(libs(&crates), [crates[0].package.id.clone()]);
        assert_eq!(libs(&crates[1..]), Vec::<String>::new());
    }

    #[test]
    fn a_crate_with_no_test_that_miri_runs_is_a_problem() {
        let crates = [found("model", true, vec![])];
        let none = BTreeSet::new();
        let (units, problems) = plan(&crates, &none, &none, Shard::ALL);
        assert_eq!(units, []);
        let problem = "`model` names `unsafe_code` but runs no tests under Miri. Add \
                       tests that reach its unsafe code.";
        assert_eq!(problems, [problem]);
        let model = BTreeSet::from([crates[0].package.id.clone()]);
        let (_, problems) = plan(&crates, &none, &model, Shard::ALL);
        assert_eq!(problems, Vec::<String>::new());
        let (units, problems) = plan(&crates, &model, &none, Shard::ALL);
        assert_eq!(problems, Vec::<String>::new());
        assert_eq!(units.len(), PASSES.len());
    }

    #[test]
    fn tested_holds_each_package_with_a_test_that_nextest_does_not_ignore() {
        let suite = |id: &str, ignored: &[bool]| {
            let cases: serde_json::Map<_, _> = (ignored.iter().enumerate())
                .map(|(i, &ignored)| (format!("t{i}"), json!({ "ignored": ignored })))
                .collect();
            json!({ "package-id": id, "testcases": cases })
        };
        let list = json!({ "rust-suites": {
            "a": suite("a#0", &[true, false]),
            "b": suite("b#0", &[true]),
            "c": suite("c#0", &[]),
        }});
        assert_eq!(tested(&list), Ok(BTreeSet::from(["a#0".to_string()])));
        assert_eq!(
            tested(&json!({})),
            Err("`cargo nextest list` gave no `rust-suites`".to_string())
        );
        assert_eq!(
            tested(&json!({ "rust-suites": { "a": {} } })),
            Err("a suite of `cargo nextest list` has no `testcases`".to_string())
        );
    }

    #[test]
    fn names_are_the_tests_of_a_terse_list() {
        let output = "a::b: test\nsrc/lib.rs - c (line 3): test\nd: bench\n\n";
        let expected = ["a::b", "src/lib.rs - c (line 3)"].map(String::from);
        assert_eq!(names(output), BTreeSet::from(expected));
    }

    #[test]
    fn a_shard_gives_its_nextest_partition_from_1() {
        let shard = Shard { index: 7, count: 8 };
        assert_eq!(shard.partition(), "hash:8/8");
        assert_eq!(shard.to_string(), "7/8");
        assert_eq!(Shard::ALL.partition(), "hash:1/1");
    }

    #[test]
    fn nextest_lists_or_runs_the_lib_tests_of_each_package_with_the_flags() {
        let nightly = crate::nightly::Toolchain::new("/w", "nightly-x");
        let libs = ["a#0", "b#0"].map(String::from);
        let args = |nextest| {
            let command = nextest_command(&nightly, nextest, &libs, PASSES[1]);
            let flags = command.get_envs().find(|(key, _)| *key == "MIRIFLAGS");
            assert_eq!(
                flags,
                Some(("MIRIFLAGS".as_ref(), Some(PASSES[1].as_ref())))
            );
            let args: Vec<_> = command.get_args().map(ToOwned::to_owned).collect();
            args
        };
        let lead = ["run", "nightly-x", "cargo", "miri", "nextest"];
        let tail = ["--lib", "-p", "a#0", "-p", "b#0"];
        let list = ["list", "--message-format", "json"];
        assert_eq!(args(Nextest::List), [&lead[..], &list, &tail].concat());
        let shard = Shard { index: 2, count: 8 };
        let run = ["run", "--partition", "hash:3/8", "--no-tests", "fail"];
        assert_eq!(args(Nextest::Run(shard)), [&lead[..], &run, &tail].concat());
    }

    #[test]
    fn a_plan_puts_the_passes_of_a_part_next_to_each_other() {
        let (a, it) = (target("bin", "a"), target("test", "it"));
        let crates = [
            found("model", false, vec![a.clone()]),
            found("other", false, vec![it.clone()]),
        ];
        let (model, other) = (&crates[0].package, &crates[1].package);
        let (docs, none) = (BTreeSet::from([model.id.clone()]), BTreeSet::new());
        let (units, problems) = plan(&crates, &docs, &none, Shard::ALL);
        assert_eq!(problems, Vec::<String>::new());
        let [first, second] = PASSES;
        assert_eq!(
            units,
            [
                (first, model, &Part::Doc),
                (second, model, &Part::Doc),
                (first, model, &a),
                (second, model, &a),
                (first, other, &it),
                (second, other, &it),
            ]
        );
        let (units, _) = plan(&crates, &docs, &none, Shard { index: 1, count: 2 });
        assert_eq!(
            units,
            [
                (second, model, &Part::Doc),
                (second, model, &a),
                (second, other, &it),
            ]
        );
    }

    #[test]
    fn a_plan_runs_doc_tests_only_of_the_crates_in_docs() {
        let it = target("test", "it");
        let crates = [found("model", false, vec![it.clone()])];
        let model = &crates[0].package;
        let none = BTreeSet::new();
        let (units, _) = plan(&crates, &none, &none, Shard::ALL);
        let [first, second] = PASSES;
        assert_eq!(units, [(first, model, &it), (second, model, &it)]);
    }

    #[test]
    fn each_command_names_the_part_that_cannot_run_miri() {
        // `cargo miri` cannot start on stable, so each command fails.
        let root = crate::common::create_stable_root("xtask-miri-stable");
        let nightly = crate::nightly::Toolchain::read(&root).unwrap();
        let (package, libs) = (model(), [model().id]);
        let units = [(PASSES[0], &package, &Part::Doc)];
        let worked = worker(&nightly, &units, &AtomicUsize::new(0));
        let ran = nextest(&nightly, &libs, PASSES[1], Shard::ALL);
        let listed = list(&nightly, &libs);
        let docs = doc_tests(&nightly, &package);
        let empty = (
            list(&nightly, &[]),
            nextest(&nightly, &[], PASSES[0], Shard::ALL),
        );
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            worked,
            Ok(vec![format!(
                "Miri with `{}` failed on the doc tests of `model`",
                PASSES[0]
            )])
        );
        assert_eq!(
            ran,
            Ok(Some(format!(
                "Miri with `{}` failed on the lib tests of shard 0/1",
                PASSES[1]
            )))
        );
        assert_eq!(
            listed,
            Err("`cargo miri nextest list` could not list the lib tests".to_string())
        );
        assert_eq!(
            docs,
            Err("Miri could not list the doc tests of `model`".to_string())
        );
        assert_eq!(empty, (Ok(BTreeSet::new()), Ok(None)), "a process started");
    }

    /// A workspace at a stable root whose crate `model` names `unsafe_code`, has the
    /// test target `it`, and has `lib`, the lines of its `[lib]` table.
    fn create_model_root(name: &str, lib: &str) -> std::path::PathBuf {
        let root = crate::common::create_stable_root(name);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        let manifest = format!(
            "[package]\nname = \"model\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [lib]\n{lib}\n\n[[test]]\nname = \"it\"\npath = \"tests/it.rs\"\n"
        );
        std::fs::write(root.join("Cargo.toml"), manifest).unwrap();
        std::fs::write(root.join("src/lib.rs"), "#![deny(unsafe_code)]\n").unwrap();
        std::fs::write(root.join("tests/it.rs"), "#[test]\nfn a() {}\n").unwrap();
        root
    }

    #[test]
    fn run_gives_the_problem_of_each_command_that_cannot_run_miri() {
        let run = |name, lib| {
            let root = create_model_root(name, lib);
            let ran = run(&root, Shard::ALL);
            std::fs::remove_dir_all(&root).unwrap();
            ran.map_err(|mut problems| {
                problems.sort();
                problems
            })
        };
        let target = run("xtask-miri-run-target", "test = false\ndoctest = false");
        let failed = |flags| {
            format!("Miri with `{flags}` failed on the test target `it` of `model`")
        };
        assert_eq!(target, Err(vec![failed(PASSES[1]), failed(PASSES[0])]));
        assert_eq!(
            run("xtask-miri-run-lib", "test = true\ndoctest = false"),
            Err(vec![
                "`cargo miri nextest list` could not list the lib tests".to_string()
            ])
        );
        assert_eq!(
            run("xtask-miri-run-doc", "test = false\ndoctest = true"),
            Err(vec![
                "Miri could not list the doc tests of `model`".to_string()
            ])
        );
    }

    /// The source of a `cargo` for a toolchain folder that lists one doc test of
    /// `model`, no lib test, and fails each other command.
    const CARGO: &str = r#"fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if args.contains("nextest list") {
        println!("{{\"rust-suites\":{{}}}}");
    } else if args.contains("--ignored") {
    } else if args.contains("--list") {
        println!("src/lib.rs - x (line 1): test");
    } else {
        std::process::exit(1);
    }
}
"#;

    #[test]
    fn run_runs_each_part_of_each_pass_and_the_lib_tests() {
        let root =
            create_model_root("xtask-miri-run-all", "test = true\ndoctest = true");
        let toolchain = root.join("toolchain");
        std::fs::create_dir_all(toolchain.join("bin")).unwrap();
        std::fs::write(root.join("cargo.rs"), CARGO).unwrap();
        let cargo = format!("bin/cargo{}", std::env::consts::EXE_SUFFIX);
        let built = Command::new("rustc")
            .arg(root.join("cargo.rs"))
            .arg("-o")
            .arg(toolchain.join(cargo))
            .status()
            .unwrap();
        assert!(built.success(), "rustc failed on the stand-in cargo");
        let pin = toolchain.to_str().unwrap();
        std::fs::write(root.join("rust-toolchain-nightly"), pin).unwrap();
        let ran = run(&root, Shard::ALL).map_err(|mut problems| {
            problems.sort();
            problems
        });
        std::fs::remove_dir_all(&root).unwrap();
        let failed = |flags, part| format!("Miri with `{flags}` failed on {part}");
        let parts = [
            "the doc tests of `model`",
            "the lib tests of shard 0/1",
            "the test target `it` of `model`",
        ];
        let [first, second] = PASSES;
        let problems = [second, first]
            .into_iter()
            .flat_map(|flags| parts.map(|part| failed(flags, part)))
            .collect();
        assert_eq!(ran, Err(problems));
    }
}

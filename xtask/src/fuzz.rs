//! Runs each fuzz target on the pinned nightly, with its oracle inputs.

use std::num::NonZeroU16;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use semver::{Version, VersionReq};
use serde_json::Value;

use crate::field;

/// The seconds of each target when the task names none.
pub(crate) const SECONDS: NonZeroU16 = NonZeroU16::new(60).unwrap();

/// The seconds of each target that `text` names. libFuzzer wraps a count above
/// `i32::MAX`, and 0 means no limit, so it refuses each count outside 1 to 65535.
pub(crate) fn seconds(text: &str) -> Result<NonZeroU16, String> {
    text.parse()
        .map_err(|e| format!("`{text}` is not a count of seconds from 1 to 65535: {e}"))
}

/// The source of each requirement on crates.io in `cargo metadata`.
const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// The least `-max_len` of a run: libFuzzer makes no input longer.
const MAX_LEN: u64 = 16 * 1024;

/// Runs each target of `fuzz/` at `root` for `seconds`, as many at once as the host has
/// cores. Each run reads `fuzz/corpus/<target>`, which libFuzzer writes to, and
/// `oracles/fuzz/<target>`. It fails before the build on each problem that [`check`]
/// finds and when the pinned nightly gives no host triple, and then when the build
/// fails or a target fails. cargo-fuzz keeps the input of a crash in
/// `fuzz/artifacts/<target>/`.
pub(crate) fn run(root: &Path, seconds: NonZeroU16) -> Result<(), Vec<String>> {
    let targets = check(root)?;
    let cargo = Cargo::new(root).map_err(|e| vec![e])?;
    let built = cargo
        .build()
        .status()
        .map_err(|e| vec![format!("rustup: {e}")])?;
    if !built.success() {
        return Err(vec!["`cargo fuzz build` failed".to_string()]);
    }
    let next = AtomicUsize::new(0);
    let worker = || {
        let mut problems = Vec::new();
        while let Some(target) = targets.get(next.fetch_add(1, Ordering::Relaxed)) {
            if let Err(problem) = run_one(root, &cargo, target, seconds) {
                problems.push(problem);
            }
        }
        problems
    };
    #[expect(clippy::disallowed_methods, reason = "a dev tool uses each core")]
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let problems: Vec<String> = std::thread::scope(|scope| {
        #[expect(clippy::disallowed_methods, reason = "a dev tool uses each core")]
        let workers: Vec<_> = (0..cores.min(targets.len()))
            .map(|_| scope.spawn(worker))
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("invariant: a worker does not panic"))
            .collect()
    });
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Runs `target` for `seconds` and prints its result.
fn run_one(
    root: &Path,
    cargo: &Cargo,
    target: &str,
    seconds: NonZeroU16,
) -> Result<(), String> {
    let oracles = root.join("oracles/fuzz").join(target);
    let corpus = root.join("fuzz/corpus").join(target);
    let sizes = std::fs::read_dir(&oracles)
        .and_then(|entries| {
            entries
                .map(|entry| Ok(entry?.metadata()?.len()))
                .collect::<Result<Vec<u64>, std::io::Error>>()
        })
        .map_err(|e| format!("{}: {e}", oracles.display()))?;
    std::fs::create_dir_all(&corpus)
        .map_err(|e| format!("{}: {e}", corpus.display()))?;
    let max_len = max_len(sizes);
    let output = cargo
        .run(target, &corpus, &oracles, seconds, max_len)
        .output()
        .map_err(|e| format!("rustup: {e}"))?;
    let (report, result) = report(target, &output);
    eprint!("{report}");
    result
}

/// The name of each folder in `dir`.
fn folders(dir: &Path) -> Result<Vec<String>, Vec<String>> {
    let read = || -> std::io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Ok(names)
    };
    read().map_err(|e| vec![format!("{}: {e}", dir.display())])
}

/// A problem when there are no `targets`, and for each target with no folder of the
/// same name in `folders` and each folder with no target.
fn unmatched(targets: &[&str], folders: &[String]) -> Vec<String> {
    if targets.is_empty() {
        return vec!["fuzz/Cargo.toml has no bin target".to_string()];
    }
    let inputless = targets
        .iter()
        .filter(|target| !folders.iter().any(|folder| folder == *target))
        .map(|target| {
            format!("fuzz target `{target}` has no inputs in oracles/fuzz/{target}/")
        });
    let targetless = folders
        .iter()
        .filter(|folder| !targets.contains(&folder.as_str()))
        .map(|folder| format!("oracles/fuzz/{folder}/ has no fuzz target"));
    inputless.chain(targetless).collect()
}

/// cargo-fuzz on the pinned nightly, for the host triple of that nightly.
struct Cargo {
    nightly: crate::Nightly,
    host: String,
}

impl Cargo {
    /// cargo-fuzz on the pinned nightly of the workspace at `root`. cargo-fuzz builds
    /// for the triple that it was itself built for unless it gets `--target`, and a
    /// musl build of it cannot build a sanitized target.
    fn new(root: &Path) -> Result<Self, String> {
        let nightly = crate::nightly(root)?;
        let host = nightly.host()?;
        Ok(Self { nightly, host })
    }

    /// The command that builds each target.
    fn build(&self) -> Command {
        self.fuzz("build")
    }

    /// The command that runs `target` for `seconds`, on the inputs in `corpus` and
    /// `oracles`, with no input longer than `max_len` bytes.
    fn run(
        &self,
        target: &str,
        corpus: &Path,
        oracles: &Path,
        seconds: NonZeroU16,
        max_len: u64,
    ) -> Command {
        let mut command = self.fuzz("run");
        command.arg(target).arg(corpus).arg(oracles).args([
            "--".to_string(),
            format!("-max_total_time={seconds}"),
            "-timeout=10".to_string(),
            "-rss_limit_mb=2048".to_string(),
            format!("-max_len={max_len}"),
        ]);
        command
    }

    /// A command that runs `cargo fuzz <sub>` for the host, at the workspace root.
    fn fuzz(&self, sub: &str) -> Command {
        let mut command = self.nightly.cargo();
        command.args(["fuzz", sub, "--target", &self.host]);
        command
    }
}

/// The `-max_len` for oracle inputs of `sizes` bytes: [`MAX_LEN`], or the largest
/// input when it is longer, so that libFuzzer cuts no oracle input.
fn max_len(sizes: impl IntoIterator<Item = u64>) -> u64 {
    sizes.into_iter().fold(MAX_LEN, u64::max)
}

/// What the run of `target` that gave `output` prints, and its problem. A run that
/// passed prints libFuzzer's last `Done` line, and any other run its whole output. A
/// run that passed with no `Done` line ran no input, which is a problem.
fn report(target: &str, output: &Output) -> (String, Result<(), String>) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let done = stderr.lines().rfind(|line| line.starts_with("Done "));
    match (output.status.success(), done) {
        (true, Some(done)) => {
            (format!("fuzz target `{target}` passed: {done}\n"), Ok(()))
        }
        (true, None) => (
            format!("fuzz target `{target}` gave no `Done` line:\n{stdout}{stderr}\n"),
            Err(format!("fuzz target `{target}` gave no `Done` line")),
        ),
        (false, _) => (
            format!("fuzz target `{target}` failed:\n{stdout}{stderr}\n"),
            Err(format!(
                "fuzz target `{target}` failed. cargo-fuzz keeps the input of a crash \
                 in fuzz/artifacts/{target}/."
            )),
        ),
    }
}

/// Checks `fuzz/` at `root` before the build and returns its targets, read from its
/// graph. It fails when `fuzz/Cargo.lock` is stale, on each problem of [`unpatched`],
/// when `fuzz/` has no target, and when the targets and the folders of `oracles/fuzz/`
/// do not match.
fn check(root: &Path) -> Result<Vec<String>, Vec<String>> {
    let graph = |dir: &str| crate::graph(&root.join(dir)).map_err(|e| vec![e]);
    let fuzz = graph("fuzz")?;
    let mut problems = unpatched(&graph(".")?, &fuzz).map_err(|e| vec![e])?;
    let targets = bins(&fuzz).map_err(|e| vec![e])?;
    let names: Vec<&str> = targets.iter().map(String::as_str).collect();
    problems.extend(unmatched(&names, &folders(&root.join("oracles/fuzz"))?));
    if problems.is_empty() {
        Ok(targets)
    } else {
        Err(problems)
    }
}

/// The name of each bin target of the workspace members of `graph`.
fn bins(graph: &Value) -> Result<Vec<String>, String> {
    let members = field::list(graph, "workspace_members")?;
    let mut bins = Vec::new();
    for package in field::list(graph, "packages")? {
        let id = field::text(package, "id")?;
        if !members.iter().any(|member| *member == id) {
            continue;
        }
        for target in field::list(package, "targets")? {
            if field::list(target, "kind")?
                .iter()
                .any(|kind| kind == "bin")
            {
                bins.push(field::text(target, "name")?.to_string());
            }
        }
    }
    Ok(bins)
}

/// A problem for each package of the `fuzz` graph that is a copy in `patches/` and
/// that the `root` graph does not build, and for each package and copy that the `root`
/// graph builds when the package has an edge to another package of the copy's name and
/// a requirement on crates.io, of a kind and target of the edge, that both the copy and
/// that package meet. A requirement that resolves to another package always gives such
/// an edge. A requirement of the kind and target of one that resolves to another
/// package is refused too, also when it resolves to the copy, or to nothing, such as an
/// optional one that is off. It fails on an edge to a package with the name of a
/// copy that no requirement of a kind and target of the edge meets.
fn unpatched(root: &Value, fuzz: &Value) -> Result<Vec<String>, String> {
    let copies = Path::new(field::text(root, "workspace_root")?).join("patches");
    let root = Package::all(root, &copies)?;
    let packages = Package::all(fuzz, &copies)?;
    let built = root
        .iter()
        .filter(|package| package.copied)
        .map(|package| Ok((package, package.release()?)))
        .collect::<Result<Vec<_>, String>>()?;
    let mut problems = strays(&root, &packages);
    for node in field::list(&fuzz["resolve"], "nodes")? {
        let dependent = find(&packages, field::text(node, "id")?)?;
        let mut reported = Vec::new();
        for edge in field::list(node, "deps")? {
            let dependency = find(&packages, field::text(edge, "pkg")?)?;
            if !built.iter().any(|(copy, _)| copy.name == dependency.name) {
                continue;
            }
            // `requirements` fails on an edge that no requirement meets; it runs before
            // the skip so that an edge to a copy fails too.
            let requirements = dependent.requirements(dependency, edge)?;
            if dependency.copied {
                continue;
            }
            for (requirement, parsed) in requirements {
                if requirement["source"].as_str() != Some(CRATES_IO) {
                    continue;
                }
                for (copy, _) in built.iter().filter(|(copy, version)| {
                    copy.name == dependency.name && parsed.matches(version)
                }) {
                    if reported.contains(&copy.id) {
                        continue;
                    }
                    reported.push(copy.id);
                    problems.push(format!(
                        "`{}` has the requirement `{}` `{}`, which the copy `{}` \
                         and another package of its name that fuzz/ builds both \
                         meet, so the graph does not show that fuzz/ builds the copy \
                         for it. If fuzz/Cargo.toml does not have the \
                         [patch.crates-io] table of the root Cargo.toml, give it \
                         that table. If it has the table, change the requirements of \
                         fuzz/ so that this requirement does not also meet the other \
                         package.",
                        dependent.id,
                        copy.name,
                        requirement["req"].as_str().unwrap_or_default(),
                        copy.manifest
                    ));
                }
            }
        }
    }
    Ok(problems)
}

/// A problem for each copy of `packages`, the packages of the `fuzz` graph, that `root`
/// does not build.
fn strays(root: &[Package<'_>], packages: &[Package<'_>]) -> Vec<String> {
    packages
        .iter()
        .filter(|package| package.copied && !root.iter().any(|p| p.id == package.id))
        .map(|package| {
            format!(
                "fuzz/Cargo.toml builds `{}` from the copy `{}`, which the root \
                 Cargo.toml does not build. Give fuzz/Cargo.toml the [patch.crates-io] \
                 table of the root Cargo.toml.",
                package.name, package.manifest
            )
        })
        .collect()
}

/// The package `id` of `packages`, the packages of the `fuzz` graph.
fn find<'p, 'a>(
    packages: &'p [Package<'a>],
    id: &str,
) -> Result<&'p Package<'a>, String> {
    packages
        .iter()
        .find(|package| package.id == id)
        .ok_or_else(|| format!("the resolve of fuzz/Cargo.lock has no package `{id}`"))
}

/// The fields of a package of `cargo metadata` that [`unpatched`] reads.
struct Package<'a> {
    value: &'a Value,
    id: &'a str,
    name: &'a str,
    version: &'a str,
    manifest: &'a str,
    /// Whether the package is a copy under the folder `copies` of [`Package::read`].
    copied: bool,
}

impl<'a> Package<'a> {
    /// Reads each package of `graph`.
    fn all(graph: &'a Value, copies: &Path) -> Result<Vec<Self>, String> {
        field::list(graph, "packages")?
            .iter()
            .map(|package| Package::read(package, copies))
            .collect()
    }

    /// Reads `package`, which is a copy when its manifest is under `copies`.
    fn read(value: &'a Value, copies: &Path) -> Result<Self, String> {
        let manifest = field::text(value, "manifest_path")?;
        Ok(Package {
            value,
            id: field::text(value, "id")?,
            name: field::text(value, "name")?,
            version: field::text(value, "version")?,
            manifest,
            copied: Path::new(manifest).starts_with(copies),
        })
    }

    /// The requirements of the package on `dependency`, each with its version
    /// requirement, that `edge` of its node in the resolve can resolve: those of a kind
    /// and target of the edge that the version of `dependency` meets. A requirement
    /// with a path resolves only to a package with no source. It fails when there is
    /// none, or on a version of `dependency` or a requirement that does not parse.
    fn requirements(
        &self,
        dependency: &Package<'_>,
        edge: &Value,
    ) -> Result<Vec<(&'a Value, VersionReq)>, String> {
        let name = field::text(edge, "name")?;
        let version = dependency.release()?;
        let kinds = field::list(edge, "dep_kinds")?;
        let mut resolved = Vec::new();
        for requirement in field::list(self.value, "dependencies")? {
            if field::text(requirement, "name")? != dependency.name {
                continue;
            }
            let kind = kinds.iter().any(|kind| {
                kind["kind"] == requirement["kind"]
                    && kind["target"] == requirement["target"]
            });
            let sourced = !requirement["source"].is_null()
                || dependency.value["source"].is_null();
            if !kind || !sourced {
                continue;
            }
            let text = field::text(requirement, "req")?;
            let parsed = VersionReq::parse(text).map_err(|error| {
                format!(
                    "`{}` needs `{}` `{text}`: {error}",
                    self.id, dependency.name
                )
            })?;
            // Only a path or git requirement can have no version, which Cargo writes
            // as `*` and which meets a pre-release too.
            let unversioned = parsed == VersionReq::STAR
                && requirement["source"]
                    .as_str()
                    .is_none_or(|source| source.starts_with("git+"));
            if unversioned || parsed.matches(&version) {
                resolved.push((requirement, parsed));
            }
        }
        if resolved.is_empty() {
            return Err(format!(
                "`{}` has no requirement that its edge `{name}` to `{}` resolves",
                self.id, dependency.id
            ));
        }
        Ok(resolved)
    }

    /// The version of the package.
    fn release(&self) -> Result<Version, String> {
        Version::parse(self.version).map_err(|error| {
            format!("`{}` has the version `{}`: {error}", self.id, self.version)
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::output;

    const PATCHED: &str = "path+file:///w/patches/noq-proto#noq-proto@1.3.0";
    const TYPES: &str = "path+file:///w/crates/types#0.0.0";
    const CRC: &str = "registry+x#crc32c@0.6.7";

    fn package(name: &str, id: &str, manifest: &str) -> Value {
        let version = id.rsplit(['@', '#']).next().unwrap();
        json!({
            "name": name,
            "id": id,
            "version": version,
            "manifest_path": manifest,
            "dependencies": [],
            "targets": [{ "name": name.replace('-', "_"), "kind": ["lib"] }],
        })
    }

    fn copy() -> Value {
        package("noq-proto", PATCHED, "/w/patches/noq-proto/Cargo.toml")
    }

    fn types() -> Value {
        package("types", TYPES, "/w/crates/types/Cargo.toml")
    }

    fn crc() -> Value {
        package("crc32c", CRC, "/r/crc32c-0.6.7/Cargo.toml")
    }

    /// The id of the registry release `version` of `noq-proto`.
    fn id(version: &str) -> String {
        format!(
            "registry+https://github.com/rust-lang/crates.io-index#noq-proto@{version}"
        )
    }

    fn release(version: &str) -> Value {
        let mut release = package(
            "noq-proto",
            &id(version),
            &format!("/r/noq-proto-{version}/Cargo.toml"),
        );
        release["source"] = json!(CRATES_IO);
        release
    }

    /// `package` with one more requirement on `name`.
    fn needs(mut package: Value, name: &str, req: &str) -> Value {
        package["dependencies"].as_array_mut().unwrap().push(json!({
            "name": name,
            "req": req,
            "source": CRATES_IO,
            "kind": null,
            "target": null,
        }));
        package
    }

    fn root() -> Value {
        json!({
            "workspace_root": "/w",
            "packages": [copy(), types(), crc()],
        })
    }

    /// A graph of `packages`, where each pair of `edges` resolves a dependency of the
    /// first package to the second.
    fn fuzz(packages: &[Value], edges: &[(&str, &str)]) -> Value {
        let nodes: Vec<Value> = packages
            .iter()
            .map(|package| {
                let id = package["id"].as_str().unwrap_or_default();
                let deps: Vec<Value> = edges
                    .iter()
                    .filter(|(from, _)| *from == id)
                    .map(|(_, to)| {
                        let name = packages
                            .iter()
                            .find(|package| package["id"] == *to)
                            .map_or("gone".to_string(), |package| {
                                package["name"].as_str().unwrap().replace('-', "_")
                            });
                        let kinds = [json!({ "kind": null, "target": null })];
                        json!({ "pkg": to, "name": name, "dep_kinds": kinds })
                    })
                    .collect();
                json!({ "id": id, "deps": deps })
            })
            .collect();
        json!({
            "workspace_root": "/w/fuzz",
            "packages": packages,
            "resolve": { "nodes": nodes },
        })
    }

    #[test]
    fn reads_each_target_of_this_repository() {
        let root = crate::fixture().join("../..");
        let mut files: Vec<String> = std::fs::read_dir(root.join("fuzz/fuzz_targets"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                path.file_stem().unwrap().to_string_lossy().into_owned()
            })
            .collect();
        files.sort_unstable();
        let mut targets = check(&root).unwrap();
        targets.sort_unstable();
        assert!(targets.contains(&"types_name".to_string()));
        assert_eq!(targets, files);
    }

    #[test]
    fn refuses_a_target_and_a_folder_that_do_not_match() {
        assert_eq!(
            check(&crate::fixture().join("unmatched")),
            Err(vec![
                "fuzz target `a` has no inputs in oracles/fuzz/a/".to_string(),
                "oracles/fuzz/b/ has no fuzz target".to_string(),
            ])
        );
    }

    #[test]
    fn checks_the_lock_before_it_builds() {
        let root = crate::fixture().join("stale");
        let problems = run(&root, SECONDS).unwrap_err();
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].contains("--locked was passed"),
            "{}",
            problems[0]
        );
    }

    #[test]
    fn refuses_a_stale_root_lock() {
        let root = crate::fixture().join("stale-root");
        let problems = check(&root).unwrap_err();
        let refusal = format!(
            "error: cannot update the lock file {} because --locked was passed to \
             prevent this",
            root.join("Cargo.lock").display()
        );
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].lines().any(|line| line == refusal),
            "{}",
            problems[0]
        );
    }

    #[test]
    fn refuses_a_stale_fuzz_lock() {
        let root = crate::fixture().join("stale");
        let problems = check(&root).unwrap_err();
        let lock = root.join("fuzz/Cargo.lock");
        let refused = format!(
            "error: cannot update the lock file {} because --locked was passed to \
             prevent this\n",
            lock.display()
        );
        assert_eq!(problems.len(), 1);
        // Cargo can first print that it waits for the lock of its package cache.
        assert!(problems[0].contains(&refused), "{}", problems[0]);
    }

    #[test]
    fn a_locked_graph_holds_each_package_of_the_lock() {
        let graph = crate::graph(&crate::fixture()).unwrap();
        let mut names: Vec<_> = graph["packages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|package| package["name"].as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["a", "a", "b", "globals", "model"]);
    }

    #[test]
    fn passes_a_fuzz_graph_that_builds_the_copy() {
        let types = needs(types(), "noq-proto", "^1.3");
        let fuzz = fuzz(&[types, copy()], &[(TYPES, PATCHED)]);
        assert_eq!(unpatched(&root(), &fuzz), Ok(Vec::new()));
    }

    /// The problem of the requirement `^1.3` of `types` on `noq-proto`.
    fn missed() -> String {
        format!(
            "`{TYPES}` has the requirement `noq-proto` `^1.3`, which the copy \
             `/w/patches/noq-proto/Cargo.toml` and another package of its name that \
             fuzz/ builds both meet, so the graph does not show that fuzz/ builds the \
             copy for it. If fuzz/Cargo.toml does not have the [patch.crates-io] \
             table of the root Cargo.toml, give it that table. If it has the table, \
             change the requirements of fuzz/ so that this requirement does not also \
             meet the other package."
        )
    }

    #[test]
    fn refuses_a_release_for_a_requirement_that_the_copy_meets() {
        let types = needs(types(), "noq-proto", "^1.3");
        let fuzz = fuzz(&[types, release("1.3.0")], &[(TYPES, &id("1.3.0"))]);
        assert_eq!(unpatched(&root(), &fuzz), Ok(vec![missed()]));
    }

    /// The problems of the case `case` of the fixture `patched`, where each case is the
    /// `fuzz/` of the root `patched`, and `vendor/` stands in for crates.io.
    fn patched(case: &str) -> Result<Vec<String>, String> {
        let root = crate::fixture().join("patched");
        let fuzz = root.join("cases").join(case);
        unpatched(&crate::graph(&root)?, &crate::graph(&fuzz)?)
    }

    /// The problem of a requirement `req` on `p` of `dependent` in the fixture
    /// `patched`.
    fn unmet(req: &str, dependent: &str) -> String {
        format!(
            "`{dependent}` has the requirement `p` `{req}`, which the copy \
             `{}/patches/p/Cargo.toml` and another package of its name that fuzz/ \
             builds both meet, so the graph does not show that fuzz/ builds the copy \
             for it. If fuzz/Cargo.toml does not have \
             the [patch.crates-io] table of the root Cargo.toml, give it that table. \
             If it has the table, change the requirements of fuzz/ so that this \
             requirement does not also meet the other package.",
            crate::fixture().join("patched").display()
        )
    }

    #[test]
    fn refuses_a_wide_requirement_that_resolves_to_the_copy_beside_a_later_series() {
        let dependent = format!(
            "path+file://{}#0.0.0",
            crate::fixture().join("patched/cases/later").display()
        );
        assert_eq!(patched("later"), Ok(vec![unmet(">=1.3", &dependent)]));
    }

    #[test]
    fn refuses_a_release_beside_the_copy_for_a_requirement_that_the_copy_meets() {
        assert_eq!(
            patched("beside"),
            Ok(vec![unmet(
                "~1.3",
                "registry+https://github.com/rust-lang/crates.io-index#q@1.0.0"
            )])
        );
    }

    #[test]
    fn passes_a_release_for_a_requirement_that_the_copy_cannot_meet() {
        assert_eq!(patched("unmet"), Ok(Vec::new()));
    }

    #[test]
    fn passes_a_requirement_off_crates_io_that_the_copy_meets() {
        assert_eq!(patched("path"), Ok(Vec::new()));
    }

    #[test]
    fn passes_an_optional_requirement_that_is_off() {
        assert_eq!(patched("optional"), Ok(Vec::new()));
    }

    #[test]
    fn passes_a_second_name_for_a_release_of_another_series() {
        assert_eq!(patched("renamed"), Ok(Vec::new()));
    }

    #[test]
    fn passes_a_release_that_a_second_requirement_of_the_same_name_meets() {
        assert_eq!(patched("twice"), Ok(Vec::new()));
    }

    #[test]
    fn passes_a_renamed_release_beside_an_unrenamed_requirement_on_the_copy() {
        assert_eq!(patched("mixed"), Ok(Vec::new()));
    }

    #[test]
    fn passes_a_requirement_from_git_that_the_copy_meets() {
        assert_eq!(patched("git"), Ok(Vec::new()));
    }

    #[test]
    fn refuses_a_self_edge_beside_a_patched_requirement_of_the_same_name() {
        assert_eq!(patched("selfname"), Ok(vec![itself("selfname", "^1")]));
    }

    /// The problem of the requirement `req` of `p` 1.4.0, the case `case` of the
    /// fixture `patched`.
    fn itself(case: &str, req: &str) -> String {
        let case = crate::fixture().join("patched/cases").join(case);
        unmet(req, &format!("path+file://{}#p@1.4.0", case.display()))
    }

    #[test]
    fn refuses_a_self_edge_that_a_patch_resolves_for_a_requirement_the_copy_meets() {
        assert_eq!(patched("selfpatch"), Ok(vec![itself("selfpatch", "^1")]));
    }

    #[test]
    fn refuses_a_self_edge_beside_a_release_that_meets_both_requirements() {
        assert_eq!(patched("selfboth"), Ok(vec![itself("selfboth", "^1")]));
    }

    #[test]
    fn refuses_a_self_edge_beside_an_edge_of_the_same_name_kind_and_target() {
        assert_eq!(patched("selfcfg"), Ok(vec![itself("selfcfg", "^1")]));
    }

    #[test]
    fn refuses_a_release_beside_a_self_edge_whose_version_meets_its_requirement() {
        assert_eq!(
            patched("selfrelease"),
            Ok(vec![itself("selfrelease", "^1")])
        );
    }

    #[test]
    fn passes_a_dependency_whose_lib_name_is_not_its_package_name() {
        assert_eq!(patched("libname"), Ok(Vec::new()));
    }

    #[test]
    fn passes_a_dev_requirement_that_the_copy_meets_of_a_registry_package() {
        assert_eq!(patched("dev"), Ok(Vec::new()));
    }

    #[test]
    fn refuses_a_release_of_the_series_of_the_copy_under_a_second_name() {
        let dependent = format!(
            "path+file://{}#0.0.0",
            crate::fixture().join("patched/cases/unpatched").display()
        );
        assert_eq!(patched("unpatched"), Ok(vec![unmet("^1", &dependent)]));
    }

    #[test]
    fn refuses_a_release_beside_a_path_to_the_copy_under_a_rename_of_the_same_name() {
        let dependent = format!(
            "path+file://{}#0.0.0",
            crate::fixture().join("patched/cases/limit").display()
        );
        assert_eq!(patched("limit"), Ok(vec![unmet("^1", &dependent)]));
    }

    #[test]
    fn refuses_a_release_beside_a_path_to_the_copy_with_a_dev_requirement_of_its_name()
    {
        let dependent = format!(
            "path+file://{}#0.0.0",
            crate::fixture().join("patched/cases/masked").display()
        );
        assert_eq!(patched("masked"), Ok(vec![unmet("^1", &dependent)]));
    }

    #[test]
    fn passes_a_path_to_the_copy_beside_a_dev_requirement_of_its_name_with_the_patch() {
        assert_eq!(patched("maskedpatched"), Ok(Vec::new()));
    }

    #[test]
    fn refuses_a_release_beside_a_path_to_the_copy_with_a_requirement_of_a_target() {
        let dependent = format!(
            "path+file://{}#0.0.0",
            crate::fixture().join("patched/cases/limitcfg").display()
        );
        assert_eq!(patched("limitcfg"), Ok(vec![unmet("^1", &dependent)]));
    }

    #[test]
    fn passes_a_requirement_from_git_that_a_patch_points_at_a_release() {
        assert_eq!(patched("gitpatch"), Ok(Vec::new()));
    }

    #[test]
    fn refuses_an_optional_requirement_that_is_off_beside_a_release_of_its_name() {
        for case in ["offrename", "split"] {
            let dependent = format!(
                "path+file://{}#0.0.0",
                crate::fixture().join("patched/cases").join(case).display()
            );
            assert_eq!(patched(case), Ok(vec![unmet("^1", &dependent)]));
        }
    }

    #[test]
    fn refuses_a_requirement_that_a_release_of_the_series_of_the_copy_takes() {
        let member = format!(
            "path+file://{}#0.0.0",
            crate::fixture().join("patched/cases/unified/u").display()
        );
        assert_eq!(patched("unified"), Ok(vec![unmet("^1", &member)]));
    }

    #[test]
    fn refuses_a_release_once_when_two_requirements_resolve_to_it() {
        let types = needs(needs(types(), "noq-proto", "^1.3"), "noq-proto", "^1.0");
        let fuzz = fuzz(&[types, release("1.3.0")], &[(TYPES, &id("1.3.0"))]);
        assert_eq!(
            unpatched(&root(), &fuzz).map(|problems| problems.len()),
            Ok(1)
        );
    }

    #[test]
    fn refuses_a_fuzz_graph_that_builds_a_copy_the_root_does_not() {
        let fuzz = fuzz(
            &[package(
                "noq-udp",
                "path+file:///w/patches/noq-udp#noq-udp@1.3.0",
                "/w/patches/noq-udp/Cargo.toml",
            )],
            &[],
        );
        assert_eq!(
            unpatched(&root(), &fuzz),
            Ok(vec![
                "fuzz/Cargo.toml builds `noq-udp` from the copy \
                 `/w/patches/noq-udp/Cargo.toml`, which the root Cargo.toml does not \
                 build. Give fuzz/Cargo.toml the [patch.crates-io] table of the root \
                 Cargo.toml."
                    .to_string()
            ])
        );
    }

    #[test]
    fn passes_a_fuzz_graph_that_lacks_a_patched_crate_or_has_others() {
        let types = needs(types(), "crc32c", "^0.6");
        let crc = package(
            "crc32c",
            "registry+x#crc32c@0.6.8",
            "/r/crc32c-0.6.8/Cargo.toml",
        );
        let fuzz = fuzz(&[types, crc], &[(TYPES, "registry+x#crc32c@0.6.8")]);
        assert_eq!(unpatched(&root(), &fuzz), Ok(Vec::new()));
    }

    #[test]
    fn names_a_missing_field_of_a_graph() {
        assert_eq!(
            unpatched(&json!({ "workspace_root": "/w" }), &fuzz(&[], &[])),
            Err("JSON has no array field `packages`".to_string())
        );
        assert_eq!(
            unpatched(&json!({ "packages": [] }), &fuzz(&[], &[])),
            Err("JSON has no string field `workspace_root`".to_string())
        );
        assert_eq!(
            unpatched(&root(), &json!({ "resolve": { "nodes": [] } })),
            Err("JSON has no array field `packages`".to_string())
        );
        assert_eq!(
            unpatched(&root(), &json!({ "packages": [] })),
            Err("JSON has no array field `nodes`".to_string())
        );
        for key in ["id", "name", "version", "manifest_path"] {
            let mut lacking = release("1.0.0");
            lacking.as_object_mut().unwrap().remove(key);
            let error = Err(format!("JSON has no string field `{key}`"));
            assert_eq!(unpatched(&root(), &fuzz(&[lacking.clone()], &[])), error);
            let mut root = root();
            root["packages"].as_array_mut().unwrap().push(lacking);
            assert_eq!(unpatched(&root, &fuzz(&[], &[])), error);
        }
        let mut lacking = fuzz(&[types()], &[]);
        lacking["resolve"]["nodes"][0]
            .as_object_mut()
            .unwrap()
            .remove("deps");
        assert_eq!(
            unpatched(&root(), &lacking),
            Err("JSON has no array field `deps`".to_string())
        );
        let mut lacking = fuzz(&[types(), release("1.3.0")], &[(TYPES, &id("1.3.0"))]);
        lacking["resolve"]["nodes"][0]["deps"][0]
            .as_object_mut()
            .unwrap()
            .remove("pkg");
        assert_eq!(
            unpatched(&root(), &lacking),
            Err("JSON has no string field `pkg`".to_string())
        );
        let mut lacking = fuzz(&[types(), release("1.3.0")], &[(TYPES, &id("1.3.0"))]);
        lacking["packages"][0]
            .as_object_mut()
            .unwrap()
            .remove("dependencies");
        assert_eq!(
            unpatched(&root(), &lacking),
            Err("JSON has no array field `dependencies`".to_string())
        );
        for key in ["name", "req"] {
            let types = needs(types(), "noq-proto", "^1.3");
            let mut lacking =
                fuzz(&[types, release("1.3.0")], &[(TYPES, &id("1.3.0"))]);
            lacking["packages"][0]["dependencies"][0]
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert_eq!(
                unpatched(&root(), &lacking),
                Err(format!("JSON has no string field `{key}`"))
            );
        }
    }

    #[test]
    fn names_a_missing_field_of_an_edge() {
        for (key, kind) in [("name", "string"), ("dep_kinds", "array")] {
            let needing = needs(types(), "noq-proto", "^1.3");
            let mut lacking =
                fuzz(&[needing, release("1.3.0")], &[(TYPES, &id("1.3.0"))]);
            lacking["resolve"]["nodes"][0]["deps"][0]
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert_eq!(
                unpatched(&root(), &lacking),
                Err(format!("JSON has no {kind} field `{key}`"))
            );
        }
    }

    #[test]
    fn names_an_edge_that_no_requirement_names() {
        let fuzz = fuzz(&[types(), release("1.3.0")], &[(TYPES, &id("1.3.0"))]);
        assert_eq!(unpatched(&root(), &fuzz), Err(unresolved(&id("1.3.0"))));
    }

    /// `noq-proto` 9.0.0 of the `fuzz` workspace, with a requirement on itself with a
    /// path and no version.
    fn own() -> Value {
        let id = "path+file:///w/fuzz#noq-proto@9.0.0";
        let mut own = package("noq-proto", id, "/w/fuzz/Cargo.toml");
        own = needs(own, "noq-proto", "*");
        own["dependencies"][0]["source"] = Value::Null;
        own
    }

    #[test]
    fn names_an_edge_to_a_copy_beside_a_self_edge_that_no_requirement_names() {
        let me = "path+file:///w/fuzz#noq-proto@9.0.0";
        let own = package("noq-proto", me, "/w/fuzz/Cargo.toml");
        let fuzz = fuzz(&[copy(), own], &[(me, PATCHED), (me, me)]);
        assert_eq!(
            unpatched(&root(), &fuzz),
            Err(format!(
                "`{me}` has no requirement that its edge `noq_proto` to `{PATCHED}` \
                 resolves"
            ))
        );
    }

    #[test]
    fn passes_an_edge_to_another_name_beside_a_self_edge() {
        let me = "path+file:///w/fuzz#noq-proto@9.0.0";
        let fuzz = fuzz(&[own(), crc()], &[(me, me), (me, CRC)]);
        assert_eq!(unpatched(&root(), &fuzz), Ok(Vec::new()));
    }

    /// `noq-proto` `version` of the `fuzz` workspace, and `types` with a requirement
    /// `req` on it from `source`.
    fn local(version: &str, req: &str, source: Value) -> (String, [Value; 2]) {
        let id = format!("path+file:///w/fuzz/noq#noq-proto@{version}");
        let noq = package("noq-proto", &id, "/w/fuzz/noq/Cargo.toml");
        let mut types = needs(types(), "noq-proto", req);
        types["dependencies"][0]["source"] = source;
        (id, [types, noq])
    }

    #[test]
    fn pairs_a_requirement_with_no_version_with_a_pre_release() {
        let (noq, packages) = local("9.0.0-dev", "*", Value::Null);
        let fuzz = fuzz(&packages, &[(TYPES, &noq)]);
        assert_eq!(unpatched(&root(), &fuzz), Ok(Vec::new()));
        let git = json!("git+https://github.com/synnaxlabs/p");
        let (noq, packages) = local("9.0.0-dev", "*", git);
        let fuzz = self::fuzz(&packages, &[(TYPES, &noq)]);
        assert_eq!(unpatched(&root(), &fuzz), Ok(Vec::new()));
        let me = "path+file:///w/fuzz#noq-proto@9.0.0-dev";
        let mut own = package("noq-proto", me, "/w/fuzz/Cargo.toml");
        own = needs(own, "noq-proto", "*");
        own["dependencies"][0]["source"] = Value::Null;
        let fuzz = self::fuzz(&[own], &[(me, me)]);
        assert_eq!(unpatched(&root(), &fuzz), Ok(Vec::new()));
    }

    #[test]
    fn pairs_no_requirement_with_a_path_and_a_version_that_the_package_does_not_meet() {
        let (noq, packages) = local("9.0.0", "^1", Value::Null);
        let fuzz = fuzz(&packages, &[(TYPES, &noq)]);
        assert_eq!(unpatched(&root(), &fuzz), Err(unresolved(&noq)));
    }

    #[test]
    fn names_the_version_of_a_dependency_that_does_not_parse() {
        let fuzz = fuzz(
            &[needs(types(), "noq-proto", "^1.3"), release("one")],
            &[(TYPES, &id("one"))],
        );
        assert_eq!(
            unpatched(&root(), &fuzz),
            Err(format!(
                "`{}` has the version `one`: unexpected character 'o' while parsing \
                 major version number",
                id("one")
            ))
        );
    }

    #[test]
    fn refuses_a_self_edge_beside_an_edge_whose_version_meets_both_requirements() {
        assert_eq!(patched("selfwide"), Ok(vec![itself("selfwide", "*")]));
    }

    #[test]
    fn passes_an_edge_that_no_requirement_names_to_another_name() {
        let other = fuzz(&[types(), crc()], &[(TYPES, CRC)]);
        assert_eq!(unpatched(&root(), &other), Ok(Vec::new()));
    }

    #[test]
    fn names_an_edge_to_a_copy_that_no_requirement_names() {
        let copied = fuzz(&[types(), copy()], &[(TYPES, PATCHED)]);
        assert_eq!(unpatched(&root(), &copied), Err(unresolved(PATCHED)));
    }

    #[test]
    fn refuses_a_release_beside_a_path_requirement_on_the_copy() {
        let mut types = needs(needs(types(), "noq-proto", "^1.3"), "noq-proto", "^1.3");
        types["dependencies"][0]["rename"] = json!("a");
        types["dependencies"][0]["source"] = Value::Null;
        types["dependencies"][1]["rename"] = json!("b");
        let release = id("1.3.0");
        let mut fuzz = fuzz(
            &[types, copy(), self::release("1.3.0")],
            &[(TYPES, PATCHED), (TYPES, &release)],
        );
        fuzz["resolve"]["nodes"][0]["deps"][0]["name"] = json!("a");
        fuzz["resolve"]["nodes"][0]["deps"][1]["name"] = json!("b");
        assert_eq!(unpatched(&root(), &fuzz), Ok(vec![missed()]));
    }

    #[test]
    fn refuses_a_release_that_can_resolve_a_requirement_that_the_copy_does_not_meet() {
        let types = needs(needs(types(), "noq-proto", "^1.3"), "noq-proto", "^1.5");
        let fuzz = fuzz(&[types, release("1.5.0")], &[(TYPES, &id("1.5.0"))]);
        assert_eq!(unpatched(&root(), &fuzz), Ok(vec![missed()]));
    }

    #[test]
    fn refuses_a_requirement_that_an_edge_to_a_copy_can_resolve_beside_a_release() {
        let types = needs(needs(types(), "noq-proto", "^1.3"), "noq-proto", "^1.5");
        let packages = [types, copy(), self::release("1.5.0")];
        let release = id("1.5.0");
        let edges = [(TYPES, PATCHED), (TYPES, release.as_str())];
        assert_eq!(
            unpatched(&root(), &fuzz(&packages, &edges)),
            Ok(vec![missed()])
        );
    }

    #[test]
    fn names_an_edge_to_a_release_that_only_a_path_requirement_names() {
        let mut needing = needs(types(), "noq-proto", "^1.3");
        needing["dependencies"][0]["source"] = Value::Null;
        let fuzz = fuzz(&[needing, release("1.3.0")], &[(TYPES, &id("1.3.0"))]);
        assert_eq!(unpatched(&root(), &fuzz), Err(unresolved(&id("1.3.0"))));
    }

    #[test]
    fn names_the_copy_of_the_name_of_the_requirement() {
        let udp = "path+file:///w/patches/noq-udp#noq-udp@1.3.0";
        let mut root = root();
        root["packages"] = json!([
            package("noq-udp", udp, "/w/patches/noq-udp/Cargo.toml"),
            copy(),
            types(),
        ]);
        let types = needs(types(), "noq-proto", "^1.3");
        let fuzz = fuzz(&[types, release("1.3.0")], &[(TYPES, &id("1.3.0"))]);
        assert_eq!(unpatched(&root, &fuzz), Ok(vec![missed()]));
    }

    #[test]
    fn names_each_copy_that_a_requirement_meets() {
        let second = "path+file:///w/patches/noq-proto-2#noq-proto@2.0.0";
        let manifest = "/w/patches/noq-proto-2/Cargo.toml";
        let mut root = root();
        root["packages"] =
            json!([copy(), package("noq-proto", second, manifest), types()]);
        let types = needs(types(), "noq-proto", ">=1");
        let fuzz = fuzz(&[types, release("3.0.0")], &[(TYPES, &id("3.0.0"))]);
        let problem = |manifest: &str| {
            missed()
                .replace("`^1.3`", "`>=1`")
                .replace("/w/patches/noq-proto/Cargo.toml", manifest)
        };
        assert_eq!(
            unpatched(&root, &fuzz),
            Ok(vec![
                problem("/w/patches/noq-proto/Cargo.toml"),
                problem(manifest)
            ])
        );
    }

    /// The error for an edge `noq_proto` of `types` to `to` that nothing resolves.
    fn unresolved(to: &str) -> String {
        format!(
            "`{TYPES}` has no requirement that its edge `noq_proto` to `{to}` resolves"
        )
    }

    /// The `fuzz` graph in which `types` needs `noq-proto` `^1.3` as `change` makes it,
    /// and resolves it to the release 1.3.0 under the edge name `name`.
    fn paired(
        name: &str,
        change: impl FnOnce(&mut Value),
    ) -> Result<Vec<String>, String> {
        let mut needing = needs(types(), "noq-proto", "^1.3");
        change(&mut needing["dependencies"][0]);
        let mut fuzz = fuzz(&[needing, release("1.3.0")], &[(TYPES, &id("1.3.0"))]);
        fuzz["resolve"]["nodes"][0]["deps"][0]["name"] = json!(name);
        unpatched(&root(), &fuzz)
    }

    #[test]
    fn pairs_an_edge_with_a_requirement_by_package_kind_and_target() {
        let unnamed = Err(unresolved(&id("1.3.0")));
        let refused = Ok(vec![missed()]);
        let renamed =
            |requirement: &mut Value| requirement["rename"] = json!("the-proto");
        assert_eq!(paired("the_proto", renamed), refused);
        assert_eq!(paired("noq_proto", renamed), refused);
        let other = |requirement: &mut Value| {
            requirement["name"] = json!("other");
            requirement["rename"] = json!("noq-proto");
        };
        assert_eq!(paired("noq_proto", other), unnamed);
        let dev = |requirement: &mut Value| requirement["kind"] = json!("dev");
        assert_eq!(paired("noq_proto", dev), unnamed);
        let unix = |requirement: &mut Value| requirement["target"] = json!("cfg(unix)");
        assert_eq!(paired("noq_proto", unix), unnamed);
        assert_eq!(paired("noq_proto", |_| ()), refused);
    }

    #[test]
    fn names_an_unknown_package_and_a_bad_version_or_requirement() {
        let unknown = fuzz(&[types()], &[(TYPES, "registry+x#gone@1.0.0")]);
        assert_eq!(
            unpatched(&root(), &unknown),
            Err(
                "the resolve of fuzz/Cargo.lock has no package `registry+x#gone@1.0.0`"
                    .to_string()
            )
        );
        let mut root = root();
        root["packages"][0]["version"] = json!("one");
        assert_eq!(
            unpatched(&root, &fuzz(&[], &[])),
            Err(format!(
                "`{PATCHED}` has the version `one`: unexpected character 'o' while \
                 parsing major version number"
            ))
        );
        let types = needs(types(), "noq-proto", "one");
        let bad = fuzz(&[types, release("1.3.0")], &[(TYPES, &id("1.3.0"))]);
        assert_eq!(
            unpatched(&self::root(), &bad),
            Err(
                "`path+file:///w/crates/types#0.0.0` needs `noq-proto` `one`: \
                 unexpected character 'o' while parsing major version number"
                    .to_string()
            )
        );
    }

    #[test]
    fn bins_are_the_bin_targets_of_the_members() {
        let target = |name: &str, kind: &str| json!({ "name": name, "kind": [kind] });
        let graph = json!({
            "workspace_members": ["m"],
            "packages": [
                { "id": "m", "targets": [target("fuzz", "lib"), target("a", "bin")] },
                { "id": "d", "targets": [target("tool", "bin")] },
            ],
        });
        assert_eq!(bins(&graph), Ok(vec!["a".to_string()]));
        assert_eq!(
            bins(&json!({ "packages": [] })),
            Err("JSON has no array field `workspace_members`".to_string())
        );
        let kindless = json!({
            "workspace_members": ["m"],
            "packages": [{ "id": "m", "targets": [{ "name": "a" }] }],
        });
        assert_eq!(
            bins(&kindless),
            Err("JSON has no array field `kind`".to_string())
        );
        let keyless = json!({
            "workspace_members": ["m"],
            "packages": [{ "targets": [target("a", "bin")] }],
        });
        assert_eq!(
            bins(&keyless),
            Err("JSON has no string field `id`".to_string())
        );
    }

    #[test]
    fn runs_the_target_on_both_corpus_directories_with_the_limits() {
        let seconds = NonZeroU16::new(5).unwrap();
        let command = arm().run(
            "spec_tree",
            Path::new("/c"),
            Path::new("/o"),
            seconds,
            20000,
        );
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "run",
                "nightly-x",
                "cargo",
                "fuzz",
                "run",
                "--target",
                "aarch64-unknown-linux-gnu",
                "spec_tree",
                "/c",
                "/o",
                "--",
                "-max_total_time=5",
                "-timeout=10",
                "-rss_limit_mb=2048",
                "-max_len=20000",
            ]
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/w")));
    }

    fn arm() -> Cargo {
        Cargo {
            nightly: crate::Nightly {
                root: "/w".into(),
                pin: "nightly-x".to_string(),
            },
            host: "aarch64-unknown-linux-gnu".to_string(),
        }
    }

    #[test]
    fn builds_for_the_host_of_the_nightly() {
        let command = arm().build();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "run",
                "nightly-x",
                "cargo",
                "fuzz",
                "build",
                "--target",
                "aarch64-unknown-linux-gnu",
            ]
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/w")));
    }

    #[test]
    fn seconds_refuses_a_count_outside_1_to_65535() {
        assert_eq!(seconds("65535"), Ok(NonZeroU16::MAX));
        assert_eq!(
            seconds("65536").err().as_deref(),
            Some(
                "`65536` is not a count of seconds from 1 to 65535: number too large \
                 to fit in target type"
            )
        );
        assert_eq!(
            seconds("0").err().as_deref(),
            Some(
                "`0` is not a count of seconds from 1 to 65535: number would be zero \
                 for non-zero type"
            )
        );
        assert_eq!(
            seconds("4294967297").err().as_deref(),
            Some(
                "`4294967297` is not a count of seconds from 1 to 65535: number too \
                 large to fit in target type"
            )
        );
    }

    #[test]
    fn max_len_is_16_kib_or_the_largest_oracle_input() {
        assert_eq!(max_len([]), 16384);
        assert_eq!(max_len([3, 16384, 70]), 16384);
        assert_eq!(max_len([3, 32875, 16385]), 32875);
    }

    #[test]
    fn reports_the_last_done_line_of_a_run_that_passed() {
        let stderr = "INFO: Seed: 1\n#2\tINITED\nDone 10 runs in 1 second(s)\n\
                      Done 99 runs in 5 second(s)\nstat::x: 1\n";
        assert_eq!(
            report("spec_tree", &output(0, "", stderr)),
            (
                "fuzz target `spec_tree` passed: Done 99 runs in 5 second(s)\n"
                    .to_string(),
                Ok(())
            )
        );
    }

    #[test]
    fn reports_the_whole_output_of_a_run_that_failed() {
        assert_eq!(
            report("spec_tree", &output(1, "built\n", "panicked at x\n")),
            (
                "fuzz target `spec_tree` failed:\nbuilt\npanicked at x\n\n".to_string(),
                Err(
                    "fuzz target `spec_tree` failed. cargo-fuzz keeps the input of a \
                     crash in fuzz/artifacts/spec_tree/."
                        .to_string()
                )
            )
        );
    }

    #[test]
    fn refuses_a_run_that_passed_with_no_done_line() {
        assert_eq!(
            report("spec_tree", &output(0, "built\n", "INFO: Seed: 1\n")),
            (
                "fuzz target `spec_tree` gave no `Done` line:\nbuilt\nINFO: Seed: 1\n\n"
                    .to_string(),
                Err("fuzz target `spec_tree` gave no `Done` line".to_string())
            )
        );
    }

    #[test]
    fn refuses_no_targets() {
        assert_eq!(
            unmatched(&[], &["spec_tree".to_string()]),
            ["fuzz/Cargo.toml has no bin target"]
        );
    }

    #[test]
    fn refuses_a_target_with_no_inputs_and_inputs_with_no_target() {
        let folders = ["spec_tree".to_string(), "gone".to_string()];
        assert_eq!(
            unmatched(&["spec_tree", "types_name"], &folders),
            [
                "fuzz target `types_name` has no inputs in oracles/fuzz/types_name/",
                "oracles/fuzz/gone/ has no fuzz target",
            ]
        );
    }

    #[test]
    fn folders_names_a_missing_folder() {
        let dir = crate::fixture().join("absent");
        assert_eq!(
            folders(&dir),
            Err(vec![format!(
                "{}: No such file or directory (os error 2)",
                dir.display()
            )])
        );
    }
}

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

/// The kinds of a lib target in `cargo metadata`.
const LIBS: [&str; 6] = ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];

/// The source of each requirement on crates.io in `cargo metadata`.
const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// The least `-max_len` of a run: libFuzzer makes no input longer.
const MAX_LEN: u64 = 16 * 1024;

/// Runs each target of `fuzz/` at `root` for `seconds`, as many at once as the host has
/// cores. Each run reads `fuzz/corpus/<target>`, which libFuzzer writes to, and
/// `oracles/fuzz/<target>`. It fails before the build on each problem that [`check`]
/// finds, and then when the build fails or a target fails. cargo-fuzz keeps the input
/// of a crash in `fuzz/artifacts/<target>/`.
pub(crate) fn run(root: &Path, seconds: NonZeroU16) -> Result<(), Vec<String>> {
    let targets = check(root)?;
    let nightly = crate::nightly(root).map_err(|e| vec![e])?;
    let built = nightly
        .cargo()
        .args(["fuzz", "build"])
        .status()
        .map_err(|e| vec![format!("rustup: {e}")])?;
    if !built.success() {
        return Err(vec!["`cargo fuzz build` failed".to_string()]);
    }
    let next = AtomicUsize::new(0);
    let worker = || {
        let mut problems = Vec::new();
        while let Some(target) = targets.get(next.fetch_add(1, Ordering::Relaxed)) {
            if let Err(problem) = run_one(root, &nightly, target, seconds) {
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
    nightly: &crate::Nightly,
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
    let output = command(nightly, target, &corpus, &oracles, seconds, max_len(sizes))
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

/// The command that runs `target` on the toolchain `nightly` for `seconds`, on the
/// inputs in `corpus` and `oracles`, with no input longer than `max_len` bytes.
fn command(
    nightly: &crate::Nightly,
    target: &str,
    corpus: &Path,
    oracles: &Path,
    seconds: NonZeroU16,
    max_len: u64,
) -> Command {
    let mut command = nightly.cargo();
    command
        .args(["fuzz", "run", target])
        .arg(corpus)
        .arg(oracles)
        .args([
            "--".to_string(),
            format!("-max_total_time={seconds}"),
            "-timeout=10".to_string(),
            "-rss_limit_mb=2048".to_string(),
            format!("-max_len={max_len}"),
        ]);
    command
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
/// that the `root` graph does not build, and for each edge of the `fuzz` graph that
/// resolves a requirement on crates.io, which a copy that the `root` graph builds
/// meets, to another package. Cargo applies a patch to each such requirement. It fails
/// on an edge to a package with the name of a copy, not a copy, that no requirement
/// resolves.
fn unpatched(root: &Value, fuzz: &Value) -> Result<Vec<String>, String> {
    let copies = Path::new(field::text(root, "workspace_root")?).join("patches");
    let root = Package::all(root, &copies)?;
    let packages = Package::all(fuzz, &copies)?;
    let built = root
        .iter()
        .filter(|package| package.copied)
        .map(|package| Ok((package, package.release()?)))
        .collect::<Result<Vec<_>, String>>()?;
    let mut problems = Vec::new();
    for package in &packages {
        if package.copied && !root.iter().any(|p| p.id == package.id) {
            problems.push(format!(
                "fuzz/Cargo.toml builds `{}` from the copy `{}`, which the root \
                 Cargo.toml does not build. Give fuzz/Cargo.toml the [patch.crates-io] \
                 table of the root Cargo.toml.",
                package.name, package.manifest
            ));
        }
    }
    for node in field::list(&fuzz["resolve"], "nodes")? {
        let dependent = find(&packages, field::text(node, "id")?)?;
        let edges = field::list(node, "deps")?;
        for edge in edges {
            let dependency = find(&packages, field::text(edge, "pkg")?)?;
            if dependency.copied
                || !built.iter().any(|(copy, _)| copy.name == dependency.name)
            {
                continue;
            }
            for requirement in
                dependent.requirements(dependency, edge, edges, &packages)?
            {
                if requirement["source"].as_str() != Some(CRATES_IO) {
                    continue;
                }
                let text = field::text(requirement, "req")?;
                let parsed = VersionReq::parse(text).map_err(|error| {
                    format!(
                        "`{}` needs `{}` `{text}`: {error}",
                        dependent.id, dependency.name
                    )
                })?;
                let Some((copy, _)) = built.iter().find(|(copy, version)| {
                    copy.name == dependency.name && parsed.matches(version)
                }) else {
                    continue;
                };
                problems.push(format!(
                    "fuzz/Cargo.toml builds `{}` `{text}` of `{}` from `{}`, not from \
                     the copy `{}` that meets it. Give fuzz/Cargo.toml the \
                     [patch.crates-io] table of the root Cargo.toml.",
                    dependency.name, dependent.id, dependency.manifest, copy.manifest
                ));
                break;
            }
        }
    }
    Ok(problems)
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

    /// The requirements of the package that `edge` of its node in the resolve, to
    /// `dependency`, resolves: those on `dependency` under the name of the edge, of a
    /// kind and target of the edge. `edges` are the edges of the node, and `packages`
    /// the packages of the graph. Cargo names an edge from a package to itself by the
    /// lib target, whatever the rename, so such an edge resolves each requirement on
    /// the package, of a kind and target of the edge, that no other edge resolves. It
    /// fails when none is.
    fn requirements(
        &self,
        dependency: &Package<'_>,
        edge: &Value,
        edges: &[Value],
        packages: &[Package<'_>],
    ) -> Result<Vec<&'a Value>, String> {
        let name = field::text(edge, "name")?;
        let kinds = field::list(edge, "dep_kinds")?;
        let mut taken = Vec::new();
        if dependency.id == self.id {
            for other in edges {
                let to = find(packages, field::text(other, "pkg")?)?;
                if to.id != self.id && to.name == self.name {
                    taken.extend(self.requirements(to, other, edges, packages)?);
                }
            }
        }
        let mut resolved = Vec::new();
        for requirement in field::list(self.value, "dependencies")? {
            if field::text(requirement, "name")? != dependency.name {
                continue;
            }
            let named = match requirement["rename"].as_str() {
                _ if dependency.id == self.id => {
                    !taken.iter().any(|other| std::ptr::eq(*other, requirement))
                }
                Some(rename) => rename.replace('-', "_") == name,
                None => dependency.lib()? == name,
            };
            let kind = kinds.iter().any(|kind| {
                kind["kind"] == requirement["kind"]
                    && kind["target"] == requirement["target"]
            });
            if named && kind {
                resolved.push(requirement);
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

    /// The name of the lib target of the package, by which a requirement with no rename
    /// names it.
    fn lib(&self) -> Result<&'a str, String> {
        for target in field::list(self.value, "targets")? {
            let kinds = field::list(target, "kind")?;
            if kinds
                .iter()
                .any(|kind| kind.as_str().is_some_and(|kind| LIBS.contains(&kind)))
            {
                return field::text(target, "name");
            }
        }
        Err(format!("`{}` has no lib target", self.id))
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
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use serde_json::json;

    use super::*;

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
        package(
            "noq-proto",
            &id(version),
            &format!("/r/noq-proto-{version}/Cargo.toml"),
        )
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

    #[test]
    fn refuses_a_release_for_a_requirement_that_the_copy_meets() {
        let types = needs(types(), "noq-proto", "^1.3");
        let fuzz = fuzz(&[types, release("1.3.0")], &[(TYPES, &id("1.3.0"))]);
        assert_eq!(
            unpatched(&root(), &fuzz),
            Ok(vec![
                "fuzz/Cargo.toml builds `noq-proto` `^1.3` of \
                 `path+file:///w/crates/types#0.0.0` from \
                 `/r/noq-proto-1.3.0/Cargo.toml`, not from the copy \
                 `/w/patches/noq-proto/Cargo.toml` that meets it. Give \
                 fuzz/Cargo.toml the [patch.crates-io] table of the root Cargo.toml."
                    .to_string()
            ])
        );
    }

    /// The problems of the case `case` of the fixture `patched`, where each case is the
    /// `fuzz/` of the root `patched`, and `vendor/` stands in for crates.io.
    fn patched(case: &str) -> Result<Vec<String>, String> {
        let root = crate::fixture().join("patched");
        let fuzz = root.join("cases").join(case);
        unpatched(&crate::graph(&root)?, &crate::graph(&fuzz)?)
    }

    /// The problem of a requirement `req` of `dependent` that resolves to the release
    /// `release` of `p` in the fixture `patched`.
    fn unmet(req: &str, dependent: &str, release: &str) -> String {
        let root = crate::fixture().join("patched");
        format!(
            "fuzz/Cargo.toml builds `p` `{req}` of `{dependent}` from \
             `{}/vendor/p-{release}/Cargo.toml`, not from the copy \
             `{}/patches/p/Cargo.toml` that meets it. Give fuzz/Cargo.toml the \
             [patch.crates-io] table of the root Cargo.toml.",
            root.display(),
            root.display()
        )
    }

    #[test]
    fn refuses_a_release_beside_the_copy_for_a_requirement_that_the_copy_meets() {
        assert_eq!(
            patched("beside"),
            Ok(vec![unmet(
                "~1.3",
                "registry+https://github.com/rust-lang/crates.io-index#q@1.0.0",
                "1.3.0"
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
    fn passes_a_self_edge_beside_a_patched_requirement_of_the_same_name() {
        assert_eq!(patched("selfname"), Ok(Vec::new()));
    }

    #[test]
    fn refuses_a_self_edge_that_a_patch_resolves_for_a_requirement_the_copy_meets() {
        let case = crate::fixture().join("patched/cases/selfpatch");
        assert_eq!(
            patched("selfpatch"),
            Ok(vec![format!(
                "fuzz/Cargo.toml builds `p` `^1` of `path+file://{0}#p@1.4.0` from \
                 `{0}/Cargo.toml`, not from the copy `{1}/patches/p/Cargo.toml` \
                 that meets it. Give fuzz/Cargo.toml the [patch.crates-io] table of \
                 the root Cargo.toml.",
                case.display(),
                crate::fixture().join("patched").display()
            )])
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
        assert_eq!(
            patched("unpatched"),
            Ok(vec![unmet("^1", &dependent, "1.5.0")])
        );
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
    fn names_a_missing_field_of_a_target_or_an_edge() {
        for (key, kind) in [("targets", "array"), ("kind", "array"), ("name", "string")]
        {
            let needing = needs(types(), "noq-proto", "^1.3");
            let mut lacking =
                fuzz(&[needing, release("1.3.0")], &[(TYPES, &id("1.3.0"))]);
            lacking["packages"][1]
                .as_object_mut()
                .unwrap()
                .remove("targets");
            if key != "targets" {
                lacking["packages"][1]["targets"] =
                    json!([{ "name": "noq_proto", "kind": ["lib"] }]);
                lacking["packages"][1]["targets"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove(key);
            }
            assert_eq!(
                unpatched(&root(), &lacking),
                Err(format!("JSON has no {kind} field `{key}`"))
            );
        }
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

    #[test]
    fn passes_an_edge_that_no_requirement_names_to_a_copy_or_another_name() {
        let copied = fuzz(&[types(), copy()], &[(TYPES, PATCHED)]);
        assert_eq!(unpatched(&root(), &copied), Ok(Vec::new()));
        let other = fuzz(&[types(), crc()], &[(TYPES, CRC)]);
        assert_eq!(unpatched(&root(), &other), Ok(Vec::new()));
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
    fn pairs_an_edge_with_a_requirement_by_rename_package_kind_and_target() {
        let unnamed = Err(unresolved(&id("1.3.0")));
        let refused = Ok(vec![format!(
            "fuzz/Cargo.toml builds `noq-proto` `^1.3` of `{TYPES}` from \
             `/r/noq-proto-1.3.0/Cargo.toml`, not from the copy \
             `/w/patches/noq-proto/Cargo.toml` that meets it. Give fuzz/Cargo.toml the \
             [patch.crates-io] table of the root Cargo.toml."
        )]);
        let renamed =
            |requirement: &mut Value| requirement["rename"] = json!("the-proto");
        assert_eq!(paired("the_proto", renamed), refused);
        assert_eq!(paired("noq_proto", renamed), unnamed);
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
    fn names_a_dependency_with_no_lib_target() {
        let mut bin = release("1.3.0");
        bin["targets"] = json!([{ "name": "noq_proto", "kind": ["bin"] }]);
        let needing = needs(types(), "noq-proto", "^1.3");
        let fuzz = fuzz(&[needing, bin], &[(TYPES, &id("1.3.0"))]);
        assert_eq!(
            unpatched(&root(), &fuzz),
            Err(format!("`{}` has no lib target", id("1.3.0")))
        );
    }

    #[test]
    fn reads_the_lib_target_of_each_kind() {
        for kind in ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"] {
            let mut lib = release("1.3.0");
            lib["targets"] = json!([
                { "name": "noq_proto", "kind": ["bin"] },
                { "name": "noq_proto", "kind": [kind] },
            ]);
            assert_eq!(
                Package::read(&lib, Path::new("/w/patches")).unwrap().lib(),
                Ok("noq_proto")
            );
        }
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
        let nightly = crate::Nightly {
            root: "/w".into(),
            pin: "nightly-x".to_string(),
        };
        let command = command(
            &nightly,
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

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
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

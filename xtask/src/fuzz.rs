//! Runs each fuzz target on the pinned nightly, with its oracle inputs.

use std::num::NonZeroU16;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

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

/// The least `-max_len` of a run: libFuzzer makes no input longer.
const MAX_LEN: u64 = 16 * 1024;

/// Runs each target of `fuzz/` at `root` for `seconds`, as many at once as the host has
/// cores. Each run reads `fuzz/corpus/<target>`, which libFuzzer writes to, and
/// `oracles/fuzz/<target>`. It fails when `fuzz/Cargo.lock` is stale, when `fuzz/`
/// does not build each crate that the root `Cargo.toml` patches from its copy, when the
/// build fails, or when a target fails. cargo-fuzz keeps the input of a crash in
/// `fuzz/artifacts/<target>/`. It also fails, before any run, when `fuzz/` has no
/// target, or when a target and the folders of `oracles/fuzz/` do not match.
pub(crate) fn run(root: &Path, seconds: NonZeroU16) -> Result<(), Vec<String>> {
    patched(root)?;
    let nightly = crate::nightly(root).map_err(|e| vec![e])?;
    let built = nightly
        .cargo()
        .args(["fuzz", "build"])
        .status()
        .map_err(|e| vec![format!("rustup: {e}")])?;
    if !built.success() {
        return Err(vec!["`cargo fuzz build` failed".to_string()]);
    }
    let listed = nightly
        .cargo()
        .args(["fuzz", "list"])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| vec![format!("rustup: {e}")])?;
    if !listed.status.success() {
        return Err(vec!["`cargo fuzz list` failed".to_string()]);
    }
    let targets = String::from_utf8_lossy(&listed.stdout);
    let targets: Vec<&str> = targets.lines().collect();
    let problems = unmatched(&targets, &folders(&root.join("oracles/fuzz"))?);
    if !problems.is_empty() {
        return Err(problems);
    }
    let next = AtomicUsize::new(0);
    let worker = || {
        let mut problems = Vec::new();
        while let Some(&target) = targets.get(next.fetch_add(1, Ordering::Relaxed)) {
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
        return vec!["`cargo fuzz list` names no target".to_string()];
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

/// Checks that `fuzz/Cargo.lock` at `root` is current, and that `fuzz/` builds each
/// crate that the root `Cargo.toml` patches from its copy in `patches/`.
fn patched(root: &Path) -> Result<(), Vec<String>> {
    let graph = |dir: &str| {
        crate::metadata(&root.join(dir), &["--locked"]).map_err(|e| vec![e])
    };
    let problems = unpatched(&graph(".")?, &graph("fuzz")?).map_err(|e| vec![e])?;
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// A problem for each package of the `fuzz` graph that is a copy in `patches/`, or
/// whose name the `root` graph builds from such a copy, when the `root` graph has no
/// package of its key.
fn unpatched(root: &Value, fuzz: &Value) -> Result<Vec<String>, String> {
    let root_packages = field::list(root, "packages")?;
    let copies = Path::new(field::text(root, "workspace_root")?).join("patches");
    let copy = |package: &Value| {
        field::text(package, "manifest_path")
            .map(|path| Path::new(path).starts_with(&copies))
    };
    let mut problems = Vec::new();
    for package in field::list(fuzz, "packages")? {
        let id = field::text(package, "id")?;
        if root_packages.iter().any(|p| p["id"] == id) {
            continue;
        }
        let name = field::text(package, "name")?;
        let manifest = field::text(package, "manifest_path")?;
        if copy(package)? {
            problems.push(format!(
                "fuzz/Cargo.toml builds `{name}` from the copy `{manifest}`, which the \
                 root Cargo.toml does not build. Give fuzz/Cargo.toml the \
                 [patch.crates-io] table of the root Cargo.toml."
            ));
        }
        for namesake in root_packages.iter().filter(|p| p["name"] == name) {
            if copy(namesake)? {
                problems.push(format!(
                    "fuzz/Cargo.toml builds `{name}` from `{manifest}`, not from the \
                     copy in patches/ that the root Cargo.toml builds. Give \
                     fuzz/Cargo.toml the [patch.crates-io] table of the root \
                     Cargo.toml."
                ));
            }
        }
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use serde_json::json;

    use super::*;

    const PATCHED: &str = "path+file:///w/patches/noq-proto#noq-proto@1.3.0";

    fn package(name: &str, id: &str, manifest: &str) -> Value {
        json!({ "name": name, "id": id, "manifest_path": manifest })
    }

    fn root() -> Value {
        json!({
            "workspace_root": "/w",
            "packages": [
                package("noq-proto", PATCHED, "/w/patches/noq-proto/Cargo.toml"),
                package(
                    "types",
                    "path+file:///w/crates/types#0.0.0",
                    "/w/crates/types/Cargo.toml",
                ),
            ],
        })
    }

    fn fuzz(packages: &[Value]) -> Value {
        json!({ "workspace_root": "/w/fuzz", "packages": packages })
    }

    #[test]
    fn fuzz_builds_each_patched_crate_of_this_repository_from_its_copy() {
        assert_eq!(patched(&crate::fixture().join("../..")), Ok(()));
    }

    #[test]
    fn refuses_a_stale_fuzz_lock() {
        let root = crate::fixture().join("stale");
        let problems = patched(&root).unwrap_err();
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
        let graph = crate::metadata(&crate::fixture(), &["--locked"]).unwrap();
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
        let fuzz = fuzz(&[package(
            "noq-proto",
            PATCHED,
            "/w/patches/noq-proto/Cargo.toml",
        )]);
        assert_eq!(unpatched(&root(), &fuzz), Ok(Vec::new()));
    }

    #[test]
    fn refuses_a_fuzz_graph_that_builds_the_registry_release() {
        let fuzz = fuzz(&[package(
            "noq-proto",
            "registry+https://github.com/rust-lang/crates.io-index#noq-proto@1.3.0",
            "/r/noq-proto-1.3.0/Cargo.toml",
        )]);
        assert_eq!(
            unpatched(&root(), &fuzz),
            Ok(vec![
                "fuzz/Cargo.toml builds `noq-proto` from \
                 `/r/noq-proto-1.3.0/Cargo.toml`, not from the copy in patches/ that \
                 the root Cargo.toml builds. Give fuzz/Cargo.toml the \
                 [patch.crates-io] table of the root Cargo.toml."
                    .to_string()
            ])
        );
    }

    #[test]
    fn refuses_a_fuzz_graph_that_builds_a_copy_the_root_does_not() {
        let fuzz = fuzz(&[package(
            "noq-udp",
            "path+file:///w/patches/noq-udp#noq-udp@1.3.0",
            "/w/patches/noq-udp/Cargo.toml",
        )]);
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
    fn names_a_missing_field_of_a_graph() {
        assert_eq!(
            unpatched(&json!({ "workspace_root": "/w" }), &fuzz(&[])),
            Err("JSON has no array field `packages`".to_string())
        );
        assert_eq!(
            unpatched(&json!({ "packages": [] }), &fuzz(&[])),
            Err("JSON has no string field `workspace_root`".to_string())
        );
        let nameless = json!({ "id": "x", "manifest_path": "/r/x/Cargo.toml" });
        assert_eq!(
            unpatched(&root(), &fuzz(&[nameless])),
            Err("JSON has no string field `name`".to_string())
        );
    }

    #[test]
    fn passes_a_fuzz_graph_that_lacks_a_patched_crate_or_has_others() {
        let fuzz = fuzz(&[
            package(
                "types",
                "path+file:///w/crates/types#0.0.0",
                "/w/crates/types/Cargo.toml",
            ),
            package(
                "crc32c",
                "registry+x#crc32c@0.6.8",
                "/r/crc32c-0.6.8/Cargo.toml",
            ),
        ]);
        assert_eq!(unpatched(&root(), &fuzz), Ok(Vec::new()));
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
            ["`cargo fuzz list` names no target"]
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
    fn each_fuzz_target_of_this_repository_has_oracle_inputs() {
        let root = crate::fixture().join("../..");
        let targets: Vec<String> = std::fs::read_dir(root.join("fuzz/fuzz_targets"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                path.file_stem().unwrap().to_string_lossy().into_owned()
            })
            .collect();
        let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
        let folders = folders(&root.join("oracles/fuzz")).unwrap();
        assert!(folders.contains(&"types_name".to_string()));
        assert_eq!(unmatched(&targets, &folders), Vec::<String>::new());
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

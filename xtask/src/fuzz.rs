//! Runs each fuzz target on the pinned nightly, with its oracle inputs.

use std::num::NonZeroU64;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

/// The seconds of each target when the task names none.
pub(crate) const SECONDS: NonZeroU64 = NonZeroU64::new(60).unwrap();

/// The least `-max_len` of a run: libFuzzer makes no input longer.
const MAX_LEN: u64 = 16 * 1024;

/// Runs each target of `fuzz/` at `root` for `seconds`, as many at once as the host has
/// cores. Each run reads `fuzz/corpus/<target>`, which libFuzzer writes to, and
/// `oracles/fuzz/<target>`. It fails when `fuzz/Cargo.lock` is stale, when `fuzz/`
/// does not build each crate that the root `Cargo.toml` patches from its copy, when the
/// build fails, or when a target fails. cargo-fuzz keeps the input of a crash in
/// `fuzz/artifacts/<target>/`.
pub(crate) fn run(root: &Path, seconds: NonZeroU64) -> Result<(), Vec<String>> {
    patched(root)?;
    let nightly = crate::nightly(root).map_err(|e| vec![e])?;
    let fuzz = |args: &[&str]| {
        let mut command = Command::new("rustup");
        command
            .current_dir(root)
            .args(["run", &nightly, "cargo", "fuzz"])
            .args(args);
        command
    };
    let built = fuzz(&["build"])
        .status()
        .map_err(|e| vec![format!("rustup: {e}")])?;
    if !built.success() {
        return Err(vec!["`cargo fuzz build` failed".to_string()]);
    }
    let listed = fuzz(&["list"])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| vec![format!("rustup: {e}")])?;
    if !listed.status.success() {
        return Err(vec!["`cargo fuzz list` failed".to_string()]);
    }
    let targets = String::from_utf8_lossy(&listed.stdout);
    let targets: Vec<&str> = targets.lines().collect();
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
    nightly: &str,
    target: &str,
    seconds: NonZeroU64,
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
    let output = command(root, nightly, target, seconds, max_len(sizes))
        .output()
        .map_err(|e| format!("rustup: {e}"))?;
    let report = report(target, &output);
    eprint!("{report}");
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "fuzz target `{target}` failed. cargo-fuzz keeps the input of a crash in \
             fuzz/artifacts/{target}/."
        ))
    }
}

/// The command that runs `target` of `fuzz/` at `root` for `seconds` on the toolchain
/// `nightly`, with no input longer than `max_len` bytes.
fn command(
    root: &Path,
    nightly: &str,
    target: &str,
    seconds: NonZeroU64,
    max_len: u64,
) -> Command {
    let mut command = Command::new("rustup");
    command
        .current_dir(root)
        .args(["run", nightly, "cargo", "fuzz", "run", target]);
    command
        .arg(root.join("fuzz/corpus").join(target))
        .arg(root.join("oracles/fuzz").join(target))
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

/// What the run of `target` that gave `output` prints: the whole output of a failed
/// run, and libFuzzer's last `Done` line for a run that passed.
fn report(target: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        let done = stderr.lines().rfind(|line| line.starts_with("Done "));
        format!(
            "fuzz target `{target}` passed: {}\n",
            done.unwrap_or("no runs")
        )
    } else {
        let stdout = String::from_utf8_lossy(&output.stdout);
        format!("fuzz target `{target}` failed:\n{stdout}{stderr}\n")
    }
}

/// Checks that `fuzz/Cargo.lock` at `root` is current, and that `fuzz/` builds each
/// crate that the root `Cargo.toml` patches from its copy in `patches/`.
fn patched(root: &Path) -> Result<(), Vec<String>> {
    let graph = |manifest: &str| graph(&root.join(manifest)).map_err(|e| vec![e]);
    let problems = unpatched(&graph("Cargo.toml")?, &graph("fuzz/Cargo.toml")?);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Runs `cargo metadata --locked` on the package or workspace of `manifest`.
fn graph(manifest: &Path) -> Result<Value, String> {
    let output = crate::cargo()
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--manifest-path",
        ])
        .arg(manifest)
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata: {e}"))
}

/// A problem for each package of the `fuzz` graph whose name the `root` graph builds
/// from a copy in `patches/`, when the `root` graph has no package of its key.
fn unpatched(root: &Value, fuzz: &Value) -> Vec<String> {
    let packages =
        |graph: &Value| graph["packages"].as_array().cloned().unwrap_or_default();
    let root_packages = packages(root);
    let copies =
        Path::new(root["workspace_root"].as_str().unwrap_or_default()).join("patches");
    let patched = |name: &str| {
        root_packages.iter().any(|package| {
            package["name"] == name
                && package["manifest_path"]
                    .as_str()
                    .is_some_and(|path| Path::new(path).starts_with(&copies))
        })
    };
    packages(fuzz)
        .iter()
        .filter(|package| package["name"].as_str().is_some_and(patched))
        .filter(|package| !root_packages.iter().any(|p| p["id"] == package["id"]))
        .map(|package| {
            format!(
                "fuzz/Cargo.toml builds `{}` from `{}`, not from the copy in patches/ that \
                 the root Cargo.toml builds. Give fuzz/Cargo.toml the [patch.crates-io] \
                 table of the root Cargo.toml.",
                package["name"].as_str().unwrap_or_default(),
                package["manifest_path"].as_str().unwrap_or_default(),
            )
        })
        .collect()
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
                package("types", "path+file:///w/crates/types#0.0.0", "/w/crates/types/Cargo.toml"),
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
    fn graph_holds_each_package_of_the_lock() {
        let graph = graph(&crate::fixture().join("Cargo.toml")).unwrap();
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
        assert_eq!(unpatched(&root(), &fuzz), Vec::<String>::new());
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
            [
                "fuzz/Cargo.toml builds `noq-proto` from `/r/noq-proto-1.3.0/Cargo.toml`, \
                 not from the copy in patches/ that the root Cargo.toml builds. Give \
                 fuzz/Cargo.toml the [patch.crates-io] table of the root Cargo.toml."
            ]
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
        assert_eq!(unpatched(&root(), &fuzz), Vec::<String>::new());
    }

    #[test]
    fn runs_the_target_on_both_corpus_directories_with_the_limits() {
        let seconds = NonZeroU64::new(5).unwrap();
        let command =
            command(Path::new("/w"), "nightly-x", "spec_tree", seconds, 20000);
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
                "/w/fuzz/corpus/spec_tree",
                "/w/oracles/fuzz/spec_tree",
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
            "fuzz target `spec_tree` passed: Done 99 runs in 5 second(s)\n"
        );
    }

    #[test]
    fn reports_the_whole_output_of_a_run_that_failed() {
        assert_eq!(
            report("spec_tree", &output(1, "built\n", "panicked at x\n")),
            "fuzz target `spec_tree` failed:\nbuilt\npanicked at x\n\n"
        );
    }
}

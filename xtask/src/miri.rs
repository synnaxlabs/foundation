//! Runs Miri on the pinned nightly.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::select;

/// The `MIRIFLAGS` of each Miri pass.
const PASSES: [&str; 2] = [
    "-Zmiri-strict-provenance",
    "-Zmiri-strict-provenance -Zmiri-tree-borrows",
];

/// Crates whose `unsafe` code only calls the OS or C, which Miri cannot run. Tests on
/// the real OS check `os` (BLOCK MEMORY). A sanitizer run will check
/// `connector-opcua` (#1912).
pub(crate) const SKIPPED: [&str; 2] = ["os", "connector-opcua"];

/// The workspace crates whose source names `unsafe_code`, the lint that each `unsafe`
/// use must expect, except those in [`SKIPPED`].
pub(crate) fn packages(metadata: &Value) -> Result<Vec<select::Package>, String> {
    let mut packages =
        select::packages(metadata, |s| select::has_word(s, "unsafe_code"))?;
    packages.retain(|package| !SKIPPED.contains(&package.name.as_str()));
    Ok(packages)
}

/// Runs `cargo miri test` once per pass in [`PASSES`] on each crate of [`packages`].
/// It uses rustup and the nightly in `rust-toolchain-nightly`. It fails when a crate
/// fails or runs no tests.
pub(crate) fn run(root: &Path) -> Result<(), Vec<String>> {
    let nightly = crate::nightly(root).map_err(|e| vec![e])?;
    let metadata = crate::metadata(root).map_err(|e| vec![e])?;
    let packages = packages(&metadata).map_err(|e| vec![e])?;
    if packages.is_empty() {
        eprintln!("no crate names `unsafe_code`, so Miri has nothing to check");
        return Ok(());
    }
    let mut problems = Vec::new();
    for flags in PASSES {
        for package in &packages {
            let output = command(&nightly, package, flags)
                .stderr(Stdio::inherit())
                .output()
                .map_err(|e| vec![format!("rustup: {e}")])?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            eprint!("{stdout}");
            if !output.status.success() {
                problems
                    .push(format!("Miri with `{flags}` failed in `{}`", package.name));
            } else if tests_ran(&stdout) == 0 {
                problems.push(format!(
                    "`{}` names `unsafe_code` but runs no tests under Miri. Add tests \
                     that reach its unsafe code.",
                    package.name
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// The command that runs Miri with `flags` on `package`, on the toolchain `nightly`.
fn command(
    nightly: &crate::Nightly,
    package: &select::Package,
    flags: &str,
) -> Command {
    let mut command = nightly.cargo();
    command
        .args(["miri", "test", "-p", &package.id])
        .env("MIRIFLAGS", flags);
    command
}

/// The sum of N over the `running N tests` lines of libtest output.
fn tests_ran(output: &str) -> usize {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("running ")?.split(' ').next())
        .filter_map(|count| count.parse::<usize>().ok())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_ran_sums_each_test_binary() {
        let output = "\nrunning 2 tests\ntest a ... ok\n\nrunning 0 tests\n\n\
                      running 1 test\ntest b ... ok\n";
        assert_eq!(tests_ran(output), 3);
    }

    #[test]
    fn packages_are_the_crates_that_name_unsafe_code() {
        let metadata = crate::metadata(&crate::fixture()).unwrap();
        let picked = packages(&metadata).unwrap();
        let names: Vec<_> = picked.into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["model"]);
    }

    #[test]
    fn selects_the_package_by_its_id() {
        let package = select::Package {
            id: "path+file:///w/crates/model#0.0.0".to_string(),
            name: "model".to_string(),
        };
        let nightly = crate::Nightly {
            root: "/w".into(),
            pin: "nightly-x".to_string(),
        };
        let command = command(&nightly, &package, PASSES[0]);
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
    fn tests_ran_is_zero_with_no_tests() {
        assert_eq!(tests_ran("\nrunning 0 tests\n\ntest result: ok.\n"), 0);
    }
}

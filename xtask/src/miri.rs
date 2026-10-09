//! Runs Miri on the pinned nightly.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::libtest::{self, Run};
use crate::select;

/// The `MIRIFLAGS` of each Miri pass.
const PASSES: [&str; 2] = [
    "-Zmiri-strict-provenance",
    "-Zmiri-strict-provenance -Zmiri-tree-borrows",
];

/// Crates whose `unsafe` code only calls the OS or C, which Miri cannot run. Tests on
/// the real OS check `os` (BLOCK MEMORY), and `cargo xtask sanitizers` checks
/// `connector-opcua`.
pub(crate) const SKIPPED: [&str; 2] = ["os", crate::sanitizers::CRATE];

/// The workspace crates whose source names `unsafe_code`, the lint that each `unsafe`
/// use must expect, except those in [`SKIPPED`].
pub(crate) fn packages(metadata: &Value) -> Result<Vec<select::Package>, String> {
    let mut packages =
        select::packages(metadata, |s| select::has_word(s, "unsafe_code"))?;
    packages.retain(|package| !SKIPPED.contains(&package.name.as_str()));
    Ok(packages)
}

/// The problem of `run`, a Miri run with `flags` of the crate `name`, if any.
fn problem(run: &Run, flags: &str, name: &str) -> Option<String> {
    match run {
        Run::Failed => Some(format!("Miri with `{flags}` failed in `{name}`")),
        Run::Empty => Some(format!(
            "`{name}` names `unsafe_code` but runs no tests under Miri. Add tests \
             that reach its unsafe code."
        )),
        Run::Passed => None,
    }
}

/// Runs `cargo miri test` once per pass in [`PASSES`] on each crate of [`packages`].
/// It uses rustup and the nightly in `rust-toolchain-nightly`. It fails when a crate
/// fails or runs no tests.
pub(crate) fn run(root: &Path) -> Result<(), Vec<String>> {
    let nightly = crate::nightly::Toolchain::read(root).map_err(|e| vec![e])?;
    let metadata = crate::metadata(root).map_err(|e| vec![e])?;
    let packages = packages(&metadata).map_err(|e| vec![e])?;
    if packages.is_empty() {
        eprintln!("no crate names `unsafe_code`, so Miri has nothing to check");
        return Ok(());
    }
    let mut problems = Vec::new();
    for flags in PASSES {
        for package in &packages {
            let run = libtest::run(&mut command(&nightly, package, flags))
                .map_err(|e| vec![e])?;
            problems.extend(problem(&run, flags, &package.name));
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
    nightly: &crate::nightly::Toolchain,
    package: &select::Package,
    flags: &str,
) -> Command {
    let mut command = nightly.cargo();
    command
        .args(["miri", "test", "-p", &package.id])
        .env("MIRIFLAGS", flags);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let nightly = crate::nightly::Toolchain::new("/w", "nightly-x");
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
    fn problem_names_each_run_that_does_not_pass() {
        let problem = |run| problem(&run, "-Zmiri-x", "model");
        assert_eq!(
            problem(Run::Failed),
            Some("Miri with `-Zmiri-x` failed in `model`".to_string())
        );
        assert_eq!(
            problem(Run::Empty),
            Some(
                "`model` names `unsafe_code` but runs no tests under Miri. Add tests \
                 that reach its unsafe code."
                    .to_string()
            )
        );
        assert_eq!(problem(Run::Passed), None);
    }
}

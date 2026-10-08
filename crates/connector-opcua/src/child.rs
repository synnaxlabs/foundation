//! Runs a test of this binary again in a child process, with the C flags it picks.

use std::process::Command;

/// The variable that marks a child process.
const MARK: &str = "CONNECTOR_OPCUA_CHILD";

/// Whether this process is a child that `run` started.
pub(crate) fn running() -> bool {
    #[expect(
        clippy::disallowed_methods,
        reason = "the parent test sets the environment of its child process"
    )]
    let mark = std::env::var_os(MARK);
    mark.is_some()
}

/// Runs the test `name` in a child process, and asserts that it passes. `cflags` is
/// the only C flags that `cc` reads there for `target` built on itself, and `cc` adds
/// its defaults.
pub(crate) fn run(name: &str, target: &str, cflags: Option<&str>) {
    let mut child = Command::new(std::env::current_exe().unwrap());
    child.args(["--exact", name]).env(MARK, "1");
    let underscored = target.replace(['-', '.'], "_");
    for var in [
        "CRATE_CC_NO_DEFAULTS",
        "CFLAGS",
        "HOST_CFLAGS",
        &format!("CFLAGS_{target}"),
        &format!("CFLAGS_{underscored}"),
    ] {
        child.env_remove(var);
    }
    if let Some(cflags) = cflags {
        child.env("CFLAGS", cflags);
    }
    let output = child.output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{name} with CFLAGS={cflags:?}: {stdout}"
    );
}

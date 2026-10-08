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

/// Runs the test `name` in a child process, and asserts that it passes. The child
/// gets only `PATH` from this process, so `cc` reads no compiler or flags from the
/// environment there, and `cflags` is its `CFLAGS`.
pub(crate) fn run(name: &str, cflags: Option<&str>) {
    #[expect(
        clippy::disallowed_methods,
        reason = "`cc` finds the compiler on the PATH of the child process"
    )]
    let path = std::env::var_os("PATH").expect("PATH is set");
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", name])
        .env_clear()
        .env("PATH", path)
        .env(MARK, "1");
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

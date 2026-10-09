//! Runs a test of this binary again in a child process, with the C flags it picks.

use std::process::{Command, Output};

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

/// Sets `build` to compile for `target` outside a build script.
pub(crate) fn configure<'a>(
    build: &'a mut cc::Build,
    target: &str,
) -> &'a mut cc::Build {
    build
        .target(target)
        .host(target)
        .opt_level(0)
        .cargo_metadata(false)
        .cargo_warnings(false)
}

/// Runs the test `name` in a child process, and asserts that it passes. The child
/// gets only `PATH`, `envs`, and, as `CC` unless `envs` sets it, the compiler that `cc`
/// picks here without its flags, so `cc` reads C flags there only from `envs`.
pub(crate) fn run(name: &str, envs: &[(&str, &str)]) {
    let output = output(name, envs.iter().copied());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{name} with {envs:?}: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs the test `name` in a child process, and gives its output. The child gets only
/// `PATH`, `envs`, and, as `CC` unless `envs` sets it, the compiler that `cc` picks
/// here without its flags.
pub(crate) fn output<'a>(
    name: &str,
    envs: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Output {
    let target = env!("CONNECTOR_OPCUA_TARGET");
    let compiler = configure(&mut cc::Build::new(), target).get_compiler();
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
        .env("CC", compiler.path())
        .env(MARK, "1")
        .envs(envs);
    child.output().unwrap()
}

//! Runs a test of this binary again in a child process, with the C flags it picks.

use std::process::{Command, Output};

/// The variable that marks a child process.
const MARK: &str = "CONNECTOR_OPCUA_CHILD";

/// Whether this process is a child that `run` or `output` started.
pub(crate) fn running() -> bool {
    #[expect(
        clippy::disallowed_methods,
        reason = "the parent test sets the environment of its child process"
    )]
    let mark = std::env::var_os(MARK);
    mark.is_some()
}

/// Gives the tool that `build` picks for `target` outside a build script.
pub(crate) fn tool(build: &mut cc::Build, target: &str) -> cc::Tool {
    build
        .target(target)
        .host(target)
        .opt_level(0)
        .cargo_metadata(false)
        .cargo_warnings(false)
        .get_compiler()
}

/// Runs the test `name` as `output` does, and asserts that it passes.
pub(crate) fn run(name: &str, envs: &[(&str, &str)]) {
    let output = output(name, envs);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{name} with {envs:?}: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs the test `name` in a child process, and gives its output. The child gets only
/// `PATH`, `envs`, and, as `CC` unless `envs` sets it, the compiler that `cc` picks
/// here without its flags, so `cc` reads C flags there only from `envs`.
pub(crate) fn output(name: &str, envs: &[(&str, &str)]) -> Output {
    let mut child = Command::new(std::env::current_exe().unwrap());
    child.args(["--exact", name]);
    prepare(&mut child, envs).output().unwrap()
}

/// Runs the test `name` as `output` does, but under `setsid`, so the child has no
/// controlling terminal, with `stdin` on a pipe that then closes.
#[cfg(unix)]
pub(crate) fn detached(name: &str, envs: &[(&str, &str)], stdin: &[u8]) -> Output {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("setsid");
    child
        .arg("--wait")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", name]);
    let mut child = prepare(&mut child, envs)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The child can exit before it reads `stdin`.
    match child.stdin.take().unwrap().write_all(stdin) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        result => result.unwrap(),
    }
    child.wait_with_output().unwrap()
}

/// Gives `child` the environment that `output` states.
fn prepare<'a>(child: &'a mut Command, envs: &[(&str, &str)]) -> &'a mut Command {
    let compiler = tool(&mut cc::Build::new(), env!("CONNECTOR_OPCUA_TARGET"));
    #[expect(
        clippy::disallowed_methods,
        reason = "`cc` finds the compiler on the PATH of the child process"
    )]
    let path = std::env::var_os("PATH").expect("PATH is set");
    child
        .env_clear()
        .env("PATH", path)
        .env("CC", compiler.path())
        .env(MARK, "1")
        .envs(envs.iter().copied())
}

/// Runs the test `name` as `output` does, asserts that it aborts, and gives its
/// output. On Windows it asserts the status of a Rust abort, or of a panic that no
/// frame catches.
pub(crate) fn aborted(name: &str, envs: &[(&str, &str)]) -> Output {
    let output = output(name, envs);
    let stderr = String::from_utf8_lossy(&output.stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        const SIGABRT: i32 = 6;
        let signal = output.status.signal();
        assert_eq!(signal, Some(SIGABRT), "{name} {envs:?}: {stderr}");
    }
    #[cfg(windows)]
    {
        // Rust aborts through `__fastfail`. A panic in the `listen` callback of C
        // ends instead with the exception of the libgcc unwinder, which nothing
        // handles (#2579).
        const STATUS_STACK_BUFFER_OVERRUN: i32 = 0xC000_0409_u32.cast_signed();
        const STATUS_GCC_THROW: i32 = 0x2047_4343;
        let code = output.status.code();
        let ends = [Some(STATUS_STACK_BUFFER_OVERRUN), Some(STATUS_GCC_THROW)];
        assert!(ends.contains(&code), "{name} {envs:?}: {code:?} {stderr}");
    }
    output
}

/// Runs the test `name` as `output` does, and asserts that it writes `line` to stderr
/// and ends through `abort` of the C runtime. On Windows the C runtime can write its
/// own text after `line`.
pub(crate) fn aborted_in_c(name: &str, envs: &[(&str, &str)], line: &str) {
    let output = output(name, envs);
    let stderr = String::from_utf8_lossy(&output.stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        const SIGABRT: i32 = 6;
        assert_eq!(
            output.status.signal(),
            Some(SIGABRT),
            "{name} {envs:?}: {stderr}"
        );
        assert_eq!(stderr, line);
    }
    #[cfg(windows)]
    {
        // The exit code of `abort` in the C runtime of MinGW.
        assert_eq!(output.status.code(), Some(3), "{name} {envs:?}: {stderr}");
        assert!(stderr.starts_with(line), "{stderr:?}");
    }
}

/// A child that exits before it reads its standard input gives its output: more bytes
/// than a pipe holds make the write fail.
#[test]
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), ignore = "needs setsid")]
fn a_detached_child_that_reads_no_stdin_gives_its_output() {
    let output = detached("child::no_such_test", &[], &vec![0; 1 << 20]);
    assert!(output.status.success(), "{output:?}");
}

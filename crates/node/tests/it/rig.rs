//! A bench for process tests: one `foundation` node in a temporary directory. Each
//! method with a `todo!` waits on the issue it names.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use env::clock::Clock;
use types::time::Span;

use crate::status::Connector;

/// How long [`Rig::wait`] and [`Rig::run`] wait: longer than the 60 s cap of a restart
/// backoff.
const PATIENCE: Span = Span::from_nanos(90_000_000_000);

/// The time between two checks of a wait.
const POLL: Duration = Duration::from_millis(100);

/// One `foundation` node in a temporary directory of its own. Drop removes the
/// directory, except while the thread panics: a failed test keeps it.
#[derive(Debug)]
pub(crate) struct Rig {
    /// The working directory of each command. It holds `plant.hcl`.
    pub(crate) dir: PathBuf,
    clock: Clock,
}

impl Rig {
    /// Makes a temporary directory for the test on this thread, with a name that no
    /// directory has: a run that was killed keeps its directory, and a later run can
    /// get the same PID.
    pub(crate) fn new() -> Self {
        let thread = std::thread::current();
        let test = thread.name().expect("invariant: libtest names the thread");
        let name = format!("foundation-node-{}-{test}", std::process::id());
        let name = name.replace("::", "-");
        let mut n = 0;
        let dir = loop {
            let dir = std::env::temp_dir().join(format!("{name}-{n}"));
            match std::fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => n += 1,
                Err(error) => panic!("make {}: {error}", dir.display()),
            }
        };
        Self {
            dir,
            clock: os::clock(),
        }
    }

    /// Writes `hcl` to `plant.hcl`.
    pub(crate) fn config(&self, hcl: &str) {
        std::fs::write(self.dir.join("plant.hcl"), hcl).expect("write plant.hcl");
    }

    /// Starts `foundation start --name edge`, with each listener of the node on port 0
    /// of loopback, and waits until it prints that the node runs.
    pub(crate) fn start(&mut self) {
        todo!("waits on #1732")
    }

    /// Runs `foundation` with `args` in [`Rig::dir`], with `input` on its standard
    /// input, and gives its output when it exits and closes its pipes. When 90 s pass
    /// first, kills it and panics with the output so far.
    pub(crate) fn run(&self, args: &[&str], input: &[u8]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_foundation"));
        command.args(args).current_dir(&self.dir);
        run(&self.clock, PATIENCE, command, input)
    }

    /// Calls `check` until it gives `Ok`, and gives that value. When 90 s pass first,
    /// panics with `what` and the last `Err`: the state that `check` saw.
    pub(crate) fn wait<T>(
        &self,
        what: &str,
        check: impl FnMut() -> Result<T, String>,
    ) -> T {
        wait(&self.clock, PATIENCE, what, check)
    }

    /// Plans `plant.hcl` into `plant.plan`, and applies that plan. Panics when either
    /// command does not exit 0.
    pub(crate) fn apply(&self) {
        todo!("waits on #337, #1744")
    }

    /// The connectors of `foundation status --json`, by name.
    pub(crate) fn status(&self) -> BTreeMap<String, Connector> {
        todo!("waits on #1735")
    }
}

/// [`Rig::wait`], with `limit` in place of 90 s.
fn wait<T>(
    clock: &Clock,
    limit: Span,
    what: &str,
    check: impl FnMut() -> Result<T, String>,
) -> T {
    poll(clock, limit, check).unwrap_or_else(|seen| late(what, limit, &seen))
}

/// Panics: `what` did not happen within `limit`, and the last check saw `seen`.
fn late(what: &str, limit: Span, seen: &str) -> ! {
    panic!("{what}: not within {limit}. The last check saw:\n{seen}")
}

/// Gives the first value of `check`, or what its last check saw once `limit` passes.
fn poll<T>(
    clock: &Clock,
    limit: Span,
    mut check: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let deadline = clock.now() + limit;
    loop {
        let seen = match check() {
            Ok(value) => return Ok(value),
            Err(seen) => seen,
        };
        if clock.now() >= deadline {
            return Err(seen);
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "a process test waits on another process in real time"
        )]
        std::thread::sleep(POLL);
    }
}

/// Runs `command` with `input` on its standard input, and gives its output when it
/// exits and closes its pipes. When `limit` passes first, kills it and panics with the
/// output so far.
fn run(clock: &Clock, limit: Span, command: Command, input: &[u8]) -> Output {
    let mut running = Running::new(command, input);
    let ended = poll(clock, limit, || {
        running.ended().ok_or_else(|| running.seen())
    });
    match ended {
        Ok(status) => running.output(status),
        Err(seen) => {
            running.process.end();
            late("the command exits and closes its pipes", limit, &seen)
        }
    }
}

/// A command that runs, with a thread for each of its pipes.
struct Running {
    process: Process,
    input: JoinHandle<()>,
    stdout: Capture,
    stderr: Capture,
}

impl Running {
    /// Starts `command`, and writes `input` to it from a thread that then closes its
    /// standard input.
    fn new(mut command: Command, input: &[u8]) -> Self {
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the command");
        let mut process = Process(child);
        let mut stdin = process.0.stdin.take().expect("piped");
        let input = input.to_vec();
        #[expect(
            clippy::disallowed_methods,
            reason = "a process test writes to another process while it runs"
        )]
        let input = std::thread::spawn(move || match stdin.write_all(&input) {
            // The command closed its standard input: it reads no more.
            Err(error) if error.kind() == ErrorKind::BrokenPipe => {}
            written => written.expect("write the input"),
        });
        Self {
            input,
            stdout: Capture::new(process.0.stdout.take().expect("piped")),
            stderr: Capture::new(process.0.stderr.take().expect("piped")),
            process,
        }
    }

    /// The exit status, once the command has exited and each of its pipes has closed.
    fn ended(&mut self) -> Option<ExitStatus> {
        let status = self.process.0.try_wait().expect("check the command")?;
        let closed =
            self.input.is_finished() && self.stdout.ended() && self.stderr.ended();
        closed.then_some(status)
    }

    /// What the command has written so far.
    fn seen(&self) -> String {
        format!(
            "stdout:\n{}\nstderr:\n{}",
            self.stdout.text(),
            self.stderr.text()
        )
    }

    /// The output of a command that [`Running::ended`] with `status`.
    fn output(self, status: ExitStatus) -> Output {
        self.input.join().expect("write the input");
        Output {
            status,
            stdout: self.stdout.end(),
            stderr: self.stderr.end(),
        }
    }
}

/// The process of a command. Drop kills it, so a test that panics leaves no
/// `foundation` process. A process that it starts lives on: `foundation` starts none.
struct Process(Child);

impl Process {
    /// Kills the command and waits for it to exit.
    fn end(&mut self) {
        // `kill` gives `Ok` for a command that exited.
        (self.0.kill())
            .and_then(|()| self.0.wait())
            .expect("kill the command");
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // While the thread panics, a wait can block and a second panic aborts the
        // test binary, so the command only gets the signal.
        if std::thread::panicking() {
            if let Err(error) = self.0.kill() {
                #[expect(clippy::print_stderr, reason = "a panic is under way")]
                {
                    eprintln!("kill the command: {error}");
                }
            }
        } else {
            self.end();
        }
    }
}

/// One output of a command, which a thread reads as the command writes it.
struct Capture {
    bytes: Arc<Mutex<Vec<u8>>>,
    reader: JoinHandle<()>,
}

impl Capture {
    fn new(mut from: impl Read + Send + 'static) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let to = Arc::clone(&bytes);
        #[expect(
            clippy::disallowed_methods,
            reason = "a process test reads another process while it runs"
        )]
        let reader = std::thread::spawn(move || {
            let mut chunk = [0; 4096];
            loop {
                match from.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(n) => {
                        to.lock().expect("no panic").extend_from_slice(&chunk[..n]);
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(error) => panic!("read the output: {error}"),
                }
            }
        });
        Self { bytes, reader }
    }

    /// The bytes read so far, as text.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes.lock().expect("no panic")).into_owned()
    }

    /// Whether the output has ended.
    fn ended(&self) -> bool {
        self.reader.is_finished()
    }

    /// All of the output. Blocks until it ends.
    fn end(self) -> Vec<u8> {
        self.reader.join().expect("read the output");
        std::mem::take(&mut *self.bytes.lock().expect("no panic"))
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // While the thread panics, a remove can block and a second panic aborts the
        // test binary.
        if !std::thread::panicking() {
            std::fs::remove_dir_all(&self.dir).expect("remove the directory");
        }
    }
}

#[test]
fn a_rig_takes_a_new_directory_when_its_name_is_in_use() {
    let kept = Rig::new();
    let rig = Rig::new();
    assert_ne!(rig.dir, kept.dir);
}

#[test]
fn a_rig_writes_the_config_as_given() {
    let rig = Rig::new();
    rig.config("node \"edge\" {}\n");
    assert_eq!(
        std::fs::read_to_string(rig.dir.join("plant.hcl")).expect("read plant.hcl"),
        "node \"edge\" {}\n"
    );
}

#[test]
fn a_rig_removes_its_directory_with_its_files() {
    let rig = Rig::new();
    rig.config("");
    let dir = rig.dir.clone();
    drop(rig);
    assert!(!dir.exists(), "{} is still there", dir.display());
}

#[test]
fn a_rig_keeps_its_directory_when_the_test_fails() {
    let mut dir = PathBuf::new();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let rig = Rig::new();
        rig.config("");
        dir.clone_from(&rig.dir);
        panic!("the test fails");
    }))
    .expect_err("the test panics");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"the test fails"));
    assert!(dir.join("plant.hcl").exists(), "{} is gone", dir.display());
    std::fs::remove_dir_all(&dir).expect("remove the directory");
}

#[test]
#[cfg(unix)]
fn a_run_past_its_limit_kills_the_command_and_panics_with_its_output() {
    let mut command = Command::new("sh");
    command.args(["-c", "echo $$; echo err >&2; exec sleep 60"]);
    let clock = os::clock();
    let started = clock.now();
    let limited =
        std::panic::AssertUnwindSafe(|| run(&clock, Span::SECOND, command, &[]));
    let panic = std::panic::catch_unwind(limited).expect_err("the run panics");
    let took = clock.now() - started;
    let message = panic.downcast_ref::<String>().expect("a message");
    let pid = message.lines().nth(2).expect("the PID");
    assert_eq!(
        message,
        &format!(
            "the command exits and closes its pipes: not within 1s. The last check \
             saw:\nstdout:\n{pid}\n\nstderr:\nerr\n"
        )
    );
    assert!(
        took < Span::from_nanos(30_000_000_000),
        "the kill took {took}"
    );
    let alive = Command::new("kill")
        .args(["-0", pid])
        .output()
        .expect("kill -0");
    assert_eq!(alive.status.code(), Some(1), "{pid} still runs");
}

/// Runs `script`, which starts a `sleep 60` that keeps one pipe of the command and
/// prints its PID, and asserts that the run panics at its limit. Then kills the
/// `sleep`.
#[cfg(unix)]
fn assert_a_run_ends_at_its_limit(script: &str, input: &[u8]) {
    let mut command = Command::new("sh");
    command.args(["-c", script]);
    let limited = std::panic::AssertUnwindSafe(|| {
        run(&os::clock(), Span::SECOND, command, input)
    });
    let panic = std::panic::catch_unwind(limited).expect_err("the run panics");
    let message = panic.downcast_ref::<String>().expect("a message");
    let pid = message.lines().nth(2).expect("the PID");
    let killed = Command::new("kill").arg(pid).status().expect("kill");
    assert_eq!(
        message,
        &format!(
            "the command exits and closes its pipes: not within 1s. The last check \
             saw:\nstdout:\n{pid}\n\nstderr:\n"
        )
    );
    assert!(killed.success(), "{pid} was gone");
}

#[test]
#[cfg(unix)]
fn a_run_ends_at_its_limit_when_a_process_of_the_command_keeps_its_output() {
    assert_a_run_ends_at_its_limit("sleep 60 2>/dev/null & echo $!", &[]);
}

#[test]
#[cfg(unix)]
fn a_run_ends_at_its_limit_when_a_process_of_the_command_keeps_its_errors() {
    assert_a_run_ends_at_its_limit("sleep 60 >/dev/null & echo $!", &[]);
}

#[test]
#[cfg(unix)]
fn a_run_ends_at_its_limit_when_a_process_of_the_command_keeps_its_input() {
    // A background job of `sh` reads `/dev/null` unless it names its input.
    assert_a_run_ends_at_its_limit(
        "exec 3<&0; sleep 60 <&3 >/dev/null 2>&1 & echo $!",
        &vec![0; 1 << 20],
    );
}

#[test]
#[cfg(unix)]
fn a_run_ends_at_its_limit_when_the_command_reads_none_of_its_input() {
    let mut command = Command::new("sh");
    command.args(["-c", "exec sleep 60"]);
    let input = vec![0; 1 << 20];
    let limited = std::panic::AssertUnwindSafe(|| {
        run(&os::clock(), Span::SECOND, command, &input)
    });
    let panic = std::panic::catch_unwind(limited).expect_err("the run panics");
    assert_eq!(
        panic.downcast_ref::<String>().map(String::as_str),
        Some(
            "the command exits and closes its pipes: not within 1s. The last check \
             saw:\nstdout:\n\nstderr:\n"
        )
    );
}

#[test]
#[cfg(unix)]
fn a_run_gives_the_output_of_a_command_that_exits_before_it_reads_its_input() {
    let mut command = Command::new("sh");
    command.args(["-c", "echo done"]);
    let output = run(&os::clock(), PATIENCE, command, &vec![0; 1 << 20]);
    assert_eq!(
        (
            output.status.code(),
            crate::text(&output.stdout),
            crate::text(&output.stderr)
        ),
        (Some(0), "done\n", "")
    );
}

#[test]
fn a_wait_gives_the_first_ok() {
    let mut calls = 0;
    let value = wait(&os::clock(), PATIENCE, "the third call", || {
        calls += 1;
        if calls == 3 {
            Ok(calls)
        } else {
            Err(format!("call {calls}"))
        }
    });
    assert_eq!(value, 3);
}

#[test]
fn a_wait_past_its_limit_panics_with_the_last_state() {
    let panic = std::panic::catch_unwind(|| {
        wait(&os::clock(), Span::ZERO, "the node runs", || {
            Err::<(), _>("state: stopped".to_owned())
        });
    })
    .expect_err("the wait panics");
    assert_eq!(
        panic.downcast_ref::<String>().map(String::as_str),
        Some("the node runs: not within 0s. The last check saw:\nstate: stopped")
    );
}

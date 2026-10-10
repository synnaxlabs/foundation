//! A bench for process tests: one `foundation` node in a temporary directory. Each
//! method with a `todo!` waits on the issue it names.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
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

/// One `foundation` node in a temporary directory of its own. Drop kills the node and
/// removes the directory, except while the thread panics: a failed test keeps it.
#[derive(Debug)]
pub(crate) struct Rig {
    /// The working directory of each command. It holds `plant.hcl`.
    pub(crate) dir: PathBuf,
    /// The disk budget that [`Rig::new`] keeps: 8 MiB for each core.
    pub(crate) disk: u64,
    clock: Clock,
    node: Option<Running>,
}

impl Rig {
    /// Makes a temporary directory for the test on this thread, with a name that no
    /// directory has: a run that was killed keeps its directory, and a later run can
    /// get the same PID. Its data directory keeps a pool budget of 1 GiB and a disk
    /// budget of 8 MiB for each core ([`Rig::keep`]), so a first start does not take
    /// a quarter of the host's free disk, and each shard's ring fits.
    pub(crate) fn new() -> Self {
        Rig::with_cores(os::shards().expect("read the cores").cores().get())
    }

    /// [`Rig::new`] for a host of `cores` cores.
    fn with_cores(cores: usize) -> Self {
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
        let rig = Self {
            dir,
            disk: u64::try_from(cores).expect("invariant: a core count fits u64")
                * (8 << 20),
            clock: os::clock(),
            node: None,
        };
        rig.keep(1 << 30, rig.disk);
        rig
    }

    /// The path of the file `budget` of the data directory `foundation-data`.
    pub(crate) fn budget(&self) -> PathBuf {
        self.dir.join("foundation-data/data/budget")
    }

    /// Writes the file [`Rig::budget`] as a node keeps its budgets: the tag, `pool`
    /// and `disk` (little-endian), and the CRC32C of those 35 bytes.
    pub(crate) fn keep(&self, pool: u64, disk: u64) {
        let mut bytes = b"foundation/budget/1".to_vec();
        bytes.extend(pool.to_le_bytes());
        bytes.extend(disk.to_le_bytes());
        bytes.extend(crc32c::crc32c(&bytes).to_le_bytes());
        let path = self.budget();
        let data = path.parent().expect("invariant: a file of a directory");
        std::fs::create_dir_all(data).expect("make the data directory");
        std::fs::write(path, bytes).expect("write the budget file");
    }

    /// Writes `hcl` to `plant.hcl`.
    pub(crate) fn config(&self, hcl: &str) {
        std::fs::write(self.dir.join("plant.hcl"), hcl).expect("write plant.hcl");
    }

    /// Starts `foundation start --name edge`, with each listener of the node on port 0
    /// of loopback, and waits until it prints that the node runs.
    pub(crate) fn start(&mut self) {
        self.start_with(&["--name", "edge"]);
    }

    /// [`Rig::start`] with `args` in place of `--name edge`. Panics when a node runs,
    /// when the node exits before it prints a line, or when 90 s pass first.
    pub(crate) fn start_with(&mut self, args: &[&str]) {
        assert!(self.node.is_none(), "a node runs");
        let mut command = Command::new(env!("CARGO_BIN_EXE_foundation"));
        command.arg("start").args(args).current_dir(&self.dir);
        // The node reads no input.
        let (mut node, _) = Running::new(command);
        let started = poll(&self.clock, PATIENCE, || match node.ended() {
            Some(status) => Ok(Some(status)),
            None if node.stdout.text().contains('\n') => Ok(None),
            None => Err(node.seen()),
        });
        match started {
            Ok(None) => self.node = Some(node),
            Ok(Some(status)) => {
                panic!("the node exited with {status}:\n{}", node.seen())
            }
            Err(seen) => {
                node.process.end();
                late("the node prints that it runs", PATIENCE, &seen)
            }
        }
    }

    /// Stops the node of [`Rig::start`] with SIGTERM, and gives its output when it
    /// exits and closes its pipes. Panics when no node runs, or when 90 s pass first.
    #[cfg(unix)]
    pub(crate) fn stop(&mut self) -> Output {
        let mut node = self.node.take().expect("a node runs");
        node.process.term();
        let ended = poll(&self.clock, PATIENCE, || {
            node.ended().ok_or_else(|| node.seen())
        });
        match ended {
            Ok(status) => node.output(status),
            Err(seen) => {
                node.process.end();
                late("the node exits at SIGTERM", PATIENCE, &seen)
            }
        }
    }

    /// Starts `foundation` with `args` in [`Rig::dir`], with `stdout` as its standard
    /// output, no standard input, and its standard error on a pipe that
    /// [`Process::errors`] reads. Waits for nothing.
    #[cfg(target_os = "linux")]
    pub(crate) fn spawn(&self, args: &[&str], stdout: Stdio) -> Process {
        let mut command = Command::new(env!("CARGO_BIN_EXE_foundation"));
        command
            .args(args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::piped());
        Process(command.spawn().expect("start the command"))
    }

    /// Runs `foundation` with `args` in [`Rig::dir`], with `input` on its standard
    /// input, and gives its output when it exits and closes its pipes. When 90 s pass
    /// first, kills it and panics with the output so far.
    pub(crate) fn run(&self, args: &[&str], input: &[u8]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_foundation"));
        command.args(args).current_dir(&self.dir);
        run(&self.clock, PATIENCE, command, input)
    }

    /// Starts `foundation` with `args` in [`Rig::dir`], for a test that writes its
    /// standard input while it runs.
    pub(crate) fn talk(&self, args: &[&str]) -> Talk {
        let mut command = Command::new(env!("CARGO_BIN_EXE_foundation"));
        command.args(args).current_dir(&self.dir);
        let (running, stdin) = Running::new(command);
        Talk {
            running,
            stdin,
            clock: self.clock.clone(),
        }
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
pub(crate) fn wait<T>(
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
    let (mut running, mut stdin) = Running::new(command);
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
    running.input = Some(input);
    end(clock, limit, running)
}

/// Gives the output of `running` when it exits and closes its pipes. When `limit`
/// passes first, kills it and panics with the output so far.
fn end(clock: &Clock, limit: Span, mut running: Running) -> Output {
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

/// A command that a test writes to while it runs: [`Rig::talk`].
#[derive(Debug)]
pub(crate) struct Talk {
    running: Running,
    stdin: ChildStdin,
    clock: Clock,
}

impl Talk {
    /// Writes `line` to the standard input of the command, and waits until all of
    /// its standard output so far is `output`. When 90 s pass first, panics with the
    /// output so far.
    pub(crate) fn ask(&mut self, line: &str, output: &str) {
        self.stdin
            .write_all(line.as_bytes())
            .expect("write the input");
        wait(&self.clock, PATIENCE, "the command answers", || {
            let seen = self.running.stdout.text();
            if seen == output {
                Ok(())
            } else {
                Err(self.running.seen())
            }
        });
    }

    /// Closes the standard input of the command, and gives its output when it exits
    /// and closes its pipes. When 90 s pass first, kills it and panics with the output
    /// so far.
    pub(crate) fn close(self) -> Output {
        drop(self.stdin);
        end(&self.clock, PATIENCE, self.running)
    }
}

/// A command that runs, with a thread for each of its output pipes and for the
/// input that [`run`] writes.
#[derive(Debug)]
struct Running {
    process: Process,
    input: Option<JoinHandle<()>>,
    stdout: Capture,
    stderr: Capture,
}

impl Running {
    /// Starts `command`, and gives it with its standard input.
    fn new(mut command: Command) -> (Self, ChildStdin) {
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the command");
        let mut process = Process(child);
        let stdin = process.0.stdin.take().expect("piped");
        let running = Self {
            input: None,
            stdout: Capture::new(process.0.stdout.take().expect("piped")),
            stderr: Capture::new(process.0.stderr.take().expect("piped")),
            process,
        };
        (running, stdin)
    }

    /// The exit status, once the command has exited and each of its pipes has closed.
    fn ended(&mut self) -> Option<ExitStatus> {
        let status = self.process.exited()?;
        let written = self.input.as_ref().is_none_or(JoinHandle::is_finished);
        let closed = written && self.stdout.ended() && self.stderr.ended();
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
        if let Some(input) = self.input {
            input.join().expect("write the input");
        }
        Output {
            status,
            stdout: self.stdout.end(),
            stderr: self.stderr.end(),
        }
    }
}

/// The process of a command. Drop kills it, so a test that panics leaves no
/// `foundation` process. A process that it starts lives on: `foundation` starts none.
#[derive(Debug)]
pub(crate) struct Process(Child);

impl Process {
    /// The PID of the command.
    #[cfg(target_os = "linux")]
    pub(crate) fn pid(&self) -> u32 {
        self.0.id()
    }

    /// The standard error of a command that [`Rig::spawn`] started and that exited.
    #[cfg(target_os = "linux")]
    pub(crate) fn errors(&mut self) -> String {
        let mut errors = String::new();
        let mut pipe = self.0.stderr.take().expect("spawn pipes standard error");
        std::io::Read::read_to_string(&mut pipe, &mut errors).expect("read the errors");
        errors
    }

    /// Sends SIGTERM to the command. Panics when `kill` fails.
    #[cfg(unix)]
    pub(crate) fn term(&self) {
        let pid = self.0.id().to_string();
        let sent = Command::new("kill").arg(&pid).status().expect("run kill");
        assert!(sent.success(), "send SIGTERM to {pid}");
    }

    /// The exit status of the command, or `None` while it runs.
    pub(crate) fn exited(&mut self) -> Option<ExitStatus> {
        self.0.try_wait().expect("check the command")
    }

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
        // `kill` gives `Ok` for a command that exited. No wait: while the thread
        // panics, a wait can block, and `run` waits for its command itself.
        match self.0.kill() {
            Err(error) if std::thread::panicking() => {
                let line = format!("kill the command: {error}");
                #[expect(
                    clippy::print_stderr,
                    reason = "only `eprintln!` writes to the output that the harness \
                              captures for the report of the test"
                )]
                match std::panic::catch_unwind(move || eprintln!("{line}")) {
                    Ok(()) => {}
                    // The test fails already, and a panic out of this drop aborts
                    // the test binary.
                    Err(_closed) => {}
                }
            }
            kill => kill.expect("kill the command"),
        }
    }
}

/// One output of a command, which a thread reads as the command writes it.
#[derive(Debug)]
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
        // test binary. The drop of the node kills it.
        if !std::thread::panicking() {
            if let Some(mut node) = self.node.take() {
                node.process.end();
            }
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
#[cfg_attr(not(target_os = "linux"), ignore = "needs /proc")]
fn a_test_that_panics_kills_its_command() {
    let mut command = Command::new("sh");
    command.args(["-c", "exec sleep 60"]);
    let (running, _) = Running::new(command);
    let pid = running.process.0.id();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _running = running;
        panic!("the test fails");
    }))
    .expect_err("the test panics");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"the test fails"));
    // Killed and not waited for, the command stays a zombie.
    wait(
        &os::clock(),
        Span::from_nanos(10_000_000_000),
        "the command ends",
        || {
            let stat =
                std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("stat");
            let state = stat.rsplit(") ").next().expect("a state").chars().next();
            if state == Some('Z') {
                Ok(())
            } else {
                Err(stat)
            }
        },
    );
}

/// Set to `drop` or `panic` in the test binary that
/// [`drop_a_command_that_the_kernel_reaped`] runs.
const REAPED: &str = "RIG_REAPED";

/// Drops a command that the kernel reaped, so its kill fails, and panics after the
/// drop when [`REAPED`] is `panic`. Does nothing when [`REAPED`] is not set.
#[test]
fn a_command_that_the_kernel_reaped_drops() {
    #[expect(
        clippy::disallowed_methods,
        reason = "the parent test sets it for the child"
    )]
    let Some(reaped) = std::env::var_os(REAPED) else {
        return;
    };
    let mut command = Command::new("sh");
    command.args(["-c", "exit 0"]);
    let (running, _) = Running::new(command);
    let pid = running.process.0.id();
    wait(
        &os::clock(),
        Span::from_nanos(10_000_000_000),
        "the kernel reaps the command",
        || {
            let exists = std::fs::exists(format!("/proc/{pid}")).expect("check /proc");
            if exists {
                Err(format!("{pid} runs"))
            } else {
                Ok(())
            }
        },
    );
    let _running = running;
    assert_ne!(reaped, "panic", "the test fails");
}

/// The test binary that runs [`a_command_that_the_kernel_reaped_drops`] with
/// [`REAPED`] set to `reaped`. It ignores `SIGCHLD`, so the kernel reaps each command
/// it starts.
fn reaped(reaped: &str) -> Command {
    let binary = std::env::current_exe().expect("the test binary");
    let mut command = Command::new("perl");
    command
        .args(["-e", "$SIG{CHLD} = 'IGNORE'; exec @ARGV or die"])
        .arg(binary)
        .args(["--exact", "rig::a_command_that_the_kernel_reaped_drops"])
        .env(REAPED, reaped)
        // With it, the harness of the child writes the panic to stderr, not stdout.
        .env_remove("RUST_TEST_NOCAPTURE");
    command
}

/// Runs [`reaped`] and gives the report of the test harness.
fn drop_a_command_that_the_kernel_reaped(reaped: &str) -> String {
    let output = self::reaped(reaped).output().expect("run the test binary");
    assert_eq!(
        output.status.code(),
        Some(101),
        "the test fails with no abort"
    );
    String::from_utf8(output.stdout).expect("UTF-8")
}

#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs perl and /proc")]
fn a_failed_kill_panics() {
    let report = drop_a_command_that_the_kernel_reaped("drop");
    assert!(
        report.contains(concat!(
            "kill the command: Os { code: 3, kind: Uncategorized, ",
            "message: \"No such process\" }\n",
        )),
        "{report}"
    );
}

#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs perl and /proc")]
fn a_failed_kill_while_the_test_panics_with_a_closed_stderr_does_not_abort() {
    let (reader, writer) = std::io::pipe().expect("a pipe");
    drop(reader);
    let output = reaped("panic")
        .arg("--nocapture")
        .stderr(writer)
        .output()
        .expect("run the test binary");
    let report = String::from_utf8(output.stdout).expect("UTF-8");
    assert_eq!(output.status.code(), Some(101), "{report}");
}

#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs perl and /proc")]
fn a_failed_kill_while_the_test_panics_reports_its_error() {
    let report = drop_a_command_that_the_kernel_reaped("panic");
    assert!(report.contains("the test fails\n"), "{report}");
    assert!(
        report.contains("kill the command: No such process (os error 3)\n"),
        "{report}"
    );
}

/// A developer can set `RUST_TEST_NOCAPTURE` for the whole run.
#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs perl and /proc")]
fn a_failed_kill_panics_in_a_run_that_does_not_capture() {
    let output = Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", "rig::a_failed_kill_panics"])
        .env("RUST_TEST_NOCAPTURE", "1")
        .output()
        .expect("run the test binary");
    let report = String::from_utf8(output.stdout).expect("UTF-8");
    assert!(report.contains("test result: ok. 1 passed"), "{report}");
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

#[test]
fn a_node_that_exits_before_it_runs_panics_with_its_output() {
    let mut rig = Rig::new();
    let start = std::panic::AssertUnwindSafe(|| rig.start_with(&[]));
    let panic = std::panic::catch_unwind(start).expect_err("the start panics");
    assert_eq!(
        panic.downcast_ref::<String>().map(String::as_str),
        Some(
            "the node exited with exit status: 1:\nstdout:\n\nstderr:\n\
             error[node.unnamed]: the data directory foundation-data holds no node\n\
             fix: Give the new node a name with `--name`\n"
        )
    );
}

#[test]
#[cfg(unix)]
fn a_rig_ends_its_node_before_it_removes_the_directory() {
    let mut rig = Rig::new();
    rig.start();
    let pid = rig
        .node
        .as_ref()
        .expect("a node")
        .process
        .0
        .id()
        .to_string();
    let dir = rig.dir.clone();
    drop(rig);
    assert!(!dir.exists(), "{} is still there", dir.display());
    let alive = Command::new("kill")
        .args(["-0", &pid])
        .output()
        .expect("kill -0");
    assert_eq!(alive.status.code(), Some(1), "{pid} still runs");
}

/// The budget that `a_disk_budget_of_8_mib_for_each_core_starts_a_host_of_any_size`
/// in `node` starts. A fixed budget of 256 MiB holds no ring on each of 64 shards.
#[test]
fn a_rig_keeps_8_mib_of_disk_for_each_core() {
    for cores in [1, 64, 1024] {
        let rig = Rig::with_cores(cores);
        #[expect(
            clippy::disallowed_methods,
            reason = "the test reads what the rig wrote to the disk"
        )]
        let kept = std::fs::read(rig.budget()).expect("read the budget file");
        let disk = u64::from_le_bytes(kept[27..35].try_into().expect("8 bytes"));
        assert_eq!(
            disk,
            u64::try_from(cores).unwrap() * (8 << 20),
            "{cores} cores"
        );
    }
}

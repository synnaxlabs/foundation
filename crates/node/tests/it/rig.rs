//! A bench for process tests: one `foundation` node in a temporary directory. Each
//! method with a `todo!` waits on the issue it names.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use env::clock::Clock;
use types::time::Span;

use crate::status::Connector;

/// How long [`Rig::wait`] waits: longer than the 60 s cap of a restart backoff.
const PATIENCE: Span = Span::from_nanos(90_000_000_000);

/// The time between two checks of a wait.
const POLL: Duration = Duration::from_millis(100);

/// One `foundation` node in a temporary directory of its own. Drop removes the
/// directory.
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

    /// Runs `foundation` with `args` in [`Rig::dir`], with no input, and gives its
    /// output when it exits.
    pub(crate) fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_foundation"))
            .args(args)
            .current_dir(&self.dir)
            .output()
            .expect("run foundation")
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
    mut check: impl FnMut() -> Result<T, String>,
) -> T {
    let deadline = clock.now() + limit;
    loop {
        let seen = match check() {
            Ok(value) => return value,
            Err(seen) => seen,
        };
        assert!(
            clock.now() < deadline,
            "{what}: not within {limit}. The last check saw:\n{seen}"
        );
        #[expect(
            clippy::disallowed_methods,
            reason = "a process test waits on another process in real time"
        )]
        std::thread::sleep(POLL);
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let removed = std::fs::remove_dir_all(&self.dir);
        // A second panic aborts the test binary.
        if !std::thread::panicking() {
            removed.expect("remove the directory");
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
fn a_rig_runs_foundation_and_gives_its_output() {
    let rig = Rig::new();
    let output = rig.run(&["version"]);
    assert_eq!(
        (
            output.status.code(),
            crate::text(&output.stdout),
            crate::text(&output.stderr)
        ),
        (
            Some(0),
            format!("{}\n", env!("CARGO_PKG_VERSION")).as_str(),
            ""
        )
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

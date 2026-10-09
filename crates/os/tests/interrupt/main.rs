//! `os::interrupt` holds the first SIGINT or SIGTERM and completes its future at it,
//! and a second one ends the process. This binary has no test harness: the threads
//! of a harness start before the hold, so a signal would end the process. With the
//! argument `child`, it is the process that the test signals, and with `blocked`, one
//! that blocks SIGTERM first.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::pin::pin;
use std::process::{Command, Stdio};
use std::time::Duration;

use rustix::process::{Pid, Signal, getpid, kill_process};
use tokio::time::{sleep, timeout};

/// How long the future must wait with no signal.
const QUIET: Duration = Duration::from_millis(200);
/// The bound of each wait for a signal. A child ends in twice this, so no wait of
/// the test on it hangs.
const BOUND: Duration = Duration::from_secs(10);

fn main() {
    match std::env::args_os().nth(1) {
        Some(arg) if arg == "child" => {
            block(&[]);
            child();
        }
        Some(arg) if arg == "blocked" => {
            block(&[libc::SIGTERM]);
            child();
        }
        #[cfg(target_os = "linux")]
        Some(arg) if arg == "early" => {
            block(&[]);
            early();
        }
        _ => {
            a_blocked_sigterm_stays_blocked_after_the_first_signal();
            a_second_signal_ends_the_process();
            the_future_completes_at_the_first_signal();
            #[cfg(target_os = "linux")]
            a_signal_before_the_first_poll_completes_the_future();
        }
    }
}

/// Holds the signals, then waits for one and the end of the process.
fn child() {
    let interrupt = os::interrupt().expect("the signal thread starts");
    let mut output = std::io::stdout();
    writeln!(output, "held").expect("a write to the test");
    runtime().block_on(async {
        timeout(BOUND, interrupt)
            .await
            .unwrap_or_else(|_| panic!("no signal came in {BOUND:?}"));
        writeln!(output, "fired").expect("a write to the test");
        sleep(BOUND).await;
    });
}

/// Sets the mask of the calling thread to `signals`. A child inherits the mask of the
/// test, which holds the signals once a test has called `interrupt`.
#[expect(unsafe_code, reason = "a signal mask is an OS call")]
fn block(signals: &[libc::c_int]) {
    let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `set` is one sigset, which the call initializes.
    let rc = unsafe { libc::sigemptyset(set.as_mut_ptr()) };
    assert_eq!(rc, 0, "sigemptyset");
    for &signal in signals {
        // SAFETY: `set` is an initialized sigset, and `signal` a valid signal.
        let rc = unsafe { libc::sigaddset(set.as_mut_ptr(), signal) };
        assert_eq!(rc, 0, "sigaddset");
    }
    // SAFETY: `set` is an initialized sigset, and the old mask is not asked for.
    let rc = unsafe {
        libc::pthread_sigmask(libc::SIG_SETMASK, set.as_ptr(), std::ptr::null_mut())
    };
    assert_eq!(rc, 0, "pthread_sigmask");
}

/// With no hold, a SIGTERM that the caller blocks does not end the process. After
/// the first SIGINT, the hold must give that back, as it does when its thread
/// cannot start.
fn a_blocked_sigterm_stays_blocked_after_the_first_signal() {
    let mut child = Command::new(std::env::current_exe().expect("the test binary"))
        .arg("blocked")
        .stdout(Stdio::piped())
        .spawn()
        .expect("the child starts");
    let pid = Pid::from_child(&child);
    let mut lines = BufReader::new(child.stdout.take().expect("a pipe")).lines();
    let mut line = || lines.next().expect("a line of the child").expect("a read");
    assert_eq!(line(), "held", "the child holds the signals");
    kill_process(pid, Signal::INT).expect("the test signals the child");
    assert_eq!(line(), "fired", "the first SIGINT completes the future");
    kill_process(pid, Signal::TERM).expect("the test signals the child");
    runtime().block_on(async { sleep(QUIET).await });
    let ended = child.try_wait().expect("a wait");
    child.kill().unwrap_or(());
    child.wait().expect("the child ends");
    assert_eq!(
        ended, None,
        "a SIGTERM that the caller blocked ended the child"
    );
}

fn a_second_signal_ends_the_process() {
    let mut child = Command::new(std::env::current_exe().expect("the test binary"))
        .arg("child")
        .stdout(Stdio::piped())
        .spawn()
        .expect("the child starts");
    let pid = Pid::from_child(&child);
    let mut lines = BufReader::new(child.stdout.take().expect("a pipe")).lines();
    let mut line = || lines.next().expect("a line of the child").expect("a read");
    assert_eq!(line(), "held", "the child holds the signals");
    kill_process(pid, Signal::TERM).expect("the test signals the child");
    assert_eq!(line(), "fired", "the first SIGTERM completes the future");
    kill_process(pid, Signal::TERM).expect("the test signals the child");
    let status = child.wait().expect("the child ends");
    assert_eq!(status.signal(), Some(libc::SIGTERM), "{status}");
}

fn the_future_completes_at_the_first_signal() {
    let interrupt = os::interrupt().expect("the signal thread starts");
    runtime().block_on(async {
        let mut interrupt = pin!(interrupt);
        assert!(
            timeout(QUIET, interrupt.as_mut()).await.is_err(),
            "the future completed with no signal"
        );
        kill_process(getpid(), Signal::INT).expect("the process signals itself");
        timeout(BOUND, interrupt)
            .await
            .unwrap_or_else(|_| panic!("no signal came in {BOUND:?}"));
    });
}

/// Holds the signals and signals itself, then, once the signal thread has taken the
/// signal, polls the future once and writes whether it completed.
#[cfg(target_os = "linux")]
fn early() {
    use std::task::{Context, Waker};

    let interrupt = os::interrupt().expect("the signal thread starts");
    kill_process(getpid(), Signal::INT).expect("the process signals itself");
    // The signal thread takes SIGINT again only after it fires the future.
    let taken = async {
        while serving_blocks_sigint() {
            sleep(Duration::from_millis(1)).await;
        }
    };
    runtime()
        .block_on(async { timeout(BOUND, taken).await })
        .unwrap_or_else(|_| panic!("no signal came in {BOUND:?}"));
    let polled = pin!(interrupt).poll(&mut Context::from_waker(Waker::noop()));
    let mut output = std::io::stdout();
    writeln!(output, "{}", polled.is_ready()).expect("a write to the test");
}

/// Whether the thread `signal` of this process has not named itself yet or blocks
/// SIGINT.
#[cfg(target_os = "linux")]
fn serving_blocks_sigint() -> bool {
    for task in std::fs::read_dir("/proc/self/task").expect("the tasks list") {
        let task = task.expect("a task").path();
        let name = std::fs::read_to_string(task.join("comm")).expect("a name");
        if name.trim_end() != "signal" {
            continue;
        }
        let status = std::fs::read_to_string(task.join("status")).expect("a status");
        let mask = status
            .lines()
            .find_map(|line| line.strip_prefix("SigBlk:"))
            .expect("a signal mask");
        let mask = u64::from_str_radix(mask.trim(), 16).expect("a hex mask");
        return mask & (1 << (libc::SIGINT - 1)) != 0;
    }
    true
}

#[cfg(target_os = "linux")]
fn a_signal_before_the_first_poll_completes_the_future() {
    let output = Command::new(std::env::current_exe().expect("the test binary"))
        .arg("early")
        .output()
        .expect("the child runs");
    assert!(output.status.success(), "{}", output.status);
    let polled = String::from_utf8(output.stdout).expect("text");
    assert_eq!(polled, "true\n", "the first poll completes");
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a runtime")
}

use std::cell::Cell;
use std::mem::ManuallyDrop;

use super::*;

/// Sets its flag when it drops.
struct Flag(Rc<Cell<bool>>);

impl Drop for Flag {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

fn flagged() -> (Rc<Cell<bool>>, Waker) {
    let dropped = Rc::new(Cell::new(false));
    (Rc::clone(&dropped), holding(Flag(dropped)))
}

#[test]
fn the_last_drop_of_a_waker_or_a_clone_drops_its_value() {
    let (dropped, waker) = flagged();
    let clone = waker.clone();
    drop(waker);
    assert!(!dropped.get());
    drop(clone);
    assert!(dropped.get());
}

#[test]
fn a_wake_by_value_drops_the_waker() {
    let (dropped, waker) = flagged();
    waker.wake();
    assert!(dropped.get());
}

#[test]
fn a_wake_by_reference_keeps_the_value() {
    let (dropped, waker) = flagged();
    waker.wake_by_ref();
    assert!(!dropped.get());
    drop(waker);
    assert!(dropped.get());
}

/// Runs `child` in a copy of this test in a child process, as `cargo test` runs it,
/// and asserts that the child aborts with the message of [`check`]. Returns the stdout
/// and the stderr of the child.
#[cfg(unix)]
fn assert_aborts(child: impl FnOnce()) -> String {
    use std::os::unix::process::ExitStatusExt;
    use std::process;
    const CHILD: &str = "WAKER_TEST_CHILD";
    const SIGABRT: i32 = 6;
    #[expect(
        clippy::disallowed_methods,
        reason = "the parent sets it for the child"
    )]
    if std::env::var_os(CHILD).is_some() {
        child();
        return String::new();
    }
    let thread = thread::current();
    let test = thread.name().expect("invariant: libtest names the thread");
    let output = process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let printed = String::from_utf8(output.stdout).unwrap()
        + &String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.signal(), Some(SIGABRT), "{printed}");
    let (made, ran) = printed
        .lines()
        .find_map(|line| line.strip_prefix("a waker of `waker::holding` made on "))
        .and_then(|threads| threads.split_once(" ran on "))
        .unwrap_or_else(|| panic!("no abort message: {printed}"));
    assert!(
        made.starts_with("ThreadId(") && ran.starts_with("ThreadId("),
        "{printed}"
    );
    assert_ne!(made, ran, "{printed}");
    printed
}

/// Runs `call` with a waker of [`holding`] on another thread.
#[cfg(unix)]
fn on_another_thread(call: fn(Waker)) {
    let (_, waker) = flagged();
    let clone = waker.clone();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test moves a waker to another thread, as safe code can"
    )]
    thread::spawn(move || call(clone))
        .join()
        .expect("the process aborts first");
}

#[cfg(unix)]
#[cfg_attr(miri, ignore = "Miri cannot spawn a process")]
#[test]
fn a_clone_on_another_thread_aborts() {
    assert_aborts(|| {
        on_another_thread(|waker| {
            let waker = ManuallyDrop::new(waker);
            let _clone = ManuallyDrop::new(Waker::clone(&waker));
        });
    });
}

#[cfg(unix)]
#[cfg_attr(miri, ignore = "Miri cannot spawn a process")]
#[test]
fn a_wake_on_another_thread_aborts() {
    assert_aborts(|| on_another_thread(Waker::wake));
}

#[cfg(unix)]
#[cfg_attr(miri, ignore = "Miri cannot spawn a process")]
#[test]
fn a_wake_by_reference_on_another_thread_aborts() {
    assert_aborts(|| {
        on_another_thread(|waker| ManuallyDrop::new(waker).wake_by_ref());
    });
}

#[cfg(unix)]
#[cfg_attr(miri, ignore = "Miri cannot spawn a process")]
#[test]
fn a_drop_on_another_thread_aborts() {
    assert_aborts(|| on_another_thread(drop));
}

/// Printed by the drop of [`Loud`].
#[cfg(unix)]
const DROPPED: &str = "the value of the waker dropped";

/// Prints [`DROPPED`] when it drops.
#[cfg(unix)]
struct Loud;

#[cfg(unix)]
impl Drop for Loud {
    #[expect(clippy::print_stderr, reason = "the parent reads it from the child")]
    fn drop(&mut self) {
        eprintln!("{DROPPED}");
    }
}

#[cfg(unix)]
#[cfg_attr(miri, ignore = "Miri cannot spawn a process")]
#[test]
fn a_last_drop_on_another_thread_aborts_before_the_value_drops() {
    let printed = assert_aborts(|| {
        let waker = holding(Loud);
        #[expect(
            clippy::disallowed_methods,
            reason = "the test moves a waker to another thread, as safe code can"
        )]
        thread::spawn(move || drop(waker))
            .join()
            .expect("the process aborts first");
    });
    assert!(!printed.contains(DROPPED), "{printed}");
}

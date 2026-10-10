use std::cell::Cell;

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

/// Runs `child` in a copy of this test in a child process, and asserts that the child
/// aborts with the message of [`check`].
#[cfg(unix)]
fn assert_aborts(child: impl FnOnce()) {
    use std::os::unix::process::ExitStatusExt;
    const CHILD: &str = "WAKER_TEST_CHILD";
    const SIGABRT: i32 = 6;
    #[expect(
        clippy::disallowed_methods,
        reason = "the parent sets it for the child"
    )]
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }
    let thread = thread::current();
    let test = thread.name().expect("invariant: libtest names the thread");
    let output = process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "--nocapture", test])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.signal(), Some(SIGABRT), "{stderr}");
    assert!(
        stderr.contains(
            "a waker of `waker::holding` ran on a thread that did not make it"
        ),
        "{stderr}"
    );
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
    assert_aborts(|| on_another_thread(|waker| drop(waker.clone())));
}

#[cfg(unix)]
#[cfg_attr(miri, ignore = "Miri cannot spawn a process")]
#[test]
fn a_wake_by_reference_on_another_thread_aborts() {
    assert_aborts(|| on_another_thread(|waker| waker.wake_by_ref()));
}

#[cfg(unix)]
#[cfg_attr(miri, ignore = "Miri cannot spawn a process")]
#[test]
fn a_drop_on_another_thread_aborts() {
    assert_aborts(|| on_another_thread(drop));
}

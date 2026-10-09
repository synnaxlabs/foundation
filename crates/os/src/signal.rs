//! The signal mask and `sigwait` calls of [`interrupt`](crate::interrupt).

use std::mem::MaybeUninit;
use std::{io, thread};

/// The set of SIGINT and SIGTERM.
pub(crate) fn set() -> libc::sigset_t {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `set` is one sigset, which the call initializes.
    let rc = unsafe { libc::sigemptyset(set.as_mut_ptr()) };
    assert_eq!(rc, 0, "invariant: sigemptyset does not fail");
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: `set` is an initialized sigset, and `signal` is a valid signal.
        let rc = unsafe { libc::sigaddset(set.as_mut_ptr(), signal) };
        assert_eq!(rc, 0, "invariant: SIGINT and SIGTERM are valid signals");
    }
    // SAFETY: `sigemptyset` initialized it.
    unsafe { set.assume_init() }
}

/// Blocks the signals of `set` on the calling thread and each thread it starts later,
/// and returns the mask it had before.
pub(crate) fn block(set: &libc::sigset_t) -> libc::sigset_t {
    mask(libc::SIG_BLOCK, set)
}

/// Sets the mask of the calling thread to `old`.
pub(crate) fn restore(old: &libc::sigset_t) {
    mask(libc::SIG_SETMASK, old);
}

/// Starts thread `signal`, which calls `fire` at the first signal of `set`, which each
/// thread blocks, then takes them as with no block. The thread has no handle: it lives
/// until the process ends, to take the second signal.
#[expect(
    clippy::disallowed_methods,
    reason = "an env Handle joins its thread, and this one never ends"
)]
pub(crate) fn start(
    set: libc::sigset_t,
    fire: impl FnOnce() + Send + 'static,
) -> io::Result<()> {
    thread::Builder::new()
        .name("signal".to_owned())
        .spawn(move || serve(&set, fire))
        .map(drop)
}

fn serve(set: &libc::sigset_t, fire: impl FnOnce()) -> ! {
    let mut signal = 0;
    // SAFETY: `set` is an initialized sigset, and `signal` an int that the call writes.
    let rc = unsafe { libc::sigwait(set, &raw mut signal) };
    assert_eq!(rc, 0, "invariant: sigwait of a valid set does not fail");
    fire();
    mask(libc::SIG_UNBLOCK, set);
    // A signal goes only to a thread that takes it, so this one stays.
    loop {
        // SAFETY: `pause` takes no arguments.
        unsafe { libc::pause() };
    }
}

fn mask(how: libc::c_int, set: &libc::sigset_t) -> libc::sigset_t {
    let mut old = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `set` is an initialized sigset, and `old` one sigset that the call fills.
    let rc = unsafe { libc::pthread_sigmask(how, set, old.as_mut_ptr()) };
    assert_eq!(rc, 0, "invariant: a valid set is a valid argument");
    // SAFETY: the call filled it.
    unsafe { old.assume_init() }
}

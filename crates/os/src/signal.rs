//! The signal mask and `sigwait` calls of [`interrupt`](crate::interrupt).

use std::mem::MaybeUninit;

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

/// Blocks the signals of `set` on the calling thread and each thread it starts later.
pub(crate) fn block(set: &libc::sigset_t) {
    mask(libc::SIG_BLOCK, set);
}

/// Takes the signals of `set` on the calling thread again.
pub(crate) fn unblock(set: &libc::sigset_t) {
    mask(libc::SIG_UNBLOCK, set);
}

/// Calls `fire` at the first signal of `set`, which the calling thread blocks, then
/// takes them as with no block.
pub(crate) fn serve(set: &libc::sigset_t, fire: impl FnOnce()) -> ! {
    let mut signal = 0;
    // SAFETY: `set` is an initialized sigset, and `signal` an int that the call writes.
    let rc = unsafe { libc::sigwait(set, &raw mut signal) };
    assert_eq!(rc, 0, "invariant: sigwait of a valid set does not fail");
    fire();
    unblock(set);
    // A signal goes only to a thread that takes it, so this one stays.
    loop {
        // SAFETY: `pause` takes no arguments.
        unsafe { libc::pause() };
    }
}

fn mask(how: libc::c_int, set: &libc::sigset_t) {
    // SAFETY: `set` is an initialized sigset, and the old mask is not asked for.
    let rc = unsafe { libc::pthread_sigmask(how, set, std::ptr::null_mut()) };
    assert_eq!(rc, 0, "invariant: a valid set is a valid argument");
}

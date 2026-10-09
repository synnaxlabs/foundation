//! An `os::interrupt` whose thread cannot start. The filter acts on the test thread
//! and each thread it starts, so it runs in a test binary of its own, with this one
//! test only.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]
#![expect(unsafe_code, reason = "a read of the signal mask is an OS call")]

#[path = "common/seccomp.rs"]
mod seccomp;

use std::mem::MaybeUninit;

/// The signals of `signals` that the calling thread blocks.
fn blocked(signals: [libc::c_int; 2]) -> Vec<libc::c_int> {
    let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: a null set changes nothing, and `mask` is one sigset for the call to
    // fill.
    let rc = unsafe {
        libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), mask.as_mut_ptr())
    };
    assert_eq!(rc, 0, "{}", std::io::Error::from_raw_os_error(rc));
    // SAFETY: the call filled it.
    let mask = unsafe { mask.assume_init() };
    // SAFETY: `mask` is an initialized sigset, and each signal is valid.
    let held = |signal| unsafe { libc::sigismember(&raw const mask, signal) } == 1;
    signals.into_iter().filter(|&signal| held(signal)).collect()
}

/// A hold that fails leaves the mask as it was: a signal that the caller did not block
/// still ends the process, and one that it blocked stays blocked.
#[test]
fn an_interrupt_whose_thread_cannot_start_keeps_the_mask() {
    let signals = [libc::SIGINT, libc::SIGTERM];
    let mut term = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `term` is one sigset, which the call initializes.
    let rc = unsafe { libc::sigemptyset(term.as_mut_ptr()) };
    assert_eq!(rc, 0);
    // SAFETY: `term` is an initialized sigset, and SIGTERM a valid signal.
    let rc = unsafe { libc::sigaddset(term.as_mut_ptr(), libc::SIGTERM) };
    assert_eq!(rc, 0);
    // SAFETY: `sigemptyset` initialized it.
    let term = unsafe { term.assume_init() };
    // SAFETY: `term` is an initialized sigset, and the old mask is not asked for.
    let rc = unsafe {
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const term, std::ptr::null_mut())
    };
    assert_eq!(rc, 0, "{}", std::io::Error::from_raw_os_error(rc));
    assert_eq!(
        blocked(signals),
        [libc::SIGTERM],
        "the caller blocks SIGTERM"
    );
    // glibc calls `clone3`, or `clone` where the kernel lacks it.
    seccomp::answer_calls(&[libc::SYS_clone3, libc::SYS_clone], |_, _| {
        Some(libc::EAGAIN)
    });
    let found = os::interrupt().map(drop).map_err(|e| e.to_string());
    let refused = "cannot start thread signal: Resource temporarily unavailable (os \
                   error 11)";
    assert_eq!(found, Err(refused.to_owned()));
    assert_eq!(blocked(signals), [libc::SIGTERM]);
}

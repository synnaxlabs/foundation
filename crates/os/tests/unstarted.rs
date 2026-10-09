//! An `os::interrupt` whose thread cannot start. The filter acts on the test thread
//! and each thread it starts, so it runs in a test binary of its own, with this one
//! test only.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]
#![expect(unsafe_code, reason = "a read of the signal mask is an OS call")]

#[path = "common/threads.rs"]
mod threads;

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

/// A hold that fails leaves the signals as they were, so they still end the process.
#[test]
fn an_interrupt_whose_thread_cannot_start_blocks_no_signal() {
    let signals = [libc::SIGINT, libc::SIGTERM];
    assert_eq!(blocked(signals), [], "the test starts with no block");
    threads::refuse();
    let found = os::interrupt().map(drop).map_err(|e| e.to_string());
    let refused = "cannot start thread signal: Resource temporarily unavailable (os \
                   error 11)";
    assert_eq!(found, Err(refused.to_owned()));
    assert_eq!(blocked(signals), []);
}

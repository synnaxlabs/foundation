//! The signal mask of the calling thread.

use std::mem::MaybeUninit;

/// Changes the mask of the calling thread by `how` (`SIG_BLOCK` or `SIG_SETMASK`) with
/// the set of `signals`.
#[expect(unsafe_code, reason = "a signal mask is an OS call")]
pub(crate) fn block(how: libc::c_int, signals: &[libc::c_int]) {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `set` is one sigset, which the call initializes.
    let rc = unsafe { libc::sigemptyset(set.as_mut_ptr()) };
    assert_eq!(rc, 0, "sigemptyset");
    for &signal in signals {
        // SAFETY: `set` is an initialized sigset, and `signal` a valid signal.
        let rc = unsafe { libc::sigaddset(set.as_mut_ptr(), signal) };
        assert_eq!(rc, 0, "sigaddset");
    }
    // SAFETY: `set` is an initialized sigset, and the old mask is not asked for.
    let rc = unsafe { libc::pthread_sigmask(how, set.as_ptr(), std::ptr::null_mut()) };
    assert_eq!(rc, 0, "{}", std::io::Error::from_raw_os_error(rc));
}

/// The signals of `signals` that the calling thread blocks.
#[expect(unsafe_code, reason = "a signal mask is an OS call")]
pub(crate) fn blocked(signals: &[libc::c_int]) -> Vec<libc::c_int> {
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
    let held = |&signal: &libc::c_int| unsafe { libc::sigismember(&raw const mask, signal) } == 1;
    signals.iter().copied().filter(held).collect()
}

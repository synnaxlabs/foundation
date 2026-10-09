//! The signal mask and `sigwait` calls of [`interrupt`](crate::interrupt).

use std::mem::MaybeUninit;
use std::thread;

use env::thread::Error;

/// Holds SIGINT and SIGTERM on the calling thread and each thread it starts later, and
/// starts thread `signal`, which calls `fire` at the first, then takes them as with no
/// hold. The thread has no handle: it lives until the process ends, to take the
/// second signal. When the thread cannot start, the mask stays as it was.
#[expect(
    clippy::disallowed_methods,
    reason = "an env Handle joins its thread, and this one never ends"
)]
pub(crate) fn hold(fire: impl FnOnce() + Send + 'static) -> Result<(), Error> {
    let set = set();
    // Before the start, so that the thread inherits the block.
    let old = mask(libc::SIG_BLOCK, &set);
    let name = "signal";
    match thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || serve(&set, fire))
    {
        Ok(_) => Ok(()),
        Err(e) => {
            mask(libc::SIG_SETMASK, &old);
            Err(Error::Start {
                name: name.to_owned(),
                reason: e.to_string(),
            })
        }
    }
}

fn set() -> libc::sigset_t {
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

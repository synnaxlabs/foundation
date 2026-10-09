//! The signals that ask the process to stop: SIGINT and SIGTERM.

use std::mem::MaybeUninit;

use crate::Error;

/// Blocks SIGINT and SIGTERM on the calling thread, then starts a thread that waits
/// for them. The future completes at the first.
pub(crate) fn hold() -> Result<impl Future<Output = ()> + Send + 'static, Error> {
    let set = set();
    // SAFETY: `set` is an initialized sigset, and the old mask is not asked for.
    let rc = unsafe {
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut())
    };
    assert_eq!(
        rc, 0,
        "invariant: SIG_BLOCK and a valid set are valid arguments"
    );
    let (fire, fired) = tokio::sync::oneshot::channel();
    let handle = crate::thread::start(
        "signal".to_owned(),
        |_| Ok(()),
        move |()| {
            wait(&set);
            // The future may be gone, as when the process stops on its own.
            fire.send(()).unwrap_or(());
            false
        },
    )
    .map_err(Error::Thread)?;
    // The thread ends at the first signal, or with the process.
    drop(handle);
    Ok(async move {
        fired
            .await
            .expect("invariant: the signal thread fires before it ends");
    })
}

/// The set of SIGINT and SIGTERM.
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

/// Blocks until a signal of `set` comes, which `set` holds blocked.
fn wait(set: &libc::sigset_t) {
    let mut signal = 0;
    // SAFETY: `set` is an initialized sigset, and `signal` an int that the call writes.
    let rc = unsafe { libc::sigwait(set, &raw mut signal) };
    assert_eq!(rc, 0, "invariant: sigwait of a valid set does not fail");
}

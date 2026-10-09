//! An `os::interrupt` whose thread cannot start. The filter acts on the test thread
//! and each thread it starts, so it runs in a test binary of its own, with this one
//! test only.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]

#[path = "common/mask.rs"]
#[expect(dead_code, reason = "this binary only adds to the mask")]
mod mask;
#[path = "common/seccomp.rs"]
mod seccomp;

/// A hold that fails leaves the mask as it was: a signal that the caller did not block
/// still ends the process, and one that it blocked stays blocked.
#[test]
fn an_interrupt_whose_thread_cannot_start_keeps_the_mask() {
    let signals = [libc::SIGINT, libc::SIGTERM];
    mask::block(&[libc::SIGTERM]);
    assert_eq!(
        mask::blocked(&signals),
        [libc::SIGTERM],
        "the caller blocks SIGTERM"
    );
    // glibc calls `clone3`, or `clone` where the kernel lacks it.
    seccomp::answer_calls(&[libc::SYS_clone3, libc::SYS_clone], |_, _| {
        Some(libc::EAGAIN)
    });
    let found = os::interrupt().map(drop).unwrap_err();
    let refused = "cannot start thread signal: Resource temporarily unavailable (os \
                   error 11)";
    assert_eq!(found.to_string(), refused);
    assert!(matches!(
        found,
        os::Error::Thread(env::thread::Error::Start { .. })
    ));
    assert_eq!(mask::blocked(&signals), [libc::SIGTERM]);
}

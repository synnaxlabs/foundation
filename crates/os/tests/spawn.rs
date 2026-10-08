//! A lookup whose thread cannot start. The filter acts on the test thread and each
//! thread it starts, so it runs in a test binary of its own, with this one test only.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]
#![expect(unsafe_code, reason = "a seccomp filter is an OS call")]

use env::net::Error;
use rustix::io::Errno;

/// Makes the OS refuse each new thread of this thread with `EAGAIN`, as a full
/// process table does. glibc calls `clone3`, or `clone` where the kernel lacks it.
fn refuse_threads() {
    use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W};

    let op = |code: u32, jt, k| libc::sock_filter {
        code: u16::try_from(code).unwrap(),
        jt,
        jf: 0,
        k,
    };
    let call = |nr: libc::c_long| u32::try_from(nr).unwrap();
    let refuse = libc::SECCOMP_RET_ERRNO | call(libc::EAGAIN.into());
    let mut filter = [
        op(BPF_LD | BPF_W | BPF_ABS, 0, 0),
        op(BPF_JMP | BPF_JEQ | BPF_K, 2, call(libc::SYS_clone3)),
        op(BPF_JMP | BPF_JEQ | BPF_K, 1, call(libc::SYS_clone)),
        op(BPF_RET | BPF_K, 0, libc::SECCOMP_RET_ALLOW),
        op(BPF_RET | BPF_K, 0, refuse),
    ];
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).unwrap(),
        filter: filter.as_mut_ptr(),
    };
    // The kernel reads each argument as a whole register.
    let (one, zero, mode): (libc::c_ulong, libc::c_ulong, libc::c_ulong) =
        (1, 0, libc::SECCOMP_MODE_FILTER.into());
    // SAFETY: the call sets one flag of this thread.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, one, zero, zero, zero) };
    assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: `program` points at `filter`, which outlives the call. The kernel copies
    // it.
    let rc = unsafe { libc::prctl(libc::PR_SET_SECCOMP, mode, &raw const program) };
    assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
}

#[test]
fn a_lookup_whose_thread_cannot_start_is_io() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds");
    refuse_threads();
    let found = runtime.block_on(os::net().resolve("localhost", 4433));
    let code = Errno::AGAIN.raw_os_error();
    assert_eq!(found, Err(Error::Io { code }));
}

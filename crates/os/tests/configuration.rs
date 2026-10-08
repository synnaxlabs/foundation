//! A lookup that cannot read part of the configuration of the C library. glibc reads
//! it only at the first lookup of a process, and the filter acts on each thread that
//! the test thread starts, so it runs in a test binary of its own, with this one test
//! only.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]
#![expect(unsafe_code, reason = "a seccomp filter is an OS call")]

use std::ffi::{CStr, c_char};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc;

use env::net::Error;
use rustix::io::Errno;

/// Makes each open of `/etc/host.conf` by a thread that this thread starts later
/// fail with `EMFILE`, as a full descriptor table does.
fn refuse_host_conf() {
    use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W};

    let (sender, listener) = mpsc::channel();
    // Started before the filter, so its own calls never wait on itself.
    #[expect(clippy::disallowed_methods, reason = "the test answers the filter")]
    std::thread::spawn(move || answer(&listener.recv().unwrap()));
    let op = |code: u32, jt, k| libc::sock_filter {
        code: u16::try_from(code).unwrap(),
        jt,
        jf: 0,
        k,
    };
    let call = |nr: libc::c_long| u32::try_from(nr).unwrap();
    let mut filter = [
        op(BPF_LD | BPF_W | BPF_ABS, 0, 0),
        op(BPF_JMP | BPF_JEQ | BPF_K, 1, call(libc::SYS_openat)),
        op(BPF_RET | BPF_K, 0, libc::SECCOMP_RET_ALLOW),
        op(BPF_RET | BPF_K, 0, libc::SECCOMP_RET_USER_NOTIF),
    ];
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).unwrap(),
        filter: filter.as_mut_ptr(),
    };
    // The kernel reads each argument as a whole register.
    let (one, zero): (libc::c_ulong, libc::c_ulong) = (1, 0);
    // SAFETY: the call sets one flag of this thread.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, one, zero, zero, zero) };
    assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
    let mode = libc::c_ulong::from(libc::SECCOMP_SET_MODE_FILTER);
    let flags = libc::SECCOMP_FILTER_FLAG_NEW_LISTENER;
    // SAFETY: `program` points at `filter`, which outlives the call. The kernel copies
    // it.
    let fd =
        unsafe { libc::syscall(libc::SYS_seccomp, mode, flags, &raw const program) };
    let fd = i32::try_from(fd).unwrap();
    assert!(fd >= 0, "{}", std::io::Error::last_os_error());
    // SAFETY: the kernel gave the listener to this process alone.
    sender.send(unsafe { OwnedFd::from_raw_fd(fd) }).unwrap();
}

/// Answers each `openat` that the filter of `listener` holds: `EMFILE` for
/// `/etc/host.conf`, and the real call for each other path.
fn answer(listener: &OwnedFd) {
    let data = libc::seccomp_data {
        nr: 0,
        arch: 0,
        instruction_pointer: 0,
        args: [0; 6],
    };
    loop {
        let mut held = libc::seccomp_notif {
            id: 0,
            pid: 0,
            flags: 0,
            data,
        };
        let receive = libc::SECCOMP_IOCTL_NOTIF_RECV;
        // SAFETY: `held` is a whole `seccomp_notif` for the kernel to fill.
        if unsafe { libc::ioctl(listener.as_raw_fd(), receive, &raw mut held) } != 0 {
            return;
        }
        let path = held.data.args[1] as *const c_char;
        // SAFETY: the thread that waits on this answer shares this address space, and
        // `openat` takes a path that ends in a NUL.
        let path = unsafe { CStr::from_ptr(path) };
        let mut reply = libc::seccomp_notif_resp {
            id: held.id,
            val: 0,
            error: 0,
            flags: 0,
        };
        if path == c"/etc/host.conf" {
            reply.error = -libc::EMFILE;
        } else {
            reply.flags =
                u32::try_from(libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE).unwrap();
        }
        let send = libc::SECCOMP_IOCTL_NOTIF_SEND;
        // SAFETY: `reply` is a whole `seccomp_notif_resp`.
        unsafe { libc::ioctl(listener.as_raw_fd(), send, &raw mut reply) };
    }
}

/// glibc gives `EAI_NONAME` with errno `EMFILE` for a name that does not exist, so
/// the error is `Io`: the answer is not final.
#[test]
fn a_lookup_that_cannot_read_its_configuration_is_io() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds");
    refuse_host_conf();
    let found = runtime.block_on(os::net().resolve("foundation.invalid", 4433));
    let code = Errno::MFILE.raw_os_error();
    assert_eq!(found, Err(Error::Io { code }));
}

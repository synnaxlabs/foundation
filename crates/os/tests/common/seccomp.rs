//! A seccomp filter whose held calls a thread of the test answers.

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc;

/// Holds each call of `calls` that this thread, or a thread that it starts later,
/// makes. The `k`th held call `nr`, from 0, fails with the errno of `plan(data, k)`,
/// or runs when that is `None`. `plan` runs on a thread that starts first, so its own
/// calls are never held.
#[expect(unsafe_code, reason = "a seccomp filter is an OS call")]
pub(crate) fn answer_calls(
    calls: &[libc::c_long],
    plan: impl FnMut(&libc::seccomp_data, usize) -> Option<i32> + Send + 'static,
) {
    use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W};

    let (sender, listener) = mpsc::channel();
    #[expect(clippy::disallowed_methods, reason = "the test answers the filter")]
    std::thread::spawn(move || answer(&listener.recv().unwrap(), plan));
    let op = |code: u32, jt, k| libc::sock_filter {
        code: u16::try_from(code).unwrap(),
        jt,
        jf: 0,
        k,
    };
    let mut filter = vec![op(BPF_LD | BPF_W | BPF_ABS, 0, 0)];
    for (i, &nr) in calls.iter().enumerate() {
        // Jumps past the next calls and the allow, to the hold.
        let jt = u8::try_from(calls.len() - i).unwrap();
        filter.push(op(
            BPF_JMP | BPF_JEQ | BPF_K,
            jt,
            u32::try_from(nr).unwrap(),
        ));
    }
    filter.push(op(BPF_RET | BPF_K, 0, libc::SECCOMP_RET_ALLOW));
    filter.push(op(BPF_RET | BPF_K, 0, libc::SECCOMP_RET_USER_NOTIF));
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

/// Answers each call that the filter of `listener` holds, as `plan` says.
#[expect(unsafe_code, reason = "the listener is read with ioctl")]
fn answer(
    listener: &OwnedFd,
    mut plan: impl FnMut(&libc::seccomp_data, usize) -> Option<i32>,
) {
    let data = libc::seccomp_data {
        nr: 0,
        arch: 0,
        instruction_pointer: 0,
        args: [0; 6],
    };
    let mut counts = BTreeMap::new();
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
        let mut reply = libc::seccomp_notif_resp {
            id: held.id,
            val: 0,
            error: 0,
            flags: 0,
        };
        let k = counts.entry(held.data.nr).or_insert(0);
        let answer = plan(&held.data, *k);
        *k += 1;
        match answer {
            Some(errno) => reply.error = -errno,
            None => {
                reply.flags =
                    u32::try_from(libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE).unwrap();
            }
        }
        let send = libc::SECCOMP_IOCTL_NOTIF_SEND;
        // SAFETY: `reply` is a whole `seccomp_notif_resp`.
        unsafe { libc::ioctl(listener.as_raw_fd(), send, &raw mut reply) };
    }
}

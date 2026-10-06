use std::time::{SystemTime, UNIX_EPOCH};

use types::time::{Span, Stamp};

/// The std wall clock.
#[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
fn system() -> Stamp {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the OS clock is after 1970");
    Stamp::from_nanos(i64::try_from(since.as_nanos()).unwrap())
}

#[test]
fn reads_the_system_time() {
    let wall = os::wall().expect("the OS gives its wall clock");
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    let (before, reading, after) = (system(), wall.now(), system());
    // A clock that counts microseconds drops the nanoseconds of a reading.
    let slack = Span::from_nanos(1_000);
    assert!(
        reading.time >= before - slack && reading.time <= after + slack,
        "read {} between {before} and {after}",
        reading.time
    );
}

#[test]
fn gives_a_bound_under_sixteen_seconds_when_a_daemon_runs() {
    let wall = os::wall().expect("the OS gives its wall clock");
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    let reading = wall.now();
    let error = reading.error.expect("a daemon keeps the clock");
    assert!(
        error >= Span::ZERO && error < Span::from_nanos(16_000_000_000),
        "{error}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_refused_read_fails_at_construction() {
    let result = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                refuse_adjtimex();
                os::wall().map(drop).map_err(|e| e.to_string())
            })
            .join()
            .unwrap()
    });
    let refused = "cannot read the wall clock: Operation not permitted (os error 1)";
    assert_eq!(result, Err(refused.to_owned()));
}

/// Makes the OS refuse `adjtimex` and `clock_adjtime` on this thread, as systemd's
/// `ProtectClock` does. glibc calls either one for `adjtimex`.
#[cfg(target_os = "linux")]
#[expect(unsafe_code, reason = "a seccomp filter is an OS call")]
fn refuse_adjtimex() {
    use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W};

    let op = |code: u32, jt, k| libc::sock_filter {
        code: u16::try_from(code).unwrap(),
        jt,
        jf: 0,
        k,
    };
    let call = |nr: libc::c_long| u32::try_from(nr).unwrap();
    let refuse = libc::SECCOMP_RET_ERRNO | call(libc::EPERM.into());
    let mut filter = [
        op(BPF_LD | BPF_W | BPF_ABS, 0, 0),
        op(BPF_JMP | BPF_JEQ | BPF_K, 2, call(libc::SYS_adjtimex)),
        op(BPF_JMP | BPF_JEQ | BPF_K, 1, call(libc::SYS_clock_adjtime)),
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

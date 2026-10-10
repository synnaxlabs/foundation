//! A lookup that cannot read part of the configuration of the C library. glibc reads
//! it only at the first lookup of a process, and the filter acts on each thread that
//! the test thread starts, so it runs in a test binary of its own, with this one test
//! only.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]
#![expect(unsafe_code, reason = "a seccomp filter is an OS call")]

#[path = "common/seccomp.rs"]
mod seccomp;

use std::ffi::{CStr, c_char};

use env::net::Error;
use rustix::io::Errno;

/// Makes each open of `/etc/host.conf` by a thread that this thread starts later
/// fail with `EMFILE`, as a full descriptor table does.
fn refuse_host_conf() {
    seccomp::answer_calls(&[libc::SYS_openat], |data, _| {
        let path = data.args[1] as *const c_char;
        // SAFETY: the thread that waits on this answer shares this address space, and
        // `openat` takes a path that ends in a NUL.
        let path = unsafe { CStr::from_ptr(path) };
        (path == c"/etc/host.conf").then_some(libc::EMFILE)
    });
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

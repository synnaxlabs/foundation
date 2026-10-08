//! Name lookups through `getaddrinfo` of the C library, so a name resolves as it does
//! for each other program on the host: hosts file, name servers, and mDNS on macOS.

use std::ffi::{CStr, CString, c_int};
use std::net::SocketAddr;
use std::{io, ptr, thread};

use env::net::Error;
use rustix::io::Errno;
use rustix::net::SocketAddrAny;
use rustix::net::addr::SocketAddrStorage;
use tokio::sync::oneshot;

/// Gives the addresses of `host`, each with `port`, in the order of `getaddrinfo`.
/// The lookup blocks an OS thread of its own, which ends with the lookup, also after
/// the future drops.
pub(super) async fn lookup(host: &str, port: u16) -> Result<Vec<SocketAddr>, Error> {
    let name =
        CString::new(host).map_err(|_nul| Error::NotFound { host: host.into() })?;
    let (sender, answer) = oneshot::channel();
    #[expect(
        clippy::disallowed_methods,
        reason = "os starts threads; a lookup blocks the thread it runs on"
    )]
    let started = thread::Builder::new()
        .name("resolve".into())
        .spawn(move || {
            // The future dropped when the send fails, so no one reads the answer.
            sender.send(getaddrinfo(&name, port)).unwrap_or_else(drop);
        });
    started.map_err(|e| super::io_error(super::errno(&e)))?;
    answer
        .await
        .expect("invariant: the resolve thread answers")
        .map_err(|failure| failure.error(host))
}

/// A failed `getaddrinfo`: its code, and `errno` as it was after the call.
#[derive(Debug)]
struct Failure {
    code: c_int,
    errno: Errno,
}

impl Failure {
    /// The error of a lookup of `host` that failed so. The code of [`Error::Io`] is
    /// always an errno: EAI codes differ between systems.
    fn error(self, host: &str) -> Error {
        let errno = match (self.code, self.errno) {
            // glibc gives `EAI_NONAME` when it cannot load its name service modules.
            (libc::EAI_SYSTEM, errno)
            | (libc::EAI_NONAME, errno @ (Errno::MFILE | Errno::NFILE)) => errno,
            (libc::EAI_NONAME | libc::EAI_NODATA, _) => {
                return Error::NotFound { host: host.into() };
            }
            (libc::EAI_AGAIN, _) => Errno::AGAIN,
            (libc::EAI_MEMORY, _) => Errno::NOMEM,
            _ => Errno::IO,
        };
        super::io_error(errno)
    }
}

/// Looks up `name` and gives each of its addresses, at least one, with `port`.
/// Blocks.
fn getaddrinfo(name: &CStr, port: u16) -> Result<Vec<SocketAddr>, Failure> {
    let hints = libc::addrinfo {
        ai_flags: 0,
        ai_family: libc::AF_UNSPEC,
        // One entry per address, not one per socket type.
        ai_socktype: libc::SOCK_STREAM,
        ai_protocol: 0,
        ai_addrlen: 0,
        ai_addr: ptr::null_mut(),
        ai_canonname: ptr::null_mut(),
        ai_next: ptr::null_mut(),
    };
    let mut list = ptr::null_mut();
    // SAFETY: `name` ends in a NUL, `hints` is a whole `addrinfo` with null links, and
    // `list` is a place for the result.
    let code = unsafe {
        libc::getaddrinfo(name.as_ptr(), ptr::null(), &raw const hints, &raw mut list)
    };
    if code != 0 {
        let errno = super::errno(&io::Error::last_os_error());
        return Err(Failure { code, errno });
    }
    Ok(List(list).addresses(port))
}

/// A list that a `getaddrinfo` that succeeded gave. Its drop frees it.
struct List(*mut libc::addrinfo);

impl List {
    /// Each address in the list, with `port`.
    fn addresses(&self, port: u16) -> Vec<SocketAddr> {
        let mut addresses = Vec::new();
        let mut entry = self.0.cast_const();
        // SAFETY: each link of the list is null or valid until the drop.
        while let Some(info) = unsafe { entry.as_ref() } {
            #[expect(
                clippy::cast_ptr_alignment,
                reason = "`SocketAddrAny::read` copies the bytes at any alignment"
            )]
            let storage = info.ai_addr.cast::<SocketAddrStorage>().cast_const();
            // SAFETY: `ai_addr` points at `ai_addrlen` bytes of one socket address.
            let any = unsafe { SocketAddrAny::read(storage, info.ai_addrlen) };
            let mut address = SocketAddr::try_from(any)
                .expect("invariant: getaddrinfo of AF_UNSPEC gives only IP addresses");
            address.set_port(port);
            addresses.push(address);
            entry = info.ai_next;
        }
        addresses
    }
}

impl Drop for List {
    fn drop(&mut self) {
        // SAFETY: a list that `getaddrinfo` gave, freed only here.
        unsafe { libc::freeaddrinfo(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod error {
        use super::*;

        /// Through a private call: no input makes `getaddrinfo` give `EAI_AGAIN` or
        /// `EAI_MEMORY` on demand.
        #[test]
        fn maps_each_code_to_its_error() {
            let host = "pump.local";
            let not_found = Error::NotFound { host: host.into() };
            let io = |errno: Errno| Error::Io {
                code: errno.raw_os_error(),
            };
            let cases = [
                (libc::EAI_NONAME, Errno::NOENT, not_found.clone()),
                (libc::EAI_NONAME, Errno::MFILE, io(Errno::MFILE)),
                (libc::EAI_NONAME, Errno::NFILE, io(Errno::NFILE)),
                (libc::EAI_NODATA, Errno::MFILE, not_found),
                (libc::EAI_SYSTEM, Errno::MFILE, io(Errno::MFILE)),
                (libc::EAI_AGAIN, Errno::MFILE, io(Errno::AGAIN)),
                (libc::EAI_MEMORY, Errno::MFILE, io(Errno::NOMEM)),
                (libc::EAI_FAIL, Errno::MFILE, io(Errno::IO)),
                (libc::EAI_SERVICE, Errno::MFILE, io(Errno::IO)),
            ];
            for (code, errno, expected) in cases {
                let failure = Failure { code, errno };
                assert_eq!(failure.error(host), expected, "{code} {errno:?}");
            }
        }
    }
}

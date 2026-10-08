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
    start(move || {
        // The future dropped when the send fails, so no one reads the answer.
        sender.send(getaddrinfo(&name, port)).unwrap_or_else(drop);
    })
    .map_err(|e| super::io_error(super::errno(&e)))?;
    answer
        .await
        .expect("invariant: the resolve thread answers")
        .map_err(|Failure { code, errno }| failure(code, errno, host))
}

#[expect(
    clippy::disallowed_methods,
    reason = "os starts threads; a lookup blocks the thread it runs on"
)]
fn start(lookup: impl FnOnce() + Send + 'static) -> io::Result<()> {
    thread::Builder::new()
        .name("resolve".into())
        .spawn(lookup)
        .map(drop)
}

/// A failed `getaddrinfo`: its code, and `errno` as it was after the call.
#[derive(Debug)]
struct Failure {
    code: c_int,
    errno: Errno,
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
    let addresses = addresses(list, port);
    // SAFETY: `list` came from a `getaddrinfo` that succeeded, and is freed once.
    unsafe { libc::freeaddrinfo(list) };
    Ok(addresses)
}

/// Each address in `list`, a list that `getaddrinfo` gave, with `port`.
fn addresses(list: *const libc::addrinfo, port: u16) -> Vec<SocketAddr> {
    let mut addresses = Vec::new();
    let mut entry = list;
    // SAFETY: each link of the list is null or valid until `freeaddrinfo`.
    while let Some(info) = unsafe { entry.as_ref() } {
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "`SocketAddrAny::read` copies `ai_addrlen` bytes, at any alignment"
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

/// The error of a lookup of `host` that failed with `code` and `errno`. The code of
/// [`Error::Io`] is always an errno: EAI codes differ between systems.
fn failure(code: c_int, errno: Errno, host: &str) -> Error {
    let errno = match code {
        libc::EAI_NONAME | libc::EAI_NODATA => {
            return Error::NotFound { host: host.into() };
        }
        libc::EAI_SYSTEM => errno,
        libc::EAI_AGAIN => Errno::AGAIN,
        libc::EAI_MEMORY => Errno::NOMEM,
        _ => Errno::IO,
    };
    super::io_error(errno)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod failure {
        use super::*;

        #[test]
        fn maps_each_code_to_its_error() {
            let host = "pump.local";
            let not_found = Error::NotFound { host: host.into() };
            let io = |errno: Errno| Error::Io {
                code: errno.raw_os_error(),
            };
            let cases = [
                (libc::EAI_NONAME, not_found.clone()),
                (libc::EAI_NODATA, not_found),
                (libc::EAI_SYSTEM, io(Errno::MFILE)),
                (libc::EAI_AGAIN, io(Errno::AGAIN)),
                (libc::EAI_MEMORY, io(Errno::NOMEM)),
                (libc::EAI_FAIL, io(Errno::IO)),
                (libc::EAI_SERVICE, io(Errno::IO)),
            ];
            for (code, expected) in cases {
                assert_eq!(failure(code, Errno::MFILE, host), expected, "{code}");
            }
        }
    }

    mod getaddrinfo {
        use std::net::SocketAddrV6;

        use super::*;

        fn lookup(host: &str) -> Vec<SocketAddr> {
            let name = CString::new(host).unwrap();
            getaddrinfo(&name, 4433).unwrap()
        }

        #[test]
        fn gives_an_ipv4_address_with_the_port() {
            let address = SocketAddr::new([192, 0, 2, 7].into(), 4433);
            assert_eq!(lookup("192.0.2.7"), [address]);
        }

        #[test]
        fn keeps_the_scope_of_an_ipv6_address() {
            let ip = "fe80::1".parse().unwrap();
            let address = SocketAddr::V6(SocketAddrV6::new(ip, 4433, 0, 1));
            assert_eq!(lookup("fe80::1%1"), [address]);
        }

        #[test]
        fn gives_the_code_of_a_failure() {
            let name = CString::new("foundation.invalid").unwrap();
            let failed = getaddrinfo(&name, 4433).unwrap_err();
            assert_eq!(failed.code, libc::EAI_NONAME);
        }
    }
}

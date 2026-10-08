//! Name lookups through the `getaddrinfo` of the system, each on an OS thread of its
//! own.

use std::ffi::{CStr, CString, c_int};
use std::io;
use std::net::SocketAddr;
use std::ptr;
use std::thread;

use env::net::Error;
use rustix::io::Errno;
use rustix::net::SocketAddrAny;
use rustix::net::addr::SocketAddrStorage;
use tokio::sync::oneshot;

use super::{errno, io_error};

/// The addresses of `host`, each with `port`, in the order of `getaddrinfo`. Needs no
/// runtime. A drop of the future stops the wait, and the thread ends with the lookup.
#[expect(
    clippy::disallowed_methods,
    reason = "`getaddrinfo` blocks, so each lookup has a thread of its own"
)]
pub(super) async fn lookup(host: &str, port: u16) -> Result<Vec<SocketAddr>, Error> {
    let Ok(name) = CString::new(host) else {
        return Err(Error::NotFound { host: host.into() });
    };
    let (reply, answered) = oneshot::channel();
    thread::Builder::new()
        .name("resolve".into())
        .spawn(move || {
            // The receiver is gone when the future was dropped.
            let _sent = reply.send(addresses(&name, port));
        })
        .map_err(|e| io_error(errno(&e)))?;
    answer(host, answered.await.expect("the lookup thread answers"))
}

/// The IP addresses of `host` with `port`, or the code of `getaddrinfo` and `errno`.
fn addresses(host: &CStr, port: u16) -> Result<Vec<SocketAddr>, (c_int, Errno)> {
    let hints = libc::addrinfo {
        ai_flags: 0,
        ai_family: libc::AF_UNSPEC,
        // One entry for each address, not one for each socket type.
        ai_socktype: libc::SOCK_STREAM,
        ai_protocol: 0,
        ai_addrlen: 0,
        ai_addr: ptr::null_mut(),
        ai_canonname: ptr::null_mut(),
        ai_next: ptr::null_mut(),
    };
    let mut list = ptr::null_mut();
    // SAFETY: `host` ends in NUL, and `hints` and `list` are valid for the call.
    let code = unsafe {
        libc::getaddrinfo(host.as_ptr(), ptr::null(), &raw const hints, &raw mut list)
    };
    if code != 0 {
        return Err((code, errno(&io::Error::last_os_error())));
    }
    let mut found = Vec::new();
    let mut entry = list;
    while !entry.is_null() {
        // SAFETY: `entry` is a node of the list from `getaddrinfo`, not yet freed.
        let info = unsafe { &*entry };
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "`read` copies the bytes, so the pointer needs no alignment"
        )]
        let storage = info.ai_addr.cast::<SocketAddrStorage>();
        // SAFETY: `ai_addr` holds `ai_addrlen` bytes of a socket address.
        let address = unsafe { SocketAddrAny::read(storage, info.ai_addrlen) };
        if let Ok(mut address) = SocketAddr::try_from(address) {
            address.set_port(port);
            found.push(address);
        }
        entry = info.ai_next;
    }
    // SAFETY: `list` is from `getaddrinfo`, and nothing reads it after this.
    unsafe { libc::freeaddrinfo(list) };
    Ok(found)
}

/// The answer to a lookup of `host` that found `found`, or that `getaddrinfo` failed
/// with a code, with `errno`.
fn answer(
    host: &str,
    found: Result<Vec<SocketAddr>, (c_int, Errno)>,
) -> Result<Vec<SocketAddr>, Error> {
    let not_found = || Error::NotFound { host: host.into() };
    let code = match found {
        Ok(found) if found.is_empty() => return Err(not_found()),
        Ok(found) => return Ok(found),
        Err((libc::EAI_NONAME | libc::EAI_NODATA, _)) => return Err(not_found()),
        Err((libc::EAI_SYSTEM, system)) => system,
        Err((libc::EAI_AGAIN, _)) => Errno::AGAIN,
        Err((libc::EAI_MEMORY, _)) => Errno::NOMEM,
        Err(_) => Errno::IO,
    };
    Err(io_error(code))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddrV6;

    use super::*;

    fn io(code: Errno) -> Result<Vec<SocketAddr>, Error> {
        Err(Error::Io {
            code: code.raw_os_error(),
        })
    }

    #[test]
    fn maps_each_code_of_getaddrinfo() {
        let system = Errno::MFILE;
        let not_found = Err(Error::NotFound { host: "plc".into() });
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
            assert_eq!(answer("plc", Err((code, system))), expected, "{code}");
        }
    }

    #[test]
    fn a_lookup_with_no_ip_address_is_not_found() {
        let not_found = Error::NotFound { host: "plc".into() };
        assert_eq!(answer("plc", Ok(vec![])), Err(not_found));
        let found = vec!["10.0.0.2:4433".parse().unwrap()];
        assert_eq!(answer("plc", Ok(found.clone())), Ok(found));
    }

    #[test]
    fn keeps_the_scope_of_an_ipv6_address() {
        let host = CString::new("fe80::1%1").unwrap();
        let scoped = SocketAddrV6::new("fe80::1".parse().unwrap(), 4433, 0, 1);
        assert_eq!(addresses(&host, 4433), Ok(vec![SocketAddr::V6(scoped)]));
    }
}

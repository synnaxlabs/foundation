//! TCP streams and listeners on the real network. Each one is a non-blocking socket
//! that registers with the I/O driver of the Tokio runtime of the thread of its first
//! poll.

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsFd, BorrowedFd};

use env::net::{Connect, Error, Resolve, tcp, udp};
use rustix::io::Errno;
use rustix::net::sockopt;
use tokio::net::TcpSocket;

use self::listener::Listener;
use self::stream::Stream;

mod listener;
#[expect(
    unsafe_code,
    reason = "`TCP_NOTSENT_LOWAT` is a `setsockopt` rustix lacks"
)]
mod lowat;
mod socket;
mod stream;

/// The network of this machine.
pub(crate) struct Driver;

impl env::net::Driver for Driver {
    fn udp(&self, _: &udp::Config) -> Result<Box<dyn udp::Driver>, Error> {
        panic!("os::net has no UDP driver yet")
    }

    fn connect<'a>(&'a self, config: &'a tcp::Config) -> Connect<'a> {
        Box::pin(connect(config))
    }

    fn listen(
        &self,
        config: &tcp::Listen,
    ) -> Result<Box<dyn env::net::listener::Driver>, Error> {
        Ok(Box::new(Listener::listen(config)?))
    }

    fn resolve<'a>(&'a self, _: &'a str, _: u16) -> Resolve<'a> {
        panic!("os::net has no resolver yet")
    }
}

/// Connects to `config.remote` through the I/O driver of the runtime that polls the
/// future. The stream it gives is registered with no driver yet, so any thread may
/// take it.
async fn connect(config: &tcp::Config) -> Result<Box<dyn tcp::Driver>, Error> {
    let remote = config.remote;
    let failed = |code: Errno| stream_error(code, remote);
    let socket = match remote {
        SocketAddr::V4(_) => TcpSocket::new_v4(),
        SocketAddr::V6(_) => TcpSocket::new_v6(),
    }
    .map_err(|e| failed(errno(&e)))?;
    apply(socket.as_fd(), &config.options).map_err(failed)?;
    let stream = socket
        .connect(remote)
        .await
        .map_err(|e| failed(errno(&e)))?;
    let stream = stream.into_std().map_err(|e| failed(errno(&e)))?;
    let local = stream.local_addr().map_err(|e| io_error(errno(&e)))?;
    Ok(Box::new(
        Stream::new(stream, local, remote).map_err(failed)?,
    ))
}

/// Sets `options` on a TCP socket.
fn apply(fd: BorrowedFd<'_>, options: &tcp::Options) -> Result<(), Errno> {
    sockopt::set_socket_send_buffer_size(fd, options.send_buffer_bytes)?;
    sockopt::set_socket_recv_buffer_size(fd, options.recv_buffer_bytes)?;
    sockopt::set_tcp_nodelay(fd, !options.delayed)?;
    lowat::set(fd, options.unsent_bytes_max)
}

/// The OS code of `error`, or `EIO` when it has none.
fn errno(error: &io::Error) -> Errno {
    Errno::from_io_error(error).unwrap_or(Errno::IO)
}

/// The error of a socket call that failed with `code`, with no remote to name.
fn io_error(code: Errno) -> Error {
    Error::Io {
        code: code.raw_os_error(),
    }
}

/// The error of a stream to `remote` that failed with `code`.
fn stream_error(code: Errno, remote: SocketAddr) -> Error {
    match code {
        Errno::CONNREFUSED => Error::Refused { remote },
        Errno::NETUNREACH | Errno::HOSTUNREACH => Error::Unreachable { remote },
        Errno::CONNRESET | Errno::PIPE => Error::Reset { remote },
        Errno::TIMEDOUT => Error::TimedOut { remote },
        _ => Error::Io {
            code: code.raw_os_error(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn loopback() -> SocketAddr {
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)
    }

    fn options(delayed: bool) -> tcp::Options {
        tcp::Options {
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 15,
            unsent_bytes_max: 1 << 14,
            delayed,
        }
    }

    /// Linux keeps twice the buffer size it is given, for its own bookkeeping.
    fn kept(bytes: usize) -> usize {
        if cfg!(target_os = "linux") {
            bytes * 2
        } else {
            bytes
        }
    }

    mod apply {
        use super::*;

        fn check(delayed: bool) {
            let fd = listener::socket(loopback()).unwrap();
            apply(fd.as_fd(), &options(delayed)).unwrap();
            assert_eq!(sockopt::socket_send_buffer_size(&fd), Ok(kept(1 << 16)));
            assert_eq!(sockopt::socket_recv_buffer_size(&fd), Ok(kept(1 << 15)));
            assert_eq!(sockopt::tcp_nodelay(&fd), Ok(!delayed));
            assert_eq!(lowat::get(fd.as_fd()), Ok(1 << 14));
            assert_eq!(sockopt::socket_linger(&fd), Ok(None), "the stream sets it");
        }

        #[test]
        fn sets_each_option_with_nagle_off() {
            check(false);
        }

        #[test]
        fn sets_each_option_with_nagle_on() {
            check(true);
        }

        #[test]
        #[cfg(target_pointer_width = "64")]
        fn gives_the_code_of_a_refused_option() {
            let fd = listener::socket(loopback()).unwrap();
            let mut options = options(false);
            options.unsent_bytes_max = usize::MAX;
            assert_eq!(apply(fd.as_fd(), &options), Err(Errno::INVAL));
        }
    }

    mod stream_error {
        use super::*;

        #[test]
        fn names_the_remote_in_each_reach_failure() {
            let remote: SocketAddr = "10.0.0.2:4433".parse().unwrap();
            let cases = [
                (Errno::CONNREFUSED, Error::Refused { remote }),
                (Errno::NETUNREACH, Error::Unreachable { remote }),
                (Errno::HOSTUNREACH, Error::Unreachable { remote }),
                (Errno::CONNRESET, Error::Reset { remote }),
                (Errno::PIPE, Error::Reset { remote }),
                (Errno::TIMEDOUT, Error::TimedOut { remote }),
                (
                    Errno::ADDRNOTAVAIL,
                    Error::Io {
                        code: Errno::ADDRNOTAVAIL.raw_os_error(),
                    },
                ),
            ];
            for (code, expected) in cases {
                assert_eq!(stream_error(code, remote), expected, "{code:?}");
            }
        }
    }

    mod errno {
        use super::*;

        #[test]
        fn gives_the_os_code_or_eio() {
            let os = io::Error::from_raw_os_error(Errno::AGAIN.raw_os_error());
            assert_eq!(errno(&os), Errno::AGAIN);
            assert_eq!(errno(&io::Error::other("no code")), Errno::IO);
        }
    }
}

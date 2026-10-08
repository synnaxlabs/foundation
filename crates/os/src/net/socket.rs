//! The thread rule of a socket: the first poll registers it with the runtime of the
//! thread, and each later poll runs on that thread.

use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::thread::{self, ThreadId};

use rustix::io::Errno;

/// A socket that registers at its first poll with the I/O driver of the Tokio runtime
/// of that thread, and then polls only on that thread.
pub(super) enum Socket<Idle, Live> {
    /// Not polled yet. It moves between threads freely.
    Idle(Idle),
    /// Registered on `thread`.
    Live { socket: Live, thread: ThreadId },
    /// The registration failed with `code`, which each poll gives again.
    Lost { code: Errno },
}

impl<Idle, Live> Socket<Idle, Live> {
    /// The live socket, for a poll. The first poll registers it with `register`.
    /// `kind` names the socket in the panic. Gives the code of a failed registration.
    ///
    /// # Panics
    ///
    /// A poll on a thread other than that of the first poll.
    pub(super) fn live(
        &mut self,
        kind: &str,
        register: impl FnOnce(Idle) -> io::Result<Live>,
    ) -> Result<&mut Live, Errno> {
        if let Self::Idle(_) = self {
            let Self::Idle(idle) =
                std::mem::replace(self, Self::Lost { code: Errno::IO })
            else {
                unreachable!("invariant: the state was checked above");
            };
            *self = match register(idle) {
                Ok(socket) => Self::Live {
                    socket,
                    thread: thread::current().id(),
                },
                Err(e) => Self::Lost {
                    code: super::errno(&e),
                },
            };
        }
        match self {
            Self::Live { socket, thread } => {
                on_thread(kind, *thread);
                Ok(socket)
            }
            Self::Lost { code } => Err(*code),
            Self::Idle(_) => unreachable!("invariant: the first poll registered"),
        }
    }

    /// The file descriptor, unless the registration failed.
    pub(super) fn fd(&self) -> Option<BorrowedFd<'_>>
    where
        Idle: AsFd,
        Live: AsFd,
    {
        match self {
            Self::Idle(idle) => Some(idle.as_fd()),
            Self::Live { socket, .. } => Some(socket.as_fd()),
            Self::Lost { .. } => None,
        }
    }
}

/// Checks the thread rule of a socket of `kind` that first polled on `thread`.
///
/// # Panics
///
/// On a thread other than `thread`.
pub(super) fn on_thread(kind: &str, thread: ThreadId) {
    assert_eq!(
        thread,
        thread::current().id(),
        "a {kind} polls only on the thread of its first poll"
    );
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;

    use super::*;

    /// A descriptor to own, with no socket.
    fn dup_stdin() -> OwnedFd {
        rustix::io::dup(std::io::stdin()).unwrap()
    }

    #[test]
    fn a_failed_registration_gives_its_code_on_each_poll() {
        let mut socket: Socket<(), ()> = Socket::Idle(());
        let failed =
            |()| Err(io::Error::from_raw_os_error(Errno::MFILE.raw_os_error()));
        assert_eq!(socket.live("stream", failed), Err(Errno::MFILE));
        let again = |()| unreachable!("a lost socket registers no second time");
        assert_eq!(socket.live("stream", again), Err(Errno::MFILE));
    }

    #[test]
    fn a_failure_with_no_os_code_is_eio() {
        let mut socket: Socket<(), ()> = Socket::Idle(());
        let failed = |()| Err(io::Error::other("no code"));
        assert_eq!(socket.live("stream", failed), Err(Errno::IO));
    }

    #[test]
    fn a_lost_socket_has_no_descriptor() {
        let mut socket: Socket<OwnedFd, OwnedFd> = Socket::Idle(dup_stdin());
        assert!(socket.fd().is_some());
        let failed = |_| Err(io::Error::from_raw_os_error(Errno::MFILE.raw_os_error()));
        let lost = socket.live("stream", failed).map(|_| ());
        assert_eq!(lost, Err(Errno::MFILE));
        assert!(socket.fd().is_none());
    }

    #[test]
    fn a_registration_runs_once() {
        let mut socket: Socket<u8, u8> = Socket::Idle(3);
        assert_eq!(socket.live("stream", |n| Ok(n + 1)), Ok(&mut 4));
        let again = |_| unreachable!("a live socket registers no second time");
        assert_eq!(socket.live("stream", again), Ok(&mut 4));
    }
}

//! The thread rule of a socket: the first poll registers it with the runtime of the
//! thread, and each later poll runs on that thread.

use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::thread::{self, ThreadId};

use rustix::io::Errno;

/// A socket that binds to the thread of its first poll, and then polls only on that
/// thread. The first poll that needs the socket registers it with the I/O driver of
/// the Tokio runtime of that thread.
pub(super) struct Socket<Idle, Live> {
    state: State<Idle, Live>,
    /// The thread of the first poll. Before it, the socket moves between threads
    /// freely.
    thread: Option<ThreadId>,
}

enum State<Idle, Live> {
    Idle(Idle),
    Live(Live),
    /// The registration failed with `code`, which each poll gives again.
    Lost {
        code: Errno,
    },
}

impl<Idle, Live> Socket<Idle, Live> {
    /// A socket that no poll bound or registered yet.
    pub(super) fn new(idle: Idle) -> Self {
        Self {
            state: State::Idle(idle),
            thread: None,
        }
    }

    /// Binds the socket to this thread at the first poll, with no registration.
    /// `kind` names the socket in the panic.
    ///
    /// # Panics
    ///
    /// A poll on a thread other than that of the first poll.
    pub(super) fn bind(&mut self, kind: &str) {
        let thread = *self.thread.get_or_insert_with(|| thread::current().id());
        on_thread(kind, thread);
    }

    /// The live socket, for a poll. It binds the socket as [`Socket::bind`] does,
    /// and the first call registers it with `register`. Gives the code of a failed
    /// registration.
    ///
    /// # Panics
    ///
    /// A poll on a thread other than that of the first poll.
    pub(super) fn live(
        &mut self,
        kind: &str,
        register: impl FnOnce(Idle) -> io::Result<Live>,
    ) -> Result<&mut Live, Errno> {
        self.bind(kind);
        if let State::Idle(_) = self.state {
            let lost = State::Lost { code: Errno::IO };
            let State::Idle(idle) = std::mem::replace(&mut self.state, lost) else {
                unreachable!("invariant: the state was checked above");
            };
            self.state = match register(idle) {
                Ok(socket) => State::Live(socket),
                Err(e) => State::Lost {
                    code: super::errno(&e),
                },
            };
        }
        match &mut self.state {
            State::Live(socket) => Ok(socket),
            State::Lost { code } => Err(*code),
            State::Idle(_) => unreachable!("invariant: the first poll registered"),
        }
    }

    /// The file descriptor, unless the registration failed.
    pub(super) fn fd(&self) -> Option<BorrowedFd<'_>>
    where
        Idle: AsFd,
        Live: AsFd,
    {
        match &self.state {
            State::Idle(idle) => Some(idle.as_fd()),
            State::Live(socket) => Some(socket.as_fd()),
            State::Lost { .. } => None,
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
        let mut socket: Socket<(), ()> = Socket::new(());
        let failed =
            |()| Err(io::Error::from_raw_os_error(Errno::MFILE.raw_os_error()));
        assert_eq!(socket.live("stream", failed), Err(Errno::MFILE));
        let again = |()| unreachable!("a lost socket registers no second time");
        assert_eq!(socket.live("stream", again), Err(Errno::MFILE));
    }

    #[test]
    fn a_failure_with_no_os_code_is_eio() {
        let mut socket: Socket<(), ()> = Socket::new(());
        let failed = |()| Err(io::Error::other("no code"));
        assert_eq!(socket.live("stream", failed), Err(Errno::IO));
    }

    #[test]
    fn a_lost_socket_has_no_descriptor() {
        let mut socket: Socket<OwnedFd, OwnedFd> = Socket::new(dup_stdin());
        assert!(socket.fd().is_some());
        let failed = |_| Err(io::Error::from_raw_os_error(Errno::MFILE.raw_os_error()));
        let lost = socket.live("stream", failed).map(|_| ());
        assert_eq!(lost, Err(Errno::MFILE));
        assert!(socket.fd().is_none());
    }

    #[test]
    fn a_registration_runs_once() {
        let mut socket: Socket<u8, u8> = Socket::new(3);
        assert_eq!(socket.live("stream", |n| Ok(n + 1)), Ok(&mut 4));
        let again = |_| unreachable!("a live socket registers no second time");
        assert_eq!(socket.live("stream", again), Ok(&mut 4));
    }
}

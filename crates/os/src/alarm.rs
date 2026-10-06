//! A one-shot OS timer that wakes a task within microseconds of its time.

use std::io;
use std::os::fd::OwnedFd;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

/// A one-shot OS timer in the reactor of a Tokio runtime. Linux gives it no slack, and
/// macOS does not coalesce it.
pub(crate) struct Alarm {
    fd: AsyncFd<OwnedFd>,
}

impl Alarm {
    /// An alarm that is not armed, in the reactor of the current runtime.
    ///
    /// # Errors
    ///
    /// The error of the OS when it gives no timer, as when the process has no free fd.
    ///
    /// # Panics
    ///
    /// When no Tokio runtime is current, or it has no I/O driver.
    pub(crate) fn new() -> io::Result<Self> {
        let fd = AsyncFd::with_interest(create()?, Interest::READABLE)?;
        Ok(Self { fd })
    }

    /// Fires the alarm once, `wait` from now, in place of the time it was armed for.
    ///
    /// # Panics
    ///
    /// When `wait` is zero or the OS refuses the timer.
    pub(crate) fn arm(&self, wait: Duration) {
        assert!(!wait.is_zero(), "an alarm waits more than zero");
        set(self.fd.get_ref(), wait)
            .unwrap_or_else(|error| panic!("the OS refused to arm a timer: {error}"));
    }

    /// Ready once the alarm fires, and then pending until it fires again. Until then
    /// it wakes `cx` when it fires. A fire from before the last `arm` may still come
    /// out.
    pub(crate) fn poll_fired(&self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            let mut guard = ready!(self.fd.poll_read_ready(cx))
                .unwrap_or_else(|error| panic!("the reactor stopped: {error}"));
            let fired = clear(self.fd.get_ref());
            // The read took each fire, so no fire is lost.
            guard.clear_ready();
            match fired {
                Ok(()) => return Poll::Ready(()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("the OS refused to read a timer: {error}"),
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn create() -> io::Result<OwnedFd> {
    use rustix::time::{TimerfdClockId, TimerfdFlags, timerfd_create};

    let flags = TimerfdFlags::NONBLOCK | TimerfdFlags::CLOEXEC;
    Ok(timerfd_create(TimerfdClockId::Monotonic, flags)?)
}

/// Arms `fd` for `wait`. A timerfd forgets its old fires.
#[cfg(target_os = "linux")]
fn set(fd: &OwnedFd, wait: Duration) -> io::Result<()> {
    use rustix::time::{Itimerspec, TimerfdTimerFlags, Timespec, timerfd_settime};

    let spec = Itimerspec {
        it_interval: Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: Timespec {
            tv_sec: i64::try_from(wait.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: wait.subsec_nanos().into(),
        },
    };
    timerfd_settime(fd, TimerfdTimerFlags::empty(), &spec)?;
    Ok(())
}

/// Takes the fires of `fd`, or fails with `WouldBlock` when there is none.
#[cfg(target_os = "linux")]
fn clear(fd: &OwnedFd) -> io::Result<()> {
    let mut count = [0; 8];
    rustix::io::read(fd, &mut count)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn create() -> io::Result<OwnedFd> {
    use std::os::fd::FromRawFd;

    // SAFETY: the call takes no argument.
    let fd = unsafe { libc::kqueue() };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a new kqueue, and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Arms the one timer of the kqueue `fd` for `wait`.
#[cfg(target_os = "macos")]
fn set(fd: &OwnedFd, wait: Duration) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let change = libc::kevent {
        ident: 0,
        filter: libc::EVFILT_TIMER,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        // Without NOTE_CRITICAL, xnu adds about 220 us of leeway to a 1 ms timer.
        fflags: libc::NOTE_NSECONDS | libc::NOTE_CRITICAL,
        data: isize::try_from(wait.as_nanos()).unwrap_or(isize::MAX),
        udata: std::ptr::null_mut(),
    };
    // SAFETY: one change, read from `change`, and no room for events.
    let rc = unsafe {
        libc::kevent(
            fd.as_raw_fd(),
            &raw const change,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Takes the fire of the kqueue `fd`, or fails with `WouldBlock` when there is none.
#[cfg(target_os = "macos")]
fn clear(fd: &OwnedFd) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    // SAFETY: zero is a value of each field of a kevent.
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    let now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: room for one event in `event`, and a zero timeout, so the call does not
    // block.
    let count = unsafe {
        libc::kevent(
            fd.as_raw_fd(),
            std::ptr::null(),
            0,
            &raw mut event,
            1,
            &raw const now,
        )
    };
    match count {
        -1 => Err(io::Error::last_os_error()),
        0 => Err(io::ErrorKind::WouldBlock.into()),
        _ => Ok(()),
    }
}

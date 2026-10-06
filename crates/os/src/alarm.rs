//! A one-shot wait that ends within microseconds of its time.

use std::io;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::time::Sleep;

/// The end of a wait that an OS timer covers. Tokio's timer rounds a deadline up to
/// the next millisecond and can wake a millisecond after that.
const TAIL: Duration = Duration::from_millis(2);

/// A one-shot wait on a Tokio runtime: a Tokio sleep until `TAIL` before its end, and
/// an OS timer for the rest. Linux gives the OS timer no slack, and macOS does not
/// coalesce it.
pub(crate) struct Alarm {
    sleep: Pin<Box<Sleep>>,
    /// The OS timer, made at the first tail. `None` while the OS has no fd or memory
    /// for it, and then the sleep covers the tail too.
    fd: Option<AsyncFd<OwnedFd>>,
    /// The timer that is armed and has not fired.
    armed: Option<Via>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Via {
    Sleep,
    Fd,
}

impl Alarm {
    /// An alarm that is not armed, on the current Tokio runtime.
    ///
    /// # Panics
    ///
    /// When no Tokio runtime is current, or it has no timer.
    pub(crate) fn new() -> Self {
        Self {
            sleep: Box::pin(tokio::time::sleep(Duration::ZERO)),
            fd: None,
            armed: None,
        }
    }

    /// Arms the alarm for `wait` from now, in place of its last arm. A wait longer than
    /// `TAIL` ends `TAIL` early, so the caller arms again for the rest.
    ///
    /// # Panics
    ///
    /// When `wait` is zero, the current runtime has no I/O driver, or the OS refuses a
    /// timer for a cause other than no fd or memory.
    pub(crate) fn arm(&mut self, wait: Duration) {
        assert!(!wait.is_zero(), "an alarm waits more than zero");
        if wait <= TAIL && self.fd.is_none() {
            self.fd = timer();
        }
        // An arm left on the other timer would wake the task for nothing.
        if let Some(fd) = self.fd.as_ref().filter(|_| wait <= TAIL) {
            if self.armed == Some(Via::Sleep) {
                self.sleep.as_mut().reset(tokio::time::Instant::now() + FAR);
            }
            set(fd.get_ref(), wait).unwrap_or_else(|error| {
                panic!("the OS refused to arm a timer: {error}")
            });
            self.armed = Some(Via::Fd);
        } else {
            if let (Some(Via::Fd), Some(fd)) = (self.armed, &self.fd) {
                disarm(fd.get_ref()).unwrap_or_else(|error| {
                    panic!("the OS refused to disarm a timer: {error}")
                });
            }
            let lead = if wait > TAIL {
                wait.saturating_sub(TAIL)
            } else {
                wait
            };
            self.sleep
                .as_mut()
                .reset(tokio::time::Instant::now() + lead);
            self.armed = Some(Via::Sleep);
        }
    }

    /// Ready when the armed wait ends, and until then wakes `cx` when it ends. Ready
    /// when the alarm is not armed.
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let fired = match (self.armed, &self.fd) {
            (None, _) => return Poll::Ready(()),
            (Some(Via::Fd), Some(fd)) => poll_fired(fd, cx),
            _ => self.sleep.as_mut().poll(cx),
        };
        if fired.is_ready() {
            self.armed = None;
        }
        fired
    }
}

/// How far a Tokio sleep moves when the OS timer takes over, so that it does not fire.
const FAR: Duration = Duration::from_hours(24 * 365);

/// A new OS timer in the reactor of the current runtime, or `None` when the OS has no
/// fd or memory for it.
///
/// # Panics
///
/// When the runtime has no I/O driver, or the OS refuses the timer for another cause.
fn timer() -> Option<AsyncFd<OwnedFd>> {
    match create().and_then(|fd| AsyncFd::with_interest(fd, Interest::READABLE)) {
        Ok(fd) => Some(fd),
        Err(error) if spent(&error) => None,
        Err(error) => panic!("the OS refused a timer: {error}"),
    }
}

/// Whether `error` says that the process or the system has no fd or memory to spare.
fn spent(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOMEM | libc::ENOSPC)
    )
}

/// Ready once `fd` fires, and then pending until it fires again.
fn poll_fired(fd: &AsyncFd<OwnedFd>, cx: &mut Context<'_>) -> Poll<()> {
    loop {
        let mut guard = ready!(fd.poll_read_ready(cx))
            .unwrap_or_else(|error| panic!("the reactor stopped: {error}"));
        let fired = clear(fd.get_ref());
        // The read took each fire, so no fire is lost.
        guard.clear_ready();
        match fired {
            Ok(()) => return Poll::Ready(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("the OS refused to read a timer: {error}"),
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

/// Stops `fd`. A zero wait stops a timerfd and forgets its fires.
#[cfg(target_os = "linux")]
fn disarm(fd: &OwnedFd) -> io::Result<()> {
    set(fd, Duration::ZERO)
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
    // Without NOTE_CRITICAL, xnu adds about 220 us of leeway to a 1 ms timer.
    let notes = libc::NOTE_NSECONDS | libc::NOTE_CRITICAL;
    let data = isize::try_from(wait.as_nanos()).unwrap_or(isize::MAX);
    change(fd, libc::EV_ADD | libc::EV_ONESHOT, notes, data)
}

/// Removes the timer of the kqueue `fd` and a fire that is not taken.
#[cfg(target_os = "macos")]
fn disarm(fd: &OwnedFd) -> io::Result<()> {
    change(fd, libc::EV_DELETE, 0, 0)
}

/// Applies one change to the timer of the kqueue `fd`.
#[cfg(target_os = "macos")]
fn change(fd: &OwnedFd, action: u16, notes: u32, data: isize) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let change = libc::kevent {
        ident: 0,
        filter: libc::EVFILT_TIMER,
        flags: action,
        fflags: notes,
        data,
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

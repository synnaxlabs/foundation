//! The monotonic clock of the node: the raw clock plus the time asleep.

use std::io;
use std::mem::MaybeUninit;
use std::pin::Pin;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::time::Sleep;
use types::time::Monotonic;

/// Reads the boot clock, which counts time asleep, from the instant it was made.
#[derive(Clone)]
pub(crate) struct Driver {
    /// The boot clock when the driver was made.
    origin: u64,
    epoch: Instant,
    /// The largest time asleep read so far, in nanoseconds, shared by the clones.
    #[cfg(target_os = "linux")]
    asleep: Arc<AtomicU64>,
}

impl Driver {
    #[expect(clippy::disallowed_methods, reason = "os reads the OS clock")]
    pub(crate) fn new() -> Self {
        let mut driver = Self {
            origin: 0,
            epoch: Instant::now(),
            #[cfg(target_os = "linux")]
            asleep: Arc::new(AtomicU64::new(0)),
        };
        driver.origin = driver.boot();
        driver
    }

    /// The boot clock in nanoseconds. It never goes back, on any thread.
    #[cfg(target_os = "macos")]
    #[expect(clippy::unused_self, reason = "Linux reads the shared time asleep")]
    fn boot(&self) -> u64 {
        read(libc::CLOCK_MONOTONIC_RAW)
    }

    /// The boot clock in nanoseconds. It never goes back, on any thread.
    ///
    /// The raw clock has no slew. The time asleep is the difference of two slewed
    /// clocks, which only grows, and a measurement of it is low by the gap between
    /// its two reads, so the largest measurement is the closest.
    #[cfg(target_os = "linux")]
    fn boot(&self) -> u64 {
        let boot = read(libc::CLOCK_BOOTTIME);
        let monotonic = read(libc::CLOCK_MONOTONIC);
        let raw = read(libc::CLOCK_MONOTONIC_RAW);
        let measured = asleep(boot, monotonic);
        let mut largest = self.asleep.load(Relaxed);
        if measured > largest {
            largest = self.asleep.fetch_max(measured, Relaxed).max(measured);
        }
        raw + largest
    }
}

/// The time asleep from one read of the boot-time clock and one of the monotonic
/// clock, in that order. Never more than the true value.
#[cfg(any(test, target_os = "linux"))]
fn asleep(boot: u64, monotonic: u64) -> u64 {
    boot.saturating_sub(monotonic)
}

impl env::clock::Driver for Driver {
    fn now(&self) -> Monotonic {
        Monotonic(self.boot() - self.origin)
    }

    fn epoch(&self) -> Instant {
        self.epoch
    }

    fn timer(&self) -> Pin<Box<dyn env::clock::Timer>> {
        Box::pin(Timer {
            driver: self.clone(),
            sleep: Box::pin(tokio::time::sleep(Duration::ZERO)),
            armed: None,
        })
    }
}

/// Reads `clock` in nanoseconds.
fn read(clock: libc::clockid_t) -> u64 {
    let mut time = MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: `time` is storage for one timespec, which the call writes.
    let rc = unsafe { libc::clock_gettime(clock, time.as_mut_ptr()) };
    assert_eq!(
        rc,
        0,
        "clock_gettime failed: {}",
        io::Error::last_os_error()
    );
    // SAFETY: a return of 0 means the call wrote `time`.
    let time = unsafe { time.assume_init() };
    let seconds = u64::try_from(time.tv_sec).expect("the clock is after boot");
    let nanos = u64::try_from(time.tv_nsec).expect("the clock is after boot");
    seconds * 1_000_000_000 + nanos
}

/// A Tokio sleep that fires at or after a deadline on the boot clock. Tokio's clock
/// may stop in a suspend and slews on Linux, so each poll checks the boot clock and
/// re-arms the sleep from it.
struct Timer {
    driver: Driver,
    sleep: Pin<Box<Sleep>>,
    /// The deadline the sleep is armed for.
    armed: Option<Monotonic>,
}

impl env::clock::Timer for Timer {
    fn poll_until(
        self: Pin<&mut Self>,
        deadline: Monotonic,
        cx: &mut Context<'_>,
    ) -> Poll<()> {
        let this = self.get_mut();
        loop {
            let now = env::clock::Driver::now(&this.driver);
            if now >= deadline {
                return Poll::Ready(());
            }
            if this.armed != Some(deadline) {
                let wait = Duration::from_nanos(deadline.0 - now.0);
                let at = tokio::time::Instant::now() + wait;
                this.sleep.as_mut().reset(at);
                this.armed = Some(deadline);
            }
            match this.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => this.armed = None,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// Steps of the true time asleep, each with the gap of a read that follows it.
    fn steps() -> impl Strategy<Value = Vec<(u64, u64)>> {
        prop::collection::vec((0..1_000_000u64, 0..1_000u64), 0..64)
    }

    proptest! {
        #[test]
        fn the_largest_measurement_never_goes_back_or_over_the_truth(steps in steps()) {
            let (mut truth, mut largest) = (0u64, 0u64);
            for (step, gap) in steps {
                truth += step;
                let monotonic = 5_000_000_000 + gap;
                let measured = asleep(5_000_000_000 + truth, monotonic);
                prop_assert!(measured <= truth);
                let next = largest.max(measured);
                prop_assert!(next >= largest);
                prop_assert!(next <= truth);
                largest = next;
            }
        }
    }

    #[test]
    fn a_measurement_below_zero_is_zero() {
        assert_eq!(asleep(100, 150), 0);
        assert_eq!(asleep(150, 100), 50);
    }
}

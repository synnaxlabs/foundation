//! The monotonic clock of the node: the raw clock plus the time asleep.

use std::io;
use std::mem::MaybeUninit;
use std::pin::Pin;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(any(test, target_os = "linux"))]
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::time::Sleep;
use types::time::Monotonic;

use crate::alarm::Alarm;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("os orders a clock read only on x86_64 and aarch64");

/// Reads the boot clock, which counts time asleep, from the instant it was made.
#[derive(Clone)]
pub(crate) struct Driver {
    /// The boot clock when the driver was made.
    origin_ns: u64,
    epoch: Instant,
    /// The largest time asleep read so far, shared by the clones.
    #[cfg(target_os = "linux")]
    asleep_ns: Arc<AtomicU64>,
}

impl Driver {
    #[expect(clippy::disallowed_methods, reason = "os reads the OS clock")]
    pub(crate) fn new() -> Self {
        let mut driver = Self {
            origin_ns: 0,
            epoch: Instant::now(),
            #[cfg(target_os = "linux")]
            asleep_ns: Arc::new(AtomicU64::new(0)),
        };
        driver.origin_ns = driver.boot_ns();
        driver
    }

    /// The boot clock. It never goes back, on any thread.
    #[cfg_attr(
        target_os = "macos",
        expect(clippy::unused_self, reason = "Linux reads the shared time asleep")
    )]
    fn boot_ns(&self) -> u64 {
        // On macOS this clock is `mach_continuous_time`, which counts time asleep.
        #[cfg(target_os = "macos")]
        return read_ns(libc::CLOCK_MONOTONIC_RAW);
        #[cfg(target_os = "linux")]
        return combine(
            &self.asleep_ns,
            read_ns(libc::CLOCK_BOOTTIME),
            read_ns(libc::CLOCK_MONOTONIC),
            read_ns(libc::CLOCK_MONOTONIC_RAW),
        );
    }
}

/// The boot clock from one read each of the boot-time, monotonic, and raw clocks, in
/// that order. Raises `asleep_ns`, the largest time asleep read so far, to this read.
///
/// The raw clock has no slew. The time asleep is the difference of two slewed clocks,
/// which only grows, and a measurement of it is low by the gap between its two reads,
/// so the largest measurement is the closest.
#[cfg(any(test, target_os = "linux"))]
fn combine(asleep_ns: &AtomicU64, boot_ns: u64, monotonic_ns: u64, raw_ns: u64) -> u64 {
    let measured = boot_ns.saturating_sub(monotonic_ns);
    let mut largest = asleep_ns.load(Relaxed);
    if measured > largest {
        largest = asleep_ns.fetch_max(measured, Relaxed).max(measured);
    }
    raw_ns + largest
}

impl env::clock::Driver for Driver {
    fn now(&self) -> Monotonic {
        // The vDSO and the macOS commpage read the counter with no fence that waits
        // for earlier stores or holds back later loads. The fences give that order.
        settle();
        let boot_ns = self.boot_ns();
        hold();
        Monotonic(boot_ns - self.origin_ns)
    }

    fn epoch(&self) -> Instant {
        self.epoch
    }

    fn timer(&self) -> Pin<Box<dyn env::clock::Timer>> {
        Box::pin(Timer {
            driver: self.clone(),
            sleep: Box::pin(tokio::time::sleep(Duration::ZERO)),
            alarm: None,
            armed: None,
        })
    }
}

/// Waits until the loads and stores before it are done, and keeps a counter read
/// after it from running before then.
fn settle() {
    // SAFETY: fences that touch no memory and no register.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::asm!("mfence", "lfence", options(nostack, preserves_flags));
    }
    // SAFETY: as above.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!("dsb ish", "isb", options(nostack, preserves_flags));
    }
}

/// Keeps the loads and stores after it from running before the counter read before
/// it is done.
fn hold() {
    // SAFETY: a fence that touches no memory and no register.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::asm!("lfence", options(nostack, preserves_flags));
    }
    // SAFETY: as above.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!("isb", options(nostack, preserves_flags));
    }
}

/// Reads `clock`.
fn read_ns(clock: libc::clockid_t) -> u64 {
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

/// The longest wait a Tokio sleep is armed for. Tokio's clock stops in a suspend and
/// slews on Linux, so a sleep completes late by at most this much from either.
const ARM_MAX: Duration = Duration::from_secs(1);

/// The end of a wait that an alarm covers. Tokio's timer rounds a deadline up to the
/// next millisecond and can wake a millisecond after that.
const TAIL: Duration = Duration::from_millis(2);

/// A timer that fires at or after a deadline on the boot clock: a Tokio sleep until
/// the tail of the wait, then an alarm. Each poll checks the boot clock and re-arms
/// from it.
struct Timer {
    driver: Driver,
    sleep: Pin<Box<Sleep>>,
    /// The alarm, made at the first tail. `None` while the OS gives no timer, and then
    /// the sleep covers the tail too.
    alarm: Option<Alarm>,
    /// The deadline and the timer armed for it.
    armed: Option<(Monotonic, Via)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Via {
    Sleep,
    Alarm,
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
            let wait = Duration::from_nanos(deadline.0 - now.0);
            if wait <= TAIL && this.alarm.is_none() {
                this.alarm = Alarm::new().ok();
            }
            let alarm = this.alarm.as_ref().filter(|_| wait <= TAIL);
            // Ends by the second pass: a timer armed from its own clock is pending.
            let fired = if let Some(alarm) = alarm {
                if this.armed != Some((deadline, Via::Alarm)) {
                    alarm.arm(wait);
                    this.armed = Some((deadline, Via::Alarm));
                }
                alarm.poll_fired(cx)
            } else {
                if this.armed != Some((deadline, Via::Sleep)) {
                    let lead = if wait > TAIL {
                        wait.saturating_sub(TAIL)
                    } else {
                        wait
                    };
                    let at = tokio::time::Instant::now() + lead.min(ARM_MAX);
                    this.sleep.as_mut().reset(at);
                    this.armed = Some((deadline, Via::Sleep));
                }
                this.sleep.as_mut().poll(cx)
            };
            match fired {
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

    const SECOND: u64 = 1_000_000_000;

    #[test]
    fn a_resume_adds_the_time_asleep_to_the_raw_clock() {
        let asleep_ns = AtomicU64::new(0);
        assert_eq!(
            combine(&asleep_ns, 10 * SECOND, 5 * SECOND, 3 * SECOND),
            8 * SECOND
        );
        assert_eq!(asleep_ns.load(Relaxed), 5 * SECOND);
    }

    #[test]
    fn a_lower_measurement_keeps_the_largest() {
        let asleep_ns = AtomicU64::new(5 * SECOND);
        assert_eq!(
            combine(&asleep_ns, 10 * SECOND, 6 * SECOND, 4 * SECOND),
            9 * SECOND
        );
        assert_eq!(asleep_ns.load(Relaxed), 5 * SECOND);
    }

    #[test]
    fn a_measurement_below_zero_adds_nothing() {
        let asleep_ns = AtomicU64::new(0);
        assert_eq!(combine(&asleep_ns, 100, 150, 7), 7);
        assert_eq!(asleep_ns.load(Relaxed), 0);
    }

    /// Steps of the true time asleep, each with the gap of a read that follows it.
    fn steps() -> impl Strategy<Value = Vec<(u64, u64)>> {
        prop::collection::vec((0..1_000_000u64, 0..1_000u64), 0..64)
    }

    proptest! {
        #[test]
        fn the_boot_clock_never_goes_back_or_past_the_truth(steps in steps()) {
            let asleep_ns = AtomicU64::new(0);
            let (mut truth, mut raw, mut last) = (0u64, 0u64, 0u64);
            for (step, gap) in steps {
                truth += step;
                raw += 1_000;
                let monotonic = 5 * SECOND + gap;
                let boot = combine(&asleep_ns, 5 * SECOND + truth, monotonic, raw);
                prop_assert!(boot >= last, "{boot} after {last}");
                prop_assert!(boot <= raw + truth, "{boot} past {}", raw + truth);
                last = boot;
            }
        }
    }
}

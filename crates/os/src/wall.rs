//! The OS wall clock and its error bound, read through the NTP interface.

use std::ffi::c_int;
use std::io;

use env::wall::Reading;
use types::time::{Span, Stamp};

/// Reads the OS wall clock and the bound its time daemon keeps.
pub(crate) struct Driver;

impl env::wall::Driver for Driver {
    fn now(&self) -> Reading {
        read()
            .unwrap_or_else(|error| panic!("the OS refused a wall clock read: {error}"))
    }
}

/// Reads the wall clock and its bound in one call.
///
/// # Errors
///
/// The error of the OS when it refuses the call, as a seccomp filter can.
pub(crate) fn read() -> io::Result<Reading> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: every field of a timex is an integer, and zero is a value of each.
        let mut timex: libc::timex = unsafe { std::mem::zeroed() };
        // SAFETY: `timex` is one timex, and with `modes` 0 the call only reads the
        // clock.
        let state = unsafe { libc::adjtimex(&raw mut timex) };
        if state == -1 {
            return Err(io::Error::last_os_error());
        }
        let fraction = fraction_ns(timex.time.tv_usec, timex.status);
        Ok(Reading {
            time: stamp(timex.time.tv_sec, fraction),
            error: bound(state, timex.maxerror),
        })
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: every field of an ntptimeval is an integer, and zero is a value of
        // each.
        let mut ntp: libc::ntptimeval = unsafe { std::mem::zeroed() };
        // SAFETY: `ntp` is one ntptimeval, which the call writes.
        let rc = unsafe { libc::ntp_gettime(&raw mut ntp) };
        let error = io::Error::last_os_error();
        if refused(rc, error.raw_os_error(), ntp.time_state) {
            return Err(error);
        }
        Ok(Reading {
            time: stamp(ntp.time.tv_sec, ntp.time.tv_nsec),
            error: bound(ntp.time_state, ntp.maxerror),
        })
    }
}

/// The fraction of a second of an `adjtimex` reading with `status`, which gives it in
/// nanoseconds with `STA_NANO` and in microseconds without.
#[cfg(any(test, target_os = "linux"))]
fn fraction_ns(fraction: impl Into<i64>, status: c_int) -> i64 {
    let fraction = fraction.into();
    if status & libc::STA_NANO == 0 {
        fraction * 1_000
    } else {
        fraction
    }
}

/// Whether a call of `ntp_gettime` that returned `rc` with error number `error` gave
/// no reading. xnu writes the reading, then gives each clock state `state` but
/// `TIME_OK` as the error number.
#[cfg(any(test, target_os = "macos"))]
fn refused(rc: c_int, error: Option<i32>, state: c_int) -> bool {
    rc != 0 && error != Some(state)
}

/// The stamp of `seconds` and `fraction_ns`. The integers are as wide as the target
/// makes them.
///
/// # Panics
///
/// When the stamp is past the end of `Stamp`.
fn stamp(seconds: impl Into<i64>, fraction_ns: impl Into<i64>) -> Stamp {
    let nanos = (seconds.into().checked_mul(1_000_000_000))
        .and_then(|seconds| seconds.checked_add(fraction_ns.into()))
        .expect("the OS wall clock is before the year 2262");
    Stamp::from_nanos(nanos)
}

/// The error bound of a reading with clock state `state` and `maxerror_us`: `None`
/// when the clock is not in sync, or the bound is negative or past the end of a
/// `Span`. Root can set any `maxerror`.
fn bound(state: c_int, maxerror_us: impl Into<i64>) -> Option<Span> {
    let maxerror_us = maxerror_us.into();
    let nanos = maxerror_us.checked_mul(1_000)?;
    (state != libc::TIME_ERROR && maxerror_us >= 0).then(|| Span::from_nanos(nanos))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn a_fraction_is_in_nanoseconds_only_with_sta_nano() {
        assert_eq!(fraction_ns(5, libc::STA_NANO), 5);
        assert_eq!(fraction_ns(5, libc::STA_NANO | libc::STA_PLL), 5);
        assert_eq!(fraction_ns(5, 0), 5_000);
        assert_eq!(fraction_ns(5, libc::STA_PLL), 5_000);
    }

    #[test]
    fn a_stamp_adds_the_fraction_to_the_seconds() {
        assert_eq!(stamp(2i64, 5i64), Stamp::from_nanos(2_000_000_005));
    }

    #[test]
    fn a_state_given_as_the_error_is_a_reading_and_a_fault_is_not() {
        assert!(!refused(0, None, libc::TIME_OK));
        assert!(!refused(0, Some(libc::EFAULT), libc::TIME_OK));
        assert!(!refused(-1, Some(libc::TIME_ERROR), libc::TIME_ERROR));
        assert!(!refused(-1, Some(libc::TIME_INS), libc::TIME_INS));
        assert!(refused(-1, Some(libc::EFAULT), libc::TIME_OK));
        assert!(refused(-1, None, libc::TIME_OK));
    }

    #[test]
    fn a_bound_past_the_end_of_a_span_is_none() {
        assert_eq!(bound(libc::TIME_OK, i64::MAX / 1_000 + 1), None);
        assert_eq!(bound(libc::TIME_OK, i64::MAX), None);
        assert_eq!(bound(libc::TIME_OK, i64::MIN), None);
    }

    #[test]
    #[should_panic(expected = "the OS wall clock is before the year 2262")]
    fn a_stamp_past_the_end_panics() {
        let _: Stamp = stamp(i64::MAX, 0i64);
    }

    proptest! {
        #[test]
        fn each_state_in_sync_gives_the_bound_in_microseconds(
            state in 0..libc::TIME_ERROR,
            maxerror in 0..=i64::MAX / 1_000,
        ) {
            let expected = Some(Span::from_nanos(maxerror * 1_000));
            prop_assert_eq!(bound(state, maxerror), expected);
        }

        #[test]
        fn an_error_state_gives_none_at_any_bound(
            maxerror in -(i64::MAX / 1_000)..=i64::MAX / 1_000,
        ) {
            prop_assert_eq!(bound(libc::TIME_ERROR, maxerror), None);
        }

        #[test]
        fn a_negative_bound_gives_none_in_any_state(
            state in 0..=libc::TIME_ERROR,
            maxerror in -(i64::MAX / 1_000)..0,
        ) {
            prop_assert_eq!(bound(state, maxerror), None);
        }
    }
}

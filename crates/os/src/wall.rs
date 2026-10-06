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
    }
}

#[cfg(target_os = "linux")]
fn read() -> Reading {
    // SAFETY: every field of a timex is an integer, and zero is a value of each.
    let mut timex: libc::timex = unsafe { std::mem::zeroed() };
    // SAFETY: `timex` is one timex, and with `modes` 0 the call only reads the clock.
    let state = unsafe { libc::adjtimex(&raw mut timex) };
    assert!(
        state >= 0,
        "adjtimex failed: {}",
        io::Error::last_os_error()
    );
    let nano = timex.status & libc::STA_NANO != 0;
    Reading {
        time: stamp(timex.time.tv_sec, timex.time.tv_usec, nano),
        error: bound(state, timex.maxerror),
    }
}

#[cfg(target_os = "macos")]
fn read() -> Reading {
    // SAFETY: every field of an ntptimeval is an integer, and zero is a value of each.
    let mut ntp: libc::ntptimeval = unsafe { std::mem::zeroed() };
    // SAFETY: `ntp` is one ntptimeval, which the call writes.
    let rc = unsafe { libc::ntp_gettime(&raw mut ntp) };
    assert_eq!(rc, 0, "ntp_gettime failed: {}", io::Error::last_os_error());
    Reading {
        time: stamp(ntp.time.tv_sec, ntp.time.tv_nsec, true),
        error: bound(ntp.time_state, ntp.maxerror),
    }
}

/// The stamp of `seconds` and a fraction in nanoseconds when `nano`, else in
/// microseconds. The integers are as wide as the target makes them.
///
/// # Panics
///
/// When the stamp is past the end of `Stamp`.
fn stamp(seconds: impl Into<i64>, fraction: impl Into<i64>, nano: bool) -> Stamp {
    let fraction = fraction.into();
    let fraction = if nano { fraction } else { fraction * 1_000 };
    let nanos = (seconds.into().checked_mul(1_000_000_000))
        .and_then(|seconds| seconds.checked_add(fraction))
        .expect("the OS wall clock is before the year 2262");
    Stamp::from_nanos(nanos)
}

/// The error bound of a reading with clock state `state` and a `maxerror` in
/// microseconds: `None` when the clock is not in sync or the bound is negative.
fn bound(state: c_int, maxerror: impl Into<i64>) -> Option<Span> {
    let maxerror = maxerror.into();
    (state != libc::TIME_ERROR && maxerror >= 0)
        .then(|| Span::from_nanos(maxerror * 1_000))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn a_nano_fraction_is_nanoseconds_and_a_micro_fraction_is_microseconds() {
        assert_eq!(stamp(2i64, 5i64, true), Stamp::from_nanos(2_000_000_005));
        assert_eq!(stamp(2i64, 5i64, false), Stamp::from_nanos(2_000_005_000));
    }

    #[test]
    #[should_panic(expected = "the OS wall clock is before the year 2262")]
    fn a_stamp_past_the_end_panics() {
        let _ = stamp(i64::MAX, 0i64, true);
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
        fn a_negative_bound_or_an_error_state_gives_none(
            state in 0..=libc::TIME_ERROR,
            maxerror in i64::MIN / 1_000..=i64::MAX / 1_000,
        ) {
            prop_assume!(state == libc::TIME_ERROR || maxerror < 0);
            prop_assert_eq!(bound(state, maxerror), None);
        }
    }
}

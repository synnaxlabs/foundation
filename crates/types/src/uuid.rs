//! The UUID text form that channel and node keys share, and the error of a text that
//! is not one.

use std::fmt;

/// The bits of a UUIDv7 from `time`, in whole milliseconds, and the low 74 bits of
/// `random`. Panics when `time` is before the Unix epoch.
pub(crate) fn v7(time: crate::time::Stamp, random: u128) -> u128 {
    let nanos = time.nanos();
    let millis =
        u128::try_from(nanos.div_euclid(1_000_000)).unwrap_or_else(|_before| {
            panic!("invariant: a key is made after the Unix epoch, not at {nanos} ns")
        });
    let rand_a = (random >> 62) & 0xfff;
    let rand_b = random & ((1 << 62) - 1);
    millis << 80 | 0x7 << 76 | rand_a << 64 | 0b10 << 62 | rand_b
}

/// Writes `bits` as a lowercase hyphenated UUID string.
pub(crate) fn write(bits: u128, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
        f,
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        bits >> 96,
        bits >> 80 & 0xffff,
        bits >> 64 & 0xffff,
        bits >> 48 & 0xffff,
        bits & 0xffff_ffff_ffff
    )
}

/// Reads a hyphenated UUID string, in either case.
pub(crate) fn read(s: &str) -> Result<u128, Error> {
    if s.len() != 36 {
        return Err(Error);
    }
    let mut bits = 0_u128;
    for (i, b) in s.bytes().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            if b != b'-' {
                return Err(Error);
            }
            continue;
        }
        let digit = char::from(b).to_digit(16).ok_or(Error)?;
        bits = bits << 4 | u128::from(digit);
    }
    Ok(bits)
}

/// A text that is not a hyphenated UUID. `Display` gives the message: a lower-case
/// clause with no final period. [`Error::fix`] gives what to do instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Error;

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(self) -> &'static str {
        "Write 32 hex digits as 8-4-4-4-12, such as \
         0192540a-6f00-7000-8000-000000000000"
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a key is not a hyphenated UUID")
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_the_problem_and_the_fix() {
        assert_eq!(Error.to_string(), "a key is not a hyphenated UUID");
        assert_eq!(
            Error.fix(),
            "Write 32 hex digits as 8-4-4-4-12, such as \
             0192540a-6f00-7000-8000-000000000000"
        );
        crate::common::assert_stated(&Error.to_string(), Error.fix());
    }
}

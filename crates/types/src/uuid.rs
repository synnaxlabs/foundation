//! The UUID text form that channel and node keys share.

use std::fmt;

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
pub(crate) fn read(s: &str) -> Result<u128, crate::ParseError> {
    let error = || crate::ParseError {
        input: s.into(),
        expected: "a hyphenated UUID such as 0192540a-6f00-7000-8000-000000000000",
    };
    if s.len() != 36 {
        return Err(error());
    }
    let mut bits = 0_u128;
    for (i, b) in s.bytes().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            if b != b'-' {
                return Err(error());
            }
            continue;
        }
        let digit = char::from(b).to_digit(16).ok_or_else(error)?;
        bits = bits << 4 | u128::from(digit);
    }
    Ok(bits)
}

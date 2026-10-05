//! Byte sizes.

use std::fmt;
use std::str::FromStr;

use crate::ParseError;
use crate::quantity;

/// A count of bytes, such as a disk or pool budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Size(u64);

impl Size {
    /// No bytes.
    pub const ZERO: Self = Self(0);
    /// One kibibyte: 1024 bytes.
    pub const KIBIBYTE: Self = Self(1 << 10);
    /// One mebibyte: 1024 kibibytes.
    pub const MEBIBYTE: Self = Self(1 << 20);
    /// One gibibyte: 1024 mebibytes.
    pub const GIBIBYTE: Self = Self(1 << 30);
    /// One tebibyte: 1024 gibibytes.
    pub const TEBIBYTE: Self = Self(1 << 40);

    /// Wraps a count of bytes.
    #[must_use]
    pub const fn from_bytes(bytes: u64) -> Self {
        Self(bytes)
    }

    /// The count of bytes.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

/// Each unit and its size as a power of two, from the largest.
const UNITS: [(u32, &str); 5] =
    [(40, "TiB"), (30, "GiB"), (20, "MiB"), (10, "KiB"), (0, "B")];

impl fmt::Display for Size {
    /// Writes the size in the largest unit that divides it, with no fraction:
    /// `200GiB`, `1536MiB`, `1023B`, `0B`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (shift, unit) = UNITS
            .into_iter()
            .find(|&(shift, _)| self.0 != 0 && self.0.trailing_zeros() >= shift)
            .unwrap_or((0, "B"));
        write!(f, "{}{unit}", self.0 >> shift)
    }
}

impl FromStr for Size {
    type Err = ParseError;

    /// Reads a number and a unit with no space: `B`, `KiB`, `MiB`, `GiB`, or `TiB`.
    /// The number may have a decimal fraction, and must give a whole number of bytes
    /// that fits in a `u64`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let error = |expected| ParseError {
            input: s.into(),
            expected,
        };
        let syntax = || error("a number and a unit, such as 1023B, 1.5GiB, or 200GiB");
        let number = quantity::split(s).ok_or_else(syntax)?;
        let (shift, _) = UNITS
            .into_iter()
            .find(|&(_, unit)| unit == number.unit)
            .ok_or_else(syntax)?;
        let part = part(number.fraction, shift)
            .ok_or_else(|| error("a whole number of bytes"))?;
        let range = || error("a size that fits in a 64-bit count of bytes");
        let whole = number
            .whole
            .bytes()
            .try_fold(0_u128, |n, b| {
                n.checked_mul(10)?.checked_add(u128::from(b - b'0'))
            })
            .and_then(|n| n.checked_mul(1 << shift))
            .ok_or_else(range)?;
        u64::try_from(whole + part)
            .map(Self)
            .map_err(|_overflow| range())
    }
}

/// The bytes that the fraction digits `fraction` give of a unit of `1 << shift` bytes,
/// or `None` when they are not a whole number.
fn part(fraction: &str, shift: u32) -> Option<u128> {
    // The k digits end in a digit other than 0. They give whole bytes only when 5^k
    // divides them, so they end in 5 and are odd, and k is at most `shift`.
    let digits = u32::try_from(fraction.len())
        .ok()
        .filter(|&digits| digits <= shift)?;
    let divisor = 5_u128.pow(digits);
    let (quotient, remainder) =
        fraction
            .bytes()
            .fold((0_u128, 0_u128), |(quotient, remainder), b| {
                let dividend = remainder * 10 + u128::from(b - b'0');
                (quotient * 10 + dividend / divisor, dividend % divisor)
            });
    (remainder == 0).then_some(quotient << (shift - digits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const SYNTAX: &str = "a number and a unit, such as 1023B, 1.5GiB, or 200GiB";
    const EXACT: &str = "a whole number of bytes";
    const RANGE: &str = "a size that fits in a 64-bit count of bytes";
    /// 1 TiB less 1 byte, in TiB.
    const ONE_BYTE_SHORT: &str = "0.9999999999990905052982270717620849609375TiB";

    fn error(input: &str, expected: &'static str) -> ParseError {
        ParseError {
            input: input.into(),
            expected,
        }
    }

    /// The exact decimal of `bytes` in a unit of `1 << shift` bytes, written one
    /// fraction digit at a time.
    fn decimal(bytes: u64, shift: u32, unit: &str) -> String {
        let mask = (1_u64 << shift) - 1;
        let mut text = (bytes >> shift).to_string();
        let mut rest = bytes & mask;
        if rest != 0 {
            text.push('.');
        }
        while rest != 0 {
            rest *= 10;
            let digit = u8::try_from(rest >> shift).expect("a digit");
            text.push(char::from(b'0' + digit));
            rest &= mask;
        }
        text + unit
    }

    proptest! {
        #[test]
        fn sizes_round_trip(bytes in any::<u64>(), shift in 0_u32..=40) {
            let size = Size::from_bytes(bytes & (u64::MAX << shift));
            prop_assert_eq!(size.to_string().parse(), Ok(size));
        }

        #[test]
        fn reads_the_exact_decimal_in_each_unit(
            bytes in any::<u64>(),
            (shift, unit) in prop::sample::select(vec![
                (0, "B"), (10, "KiB"), (20, "MiB"), (30, "GiB"), (40, "TiB"),
            ]),
        ) {
            let text = decimal(bytes, shift, unit);
            prop_assert_eq!(text.parse(), Ok(Size::from_bytes(bytes)), "{}", text);
        }
    }

    #[test]
    fn writes_the_largest_unit_that_divides() {
        for (bytes, text) in [
            (0, "0B"),
            (1, "1B"),
            (1023, "1023B"),
            (1024, "1KiB"),
            (1025, "1025B"),
            (1536 << 20, "1536MiB"),
            (200 << 30, "200GiB"),
            (1 << 40, "1TiB"),
            (u64::MAX << 40, "16777215TiB"),
            (u64::MAX, "18446744073709551615B"),
        ] {
            assert_eq!(Size::from_bytes(bytes).to_string(), text);
        }
    }

    #[test]
    fn reads_a_number_and_a_unit() {
        let largest = format!("16777215{}", &ONE_BYTE_SHORT[1..]);
        for (text, size) in [
            ("0B", Size::ZERO),
            ("1B", Size::from_bytes(1)),
            ("1KiB", Size::KIBIBYTE),
            ("1MiB", Size::MEBIBYTE),
            ("1GiB", Size::GIBIBYTE),
            ("1TiB", Size::TEBIBYTE),
            ("007B", Size::from_bytes(7)),
            ("1.0B", Size::from_bytes(1)),
            ("0.25KiB", Size::from_bytes(256)),
            ("1.5GiB", Size::from_bytes(1_610_612_736)),
            ("1.50MiB", Size::from_bytes(1_572_864)),
            ("16777215TiB", Size::from_bytes(u64::MAX << 40)),
            ("18446744073709551615B", Size::from_bytes(u64::MAX)),
            (
                "0.0000000000009094947017729282379150390625TiB",
                Size::from_bytes(1),
            ),
            (ONE_BYTE_SHORT, Size::from_bytes((1 << 40) - 1)),
            (&largest, Size::from_bytes(u64::MAX)),
        ] {
            assert_eq!(text.parse(), Ok(size), "{text}");
        }
    }

    #[test]
    fn rejects_bad_syntax() {
        for text in [
            "", "200", "GiB", "200 GiB", "200gib", "200GB", "200Gb", "1KB", "1kiB",
            "1PiB", "-1B", "+1B", "1e3B", "1_000B", "1.GiB", ".5GiB", "1.5.5B", "1 B",
            "1B ",
        ] {
            assert_eq!(text.parse::<Size>(), Err(error(text, SYNTAX)), "{text}");
        }
    }

    #[test]
    fn rejects_a_fraction_of_a_byte() {
        let past = format!("0.{}1TiB", "0".repeat(40));
        let half = "0.00000000000045474735088646411895751953125TiB";
        let not_five = "0.0000000000009094947017729282379150390624TiB";
        for text in ["0.5B", "0.3KiB", "1.0000001KiB", half, not_five, &past] {
            assert_eq!(text.parse::<Size>(), Err(error(text, EXACT)), "{text}");
        }
    }

    #[test]
    fn rejects_sizes_that_do_not_fit() {
        let long = "9".repeat(40) + "B";
        for text in [
            "16777216TiB",
            "17179869184GiB",
            "18446744073709551616B",
            &long,
        ] {
            assert_eq!(text.parse::<Size>(), Err(error(text, RANGE)), "{text}");
        }
    }
}

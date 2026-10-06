//! Byte sizes.

use std::fmt;
use std::str::FromStr;

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

/// Each unit, from the largest.
const UNITS: [(Size, &str); 5] = [
    (Size::TEBIBYTE, "TiB"),
    (Size::GIBIBYTE, "GiB"),
    (Size::MEBIBYTE, "MiB"),
    (Size::KIBIBYTE, "KiB"),
    (Size(1), "B"),
];

impl fmt::Display for Size {
    /// Writes the size in the largest unit that divides it, with no fraction:
    /// `200GiB`, `1536MiB`, `1023B`, `0B`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("0B");
        }
        let (unit, name) = UNITS
            .into_iter()
            .find(|(unit, _)| self.0.is_multiple_of(unit.0))
            .expect("invariant: the last unit is one byte");
        write!(f, "{}{name}", self.0 / unit.0)
    }
}

impl FromStr for Size {
    type Err = Error;

    /// Reads a number and a unit with no space: `B`, `KiB`, `MiB`, `GiB`, or `TiB`.
    /// The number may have a decimal fraction, and must give a whole number of bytes
    /// that fits in a `u64`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let number = quantity::split(s).ok_or(Error::Syntax)?;
        let (unit, _) = UNITS
            .into_iter()
            .find(|&(_, name)| name == number.unit)
            .ok_or_else(|| unknown(number.unit))?;
        let part = fraction(number.fraction, unit.0.trailing_zeros())
            .ok_or(Error::Fraction)?;
        number
            .whole
            .bytes()
            .try_fold(0_u64, |n, b| {
                n.checked_mul(10)?.checked_add(u64::from(b - b'0'))
            })
            .and_then(|n| n.checked_mul(unit.0)?.checked_add(part))
            .map(Self)
            .ok_or(Error::Range {
                largest: Self(u64::MAX / unit.0 * unit.0),
            })
    }
}

/// Why a text is not a byte size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The text is not digits, an optional `.` and digits, then a unit of letters,
    /// such as `GiB`, `200 GiB`, or `1e3B`.
    Syntax,
    /// The unit is letters, but not `B`, `KiB`, `MiB`, `GiB`, or `TiB`.
    Unit {
        /// The unit that the text likely means: `GiB` for `gib` or `GB`. `None` for
        /// `Gb` or `b`, which mean bits, and for a unit with no match, such as `PiB`.
        meant: Option<&'static str>,
    },
    /// The number gives part of a byte, such as `0.3B`.
    Fraction,
    /// The size is more than `u64::MAX` bytes.
    Range {
        /// The largest whole number of the text's unit that fits, such as
        /// `16777215TiB`.
        largest: Size,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Syntax | Self::Unit { .. } => {
                "expected a number and a unit, such as 1023B, 1.5GiB, or 200GiB"
            }
            Self::Fraction => "expected a whole number of bytes",
            Self::Range { .. } => {
                "expected a size that fits in a 64-bit count of bytes"
            }
        })
    }
}

impl std::error::Error for Error {}

/// The error for the unit text `unit`, which is not a unit name.
fn unknown(unit: &str) -> Error {
    if !unit.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Error::Syntax;
    }
    let decimal = |name: &str| {
        unit.len() == 2
            && unit.ends_with('B')
            && unit.as_bytes()[0].eq_ignore_ascii_case(&name.as_bytes()[0])
    };
    let meant = UNITS
        .into_iter()
        .map(|(_, name)| name)
        .filter(|name| name.len() == 3)
        .find(|&name| unit.eq_ignore_ascii_case(name) || decimal(name));
    Error::Unit { meant }
}

/// The bytes that the fraction digits `digits` give of a unit of `1 << shift` bytes,
/// or `None` when they are not a whole number.
fn fraction(digits: &str, shift: u32) -> Option<u64> {
    // The k digits do not end in 0. They give whole bytes only when 5^k divides them,
    // so they are odd, and k is at most `shift`.
    let k = u32::try_from(digits.len()).ok().filter(|&k| k <= shift)?;
    let divisor = 5_u128.pow(k);
    let (quotient, remainder) =
        digits
            .bytes()
            .fold((0_u128, 0_u128), |(quotient, remainder), b| {
                let dividend = remainder * 10 + u128::from(b - b'0');
                (quotient * 10 + dividend / divisor, dividend % divisor)
            });
    let quotient = u64::try_from(quotient)
        .expect("invariant: the digits are below 10^k, so the quotient is below 2^k");
    (remainder == 0).then_some(quotient << (shift - k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// 1 TiB less 1 byte, in TiB.
    const ONE_BYTE_SHORT: &str = "0.9999999999990905052982270717620849609375TiB";

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
            let read = text.parse::<Size>().map(Size::bytes);
            prop_assert_eq!(read, Ok(bytes), "{}", text);
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
            "", "200", "GiB", "200 GiB", "-1B", "+1B", "1e3B", "1_000B", "1.GiB",
            ".5GiB", "1.5.5B", "1 B", "1B ", "1µB",
        ] {
            assert_eq!(text.parse::<Size>(), Err(Error::Syntax), "{text}");
        }
    }

    #[test]
    fn rejects_an_unknown_unit_with_the_unit_it_likely_means() {
        for (text, meant) in [
            ("200gib", Some("GiB")),
            ("200GIB", Some("GiB")),
            ("200GB", Some("GiB")),
            ("200gB", Some("GiB")),
            ("1KB", Some("KiB")),
            ("1kB", Some("KiB")),
            ("1kiB", Some("KiB")),
            ("1MB", Some("MiB")),
            ("1TB", Some("TiB")),
            ("1tib", Some("TiB")),
            ("200Gb", None),
            ("200gb", None),
            ("1b", None),
            ("1PiB", None),
            ("1PB", None),
            ("1iB", None),
            ("1GxB", None),
            ("1GiBs", None),
            ("1bytes", None),
        ] {
            assert_eq!(text.parse::<Size>(), Err(Error::Unit { meant }), "{text}");
        }
    }

    #[test]
    fn rejects_a_fraction_of_a_byte() {
        let past = format!("0.{}1TiB", "0".repeat(40));
        let half = "0.00000000000045474735088646411895751953125TiB";
        let not_five = "0.0000000000009094947017729282379150390624TiB";
        for text in ["0.5B", "0.3KiB", "1.0000001KiB", half, not_five, &past] {
            assert_eq!(text.parse::<Size>(), Err(Error::Fraction), "{text}");
        }
    }

    #[test]
    fn rejects_sizes_that_do_not_fit_with_the_largest_that_does() {
        let long = "9".repeat(40) + "B";
        for (text, largest) in [
            ("16777216TiB", "16777215TiB"),
            ("16777216.5TiB", "16777215TiB"),
            ("17179869184GiB", "17179869183GiB"),
            ("17592186044416MiB", "17592186044415MiB"),
            ("18014398509481984KiB", "18014398509481983KiB"),
            ("18446744073709551616B", "18446744073709551615B"),
            (&long, "18446744073709551615B"),
        ] {
            let largest = largest.parse().expect("the largest size reads");
            assert_eq!(
                text.parse::<Size>(),
                Err(Error::Range { largest }),
                "{text}"
            );
        }
    }

    #[test]
    fn one_unit_past_the_largest_size_does_not_fit() {
        for (unit, name) in UNITS {
            let largest = Size(u64::MAX / unit.0 * unit.0);
            let text = largest.to_string();
            let whole = text
                .strip_suffix(name)
                .expect("the largest prints in its unit");
            let past = format!("{}{name}", u128::from(largest.0 / unit.0) + 1);
            assert_eq!(whole.parse::<u64>(), Ok(largest.0 / unit.0), "{text}");
            assert_eq!(text.parse(), Ok(largest), "{text}");
            assert_eq!(
                past.parse::<Size>(),
                Err(Error::Range { largest }),
                "{past}"
            );
        }
    }

    #[test]
    fn displays_what_each_error_expected() {
        let syntax = "expected a number and a unit, such as 1023B, 1.5GiB, or 200GiB";
        for (error, text) in [
            (Error::Syntax, syntax),
            (Error::Unit { meant: Some("GiB") }, syntax),
            (Error::Unit { meant: None }, syntax),
            (Error::Fraction, "expected a whole number of bytes"),
            (
                Error::Range {
                    largest: Size::from_bytes(u64::MAX),
                },
                "expected a size that fits in a 64-bit count of bytes",
            ),
        ] {
            assert_eq!(error.to_string(), text);
        }
    }
}

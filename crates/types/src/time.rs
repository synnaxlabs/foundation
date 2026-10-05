//! Time values. Every timestamp is in mesh time: nanoseconds since the Unix epoch, UTC.

mod calendar;

use std::fmt;
use std::iter;
use std::ops::{Add, Sub};
use std::str::FromStr;

use crate::ParseError;

/// A point in mesh time: nanoseconds since the Unix epoch, UTC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stamp(i64);

impl Stamp {
    /// The Unix epoch.
    pub const EPOCH: Self = Self(0);

    /// Wraps nanoseconds since the Unix epoch.
    #[must_use]
    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    /// Nanoseconds since the Unix epoch.
    #[must_use]
    pub const fn nanos(self) -> i64 {
        self.0
    }

    /// Adds a span, or returns `None` on overflow. Use it for values from outside.
    #[must_use]
    pub const fn checked_add(self, span: Span) -> Option<Self> {
        match self.0.checked_add(span.0) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// Subtracts a span, or returns `None` on overflow. Use it for values from
    /// outside.
    #[must_use]
    pub const fn checked_sub(self, span: Span) -> Option<Self> {
        match self.0.checked_sub(span.0) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// The span from `earlier` to this stamp, or `None` when it does not fit in a
    /// [`Span`]. Use it for values from outside.
    #[must_use]
    pub const fn checked_since(self, earlier: Self) -> Option<Span> {
        match self.0.checked_sub(earlier.0) {
            Some(n) => Some(Span(n)),
            None => None,
        }
    }
}

impl Add<Span> for Stamp {
    type Output = Self;

    /// # Panics
    ///
    /// On overflow. Values from outside use [`Stamp::checked_add`].
    fn add(self, span: Span) -> Self {
        self.checked_add(span)
            .unwrap_or_else(|| panic!("stamp overflow: {} ns + {} ns", self.0, span.0))
    }
}

impl Sub<Span> for Stamp {
    type Output = Self;

    /// # Panics
    ///
    /// On overflow. Values from outside use [`Stamp::checked_sub`].
    fn sub(self, span: Span) -> Self {
        self.checked_sub(span)
            .unwrap_or_else(|| panic!("stamp overflow: {} ns - {} ns", self.0, span.0))
    }
}

impl Sub for Stamp {
    type Output = Span;

    /// # Panics
    ///
    /// On overflow. Values from outside use [`Stamp::checked_since`].
    fn sub(self, other: Self) -> Span {
        self.checked_since(other)
            .unwrap_or_else(|| panic!("span overflow: {} ns - {} ns", self.0, other.0))
    }
}

const NANOS_PER_SECOND: i64 = Span::SECOND.0;
const SECONDS_PER_DAY: i64 = 86_400;

impl fmt::Display for Stamp {
    /// Writes RFC 3339 in UTC with nine fraction digits:
    /// `2026-10-04T12:00:00.000000000Z`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.div_euclid(NANOS_PER_SECOND);
        let nanos = self.0.rem_euclid(NANOS_PER_SECOND);
        let (year, month, day) = calendar::date(seconds.div_euclid(SECONDS_PER_DAY));
        let of_day = seconds.rem_euclid(SECONDS_PER_DAY);
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{nanos:09}Z",
            of_day / 3600,
            of_day / 60 % 60,
            of_day % 60
        )
    }
}

impl FromStr for Stamp {
    type Err = ParseError;

    /// Reads RFC 3339 with any offset and up to nine fraction digits.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let syntax = || ParseError {
            input: s.into(),
            expected: "an RFC 3339 time with an offset and up to nine fraction digits, \
                       such as 2026-10-04T12:00:00Z",
        };
        let mut r = Reader(s.as_bytes());
        let year = r.digits(4).ok_or_else(syntax)?;
        r.byte(b"-").ok_or_else(syntax)?;
        let month = r.digits(2).ok_or_else(syntax)?;
        r.byte(b"-").ok_or_else(syntax)?;
        let day = r.digits(2).ok_or_else(syntax)?;
        r.byte(b"Tt").ok_or_else(syntax)?;
        let hour = r.digits(2).ok_or_else(syntax)?;
        r.byte(b":").ok_or_else(syntax)?;
        let minute = r.digits(2).ok_or_else(syntax)?;
        r.byte(b":").ok_or_else(syntax)?;
        let second = r.digits(2).ok_or_else(syntax)?;
        let nanos = if r.byte(b".").is_some() {
            r.fraction().ok_or_else(syntax)?
        } else {
            0
        };
        let offset = match r.byte(b"Zz+-").ok_or_else(syntax)? {
            b'Z' | b'z' => 0,
            sign => {
                let hours = r.digits(2).filter(|h| *h < 24).ok_or_else(syntax)?;
                r.byte(b":").ok_or_else(syntax)?;
                let minutes = r.digits(2).filter(|m| *m < 60).ok_or_else(syntax)?;
                let offset = hours * 3600 + minutes * 60;
                if sign == b'-' { -offset } else { offset }
            }
        };
        let valid = (1..=12).contains(&month)
            && (1..=calendar::days_in_month(year, month)).contains(&day)
            && hour < 24
            && minute < 60
            && second < 60;
        if !r.0.is_empty() {
            return Err(syntax());
        }
        if !valid {
            return Err(ParseError {
                input: s.into(),
                expected: "a date and time that exist, with seconds from 00 to 59",
            });
        }
        let seconds = calendar::days(year, month, day) * SECONDS_PER_DAY
            + hour * 3600
            + minute * 60
            + second
            - offset;
        // The seconds alone may pass the lower limit that the fraction brings back.
        let nanos =
            i128::from(seconds) * i128::from(NANOS_PER_SECOND) + i128::from(nanos);
        i64::try_from(nanos)
            .map(Self)
            .map_err(|_overflow| ParseError {
                input: s.into(),
                expected: "a time from 1677-09-21T00:12:43.145224192Z to \
                           2262-04-11T23:47:16.854775807Z",
            })
    }
}

/// The ASCII text left to read.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    /// Reads exactly `n` digits.
    fn digits(&mut self, n: usize) -> Option<i64> {
        let (digits, rest) = self.0.split_at_checked(n)?;
        let mut value = 0;
        for b in digits {
            if !b.is_ascii_digit() {
                return None;
            }
            value = value * 10 + i64::from(b - b'0');
        }
        self.0 = rest;
        Some(value)
    }

    /// Reads one to nine digits as a fraction of a second, in nanoseconds.
    fn fraction(&mut self) -> Option<i64> {
        let len = self.0.iter().take_while(|b| b.is_ascii_digit()).count();
        if !(1..=9).contains(&len) {
            return None;
        }
        let (digits, rest) = self.0.split_at(len);
        self.0 = rest;
        let padded = digits.iter().chain(iter::repeat(&b'0')).take(9);
        Some(padded.fold(0, |n, b| n * 10 + i64::from(b - b'0')))
    }

    /// Reads one byte when it is one of `allowed`.
    fn byte(&mut self, allowed: &[u8]) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        if !allowed.contains(&b) {
            return None;
        }
        self.0 = rest;
        Some(b)
    }
}

/// A length of time in nanoseconds. It may be negative.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span(i64);

impl Span {
    /// No time.
    pub const ZERO: Self = Self(0);
    /// One nanosecond.
    pub const NANOSECOND: Self = Self(1);
    /// One microsecond.
    pub const MICROSECOND: Self = Self(1_000);
    /// One millisecond.
    pub const MILLISECOND: Self = Self(1_000_000);
    /// One second.
    pub const SECOND: Self = Self(1_000_000_000);
    /// One minute.
    pub const MINUTE: Self = Self(60 * Self::SECOND.0);
    /// One hour.
    pub const HOUR: Self = Self(60 * Self::MINUTE.0);
    /// One day.
    pub const DAY: Self = Self(24 * Self::HOUR.0);

    /// Wraps nanoseconds.
    #[must_use]
    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    /// Nanoseconds.
    #[must_use]
    pub const fn nanos(self) -> i64 {
        self.0
    }
}

/// Units in nanoseconds that a span is written in only when it is a whole number of
/// them.
const WHOLE_UNITS: [(u64, &str); 3] = [
    (Span::DAY.0.unsigned_abs(), "d"),
    (Span::HOUR.0.unsigned_abs(), "h"),
    (Span::MINUTE.0.unsigned_abs(), "m"),
];

/// Units in nanoseconds, with their fraction digits, that a span may be written in
/// with a decimal fraction.
const DECIMAL_UNITS: [(u64, usize, &str); 4] = [
    (Span::SECOND.0.unsigned_abs(), 9, "s"),
    (Span::MILLISECOND.0.unsigned_abs(), 6, "ms"),
    (Span::MICROSECOND.0.unsigned_abs(), 3, "us"),
    (1, 0, "ns"),
];

impl fmt::Display for Span {
    /// Writes the span with the largest unit that keeps it exact: `250us`, `1.5s`,
    /// `3d`. Days, hours, and minutes are used only for a whole number of them;
    /// otherwise the largest of `s`, `ms`, `us`, and `ns` that is not more than the
    /// span, with a decimal fraction.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("0s");
        }
        if self.0 < 0 {
            f.write_str("-")?;
        }
        let nanos = self.0.unsigned_abs();
        if let Some((unit, suffix)) = WHOLE_UNITS
            .iter()
            .find(|(unit, _)| nanos.is_multiple_of(*unit))
        {
            return write!(f, "{}{suffix}", nanos / unit);
        }
        let (unit, mut digits, suffix) = DECIMAL_UNITS
            .into_iter()
            .find(|(unit, ..)| nanos >= *unit)
            .expect("invariant: the last unit is one nanosecond");
        write!(f, "{}", nanos / unit)?;
        let mut fraction = nanos % unit;
        if fraction != 0 {
            while fraction.is_multiple_of(10) {
                fraction /= 10;
                digits -= 1;
            }
            write!(f, ".{fraction:0digits$}")?;
        }
        f.write_str(suffix)
    }
}

impl FromStr for Span {
    type Err = ParseError;

    /// Reads a number and a unit: `ns`, `us`, `ms`, `s`, `m`, `h`, or `d`. The number
    /// may have a decimal fraction and a leading `-`, and must give a whole number of
    /// nanoseconds.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let error = |expected| ParseError {
            input: s.into(),
            expected,
        };
        let syntax = || error("a number and a unit, such as 250us, 1.5s, or 3d");
        let (negative, body) = match s.strip_prefix('-') {
            Some(body) => (true, body),
            None => (false, s),
        };
        let split = body
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .ok_or_else(syntax)?;
        let (number, unit) = body.split_at(split);
        let unit = match unit {
            "ns" => 1,
            "us" => Self::MICROSECOND.0,
            "ms" => Self::MILLISECOND.0,
            "s" => Self::SECOND.0,
            "m" => Self::MINUTE.0,
            "h" => Self::HOUR.0,
            "d" => Self::DAY.0,
            _ => return Err(syntax()),
        };
        let (whole, fraction) = number.split_once('.').unwrap_or((number, "0"));
        let digit_run =
            |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
        if !digit_run(whole) || !digit_run(fraction) {
            return Err(syntax());
        }
        let fraction = fraction.trim_end_matches('0');
        // The last digit is not 0, so the mantissa lacks factors of 2 or of 5, and no
        // unit has more than 16 of either. The bound also keeps `scale` in `u128`.
        if fraction.len() > 16 {
            return Err(error("a whole number of nanoseconds"));
        }
        let range = || error("a span that fits in 64-bit nanoseconds");
        let mantissa = whole
            .bytes()
            .chain(fraction.bytes())
            .try_fold(0_u128, |n, b| {
                n.checked_mul(10)?.checked_add(u128::from(b - b'0'))
            })
            .ok_or_else(range)?;
        let scale = fraction.bytes().fold(1_u128, |scale, _| scale * 10);
        let scaled = mantissa
            .checked_mul(unit.unsigned_abs().into())
            .ok_or_else(range)?;
        if scaled % scale != 0 {
            return Err(error("a whole number of nanoseconds"));
        }
        let nanos = i128::try_from(scaled / scale).map_err(|_overflow| range())?;
        let nanos = if negative { -nanos } else { nanos };
        i64::try_from(nanos).map(Self).map_err(|_overflow| range())
    }
}

/// A half-open range of mesh time: from `start`, up to but not including `end`. The
/// end is never before the start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Range {
    start: Stamp,
    end: Stamp,
}

impl Range {
    /// Makes the range from `start` up to `end`, or returns `None` when `end` is
    /// before `start`. When they are equal, the range is empty.
    #[must_use]
    pub const fn new(start: Stamp, end: Stamp) -> Option<Self> {
        if end.0 < start.0 {
            None
        } else {
            Some(Self { start, end })
        }
    }

    /// The first instant in the range.
    #[must_use]
    pub const fn start(self) -> Stamp {
        self.start
    }

    /// The first instant after the range.
    #[must_use]
    pub const fn end(self) -> Stamp {
        self.end
    }

    /// Reports whether `stamp` is in the range.
    #[must_use]
    pub fn contains(&self, stamp: Stamp) -> bool {
        self.start <= stamp && stamp < self.end
    }
}

impl fmt::Display for Range {
    /// Writes the ISO 8601 interval `<start>/<end>`, each in the [`Stamp`] format.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.start, self.end)
    }
}

impl FromStr for Range {
    type Err = ParseError;

    /// Reads `<start>/<end>`, each in the [`Stamp`] grammar. An `end` before `start`
    /// is an error; a stamp that does not read returns that stamp's error.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let error = |expected| ParseError {
            input: s.into(),
            expected,
        };
        let (start, end) = s
            .split_once('/')
            .ok_or_else(|| error("a range written <start>/<end>"))?;
        Self::new(start.parse()?, end.parse()?)
            .ok_or_else(|| error("a range whose end is not before its start"))
    }
}

/// A reading of mesh time with its error bound: the true time is between `earliest`
/// and `latest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Interval {
    /// The earliest the true time can be.
    pub earliest: Stamp,
    /// The latest the true time can be.
    pub latest: Stamp,
}

/// A reading of one node's monotonic clock, in nanoseconds since an arbitrary start.
/// It never goes backwards, and it means nothing on another node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Monotonic(pub u64);

/// A sample rate in samples per second, kept as an exact reduced fraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rate {
    num: u64,
    den: u64,
}

impl Rate {
    /// Makes the rate `num / den` samples per second, reduced.
    ///
    /// # Errors
    ///
    /// When `num` or `den` is zero.
    pub fn new(num: u64, den: u64) -> Result<Self, ParseError> {
        Self::reduced(u128::from(num), u128::from(den)).map_err(|expected| ParseError {
            input: format!("{num}/{den}"),
            expected,
        })
    }

    /// Reduces `num / den`, or returns what the parts should be.
    fn reduced(num: u128, den: u128) -> Result<Self, &'static str> {
        if num == 0 {
            return Err(RATE_ZERO);
        }
        if den == 0 {
            return Err(RATE_DENOMINATOR);
        }
        let divisor = gcd(num, den);
        match (u64::try_from(num / divisor), u64::try_from(den / divisor)) {
            (Ok(num), Ok(den)) => Ok(Self { num, den }),
            _ => Err(RATE_RANGE),
        }
    }

    /// The numerator of the reduced fraction.
    #[must_use]
    pub const fn num(self) -> u64 {
        self.num
    }

    /// The denominator of the reduced fraction.
    #[must_use]
    pub const fn den(self) -> u64 {
        self.den
    }

    /// The exact time of sample `n` after a sample at `start`, rounded down to the
    /// nanosecond.
    ///
    /// # Panics
    ///
    /// When the result does not fit in a [`Stamp`].
    #[must_use]
    #[track_caller]
    pub fn stamp(self, start: Stamp, n: u64) -> Stamp {
        let seconds = u128::from(n) * u128::from(self.den);
        let num = u128::from(self.num);
        let per_second = NANOS_PER_SECOND.unsigned_abs().into();
        let fraction = seconds % num * per_second / num;
        let stamp = (seconds / num)
            .checked_mul(per_second)
            .and_then(|whole| i64::try_from(whole + fraction).ok())
            .and_then(|nanos| start.checked_add(Span(nanos)));
        let Some(stamp) = stamp else {
            panic!("stamp overflow: sample {n} at {self} after {start}")
        };
        stamp
    }
}

impl fmt::Display for Rate {
    /// Writes the rate in hertz: `1kHz`, `100Hz`, `1/3Hz`. A whole rate uses the
    /// largest of `M` and `k` that divides it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.den != 1 {
            return write!(f, "{}/{}Hz", self.num, self.den);
        }
        for (prefix, scale) in [("M", 1_000_000), ("k", 1_000)] {
            if self.num.is_multiple_of(scale) {
                return write!(f, "{}{prefix}Hz", self.num / scale);
            }
        }
        write!(f, "{}Hz", self.num)
    }
}

impl FromStr for Rate {
    type Err = ParseError;

    /// Reads a rate in hertz: a whole number, a decimal, or a fraction, with an
    /// optional `k` or `M` prefix (`100Hz`, `2.5MHz`, `1/3Hz`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let error = |expected| ParseError {
            input: s.into(),
            expected,
        };
        let value = s.strip_suffix("Hz").ok_or_else(|| error(RATE_SYNTAX))?;
        let (value, scale) = if let Some(value) = value.strip_suffix('M') {
            (value, 1_000_000)
        } else if let Some(value) = value.strip_suffix('k') {
            (value, 1_000)
        } else {
            (value, 1)
        };
        let (num, den) = match value.split_once('/') {
            Some((num, den)) => (whole(num), whole(den)),
            None => decimal(value),
        };
        let num = num.and_then(|num| num.checked_mul(scale).ok_or(RATE_RANGE));
        Self::reduced(num.map_err(error)?, den.map_err(error)?).map_err(error)
    }
}

const RATE_SYNTAX: &str = "a rate in hertz, such as 100Hz, 2.5kHz, 1MHz, or 1/3Hz";
const RATE_ZERO: &str = "a rate above zero";
const RATE_DENOMINATOR: &str = "a fraction with a denominator above zero";
const RATE_RANGE: &str =
    "a rate whose reduced numerator and denominator fit in 64 bits";

/// Reads digits as a whole number, or returns what the text should be.
fn whole(text: &str) -> Result<u128, &'static str> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(RATE_SYNTAX);
    }
    text.bytes()
        .try_fold(0_u128, |n, b| {
            n.checked_mul(10)?.checked_add(u128::from(b - b'0'))
        })
        .ok_or(RATE_RANGE)
}

/// Reads `<digits>[.<digits>]` as a numerator and denominator.
fn decimal(text: &str) -> (Result<u128, &'static str>, Result<u128, &'static str>) {
    let (integer, fraction) = text.split_once('.').unwrap_or((text, "0"));
    if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return (Err(RATE_SYNTAX), Ok(1));
    }
    let num = whole(integer).and_then(|_| whole(&format!("{integer}{fraction}")));
    let den = fraction
        .bytes()
        .try_fold(1_u128, |den, _| den.checked_mul(10))
        .ok_or(RATE_RANGE);
    (num, den)
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const STAMP_SYNTAX: &str = "an RFC 3339 time with an offset and up to nine \
                                fraction digits, such as 2026-10-04T12:00:00Z";
    const STAMP_RANGE: &str = "a time from 1677-09-21T00:12:43.145224192Z to \
                               2262-04-11T23:47:16.854775807Z";
    const STAMP_DATE: &str = "a date and time that exist, with seconds from 00 to 59";
    const SPAN_SYNTAX: &str = "a number and a unit, such as 250us, 1.5s, or 3d";
    const SPAN_EXACT: &str = "a whole number of nanoseconds";
    const SPAN_RANGE: &str = "a span that fits in 64-bit nanoseconds";

    fn error(input: &str, expected: &'static str) -> ParseError {
        ParseError {
            input: input.into(),
            expected,
        }
    }

    fn seconds(s: i64) -> Stamp {
        Stamp::from_nanos(s * NANOS_PER_SECOND)
    }

    mod stamp {
        use super::*;

        mod display {
            use super::*;

            #[test]
            fn writes_utc_with_nine_fraction_digits() {
                for (stamp, text) in [
                    (Stamp::EPOCH, "1970-01-01T00:00:00.000000000Z"),
                    (seconds(1_791_115_200), "2026-10-04T12:00:00.000000000Z"),
                    (seconds(1_709_251_199), "2024-02-29T23:59:59.000000000Z"),
                    (seconds(951_782_400), "2000-02-29T00:00:00.000000000Z"),
                    (seconds(951_868_800), "2000-03-01T00:00:00.000000000Z"),
                    (Stamp::from_nanos(-1), "1969-12-31T23:59:59.999999999Z"),
                    (
                        Stamp::from_nanos(i64::MIN),
                        "1677-09-21T00:12:43.145224192Z",
                    ),
                    (
                        Stamp::from_nanos(i64::MAX),
                        "2262-04-11T23:47:16.854775807Z",
                    ),
                ] {
                    assert_eq!(stamp.to_string(), text);
                }
            }
        }

        mod parse {
            use super::*;

            #[test]
            fn applies_the_offset() {
                let noon = seconds(1_791_115_200);
                for text in [
                    "2026-10-04T12:00:00Z",
                    "2026-10-04t12:00:00z",
                    "2026-10-04T17:30:00+05:30",
                    "2026-10-04T04:00:00-08:00",
                    "2026-10-04T12:00:00-00:00",
                    "2026-10-05T11:59:00+23:59",
                ] {
                    assert_eq!(text.parse(), Ok(noon), "{text}");
                }
            }

            #[test]
            fn reads_one_to_nine_fraction_digits() {
                let noon = seconds(1_791_115_200);
                for (text, nanos) in [
                    ("2026-10-04T12:00:00.5Z", 500_000_000),
                    ("2026-10-04T12:00:00.000000001Z", 1),
                    ("2026-10-04T12:00:00.123456789Z", 123_456_789),
                ] {
                    assert_eq!(text.parse(), Ok(noon + Span::from_nanos(nanos)));
                }
            }

            #[test]
            fn reads_the_limits() {
                for nanos in [i64::MIN, i64::MAX] {
                    let stamp = Stamp::from_nanos(nanos);
                    assert_eq!(stamp.to_string().parse(), Ok(stamp));
                }
            }

            #[test]
            fn rejects_times_outside_the_limits() {
                for text in [
                    "2262-04-11T23:47:16.854775808Z",
                    "1677-09-21T00:12:43.145224191Z",
                    "9999-12-31T23:59:59Z",
                    "0000-01-01T00:00:00Z",
                ] {
                    assert_eq!(text.parse::<Stamp>(), Err(error(text, STAMP_RANGE)));
                }
            }

            #[test]
            fn rejects_bad_syntax() {
                for text in [
                    "",
                    "2026-10-04T12:00:00",
                    "2026-10-04 12:00:00Z",
                    "2026-10-04T12:00:00.Z",
                    "2026-10-04T12:00:00.1234567890Z",
                    "2026-10-04T12:00Z",
                    "2026-10-4T12:00:00Z",
                    "2026-10-04T12:00:00+0530",
                    "2026-10-04T12:00:00+24:00",
                    "2026-10-04T12:00:00+05:60",
                    "2026-10-04T12:00:00Zx",
                    "+2026-10-04T12:00:00Z",
                    "2026-10-04T12:00:00\u{ff}Z",
                ] {
                    assert_eq!(text.parse::<Stamp>(), Err(error(text, STAMP_SYNTAX)));
                }
            }

            #[test]
            fn rejects_dates_and_times_that_do_not_exist() {
                for text in [
                    "2026-13-01T00:00:00Z",
                    "2026-00-01T00:00:00Z",
                    "2026-04-31T00:00:00Z",
                    "2026-06-31T00:00:00Z",
                    "2026-09-31T00:00:00Z",
                    "2026-11-31T00:00:00Z",
                    "2026-01-00T00:00:00Z",
                    "2025-02-29T00:00:00Z",
                    "2100-02-29T00:00:00Z",
                    "2026-10-04T24:00:00Z",
                    "2026-10-04T12:60:00Z",
                    "2026-10-04T12:00:60Z",
                ] {
                    assert_eq!(text.parse::<Stamp>(), Err(error(text, STAMP_DATE)));
                }
                assert_eq!("2000-02-29T00:00:00Z".parse(), Ok(seconds(951_782_400)));
                assert_eq!("2024-02-29T00:00:00Z".parse(), Ok(seconds(1_709_164_800)));
            }

            #[test]
            fn shows_the_input_and_the_expected_form() {
                assert_eq!(
                    "noon".parse::<Stamp>().unwrap_err().to_string(),
                    "cannot read \"noon\": expected an RFC 3339 time with an offset \
                     and up to nine fraction digits, such as 2026-10-04T12:00:00Z"
                );
            }
        }

        proptest! {
            #[test]
            fn stamps_round_trip(nanos in any::<i64>()) {
                let stamp = Stamp::from_nanos(nanos);
                prop_assert_eq!(stamp.to_string().parse(), Ok(stamp));
            }
        }

        mod math {
            use super::*;

            #[test]
            fn subtracts_stamps_into_a_span() {
                assert_eq!(
                    seconds(10) - seconds(4),
                    Span::from_nanos(6 * NANOS_PER_SECOND)
                );
                assert_eq!(
                    seconds(4) - seconds(10),
                    Span::from_nanos(-6 * NANOS_PER_SECOND)
                );
                assert_eq!(seconds(4) + Span::SECOND - Span::SECOND, seconds(4));
            }

            #[test]
            fn checked_since_returns_none_past_the_span_limits() {
                let max = Stamp::from_nanos(i64::MAX);
                let min = Stamp::from_nanos(i64::MIN);
                assert_eq!(
                    seconds(10).checked_since(seconds(4)),
                    Some(seconds(6) - Stamp::EPOCH)
                );
                assert_eq!(
                    max.checked_since(Stamp::EPOCH),
                    Some(Span::from_nanos(i64::MAX))
                );
                assert_eq!(max.checked_since(Stamp::from_nanos(-1)), None);
                assert_eq!(min.checked_since(Stamp::from_nanos(1)), None);
            }

            #[test]
            fn checked_math_returns_none_on_overflow() {
                let max = Stamp::from_nanos(i64::MAX);
                let min = Stamp::from_nanos(i64::MIN);
                assert_eq!(max.checked_add(Span::NANOSECOND), None);
                assert_eq!(min.checked_sub(Span::NANOSECOND), None);
                assert_eq!(
                    max.checked_sub(Span::NANOSECOND),
                    Some(Stamp::from_nanos(i64::MAX - 1))
                );
            }

            #[test]
            #[should_panic(expected = "stamp overflow: 9223372036854775807 ns + 1 ns")]
            fn adding_past_the_limit_panics() {
                let _ = Stamp::from_nanos(i64::MAX) + Span::NANOSECOND;
            }

            #[test]
            #[should_panic(expected = "stamp overflow: -9223372036854775808 ns - 1 ns")]
            fn subtracting_past_the_limit_panics() {
                let _ = Stamp::from_nanos(i64::MIN) - Span::NANOSECOND;
            }

            #[test]
            #[should_panic(expected = "span overflow: 9223372036854775807 ns - -1 ns")]
            fn a_difference_past_the_limit_panics() {
                let _ = Stamp::from_nanos(i64::MAX) - Stamp::from_nanos(-1);
            }
        }
    }

    mod span {
        use super::*;

        proptest! {
            #[test]
            fn spans_round_trip(nanos in any::<i64>()) {
                let span = Span::from_nanos(nanos);
                prop_assert_eq!(span.to_string().parse(), Ok(span));
            }
        }

        #[test]
        fn writes_the_largest_exact_unit() {
            for (nanos, text) in [
                (0, "0s"),
                (1, "1ns"),
                (999, "999ns"),
                (250_000, "250us"),
                (1_500_000_000, "1.5s"),
                (1_000_001, "1.000001ms"),
                (-1_500_000_000, "-1.5s"),
                (61 * NANOS_PER_SECOND, "61s"),
                (90 * NANOS_PER_SECOND, "90s"),
                (90 * Span::MINUTE.0, "90m"),
                (36 * Span::HOUR.0, "36h"),
                (3 * Span::DAY.0, "3d"),
                (-3 * Span::DAY.0, "-3d"),
                (i64::MAX, "9223372036.854775807s"),
                (i64::MIN, "-9223372036.854775808s"),
            ] {
                assert_eq!(Span::from_nanos(nanos).to_string(), text);
            }
        }

        #[test]
        fn reads_a_number_and_a_unit() {
            for (text, nanos) in [
                ("0s", 0),
                ("-0s", 0),
                ("7ns", 7),
                ("250us", 250_000),
                ("1.5s", 1_500_000_000),
                ("1.50s", 1_500_000_000),
                ("0.5us", 500),
                ("1.5m", 90 * NANOS_PER_SECOND),
                ("0.25h", 15 * Span::MINUTE.0),
                ("007ms", 7_000_000),
                ("3d", 3 * Span::DAY.0),
                ("1.00000000000000000000000000000s", NANOS_PER_SECOND),
                ("9223372036.854775807s", i64::MAX),
                ("-9223372036.854775808s", i64::MIN),
            ] {
                assert_eq!(text.parse(), Ok(Span::from_nanos(nanos)), "{text}");
            }
        }

        #[test]
        fn rejects_bad_syntax() {
            for text in [
                "", "5", "s", "-", "1.s", ".5s", "1.5.5s", "5 s", "5sec", "+5s",
                "--5s", "5µs", "5S", "1e3s",
            ] {
                assert_eq!(
                    text.parse::<Span>(),
                    Err(error(text, SPAN_SYNTAX)),
                    "{text}"
                );
            }
        }

        #[test]
        fn rejects_fractions_of_a_nanosecond() {
            let long = format!("0.{}1s", "0".repeat(38));
            for text in ["0.5ns", "1.0000000001s", "0.000000000000000000001d", &long] {
                assert_eq!(
                    text.parse::<Span>(),
                    Err(error(text, SPAN_EXACT)),
                    "{text}"
                );
            }
        }

        #[test]
        fn rejects_spans_that_do_not_fit() {
            for text in [
                "9223372036.854775808s",
                "-9223372036.854775809s",
                "106752d",
                "999999999999999999999999999999999999999999d",
            ] {
                assert_eq!(
                    text.parse::<Span>(),
                    Err(error(text, SPAN_RANGE)),
                    "{text}"
                );
            }
        }
    }

    mod range {
        use super::*;

        fn range(start: Stamp, end: Stamp) -> Range {
            Range::new(start, end).unwrap()
        }

        #[test]
        fn is_half_open() {
            let range = range(seconds(1), seconds(2));
            assert!(!range.contains(seconds(1) - Span::NANOSECOND));
            assert!(range.contains(seconds(1)));
            assert!(range.contains(seconds(2) - Span::NANOSECOND));
            assert!(!range.contains(seconds(2)));
        }

        #[test]
        fn holds_its_start_and_end() {
            let range = range(seconds(1), seconds(2));
            assert_eq!((range.start(), range.end()), (seconds(1), seconds(2)));
        }

        #[test]
        fn is_never_reversed() {
            assert_eq!(Range::new(seconds(2), seconds(1)), None);
            assert!(!range(seconds(1), seconds(1)).contains(seconds(1)));
        }

        #[test]
        fn writes_an_iso_interval() {
            let range = range(seconds(1_791_115_200), seconds(1_791_118_800));
            let text = "2026-10-04T12:00:00.000000000Z/2026-10-04T13:00:00.000000000Z";
            assert_eq!(range.to_string(), text);
            assert_eq!(text.parse(), Ok(range));
        }

        #[test]
        fn reads_an_empty_range() {
            let text = "2026-10-04T12:00:00Z/2026-10-04T12:00:00Z";
            let noon = seconds(1_791_115_200);
            assert_eq!(text.parse(), Ok(range(noon, noon)));
        }

        #[test]
        fn rejects_an_end_before_the_start() {
            let text = "2026-10-04T12:00:00Z/2026-10-04T11:59:59Z";
            assert_eq!(
                text.parse::<Range>(),
                Err(error(text, "a range whose end is not before its start"))
            );
        }

        #[test]
        fn rejects_a_missing_slash() {
            let text = "2026-10-04T12:00:00Z";
            assert_eq!(
                text.parse::<Range>(),
                Err(error(text, "a range written <start>/<end>"))
            );
        }

        #[test]
        fn returns_the_error_of_the_stamp_that_does_not_read() {
            for (text, stamp) in [
                ("2026-10-04T12:00:00Z/later", "later"),
                ("later/2026-10-04T12:00:00Z", "later"),
            ] {
                assert_eq!(text.parse::<Range>(), Err(error(stamp, STAMP_SYNTAX)));
            }
        }

        proptest! {
            #[test]
            fn ranges_round_trip(a in any::<i64>(), b in any::<i64>()) {
                let (start, end) = (a.min(b), a.max(b));
                let range = range(Stamp::from_nanos(start), Stamp::from_nanos(end));
                prop_assert_eq!(range.to_string().parse(), Ok(range));
            }
        }
    }

    mod rate {
        use super::*;

        const RATE_SYNTAX: &str = super::super::RATE_SYNTAX;

        fn rate(num: u64, den: u64) -> Rate {
            Rate::new(num, den).unwrap()
        }

        mod new {
            use super::*;

            #[test]
            fn reduces_the_fraction() {
                let rate = rate(6, 4);
                assert_eq!((rate.num(), rate.den()), (3, 2));
                assert_eq!(rate, Rate::new(3, 2).unwrap());
            }

            #[test]
            fn rejects_zero() {
                assert_eq!(Rate::new(0, 1), Err(error("0/1", RATE_ZERO)));
                assert_eq!(Rate::new(1, 0), Err(error("1/0", RATE_DENOMINATOR)));
            }
        }

        mod stamp {
            use super::*;

            #[test]
            fn rounds_each_sample_down_to_the_nanosecond() {
                let start = seconds(1_791_115_200);
                for (rate, n, offset) in [
                    (rate(1_000, 1), 3, 3_000_000),
                    (rate(3, 1), 1, 333_333_333),
                    (rate(3, 1), 2, 666_666_666),
                    (rate(3, 1), 3, NANOS_PER_SECOND),
                    (rate(1, 3), 1, 3 * NANOS_PER_SECOND),
                    (rate(1, 1), 0, 0),
                ] {
                    let expected = start + Span::from_nanos(offset);
                    assert_eq!(rate.stamp(start, n), expected, "{rate} sample {n}");
                }
            }

            #[test]
            fn never_drifts() {
                let day = 86_400;
                assert_eq!(
                    rate(48_000, 1).stamp(Stamp::EPOCH, 48_000 * day),
                    seconds(86_400)
                );
                assert_eq!(
                    rate(30_000, 1_001).stamp(Stamp::EPOCH, 30_000 * day),
                    seconds(1_001 * 86_400)
                );
            }

            #[test]
            fn reaches_the_last_stamp() {
                let max = i64::MAX.unsigned_abs();
                let rate = rate(1_000_000_000, 1);
                assert_eq!(rate.stamp(Stamp::EPOCH, max), Stamp::from_nanos(i64::MAX));
                let rate = super::rate(1, u64::MAX);
                assert_eq!(
                    rate.stamp(Stamp::from_nanos(i64::MIN), 0),
                    Stamp::from_nanos(i64::MIN)
                );
            }

            #[test]
            #[should_panic(
                expected = "stamp overflow: sample 9223372036854775808 at 1000MHz after \
                            1970-01-01T00:00:00.000000000Z"
            )]
            fn panics_past_the_last_stamp() {
                let rate = rate(1_000_000_000, 1);
                std::hint::black_box(rate.stamp(Stamp::EPOCH, 1 << 63));
            }

            #[test]
            #[should_panic(
                expected = "stamp overflow: sample 18446744073709551615 at 1/18446744073709551615Hz"
            )]
            fn panics_when_the_offset_passes_u128() {
                let rate = rate(1, u64::MAX);
                std::hint::black_box(rate.stamp(Stamp::EPOCH, u64::MAX));
            }

            proptest! {
                #[test]
                fn matches_exact_math(
                    num in 1_u64..1 << 40,
                    den in 1_u64..1 << 40,
                    n in 0_u64..1 << 40,
                ) {
                    let rate = rate(num, den);
                    let exact = u128::from(n) * u128::from(rate.den()) * 1_000_000_000
                        / u128::from(rate.num());
                    let expected = i64::try_from(exact).ok().map(Span::from_nanos);
                    let actual = expected.map(|_| rate.stamp(Stamp::EPOCH, n) - Stamp::EPOCH);
                    prop_assert_eq!(actual, expected);
                }
            }
        }

        mod display {
            use super::*;

            #[test]
            fn uses_the_largest_exact_prefix_or_a_fraction() {
                for (rate, text) in [
                    (rate(1, 1), "1Hz"),
                    (rate(100, 1), "100Hz"),
                    (rate(1_000, 1), "1kHz"),
                    (rate(1_500, 1), "1500Hz"),
                    (rate(2_500_000, 1), "2500kHz"),
                    (rate(3_000_000, 1), "3MHz"),
                    (rate(1, 3), "1/3Hz"),
                    (rate(5, 2), "5/2Hz"),
                    (
                        rate(u64::MAX, u64::MAX - 1),
                        "18446744073709551615/18446744073709551614Hz",
                    ),
                ] {
                    assert_eq!(rate.to_string(), text);
                }
            }
        }

        mod parse {
            use super::*;

            #[test]
            fn reads_whole_decimal_and_fraction_rates() {
                for (text, rate) in [
                    ("100Hz", rate(100, 1)),
                    ("1kHz", rate(1_000, 1)),
                    ("2.5kHz", rate(2_500, 1)),
                    ("2.5MHz", rate(2_500_000, 1)),
                    ("0.5Hz", rate(1, 2)),
                    ("33.3333Hz", rate(333_333, 10_000)),
                    ("1.000Hz", rate(1, 1)),
                    ("1/3Hz", rate(1, 3)),
                    ("2/6Hz", rate(1, 3)),
                    ("1/3kHz", rate(1_000, 3)),
                    ("0.000000000000000001MHz", rate(1, 1_000_000_000_000)),
                ] {
                    assert_eq!(text.parse(), Ok(rate), "{text}");
                }
            }

            #[test]
            fn rejects_bad_syntax() {
                for text in [
                    "", "Hz", "100", "100hz", "1k", "1 Hz", " 1Hz", "-1Hz", "+1Hz",
                    "1e3Hz", "1.Hz", ".5Hz", "1.5.5Hz", "1/Hz", "/3Hz", "1.5/2Hz",
                    "1/2/3Hz", "1GHz", "1kkHz", "1MkHz",
                ] {
                    assert_eq!(
                        text.parse::<Rate>(),
                        Err(error(text, RATE_SYNTAX)),
                        "{text}"
                    );
                }
            }

            #[test]
            fn rejects_zero() {
                for text in ["0Hz", "0.0kHz", "0/3Hz"] {
                    assert_eq!(
                        text.parse::<Rate>(),
                        Err(error(text, RATE_ZERO)),
                        "{text}"
                    );
                }
                assert_eq!(
                    "1/0Hz".parse::<Rate>(),
                    Err(error("1/0Hz", RATE_DENOMINATOR))
                );
            }

            #[test]
            fn rejects_rates_that_do_not_fit() {
                for text in [
                    "18446744073709551616Hz",
                    "18446744073709552kHz",
                    "0.00000000000000000001Hz",
                    "1/18446744073709551616Hz",
                    "999999999999999999999999999999999999999Hz",
                ] {
                    assert_eq!(
                        text.parse::<Rate>(),
                        Err(error(text, RATE_RANGE)),
                        "{text}"
                    );
                }
                assert_eq!("18446744073709551615Hz".parse(), Ok(rate(u64::MAX, 1)));
                assert_eq!("36893488147419103230/2Hz".parse(), Ok(rate(u64::MAX, 1)));
            }

            proptest! {
                #[test]
                fn rates_round_trip(num in 1..=u64::MAX, den in 1..=u64::MAX) {
                    let rate = rate(num, den);
                    prop_assert_eq!(rate.to_string().parse(), Ok(rate));
                }
            }
        }
    }
}

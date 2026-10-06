//! Time values. Every timestamp is in mesh time: nanoseconds since the Unix epoch, UTC.

mod calendar;

use std::fmt;
use std::iter;
use std::ops::{Add, Sub};
use std::str::FromStr;

use crate::quantity;

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
    #[track_caller]
    fn add(self, span: Span) -> Self {
        let Some(stamp) = self.checked_add(span) else {
            panic!("stamp overflow: {} ns + {} ns", self.0, span.0)
        };
        stamp
    }
}

impl Sub<Span> for Stamp {
    type Output = Self;

    /// # Panics
    ///
    /// On overflow. Values from outside use [`Stamp::checked_sub`].
    #[track_caller]
    fn sub(self, span: Span) -> Self {
        let Some(stamp) = self.checked_sub(span) else {
            panic!("stamp overflow: {} ns - {} ns", self.0, span.0)
        };
        stamp
    }
}

impl Sub for Stamp {
    type Output = Span;

    /// # Panics
    ///
    /// On overflow. Values from outside use [`Stamp::checked_since`].
    #[track_caller]
    fn sub(self, other: Self) -> Span {
        let Some(span) = self.checked_since(other) else {
            panic!("span overflow: {} ns - {} ns", self.0, other.0)
        };
        span
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
    type Err = Error;

    /// Reads RFC 3339 with any offset and up to nine fraction digits.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut r = Reader(s.as_bytes());
        let year = r.digits(4).ok_or(Error::Stamp)?;
        r.byte(b"-").ok_or(Error::Stamp)?;
        let month = r.digits(2).ok_or(Error::Stamp)?;
        r.byte(b"-").ok_or(Error::Stamp)?;
        let day = r.digits(2).ok_or(Error::Stamp)?;
        r.byte(b"Tt").ok_or(Error::Stamp)?;
        let hour = r.digits(2).ok_or(Error::Stamp)?;
        r.byte(b":").ok_or(Error::Stamp)?;
        let minute = r.digits(2).ok_or(Error::Stamp)?;
        r.byte(b":").ok_or(Error::Stamp)?;
        let second = r.digits(2).ok_or(Error::Stamp)?;
        let nanos = if r.byte(b".").is_some() {
            r.fraction().ok_or(Error::Stamp)?
        } else {
            0
        };
        let offset = match r.byte(b"Zz+-").ok_or(Error::Stamp)? {
            b'Z' | b'z' => 0,
            sign => {
                let hours = r.digits(2).filter(|h| *h < 24).ok_or(Error::Stamp)?;
                r.byte(b":").ok_or(Error::Stamp)?;
                let minutes = r.digits(2).filter(|m| *m < 60).ok_or(Error::Stamp)?;
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
            return Err(Error::Stamp);
        }
        if !valid {
            return Err(Error::Date);
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
            .map_err(|_overflow| Error::Era)
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
    type Err = Error;

    /// Reads a number and a unit: `ns`, `us`, `ms`, `s`, `m`, `h`, or `d`. The number
    /// may have a decimal fraction and a leading `-`, and must give a whole number of
    /// nanoseconds.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (negative, body) = match s.strip_prefix('-') {
            Some(body) => (true, body),
            None => (false, s),
        };
        let quantity::Quantity {
            whole,
            fraction,
            unit,
        } = quantity::split(body).ok_or(Error::Span)?;
        let unit = match unit {
            "ns" => 1,
            "us" => Self::MICROSECOND.0,
            "ms" => Self::MILLISECOND.0,
            "s" => Self::SECOND.0,
            "m" => Self::MINUTE.0,
            "h" => Self::HOUR.0,
            "d" => Self::DAY.0,
            _ => return Err(Error::Span),
        };
        // The last digit is not 0, so the mantissa lacks factors of 2 or of 5, and no
        // unit has more than 16 of either. The bound also keeps `scale` in `u128`.
        if fraction.len() > 16 {
            return Err(Error::Fraction);
        }
        let mantissa = whole
            .bytes()
            .chain(fraction.bytes())
            .try_fold(0_u128, |n, b| {
                n.checked_mul(10)?.checked_add(u128::from(b - b'0'))
            })
            .ok_or(Error::Long)?;
        let scale = fraction.bytes().fold(1_u128, |scale, _| scale * 10);
        let scaled = mantissa
            .checked_mul(unit.unsigned_abs().into())
            .ok_or(Error::Long)?;
        if scaled % scale != 0 {
            return Err(Error::Fraction);
        }
        let nanos = i128::try_from(scaled / scale).map_err(|_overflow| Error::Long)?;
        let nanos = if negative { -nanos } else { nanos };
        i64::try_from(nanos)
            .map(Self)
            .map_err(|_overflow| Error::Long)
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
    type Err = Error;

    /// Reads `<start>/<end>`, each in the [`Stamp`] grammar. An `end` before `start`
    /// is an error; a stamp that does not read returns that stamp's error.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (start, end) = s.split_once('/').ok_or(Error::Range)?;
        Self::new(start.parse()?, end.parse()?).ok_or(Error::Reversed)
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

/// A reading of one local monotonic clock, in nanoseconds since an arbitrary start:
/// a node's clock, or a device's sample clock. It never goes backwards, and it means
/// nothing outside that clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Monotonic(pub u64);

impl Monotonic {
    /// Adds a span, or returns `None` when the result is outside `u64`. Use it for
    /// values from outside.
    #[must_use]
    pub const fn checked_add(self, span: Span) -> Option<Self> {
        match self.0.checked_add_signed(span.0) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// Subtracts a span, or returns `None` when the result is outside `u64`. Use it
    /// for values from outside.
    #[must_use]
    pub const fn checked_sub(self, span: Span) -> Option<Self> {
        match self.0.checked_sub_signed(span.0) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }
}

impl Add<Span> for Monotonic {
    type Output = Self;

    /// # Panics
    ///
    /// When the result is outside `u64`. Values from outside use
    /// [`Monotonic::checked_add`].
    #[track_caller]
    fn add(self, span: Span) -> Self {
        let Some(reading) = self.checked_add(span) else {
            panic!("monotonic overflow: {} ns + {} ns", self.0, span.0)
        };
        reading
    }
}

impl Sub<Span> for Monotonic {
    type Output = Self;

    /// # Panics
    ///
    /// When the result is outside `u64`. Values from outside use
    /// [`Monotonic::checked_sub`].
    #[track_caller]
    fn sub(self, span: Span) -> Self {
        let Some(reading) = self.checked_sub(span) else {
            panic!("monotonic overflow: {} ns - {} ns", self.0, span.0)
        };
        reading
    }
}

impl Sub for Monotonic {
    type Output = Span;

    /// # Panics
    ///
    /// When the difference is outside `i64`.
    #[track_caller]
    fn sub(self, other: Self) -> Span {
        let Some(nanos) = self.0.checked_signed_diff(other.0) else {
            panic!("span overflow: {} ns - {} ns", self.0, other.0)
        };
        Span(nanos)
    }
}

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
    /// When `num` or `den` is zero, or when one sample period is under 1 ns or does not
    /// fit in a [`Span`]. Under 1 ns, two samples would share a [`Stamp`].
    pub fn new(num: u64, den: u64) -> Result<Self, Error> {
        if num == 0 || den == 0 {
            return Err(Error::Zero);
        }
        let divisor = gcd(num, den);
        let rate = Self {
            num: num / divisor,
            den: den / divisor,
        };
        let period = rate.nanos(1).and_then(|nanos| i64::try_from(nanos).ok());
        if period.is_none_or(|period| period < 1) {
            return Err(Error::Period);
        }
        Ok(rate)
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

    /// The exact time that `n` samples take at this rate, rounded down to the
    /// nanosecond.
    ///
    /// # Panics
    ///
    /// When the result does not fit in a [`Span`].
    #[must_use]
    #[track_caller]
    pub fn span(self, n: u64) -> Span {
        let nanos = self.nanos(n).and_then(|nanos| i64::try_from(nanos).ok());
        let Some(nanos) = nanos else {
            panic!("span overflow: {n} samples at {}/{} Hz", self.num, self.den)
        };
        Span(nanos)
    }

    /// The number of whole sample periods in `span`: the largest `n` with
    /// `self.span(n) <= span`. Zero for a span of zero or less.
    #[must_use]
    #[expect(clippy::missing_panics_doc, reason = "`new` keeps the count in a u64")]
    pub fn count(self, span: Span) -> u64 {
        let Ok(nanos) = u128::try_from(span.0) else {
            return 0;
        };
        // span(n) <= nanos exactly when n * den * 1e9 < (nanos + 1) * num.
        let limit = (nanos + 1) * u128::from(self.num) - 1;
        let count = limit / (u128::from(self.den) * nanos_per_second());
        u64::try_from(count).expect("invariant: a period of 1 ns or more counts a u64")
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
        let stamp = self
            .nanos(n)
            .and_then(|nanos| i128::try_from(nanos).ok())
            .and_then(|nanos| i64::try_from(i128::from(start.0) + nanos).ok());
        let Some(stamp) = stamp else {
            panic!(
                "stamp overflow: {} ns + {n} samples at {}/{} Hz",
                start.0, self.num, self.den
            )
        };
        Stamp(stamp)
    }

    /// The time that `n` samples take in nanoseconds, rounded down, or `None` past
    /// `u128`.
    fn nanos(self, n: u64) -> Option<u128> {
        let samples = u128::from(n) * u128::from(self.den);
        Some(samples.checked_mul(nanos_per_second())? / u128::from(self.num))
    }
}

/// [`NANOS_PER_SECOND`] as a `u128`.
fn nanos_per_second() -> u128 {
    u128::from(NANOS_PER_SECOND.unsigned_abs())
}

/// The greatest common divisor of `a` and `b`.
const fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

impl fmt::Display for Rate {
    /// Writes the rate in hertz: `1kHz`, `100Hz`, `1/3Hz`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = f;
        todo!()
    }
}

impl FromStr for Rate {
    type Err = Error;

    /// Reads a rate in hertz, with an optional `k` or `M` prefix or a fraction.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let _ = s;
        todo!()
    }
}

/// A time value that could not be read or made. `Display` gives the message: a
/// lower-case clause with no final period. [`Error::fix`] gives what to do instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Error {
    /// The text is not RFC 3339 with an offset and up to nine fraction digits.
    Stamp,
    /// The text is RFC 3339, but its date or time does not exist, such as
    /// `2026-02-30` or a leap second.
    Date,
    /// The time is before 1677-09-21T00:12:43.145224192Z or after
    /// 2262-04-11T23:47:16.854775807Z, which a [`Stamp`] cannot hold.
    Era,
    /// The text is not a number and a unit: `ns`, `us`, `ms`, `s`, `m`, `h`, or `d`.
    Span,
    /// The span is not a whole number of nanoseconds, such as `0.5ns`.
    Fraction,
    /// The span does not fit in 64-bit nanoseconds.
    Long,
    /// The text has no `/` between two times.
    Range,
    /// The range ends before it starts.
    Reversed,
    /// The numerator or the denominator of a rate is zero.
    Zero,
    /// The sample period of a rate is under 1 ns or does not fit in a [`Span`].
    Period,
}

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(self) -> &'static str {
        match self {
            Self::Stamp => "Write a time such as 2026-10-04T12:00:00Z",
            Self::Date => {
                "Use a date that exists, an hour to 23, and minutes and seconds to 59"
            }
            Self::Era => {
                "Use a time from 1677-09-21T00:12:43.145224192Z to \
                 2262-04-11T23:47:16.854775807Z"
            }
            Self::Span => "Write a span such as 250us, 1.5s, or 3d",
            Self::Fraction => "Use fewer fraction digits or a smaller unit",
            Self::Long => "Use a span from -106751d to 106751d",
            Self::Range => "Write a range as `<start>/<end>`",
            Self::Reversed => "Put the earlier time first",
            Self::Zero => "Use a rate above zero",
            Self::Period => "Use a rate from one sample in 106751d to 1 GHz",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stamp => {
                "a time is not RFC 3339 with an offset and up to nine fraction digits"
            }
            Self::Date => "a date or time does not exist",
            Self::Era => "a time is outside the range of a stamp",
            Self::Span => "a span is not a number and a unit",
            Self::Fraction => "a span is not a whole number of nanoseconds",
            Self::Long => "a span does not fit in 64-bit nanoseconds",
            Self::Range => "a range has no `/` between two times",
            Self::Reversed => "a range ends before it starts",
            Self::Zero => "a rate has a zero numerator or denominator",
            Self::Period => {
                "the sample period of a rate is under 1 ns or longer than a span"
            }
        })
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn seconds(s: i64) -> Stamp {
        Stamp::from_nanos(s * NANOS_PER_SECOND)
    }

    #[test]
    fn states_each_problem_and_its_fix() {
        for (error, message, fix) in [
            (
                Error::Stamp,
                "a time is not RFC 3339 with an offset and up to nine fraction digits",
                "Write a time such as 2026-10-04T12:00:00Z",
            ),
            (
                Error::Date,
                "a date or time does not exist",
                "Use a date that exists, an hour to 23, and minutes and seconds to 59",
            ),
            (
                Error::Era,
                "a time is outside the range of a stamp",
                "Use a time from 1677-09-21T00:12:43.145224192Z to \
                 2262-04-11T23:47:16.854775807Z",
            ),
            (
                Error::Span,
                "a span is not a number and a unit",
                "Write a span such as 250us, 1.5s, or 3d",
            ),
            (
                Error::Fraction,
                "a span is not a whole number of nanoseconds",
                "Use fewer fraction digits or a smaller unit",
            ),
            (
                Error::Long,
                "a span does not fit in 64-bit nanoseconds",
                "Use a span from -106751d to 106751d",
            ),
            (
                Error::Range,
                "a range has no `/` between two times",
                "Write a range as `<start>/<end>`",
            ),
            (
                Error::Reversed,
                "a range ends before it starts",
                "Put the earlier time first",
            ),
            (
                Error::Zero,
                "a rate has a zero numerator or denominator",
                "Use a rate above zero",
            ),
            (
                Error::Period,
                "the sample period of a rate is under 1 ns or longer than a span",
                "Use a rate from one sample in 106751d to 1 GHz",
            ),
        ] {
            assert_eq!(error.to_string(), message);
            assert_eq!(error.fix(), fix, "{error:?}");
            crate::common::assert_stated(message, fix);
        }
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
                    assert_eq!(text.parse::<Stamp>(), Err(Error::Era), "{text}");
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
                    assert_eq!(text.parse::<Stamp>(), Err(Error::Stamp), "{text}");
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
                    assert_eq!(text.parse::<Stamp>(), Err(Error::Date), "{text}");
                }
                assert_eq!("2000-02-29T00:00:00Z".parse(), Ok(seconds(951_782_400)));
                assert_eq!("2024-02-29T00:00:00Z".parse(), Ok(seconds(1_709_164_800)));
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
                assert_eq!(text.parse::<Span>(), Err(Error::Span), "{text}");
            }
        }

        #[test]
        fn rejects_fractions_of_a_nanosecond() {
            let long = format!("0.{}1s", "0".repeat(38));
            for text in ["0.5ns", "1.0000000001s", "0.000000000000000000001d", &long] {
                assert_eq!(text.parse::<Span>(), Err(Error::Fraction), "{text}");
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
                assert_eq!(text.parse::<Span>(), Err(Error::Long), "{text}");
            }
        }
    }

    mod monotonic {
        use super::*;

        const MAX: u64 = u64::MAX;
        const HALF: u64 = 1 << 63;

        mod checked {
            use super::*;

            fn check(start: u64, span: i64, sum: Option<u64>, difference: Option<u64>) {
                let (reading, span) = (Monotonic(start), Span::from_nanos(span));
                assert_eq!(
                    reading.checked_add(span),
                    sum.map(Monotonic),
                    "{start} + {span}"
                );
                assert_eq!(
                    reading.checked_sub(span),
                    difference.map(Monotonic),
                    "{start} - {span}"
                );
            }

            #[test]
            fn moves_by_a_span_of_either_sign() {
                check(10, 5, Some(15), Some(5));
                check(10, -5, Some(5), Some(15));
                check(10, 0, Some(10), Some(10));
            }

            #[test]
            fn returns_none_outside_u64() {
                check(MAX, 1, None, Some(MAX - 1));
                check(MAX, -1, Some(MAX - 1), None);
                check(0, 1, Some(1), None);
                check(0, -1, None, Some(1));
                check(0, i64::MIN, None, Some(HALF));
                check(HALF, i64::MIN, Some(0), None);
                check(HALF - 1, i64::MIN, None, Some(MAX));
                check(HALF + 1, i64::MIN, Some(1), None);
            }

            proptest! {
                #[test]
                fn matches_exact_math(start in any::<u64>(), span in any::<i64>()) {
                    let exact = |n: i128| u64::try_from(n).ok().map(Monotonic);
                    let (reading, wide) = (Monotonic(start), i128::from(start));
                    let span = Span::from_nanos(span);
                    let nanos = i128::from(span.nanos());
                    prop_assert_eq!(reading.checked_add(span), exact(wide + nanos));
                    prop_assert_eq!(reading.checked_sub(span), exact(wide - nanos));
                    if let Some(sum) = reading.checked_add(span) {
                        prop_assert_eq!(sum - reading, span);
                        prop_assert_eq!(sum - span, reading);
                    }
                }
            }
        }

        mod operators {
            use super::*;

            #[test]
            fn subtract_readings_to_the_i64_limits() {
                for (a, b, span) in [
                    (15, 10, 5),
                    (10, 15, -5),
                    (MAX, HALF, i64::MAX),
                    (0, HALF, i64::MIN),
                ] {
                    assert_eq!(Monotonic(a) - Monotonic(b), Span::from_nanos(span));
                }
            }

            #[test]
            #[should_panic(expected = "monotonic overflow: 0 ns + -1 ns")]
            fn panic_when_a_sum_passes_zero() {
                std::hint::black_box(Monotonic(0) + Span::from_nanos(-1));
            }

            #[test]
            #[should_panic(
                expected = "monotonic overflow: 18446744073709551615 ns + 1 ns"
            )]
            fn panic_when_a_sum_passes_the_maximum() {
                std::hint::black_box(Monotonic(MAX) + Span::NANOSECOND);
            }

            #[test]
            #[should_panic(expected = "monotonic overflow: 0 ns - 1 ns")]
            fn panic_when_a_difference_passes_zero() {
                std::hint::black_box(Monotonic(0) - Span::NANOSECOND);
            }

            #[test]
            #[should_panic(
                expected = "monotonic overflow: 18446744073709551615 ns - -1 ns"
            )]
            fn panic_when_a_difference_passes_the_maximum() {
                std::hint::black_box(Monotonic(MAX) - Span::from_nanos(-1));
            }

            #[test]
            #[should_panic(expected = "span overflow: 18446744073709551615 ns - 0 ns")]
            fn panic_when_a_reading_is_too_far_ahead() {
                std::hint::black_box(Monotonic(MAX) - Monotonic(0));
            }

            #[test]
            #[should_panic(expected = "span overflow: 0 ns - 9223372036854775809 ns")]
            fn panic_when_a_reading_is_too_far_behind() {
                std::hint::black_box(Monotonic(0) - Monotonic(HALF + 1));
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
            assert_eq!(text.parse::<Range>(), Err(Error::Reversed));
        }

        #[test]
        fn rejects_a_missing_slash() {
            let text = "2026-10-04T12:00:00Z";
            assert_eq!(text.parse::<Range>(), Err(Error::Range));
        }

        #[test]
        fn returns_the_error_of_the_stamp_that_does_not_read() {
            for (text, error) in [
                ("2026-10-04T12:00:00Z/later", Error::Stamp),
                ("later/2026-10-04T12:00:00Z", Error::Stamp),
                ("2026-10-04T12:00:00Z/2026-02-30T00:00:00Z", Error::Date),
                ("9999-01-01T00:00:00Z/2026-10-04T12:00:00Z", Error::Era),
                ("2026-10-04T12:00:00Z/2026-10-04T12:00:00Z/", Error::Stamp),
            ] {
                assert_eq!(text.parse::<Range>(), Err(error), "{text}");
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

        fn rate(num: u64, den: u64) -> Rate {
            Rate::new(num, den).unwrap()
        }

        fn rates() -> impl Strategy<Value = Rate> {
            prop_oneof![
                (1..=u64::MAX, 1..=u64::MAX),
                (1..=1_000_000_000_u64, Just(1)),
                (Just(1), 1..=1_000_000_u64),
                (1..=1000_u64, 1..=1000_u64),
            ]
            .prop_filter_map("a valid rate", |(num, den)| Rate::new(num, den).ok())
        }

        #[test]
        fn reduces_the_fraction() {
            for (num, den, reduced) in [
                (1, 1, (1, 1)),
                (1000, 1, (1000, 1)),
                (6, 4, (3, 2)),
                (4, 12, (1, 3)),
                (u64::MAX, u64::MAX, (1, 1)),
                (2_000_000_000, 2, (1_000_000_000, 1)),
            ] {
                let rate = rate(num, den);
                assert_eq!((rate.num(), rate.den()), reduced, "{num}/{den}");
            }
        }

        #[test]
        fn refuses_a_zero() {
            for (num, den) in [(0, 1), (1, 0), (0, 0)] {
                assert_eq!(Rate::new(num, den), Err(Error::Zero), "{num}/{den}");
            }
        }

        #[test]
        fn refuses_a_period_out_of_range() {
            for (num, den) in [
                (1_000_000_001, 1),
                (2_000_000_002, 2),
                (2_000_000_001, 2),
                (u64::MAX, 1),
                (1, 9_223_372_037),
                (1, u64::MAX),
            ] {
                assert_eq!(Rate::new(num, den), Err(Error::Period), "{num}/{den}");
            }
            for (num, den) in
                [(1_000_000_000, 1), (1_999_999_999, 2), (1, 9_223_372_036)]
            {
                assert!(Rate::new(num, den).is_ok(), "{num}/{den}");
            }
        }

        #[test]
        fn spans_whole_samples_rounded_down() {
            for (num, den, n, nanos) in [
                (7, 1, 0, 0),
                (1, 1, 3, 3_000_000_000),
                (3, 1, 1, 333_333_333),
                (3, 1, 2, 666_666_666),
                (3, 1, 3, 1_000_000_000),
                (1, 3, 1, 3_000_000_000),
                (1_000_000_000, 1, 7, 7),
                (1_999_999_999, 2, 1, 1),
                (1_999_999_999, 2, 1_999_999_999, 2_000_000_000),
                (1, 1, 9_223_372_036, 9_223_372_036_000_000_000),
            ] {
                let span = rate(num, den).span(n);
                assert_eq!(span, Span::from_nanos(nanos), "{n} at {num}/{den} Hz");
            }
        }

        #[test]
        #[should_panic(expected = "span overflow: 9223372037 samples at 1/1 Hz")]
        fn span_panics_past_i64() {
            let span = rate(1, 1).span(9_223_372_037);
            unreachable!("got {span}");
        }

        #[test]
        #[should_panic(expected = "span overflow: 4000000000000000000 samples at \
                                   11/100000000000 Hz")]
        fn span_panics_past_u128() {
            let span = rate(11, 100_000_000_000).span(4_000_000_000_000_000_000);
            unreachable!("got {span}");
        }

        #[test]
        fn counts_the_samples_that_fit() {
            for (num, den, nanos, count) in [
                (1, 1, 0, 0),
                (1, 1, 999_999_999, 0),
                (1, 1, 1_000_000_000, 1),
                (3, 1, 333_333_332, 0),
                (3, 1, 333_333_333, 1),
                (3, 1, 1_000_000_000, 3),
                (1, 3, 5_999_999_999, 1),
                (1_000_000_000, 1, 0, 0),
                (1_000_000_000, 1, 1, 1),
                (1_000_000_000, 1, i64::MAX, 9_223_372_036_854_775_807),
                (1, 1, i64::MAX, 9_223_372_036),
                (1, 1, -1, 0),
                (1, 1, i64::MIN, 0),
                (1_000_000_000, 1, -1, 0),
                (1_000_000_000, 1, i64::MIN, 0),
            ] {
                let span = Span::from_nanos(nanos);
                assert_eq!(
                    rate(num, den).count(span),
                    count,
                    "{span} at {num}/{den} Hz"
                );
            }
        }

        #[test]
        fn stamps_a_span_after_the_start() {
            let start = seconds(10);
            assert_eq!(
                rate(3, 1).stamp(start, 1),
                Stamp::from_nanos(10_333_333_333)
            );
            assert_eq!(rate(3, 1).stamp(start, 0), start);
        }

        #[test]
        fn stamps_past_a_span_that_does_not_fit() {
            let start = Stamp::from_nanos(i64::MIN);
            let stamp = rate(1, 1).stamp(start, 9_223_372_037);
            assert_eq!(stamp, Stamp::from_nanos(145_224_192));
        }

        #[test]
        #[should_panic(
            expected = "stamp overflow: 9223372036854775807 ns + 1 samples \
                                   at 1000000000/1 Hz"
        )]
        fn stamp_panics_past_i64() {
            let stamp = rate(1_000_000_000, 1).stamp(Stamp::from_nanos(i64::MAX), 1);
            unreachable!("got {stamp}");
        }

        #[test]
        #[should_panic(expected = "stamp overflow: -9223372036854775808 ns + \
                                   4000000000000000000 samples at 11/100000000000 Hz")]
        fn stamp_panics_past_u128() {
            let start = Stamp::from_nanos(i64::MIN);
            let stamp =
                rate(11, 100_000_000_000).stamp(start, 4_000_000_000_000_000_000);
            unreachable!("got {stamp}");
        }

        proptest! {
            #[test]
            fn counts_agree_with_spans(
                rate in rates(),
                nanos in prop_oneof![0..=i64::MAX, 0..=1_000_000_000_i64],
            ) {
                let span = Span::from_nanos(nanos);
                let count = rate.count(span);
                prop_assert!(rate.span(count) <= span);
                let next = rate.nanos(count + 1).and_then(|n| i64::try_from(n).ok());
                if let Some(next) = next {
                    prop_assert!(span < Span::from_nanos(next));
                }
            }

            #[test]
            fn counts_nothing_before_zero(rate in rates(), nanos in i64::MIN..0) {
                prop_assert_eq!(rate.count(Span::from_nanos(nanos)), 0);
            }

            #[test]
            fn spans_round_down(
                rate in rates(),
                n in prop_oneof![any::<u64>(), 0..=1_u64 << 34],
            ) {
                let (num, den) = (u128::from(rate.num()), u128::from(rate.den()));
                let exact = (u128::from(n) * den).checked_mul(1_000_000_000);
                match (exact, rate.nanos(n)) {
                    (Some(exact), Some(nanos)) => {
                        prop_assert!(nanos * num <= exact);
                        let above = (nanos + 1).checked_mul(num);
                        prop_assert!(above.is_none_or(|above| exact < above));
                    }
                    (exact, nanos) => prop_assert_eq!(exact.is_none(), nanos.is_none()),
                }
            }

            #[test]
            fn stamps_add_the_span(
                rate in rates(),
                start in any::<i64>(),
                n in 0..=1_u64 << 34,
            ) {
                let start = Stamp::from_nanos(start);
                let nanos = rate.nanos(n).and_then(|n| i64::try_from(n).ok());
                let stamp = nanos.and_then(|n| start.checked_add(Span::from_nanos(n)));
                if let Some(stamp) = stamp {
                    prop_assert_eq!(rate.stamp(start, n), stamp);
                }
            }
        }
    }
}

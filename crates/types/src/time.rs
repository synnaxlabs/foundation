//! Time values. Every timestamp is in mesh time: nanoseconds since the Unix epoch, UTC.

use std::fmt;
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
}

impl Add<Span> for Stamp {
    type Output = Self;

    /// # Panics
    ///
    /// On overflow. Values from outside use [`Stamp::checked_add`].
    fn add(self, span: Span) -> Self {
        self.checked_add(span).expect("stamp overflow")
    }
}

impl Sub<Span> for Stamp {
    type Output = Self;

    /// # Panics
    ///
    /// On overflow. Values from outside use [`Stamp::checked_sub`].
    fn sub(self, span: Span) -> Self {
        self.checked_sub(span).expect("stamp overflow")
    }
}

impl Sub for Stamp {
    type Output = Span;

    /// # Panics
    ///
    /// On overflow.
    fn sub(self, other: Self) -> Span {
        Span(self.0.checked_sub(other.0).expect("span overflow"))
    }
}

impl fmt::Display for Stamp {
    /// Writes RFC 3339 in UTC with nine fraction digits:
    /// `2026-10-04T12:00:00.000000000Z`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = f;
        todo!()
    }
}

impl FromStr for Stamp {
    type Err = ParseError;

    /// Reads RFC 3339 with any offset and up to nine fraction digits.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let _ = s;
        todo!()
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

impl fmt::Display for Span {
    /// Writes the span with the largest unit that keeps it exact: `250us`, `1.5s`,
    /// `3d`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = f;
        todo!()
    }
}

impl FromStr for Span {
    type Err = ParseError;

    /// Reads a number and a unit: `ns`, `us`, `ms`, `s`, `m`, `h`, or `d`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let _ = s;
        todo!()
    }
}

/// A half-open range of mesh time: from `start`, up to but not including `end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Range {
    /// The first instant in the range.
    pub start: Stamp,
    /// The first instant after the range.
    pub end: Stamp,
}

impl Range {
    /// Reports whether `stamp` is in the range.
    #[must_use]
    pub fn contains(&self, stamp: Stamp) -> bool {
        self.start <= stamp && stamp < self.end
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
        let _ = (num, den);
        todo!()
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
    pub fn stamp(self, start: Stamp, n: u64) -> Stamp {
        let _ = (start, n);
        todo!()
    }
}

impl fmt::Display for Rate {
    /// Writes the rate in hertz: `1kHz`, `100Hz`, `1/3Hz`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = f;
        todo!()
    }
}

impl FromStr for Rate {
    type Err = ParseError;

    /// Reads a rate in hertz, with an optional `k` or `M` prefix or a fraction.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let _ = s;
        todo!()
    }
}

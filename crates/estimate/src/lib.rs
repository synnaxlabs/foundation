//! Computes clock offset and error bounds from measurements, exchanges with other
//! clocks, and device oscillator fits.
//!
//! Each time source gives [`Measurement`]s. A [`Filter`] keeps the recent ones of one
//! source, and [`combine`] intersects the best of each source into one estimate. An
//! [`Overlap`] keeps what every reading of one device clock allows. An [`Exchange`]
//! turns one round trip to another clock into a measurement. The crate never knows
//! what a source is, and it reads no clock: the caller passes the local time.

#![deny(clippy::wildcard_enum_match_arm)]

mod combine;
mod drift;
mod exchange;
mod filter;
mod measurement;
mod overlap;
#[cfg(test)]
mod world;

use std::fmt;

use types::time::{Monotonic, Span};

pub use combine::combine;
pub use drift::Drift;
pub use exchange::Exchange;
pub use filter::Filter;
pub use measurement::Measurement;
pub use overlap::Overlap;

/// Why an estimate failed.
///
/// ```
/// let e = estimate::Error::NoMajority { sources: 2, agreeing: 1 };
/// assert_eq!(e.to_string(), "no majority of time sources agree: at most 1 of 2");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// An error bound is negative or more than 36500 days.
    Bound {
        /// The error bound.
        error: Span,
    },
    /// A drift rate is more than 10%.
    Drift {
        /// The rate in parts per billion.
        ppb: u32,
    },
    /// A reading is older than the newest one in an [`Overlap`].
    Backwards {
        /// The local time of the reading.
        at: Monotonic,
        /// The local time of the newest reading in the overlap.
        newest: Monotonic,
    },
    /// A reading shares no offset with an [`Overlap`].
    Disjoint,
    /// There are no measurements to combine.
    NoSources,
    /// No offset is inside the bounds of more than half of the sources.
    NoMajority {
        /// The number of sources.
        sources: usize,
        /// The most sources whose bounds share an offset.
        agreeing: usize,
    },
    /// An [`Overlap`] has no low edge or no high edge.
    Open,
    /// An [`Exchange`] allows no offset.
    Crossed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bound { error } => write!(
                f,
                "error bound {error} is not between 0s and {}",
                measurement::MAX_ERROR
            ),
            Self::Drift { ppb } => {
                write!(f, "drift {ppb} ppb is more than 100000000 ppb (10%)")
            }
            Self::Backwards { at, newest } => write!(
                f,
                "reading at {}ns is older than the newest at {}ns",
                at.0, newest.0
            ),
            Self::Disjoint => f.write_str("reading shares no offset with the overlap"),
            Self::NoSources => f.write_str("no time sources to combine"),
            Self::NoMajority { sources, agreeing } => write!(
                f,
                "no majority of time sources agree: at most {agreeing} of {sources}"
            ),
            Self::Open => f.write_str("overlap has no low edge or no high edge"),
            Self::Crossed => f.write_str("exchange allows no offset"),
        }
    }
}

impl std::error::Error for Error {}

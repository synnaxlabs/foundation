//! Computes clock offset and error bounds from measurements, the peer exchange, and
//! device oscillator fits.
//!
//! Each time source gives [`Measurement`]s. A [`Filter`] keeps the recent ones of one
//! source, and [`combine`] intersects the best of each source into one estimate. The
//! crate never knows what a source is, and it reads no clock: the caller passes the
//! local monotonic time.

#![deny(clippy::wildcard_enum_match_arm)]

mod combine;
mod drift;
mod filter;
mod measurement;

use std::fmt;

use types::time::Span;

pub use combine::combine;
pub use drift::Drift;
pub use filter::Filter;
pub use measurement::Measurement;

/// Why an estimate failed.
///
/// ```
/// let e = estimate::Error::NoMajority { sources: 2, agreeing: 1 };
/// assert_eq!(e.to_string(), "no majority of time sources agree: at most 1 of 2");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A measurement's error bound is negative.
    NegativeBound {
        /// The error bound.
        error: Span,
    },
    /// There are no measurements to combine.
    NoSources,
    /// No offset is inside the bounds of more than half of the sources.
    NoMajority {
        /// The number of sources.
        sources: usize,
        /// The most sources whose bounds share an offset.
        agreeing: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NegativeBound { error } => {
                write!(f, "error bound {error} is negative")
            }
            Self::NoSources => f.write_str("no time sources to combine"),
            Self::NoMajority { sources, agreeing } => write!(
                f,
                "no majority of time sources agree: at most {agreeing} of {sources}"
            ),
        }
    }
}

impl std::error::Error for Error {}

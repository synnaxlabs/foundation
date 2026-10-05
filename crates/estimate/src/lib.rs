//! Computes clock offset and error bounds from measurements, exchanges with other
//! clocks, and device oscillator fits.
//!
//! Each time source gives [`Measurement`]s. A [`Filter`] keeps the recent ones of one
//! source, and [`combine()`] intersects the best of each source into one estimate. An
//! [`Overlap`] keeps what every reading of one device clock allows. An [`Exchange`]
//! turns one round trip to another clock into a measurement. The crate never knows
//! what a source is, and it reads no clock: the caller passes the local time.

#![deny(clippy::wildcard_enum_match_arm)]

pub mod combine;
mod drift;
pub mod exchange;
mod filter;
mod measurement;
pub mod overlap;
#[cfg(test)]
mod world;

pub use combine::combine;
pub use drift::Drift;
pub use exchange::Exchange;
pub use filter::Filter;
pub use measurement::Measurement;
pub use overlap::Overlap;

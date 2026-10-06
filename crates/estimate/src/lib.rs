//! Computes clock offset and error bounds from measurements, exchanges with other
//! clocks, and device oscillator fits.
//!
//! Each time source gives [`Measurement`]s. A [`Filter`] keeps the recent ones of one
//! source, and [`combine::combine`] combines the best of each source into one
//! estimate. An [`overlap::Overlap`] keeps what every reading of one device clock
//! allows. An [`exchange::Exchange`] turns one round trip to another clock into a
//! measurement. A [`Slew`] moves mesh time toward an estimate without going back, and a
//! [`discipline::Discipline`] chooses what mesh time follows as estimates come and go.
//! The crate never knows what a source is, and it reads no clock: the caller passes
//! the local time.

#![deny(clippy::wildcard_enum_match_arm)]

pub mod combine;
pub mod discipline;
mod drift;
pub mod exchange;
mod filter;
mod measurement;
pub mod overlap;
mod slew;
#[cfg(test)]
mod world;

pub use drift::Drift;
pub use filter::Filter;
pub use measurement::Measurement;
pub use slew::Slew;

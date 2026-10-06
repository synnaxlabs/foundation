//! Writes a reader's samples to InfluxDB.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the InfluxDB client of #341 is the first user")
)]
mod http;
pub mod line;

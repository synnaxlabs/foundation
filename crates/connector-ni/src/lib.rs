//! Reads and writes NI data acquisition devices.

#![expect(unsafe_code, reason = "NI's driver is a C library")]
#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

pub mod daqmx;

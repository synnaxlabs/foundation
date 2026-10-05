//! Compresses and checks one series: per-vector selection, codecs, header validation,
//! format version.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

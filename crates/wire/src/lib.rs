//! Defines every message between two nodes: per-connection short numbers, predicted seq
//! and counts, session, credit, and replication messages, format version.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

pub mod protocol;

/// The wire format version this node writes and reads.
pub const VERSION: u16 = 1;

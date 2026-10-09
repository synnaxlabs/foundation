//! Defines every message between two nodes: per-connection short numbers, predicted seq
//! and counts, session, credit, and replication messages, format version.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

pub mod blob;
pub mod clock;
mod common;
pub mod header;
pub mod hub;
pub mod session;

/// The wire format version this node writes and reads. It covers every byte after the
/// header, encoded series included.
pub const VERSION: u16 = 1;

/// The protocol that a stream or datagram carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// Clock offset exchange.
    Clock,
    /// Region consensus and region state.
    Mesh,
    /// An index's log, from its home to a standby or copy node.
    Replica,
    /// Content by hash: spec chunks and binaries.
    Blob,
    /// Reads and writes across homes.
    Hub,
}

impl Protocol {
    const ALL: [Self; 5] = [
        Self::Clock,
        Self::Mesh,
        Self::Replica,
        Self::Blob,
        Self::Hub,
    ];

    fn number(self) -> u8 {
        match self {
            Self::Clock => 1,
            Self::Mesh => 2,
            Self::Replica => 3,
            Self::Blob => 4,
            Self::Hub => 5,
        }
    }
}

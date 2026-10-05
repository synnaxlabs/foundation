//! Reads and commands Modbus TCP devices.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use std::fmt;

pub mod pdu;
pub mod tcp;

/// Why a Modbus frame or PDU is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A PDU whose length is not the one its function and counts give.
    Size {
        /// The length the PDU needs.
        want: usize,
        /// The length it has.
        got: usize,
    },
    /// An MBAP header with a protocol other than 0 (Modbus).
    Protocol(u16),
    /// An MBAP length outside 2 to 254.
    Length(u16),
    /// A function code this crate does not read.
    Function(u8),
    /// A count of 0, or above the most that one PDU carries.
    Count {
        /// The count.
        count: u16,
        /// The most for the function.
        max: u16,
    },
    /// A range that runs past address 65535.
    Range {
        /// The first address.
        start: u16,
        /// The number of items.
        count: u16,
    },
    /// A coil value other than 0x0000 or 0xFF00.
    Coil(u16),
    /// A reply of another function than the request's.
    Answer {
        /// The request's function.
        want: u8,
        /// The reply's function.
        got: u8,
    },
    /// A byte count that is not the one the item count gives.
    Bytes {
        /// The byte count the item count gives.
        want: usize,
        /// The byte count in the PDU.
        got: usize,
    },
    /// A write reply whose echo of address and value (or count) differs.
    Echo {
        /// The address and value (or count) written.
        want: [u16; 2],
        /// The ones the device echoed.
        got: [u16; 2],
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Size { want, got } => {
                write!(f, "a PDU of {got} bytes where its function gives {want}")
            }
            Self::Protocol(protocol) => {
                write!(f, "an MBAP header with protocol {protocol}, not 0")
            }
            Self::Length(length) => {
                write!(f, "an MBAP length of {length}, outside 2 to 254")
            }
            Self::Function(function) => {
                write!(
                    f,
                    "function code {function} is not one this connector reads"
                )
            }
            Self::Count { count, max } => {
                write!(f, "a count of {count}, outside 1 to {max}")
            }
            Self::Range { start, count } => {
                write!(
                    f,
                    "{count} items from address {start} run past address 65535"
                )
            }
            Self::Coil(value) => {
                write!(f, "a coil value of {value:#06x}, not 0x0000 or 0xff00")
            }
            Self::Answer { want, got } => {
                write!(
                    f,
                    "a reply of function {got} to a request of function {want}"
                )
            }
            Self::Bytes { want, got } => {
                write!(f, "a byte count of {got} where the item count gives {want}")
            }
            Self::Echo { want, got } => {
                write!(f, "a write reply that echoes {got:?}, not {want:?}")
            }
        }
    }
}

impl std::error::Error for Error {}

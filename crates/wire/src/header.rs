//! The header at the start of each stream's first message and of each datagram: the
//! wire version (little-endian `u16`), then the protocol number (`u8`).

use std::fmt;

use crate::{Protocol, VERSION};

/// The bytes of a header.
pub const LEN: usize = 3;

/// The code that stops a stream whose header is not valid. Stop and reset codes 1 to
/// 15 belong to the header; each protocol numbers its own codes from 16.
pub const REJECTED: u32 = 1;

/// Returns the header of a stream or datagram that carries `protocol`, at
/// [`VERSION`].
#[must_use]
pub fn encode(protocol: Protocol) -> [u8; LEN] {
    let [low, high] = VERSION.to_le_bytes();
    [low, high, protocol.number()]
}

/// Decodes the header at the front of `bytes`. Returns its protocol and the bytes
/// after it.
///
/// # Errors
///
/// Returns [`Error::Version`] when the version is not [`VERSION`],
/// [`Error::Protocol`] when the protocol number is unknown, and [`Error::Truncated`]
/// when `bytes` ends before either is complete.
pub fn decode(bytes: &[u8]) -> Result<(Protocol, &[u8]), Error> {
    let truncated = || Error::Truncated {
        available: bytes.len(),
    };
    let (&version, rest) = bytes.split_first_chunk().ok_or_else(truncated)?;
    let version = u16::from_le_bytes(version);
    if version != VERSION {
        return Err(Error::Version { version });
    }
    let (&number, rest) = rest.split_first().ok_or_else(truncated)?;
    let protocol = Protocol::ALL
        .into_iter()
        .find(|protocol| protocol.number() == number)
        .ok_or(Error::Protocol { number })?;
    Ok((protocol, rest))
}

/// A header that is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The bytes end inside the header.
    Truncated {
        /// The bytes that arrived.
        available: usize,
    },
    /// The wire version is not [`VERSION`].
    Version {
        /// The version in the header.
        version: u16,
    },
    /// The protocol number is unknown.
    Protocol {
        /// The protocol number in the header.
        number: u8,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { available } => {
                write!(
                    f,
                    "the message ends after {available} of the {LEN} header bytes"
                )
            }
            Self::Version { version } => write!(
                f,
                "the peer writes wire version {version} and this node writes \
                 {VERSION}: upgrade the node with the lower version"
            ),
            Self::Protocol { number } => write!(
                f,
                "the header names protocol {number}, which this node does not know"
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use proptest::sample::select;

    use super::*;

    const PROTOCOLS: [(Protocol, u8); 5] = [
        (Protocol::Clock, 1),
        (Protocol::Mesh, 2),
        (Protocol::Replica, 3),
        (Protocol::Blob, 4),
        (Protocol::Hub, 5),
    ];

    #[test]
    fn pins_the_wire_values() {
        for (protocol, number) in PROTOCOLS {
            assert_eq!(encode(protocol), [1, 0, number], "{protocol:?}");
        }
        assert_eq!(REJECTED, 1);
    }

    #[test]
    fn rejects_unknown_protocol_numbers() {
        for number in (0..=u8::MAX).filter(|n| !(1..=5).contains(n)) {
            assert_eq!(decode(&[1, 0, number]), Err(Error::Protocol { number }));
        }
    }

    #[test]
    fn rejects_versions_other_than_this_one() {
        for version in [0, 2, u16::MAX] {
            let [low, high] = version.to_le_bytes();
            assert_eq!(decode(&[low, high, 1]), Err(Error::Version { version }));
        }
    }

    #[test]
    fn checks_the_version_before_the_rest() {
        assert_eq!(decode(&[2, 0, 0]), Err(Error::Version { version: 2 }));
        assert_eq!(decode(&[2, 0]), Err(Error::Version { version: 2 }));
    }

    #[test]
    fn rejects_bytes_that_end_inside_the_header() {
        for bytes in [&[][..], &[1], &[1, 0]] {
            let available = bytes.len();
            assert_eq!(decode(bytes), Err(Error::Truncated { available }));
        }
    }

    #[test]
    fn describes_each_error() {
        for (error, text) in [
            (
                Error::Truncated { available: 1 },
                "the message ends after 1 of the 3 header bytes",
            ),
            (
                Error::Version { version: 7 },
                "the peer writes wire version 7 and this node writes 1: upgrade the \
                 node with the lower version",
            ),
            (
                Error::Protocol { number: 9 },
                "the header names protocol 9, which this node does not know",
            ),
        ] {
            assert_eq!(error.to_string(), text);
        }
    }

    /// Random bytes, half of them after a valid version and a protocol number from 0
    /// to 7.
    fn bytes() -> impl Strategy<Value = Vec<u8>> {
        let tail = proptest::collection::vec(any::<u8>(), 0..8);
        let valid = (0..8_u8, tail.clone())
            .prop_map(|(number, tail)| [[1, 0, number].as_slice(), &tail].concat());
        prop_oneof![tail, valid]
    }

    proptest! {
        #[test]
        fn round_trips_with_the_bytes_after_it(
            (protocol, _) in select(&PROTOCOLS),
            rest in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            let bytes = [encode(protocol).as_slice(), &rest].concat();
            prop_assert_eq!(decode(&bytes), Ok((protocol, rest.as_slice())));
        }

        #[test]
        fn decodes_only_what_it_encodes(bytes in bytes()) {
            if let Ok((protocol, rest)) = decode(&bytes) {
                prop_assert_eq!([encode(protocol).as_slice(), rest].concat(), bytes);
            }
        }
    }
}

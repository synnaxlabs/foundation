//! The header at the start of each stream and datagram: the wire version
//! (little-endian `u16`), then the protocol number (`u8`).

use std::fmt;

use crate::VERSION;

/// The stream reset code for a header that is not valid. Codes 1 to 15 belong to the
/// header; each protocol numbers its own codes from 16.
pub const REJECTED: u32 = 1;

/// A protocol between two nodes.
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

/// The first bytes of a stream's first message and of each datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// The wire version of the bytes after the header.
    pub version: u16,
    /// The protocol of the stream or datagram.
    pub protocol: Protocol,
}

impl Header {
    /// The bytes of an encoded header. Each datagram carries this many more bytes.
    pub const LEN: usize = 3;

    /// Returns the bytes that start a stream's first message or a datagram.
    #[must_use]
    pub fn encode(self) -> [u8; Self::LEN] {
        let [low, high] = self.version.to_le_bytes();
        [low, high, self.protocol.number()]
    }

    /// Decodes the header at the front of `bytes`. Returns it and the bytes after it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] when `bytes` is shorter than [`Header::LEN`],
    /// [`Error::Version`] when this node does not read the version, and
    /// [`Error::Protocol`] when the protocol number is unknown.
    pub fn decode(bytes: &[u8]) -> Result<(Self, &[u8]), Error> {
        let Some((&[low, high, number], rest)) = bytes.split_first_chunk() else {
            return Err(Error::Truncated {
                available: bytes.len(),
            });
        };
        let version = u16::from_le_bytes([low, high]);
        if version != VERSION {
            return Err(Error::Version { version });
        }
        let protocol = Protocol::ALL
            .into_iter()
            .find(|protocol| protocol.number() == number)
            .ok_or(Error::Protocol { protocol: number })?;
        Ok((Self { version, protocol }, rest))
    }
}

/// A header that is not valid. It comes from a peer: log it, then reset the stream
/// with [`REJECTED`] or drop the datagram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The bytes end before the header does.
    Truncated {
        /// The bytes that arrived.
        available: usize,
    },
    /// This node does not read the wire version.
    Version {
        /// The version in the header.
        version: u16,
    },
    /// The protocol number is unknown.
    Protocol {
        /// The protocol number in the header.
        protocol: u8,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { available } => {
                let len = Header::LEN;
                write!(
                    f,
                    "the peer sent {available} bytes, fewer than the {len} of a \
                     protocol header"
                )
            }
            Self::Version { version } => write!(
                f,
                "the peer writes wire version {version}, which this node does not read"
            ),
            Self::Protocol { protocol } => write!(
                f,
                "the peer names protocol {protocol}, which this node does not know"
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

    fn header(protocol: Protocol) -> Header {
        Header {
            version: VERSION,
            protocol,
        }
    }

    #[test]
    fn encodes_the_version_then_the_protocol_number() {
        for (protocol, number) in PROTOCOLS {
            assert_eq!(header(protocol).encode(), [1, 0, number], "{protocol:?}");
        }
        let header = Header {
            version: 0x0102,
            protocol: Protocol::Hub,
        };
        assert_eq!(header.encode(), [2, 1, 5]);
    }

    #[test]
    fn rejects_unknown_protocol_numbers() {
        for protocol in (0..=u8::MAX).filter(|n| !(1..=5).contains(n)) {
            assert_eq!(
                Header::decode(&[1, 0, protocol]),
                Err(Error::Protocol { protocol })
            );
        }
    }

    #[test]
    fn rejects_versions_this_node_does_not_read() {
        for version in [0, 2, u16::MAX] {
            let [low, high] = version.to_le_bytes();
            assert_eq!(
                Header::decode(&[low, high, 1]),
                Err(Error::Version { version })
            );
        }
    }

    #[test]
    fn checks_the_version_before_the_protocol() {
        assert_eq!(
            Header::decode(&[2, 0, 0]),
            Err(Error::Version { version: 2 })
        );
    }

    #[test]
    fn rejects_bytes_shorter_than_a_header() {
        for bytes in [&[][..], &[1], &[1, 0]] {
            assert_eq!(
                Header::decode(bytes),
                Err(Error::Truncated {
                    available: bytes.len()
                })
            );
        }
    }

    #[test]
    fn describes_each_error() {
        for (error, text) in [
            (
                Error::Truncated { available: 2 },
                "the peer sent 2 bytes, fewer than the 3 of a protocol header",
            ),
            (
                Error::Version { version: 7 },
                "the peer writes wire version 7, which this node does not read",
            ),
            (
                Error::Protocol { protocol: 9 },
                "the peer names protocol 9, which this node does not know",
            ),
        ] {
            assert_eq!(error.to_string(), text);
        }
    }

    proptest! {
        #[test]
        fn round_trips_with_the_bytes_after_it(
            (protocol, _) in select(&PROTOCOLS),
            rest in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            let header = header(protocol);
            let bytes = [header.encode().as_slice(), &rest].concat();
            prop_assert_eq!(Header::decode(&bytes), Ok((header, rest.as_slice())));
        }

        #[test]
        fn never_panics_on_random_bytes(
            bytes in proptest::collection::vec(any::<u8>(), 0..8),
        ) {
            if let Ok((header, rest)) = Header::decode(&bytes) {
                prop_assert_eq!(
                    [header.encode().as_slice(), rest].concat(),
                    bytes
                );
            }
        }
    }
}

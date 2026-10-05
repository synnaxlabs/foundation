use std::fmt;

use types::node::PublicKey;

use crate::code::Code;

/// An error from a [`Transport`](crate::Transport), a [`Session`](crate::Session), or
/// a stream.
///
/// ```
/// use transport::{Code, Error};
///
/// fn superseded(error: &Error) -> bool {
///     *error == Error::Reset { code: Code(1) }
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// No address reached the peer.
    Unreachable {
        /// The peer that was dialed.
        peer: PublicKey,
    },
    /// The peer could not prove that it holds the key that was dialed.
    Authentication {
        /// The key that was dialed.
        expected: PublicKey,
    },
    /// This node closed the session.
    Closed {
        /// The code it closed with.
        code: Code,
    },
    /// The peer closed the session.
    PeerClosed {
        /// The code the peer closed with.
        code: Code,
    },
    /// The peer was silent for longer than [`Config::idle`](crate::Config::idle).
    TimedOut,
    /// The peer cancelled the stream before it finished sending.
    Reset {
        /// The code the peer reset with.
        code: Code,
    },
    /// The peer stopped reading the stream.
    Stopped {
        /// The code the peer stopped with.
        code: Code,
    },
    /// A message is larger than
    /// [`Config::message_bytes_max`](crate::Config::message_bytes_max) or the
    /// session's datagram limit.
    TooLarge {
        /// The message's size.
        bytes: usize,
        /// The largest size allowed.
        bytes_max: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable { peer } => {
                write!(f, "no address reached peer {}", Hex(peer))
            }
            Self::Authentication { expected } => {
                write!(f, "the peer did not prove key {}", Hex(expected))
            }
            Self::Closed { code } => {
                write!(f, "this node closed the session ({})", code.0)
            }
            Self::PeerClosed { code } => {
                write!(f, "the peer closed the session ({})", code.0)
            }
            Self::TimedOut => write!(f, "the peer stopped answering"),
            Self::Reset { code } => write!(f, "the peer reset the stream ({})", code.0),
            Self::Stopped { code } => {
                write!(f, "the peer stopped the stream ({})", code.0)
            }
            Self::TooLarge { bytes, bytes_max } => {
                write!(
                    f,
                    "a message of {bytes} bytes is over the limit of {bytes_max}"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

/// Writes a key as 64 lowercase hex digits.
struct Hex<'a>(&'a PublicKey);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod display {
        use super::*;

        #[test]
        fn writes_the_peer_key_in_hex() {
            let mut key = [0; 32];
            key[0] = 0xab;
            key[31] = 0x01;
            let error = Error::Unreachable {
                peer: PublicKey(key),
            };
            assert_eq!(
                error.to_string(),
                format!("no address reached peer ab{}01", "00".repeat(30))
            );
        }

        #[test]
        fn names_the_code_of_a_reset() {
            let error = Error::Reset { code: Code(7) };
            assert_eq!(error.to_string(), "the peer reset the stream (7)");
        }

        #[test]
        fn names_both_sizes_of_a_large_message() {
            let error = Error::TooLarge {
                bytes: 20,
                bytes_max: 16,
            };
            assert_eq!(
                error.to_string(),
                "a message of 20 bytes is over the limit of 16"
            );
        }
    }
}

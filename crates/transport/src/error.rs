use std::fmt;

use types::node::PublicKey;

use crate::address::Address;
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
        /// Each address tried, with why it failed.
        attempts: Vec<(Address, Error)>,
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
    /// A message is larger than the peer accepts on a stream, or than a datagram
    /// carries.
    TooLarge {
        /// The message's size.
        bytes: usize,
        /// The largest size allowed.
        bytes_max: usize,
    },
    /// The connection ended with no close from either side: a reset, a protocol
    /// violation, or a failed TLS check.
    Broken {
        /// What broke, for people to read.
        reason: String,
    },
    /// The shard's pool has no room for a received message now. The message stays
    /// queued; call again when a block frees.
    Pool {
        /// The message's size.
        bytes: usize,
        /// The bytes of the pool's budget that are free. A block for `bytes` needs
        /// more.
        available: usize,
    },
    /// A [`Config`](crate::Config) value is out of range.
    Config {
        /// The field's name.
        field: &'static str,
        /// The range it must be in.
        rule: &'static str,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable { peer, attempts } => {
                write!(f, "no address reached peer {peer}")?;
                attempts.iter().try_for_each(|(address, error)| {
                    write!(f, "; {address:?}: {error}")
                })
            }
            Self::Authentication { expected } => {
                write!(f, "the peer did not prove key {expected}")
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
            Self::Broken { reason } => write!(f, "the connection broke: {reason}"),
            Self::Pool { bytes, available } => write!(
                f,
                "no room for a received message of {bytes} bytes ({available} free)"
            ),
            Self::Config { field, rule } => write!(f, "config {field} {rule}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    mod display {
        use super::*;

        fn check(error: &Error, expected: &str) {
            assert_eq!(error.to_string(), expected);
        }

        fn key() -> PublicKey {
            let mut key = [0; 32];
            key[0] = 0xab;
            key[31] = 0x01;
            PublicKey(key)
        }

        fn hex() -> String {
            format!("ab{}01", "00".repeat(30))
        }

        #[test]
        fn names_each_failed_address() {
            let at: SocketAddr = "10.0.0.1:7400".parse().expect("a valid address");
            let error = Error::Unreachable {
                peer: key(),
                attempts: vec![
                    (Address::Udp(at), Error::TimedOut),
                    (Address::Tcp(at), Error::Authentication { expected: key() }),
                ],
            };
            check(
                &error,
                &format!(
                    "no address reached peer {0}; Udp(10.0.0.1:7400): the peer stopped \
                     answering; Tcp(10.0.0.1:7400): the peer did not prove key {0}",
                    hex()
                ),
            );
        }

        #[test]
        fn names_the_key_that_was_dialed() {
            let error = Error::Authentication { expected: key() };
            check(&error, &format!("the peer did not prove key {}", hex()));
        }

        #[test]
        fn names_who_closed_and_the_code() {
            check(
                &Error::Closed { code: Code(3) },
                "this node closed the session (3)",
            );
            check(
                &Error::PeerClosed { code: Code(4) },
                "the peer closed the session (4)",
            );
        }

        #[test]
        fn says_the_peer_went_silent() {
            check(&Error::TimedOut, "the peer stopped answering");
        }

        #[test]
        fn names_the_code_of_a_reset_or_stop() {
            check(
                &Error::Reset { code: Code(7) },
                "the peer reset the stream (7)",
            );
            check(
                &Error::Stopped { code: Code(8) },
                "the peer stopped the stream (8)",
            );
        }

        #[test]
        fn names_both_sizes_of_a_large_message() {
            check(
                &Error::TooLarge {
                    bytes: 20,
                    bytes_max: 16,
                },
                "a message of 20 bytes is over the limit of 16",
            );
        }

        #[test]
        fn gives_the_reason_a_connection_broke() {
            let error = Error::Broken {
                reason: "stateless reset".to_owned(),
            };
            check(&error, "the connection broke: stateless reset");
        }

        #[test]
        fn gives_the_size_and_the_room_of_the_pool() {
            let error = Error::Pool {
                bytes: 10,
                available: 4,
            };
            check(
                &error,
                "no room for a received message of 10 bytes (4 free)",
            );
        }

        #[test]
        fn names_the_field_and_its_range() {
            let error = Error::Config {
                field: "idle",
                rule: "must be positive",
            };
            check(&error, "config idle must be positive");
        }
    }
}

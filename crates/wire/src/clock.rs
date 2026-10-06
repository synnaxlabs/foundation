//! The clock exchange: a node asks a peer for its time, and the peer answers. Each
//! message is one datagram of [`Protocol::Clock`](crate::Protocol::Clock), after the
//! header.
//!
//! A message is a kind byte, then little-endian fields of 8 bytes each:
//!
//! - 1, a [`Request`]: `sent` (`u64`). 9 bytes.
//! - 2, an [`Answer`] with [`Time::Known`]: `sent`, then the `earliest` and `latest`
//!   (`i64`) of `received` and then of `answered`. 41 bytes.
//! - 3, an [`Answer`] with [`Time::Unknown`]: `sent`, then `answered` (`i64`). 17
//!   bytes.

use std::{fmt, iter};

use types::time::{Interval, Monotonic, Stamp};

/// The bytes of the largest message.
pub const MAX_LEN: usize = 41;

const REQUEST: u8 = 1;
const KNOWN: u8 = 2;
const UNKNOWN: u8 = 3;

/// A datagram of [`Protocol::Clock`](crate::Protocol::Clock), after the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    /// A node asks a peer for its time.
    Request(Request),
    /// The peer's answer to a request.
    Answer(Answer),
}

/// A request for a peer's time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// The asking node's monotonic reading when the request left. The answer echoes
    /// it.
    pub sent: Monotonic,
}

/// A peer's answer to a [`Request`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The `sent` of the request, echoed.
    pub sent: Monotonic,
    /// The peer's time.
    pub time: Time,
}

/// The time of the node that answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Time {
    /// Its time with a known bound when the request arrived and when it answered, as
    /// it sent them: nothing checks the order of either interval.
    Known {
        /// Its time when the request arrived.
        received: Interval,
        /// Its time when it answered.
        answered: Interval,
    },
    /// Its time has an unknown bound. Holds its best guess when it answered.
    Unknown {
        /// Its best guess of the time when it answered.
        answered: Stamp,
    },
}

/// Writes `message` into `out` and returns the bytes it wrote. A datagram carries them
/// after its header.
#[must_use]
pub fn encode<'o>(message: &Message, out: &'o mut [u8; MAX_LEN]) -> &'o [u8] {
    let word = |stamp: Stamp| stamp.nanos().cast_unsigned();
    match *message {
        Message::Request(Request { sent }) => put(out, REQUEST, [sent.0]),
        Message::Answer(Answer { sent, time }) => match time {
            Time::Known { received, answered } => put(
                out,
                KNOWN,
                [
                    sent.0,
                    word(received.earliest),
                    word(received.latest),
                    word(answered.earliest),
                    word(answered.latest),
                ],
            ),
            Time::Unknown { answered } => put(out, UNKNOWN, [sent.0, word(answered)]),
        },
    }
}

/// Decodes the message in `bytes`, the datagram after its header.
///
/// # Errors
///
/// [`Error::Empty`] when `bytes` is empty, [`Error::Kind`] when the first byte names
/// no message, and [`Error::Length`] when the length is not the length of that kind.
pub fn decode(bytes: &[u8]) -> Result<Message, Error> {
    let &kind = bytes.first().ok_or(Error::Empty)?;
    let stamp = |word: u64| Stamp::from_nanos(word.cast_signed());
    let interval = |earliest, latest| Interval {
        earliest: stamp(earliest),
        latest: stamp(latest),
    };
    let message = match kind {
        REQUEST => {
            let [sent] = words(bytes)?;
            Message::Request(Request {
                sent: Monotonic(sent),
            })
        }
        KNOWN => {
            let [sent, received_0, received_1, answered_0, answered_1] = words(bytes)?;
            let time = Time::Known {
                received: interval(received_0, received_1),
                answered: interval(answered_0, answered_1),
            };
            answer(sent, time)
        }
        UNKNOWN => {
            let [sent, answered] = words(bytes)?;
            answer(
                sent,
                Time::Unknown {
                    answered: stamp(answered),
                },
            )
        }
        kind => return Err(Error::Kind { kind }),
    };
    Ok(message)
}

/// A clock message that is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// No bytes follow the header.
    Empty,
    /// The first byte names no message.
    Kind {
        /// The first byte.
        kind: u8,
    },
    /// The message is not the length of its kind.
    Length {
        /// The bytes of the message.
        len: usize,
        /// The bytes of a message of its kind.
        expected: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the clock message is empty"),
            Self::Kind { kind } => write!(
                f,
                "the clock message has kind {kind}, which this node does not know"
            ),
            Self::Length { len, expected } => write!(
                f,
                "the clock message has {len} bytes, and a message of its kind has \
                 {expected}"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Writes `kind` and then `words` to the front of `out`, and returns those bytes.
fn put<const N: usize>(out: &mut [u8; MAX_LEN], kind: u8, words: [u64; N]) -> &[u8] {
    let len = const {
        let len = 1 + 8 * N;
        assert!(len <= MAX_LEN, "a message is longer than MAX_LEN");
        len
    };
    let bytes = iter::once(kind).chain(words.into_iter().flat_map(u64::to_le_bytes));
    for (at, byte) in out.iter_mut().zip(bytes) {
        *at = byte;
    }
    out.split_at(len).0
}

/// Reads the `N` words after the kind byte of `message`.
fn words<const N: usize>(message: &[u8]) -> Result<[u64; N], Error> {
    let (words, rest) = message.get(1..).unwrap_or_default().as_chunks::<8>();
    match <&[[u8; 8]; N]>::try_from(words) {
        Ok(words) if rest.is_empty() => Ok(words.map(u64::from_le_bytes)),
        _ => Err(Error::Length {
            len: message.len(),
            expected: const { 1 + 8 * N },
        }),
    }
}

fn answer(sent: u64, time: Time) -> Message {
    Message::Answer(Answer {
        sent: Monotonic(sent),
        time,
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn interval(earliest: i64, latest: i64) -> Interval {
        Interval {
            earliest: Stamp::from_nanos(earliest),
            latest: Stamp::from_nanos(latest),
        }
    }

    /// The message of `kind` with `len` bytes, zeros after the kind.
    fn zeros(kind: u8, len: usize) -> Vec<u8> {
        iter::once(kind).chain(iter::repeat(0)).take(len).collect()
    }

    #[test]
    fn pins_the_wire_values() {
        let request = Message::Request(Request {
            sent: Monotonic(0x0102_0304_0506_0708),
        });
        let known = Message::Answer(Answer {
            sent: Monotonic(1),
            time: Time::Known {
                received: interval(-1, 2),
                answered: interval(3, i64::MAX),
            },
        });
        let unknown = Message::Answer(Answer {
            sent: Monotonic(9),
            time: Time::Unknown {
                answered: Stamp::from_nanos(i64::MIN),
            },
        });
        let cases = [
            (request, vec![1, 8, 7, 6, 5, 4, 3, 2, 1]),
            (
                known,
                [
                    [2].as_slice(),
                    &[1, 0, 0, 0, 0, 0, 0, 0],
                    &[0xff; 8],
                    &[2, 0, 0, 0, 0, 0, 0, 0],
                    &[3, 0, 0, 0, 0, 0, 0, 0],
                    &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f],
                ]
                .concat(),
            ),
            (
                unknown,
                [
                    [3].as_slice(),
                    &[9, 0, 0, 0, 0, 0, 0, 0],
                    &[0, 0, 0, 0, 0, 0, 0, 0x80],
                ]
                .concat(),
            ),
        ];
        for (message, bytes) in cases {
            assert_eq!(encode(&message, &mut [0; MAX_LEN]), bytes, "{message:?}");
            assert_eq!(decode(&bytes), Ok(message));
        }
        assert_eq!(MAX_LEN, 41);
    }

    #[test]
    fn rejects_an_empty_message() {
        assert_eq!(decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn rejects_unknown_kinds() {
        for kind in (0..=u8::MAX).filter(|kind| !(1..=3).contains(kind)) {
            assert_eq!(decode(&zeros(kind, 9)), Err(Error::Kind { kind }));
            assert_eq!(decode(&[kind]), Err(Error::Kind { kind }));
        }
    }

    #[test]
    fn rejects_a_message_not_the_length_of_its_kind() {
        for (kind, expected, lens) in [
            (1, 9, [1, 8, 10, 17]),
            (2, 41, [1, 40, 42, 49]),
            (3, 17, [1, 16, 18, 25]),
        ] {
            for len in lens {
                let error = Error::Length { len, expected };
                assert_eq!(decode(&zeros(kind, len)), Err(error), "{kind}");
            }
        }
    }

    #[test]
    fn describes_each_error() {
        for (error, text) in [
            (Error::Empty, "the clock message is empty"),
            (
                Error::Kind { kind: 9 },
                "the clock message has kind 9, which this node does not know",
            ),
            (
                Error::Length {
                    len: 8,
                    expected: 9,
                },
                "the clock message has 8 bytes, and a message of its kind has 9",
            ),
        ] {
            assert_eq!(error.to_string(), text);
        }
    }

    fn message() -> impl Strategy<Value = Message> {
        let sent = any::<u64>().prop_map(Monotonic);
        let stamp = any::<i64>().prop_map(Stamp::from_nanos);
        let interval = (stamp.clone(), stamp.clone())
            .prop_map(|(earliest, latest)| Interval { earliest, latest });
        let time = prop_oneof![
            (interval.clone(), interval)
                .prop_map(|(received, answered)| Time::Known { received, answered }),
            stamp.prop_map(|answered| Time::Unknown { answered }),
        ];
        prop_oneof![
            sent.clone()
                .prop_map(|sent| Message::Request(Request { sent })),
            (sent, time)
                .prop_map(|(sent, time)| Message::Answer(Answer { sent, time })),
        ]
    }

    /// A kind byte from 0 to 4, then random bytes, most of them the length of a kind.
    fn bytes() -> impl Strategy<Value = Vec<u8>> {
        let body = prop_oneof![Just(8), Just(16), Just(40), 0..48_usize];
        (0..5_u8, body).prop_flat_map(|(kind, body)| {
            proptest::collection::vec(any::<u8>(), body)
                .prop_map(move |body| [[kind].as_slice(), &body].concat())
        })
    }

    proptest! {
        #[test]
        fn round_trips(message in message(), stale in any::<u8>()) {
            let mut out = [stale; MAX_LEN];
            prop_assert_eq!(decode(encode(&message, &mut out)), Ok(message));
        }

        #[test]
        fn decodes_only_what_it_encodes(bytes in bytes()) {
            if let Ok(message) = decode(&bytes) {
                let mut out = [0; MAX_LEN];
                prop_assert_eq!(encode(&message, &mut out), bytes.as_slice());
            }
        }
    }
}

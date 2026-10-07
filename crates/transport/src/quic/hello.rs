//! The hello: the limits a node sends on its first one-way stream, which its peer
//! obeys. It is (id, value) pairs, both QUIC varints, ids strictly increasing.

use noq_proto::{Dir, ReadError, StreamEvent, StreamId, VarInt};

use super::connection::Fault;
use crate::MESSAGE_BYTES_MIN;
use crate::varint::{self, Varint};

/// The most bytes a hello takes.
pub(super) const BYTES_MAX: usize = 256;

const WINDOW: u64 = 0;
const MESSAGE: u64 = 1;

/// The limits of the node that sends the hello.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    /// Its `window_bytes`: the most bytes of started messages it takes in.
    pub window_bytes: usize,
    /// Its `message_bytes_max`: the largest message it takes.
    pub message_bytes_max: usize,
}

impl Hello {
    /// The bytes of the hello. A value over 2^62 − 1 is sent as 2^62 − 1.
    #[must_use]
    #[cfg_attr(
        feature = "fuzzing",
        expect(
            clippy::missing_panics_doc,
            reason = "the ids are constants below 2^62"
        )
    )]
    pub fn encode(&self) -> Vec<u8> {
        let pairs = [
            (WINDOW, self.window_bytes),
            (MESSAGE, self.message_bytes_max),
        ];
        let mut bytes = Vec::with_capacity(4 * varint::BYTES_MAX);
        for (id, value) in pairs {
            bytes.extend_from_slice(&Varint::new(id).expect("an id is a varint"));
            bytes.extend_from_slice(&Varint::new(value).unwrap_or(Varint::MAX));
        }
        bytes
    }

    /// The hello in `bytes`, all the bytes of the stream. An unknown id is ignored,
    /// and a value over `usize::MAX` gives `usize::MAX`.
    ///
    /// # Errors
    ///
    /// The fault when the hello is over 256 bytes, ends inside a pair, has an id
    /// at or below the one before it, misses id 0, misses id 1, has a
    /// `message_bytes_max` below 1472, or has a `window_bytes` below its
    /// `message_bytes_max`. When more than one applies, the first in this list,
    /// except that the pairs are read in order: the first pair that is cut or has a
    /// low id gives its fault.
    pub fn decode(mut bytes: &[u8]) -> Result<Self, Fault> {
        if bytes.len() > BYTES_MAX {
            return Err(Fault(format!("a hello over {BYTES_MAX} bytes")));
        }
        let (mut window, mut message, mut last) = (None, None, None);
        while !bytes.is_empty() {
            let (Some(id), Some(value)) =
                (varint::take(&mut bytes), varint::take(&mut bytes))
            else {
                return Err(Fault("a hello that ends inside a pair".to_owned()));
            };
            if let Some(last) = last.filter(|&last| id <= last) {
                return Err(Fault(format!("a hello with id {id} after id {last}")));
            }
            last = Some(id);
            let value = usize::try_from(value).unwrap_or(usize::MAX);
            match id {
                WINDOW => window = Some(value),
                MESSAGE => message = Some(value),
                _ => {}
            }
        }
        let window_bytes =
            window.ok_or_else(|| Fault("a hello with no window_bytes".to_owned()))?;
        let message_bytes_max = message
            .ok_or_else(|| Fault("a hello with no message_bytes_max".to_owned()))?;
        if message_bytes_max < MESSAGE_BYTES_MIN {
            return Err(Fault(format!(
                "a hello with a message_bytes_max of {message_bytes_max}, below \
                 {MESSAGE_BYTES_MIN}"
            )));
        }
        if window_bytes < message_bytes_max {
            return Err(Fault(format!(
                "a hello with window_bytes {window_bytes} below message_bytes_max \
                 {message_bytes_max}"
            )));
        }
        Ok(Self {
            window_bytes,
            message_bytes_max,
        })
    }
}

/// The peer's hello, as it arrives.
#[derive(Debug)]
pub(super) enum Peer {
    Waiting {
        /// The peer's first one-way stream, once it opened.
        stream: Option<StreamId>,
        /// The bytes of the hello that arrived.
        bytes: Vec<u8>,
    },
    Arrived(Hello),
}

impl Peer {
    /// A hello that has not started to arrive.
    pub(super) fn new() -> Self {
        Self::Waiting {
            stream: None,
            bytes: Vec::new(),
        }
    }

    /// The hello, once it arrived.
    pub(super) fn hello(&self) -> Option<Hello> {
        match *self {
            Self::Waiting { .. } => None,
            Self::Arrived(hello) => Some(hello),
        }
    }

    /// Takes `event` of `inner` toward the hello: accepts the peer's first one-way
    /// stream, keeps its credit from coming back to the peer when it ends, and reads
    /// the stream. Gives the hello once its stream ended. Ignores every other event.
    ///
    /// # Errors
    ///
    /// [`Fault`] when the hello is over [`BYTES_MAX`], does not decode, or resets.
    ///
    /// # Panics
    ///
    /// After the hello arrived.
    #[expect(
        clippy::unwrap_in_result,
        reason = "an `Opened` event has a stream to accept, and this side reads the \
                  hello stream only until it ends"
    )]
    pub(super) fn read(
        &mut self,
        inner: &mut noq_proto::Connection,
        event: &StreamEvent,
    ) -> Result<Option<Hello>, Fault> {
        let Self::Waiting { stream, bytes } = self else {
            panic!("invariant: the hello is read until it arrives");
        };
        let id = match (*stream, event) {
            (None, &StreamEvent::Opened { dir: Dir::Uni }) => {
                let id = inner.streams().accept(Dir::Uni);
                let id = *stream.insert(id.expect("invariant: a stream opened"));
                // Set before the hello stream ends, so noq-proto gives the peer no
                // credit for it.
                let limit = inner.max_concurrent_streams(Dir::Uni) - 1;
                let limit = VarInt::from_u64(limit);
                let limit = limit.expect("invariant: a smaller limit is a varint");
                inner.set_max_concurrent_streams(Dir::Uni, limit);
                id
            }
            // Another stream's `Readable` finds no hello bytes that the hello
            // stream's own `Readable` would not.
            (Some(id), &StreamEvent::Readable { .. }) => id,
            _ => return Ok(None),
        };
        let mut recv = inner.recv_stream(id);
        let mut chunks = recv
            .read(true)
            .expect("invariant: the hello stream is open");
        while bytes.len() <= BYTES_MAX {
            match chunks.next(BYTES_MAX) {
                Ok(Some(chunk)) => bytes.extend_from_slice(&chunk.bytes),
                Ok(None) => break,
                Err(ReadError::Blocked) => return Ok(None),
                Err(ReadError::Reset(_)) => {
                    return Err(Fault("a hello that reset".to_owned()));
                }
            }
        }
        let hello = Hello::decode(bytes)?;
        *self = Self::Arrived(hello);
        Ok(Some(hello))
    }
}

#[cfg(test)]
mod tests {
    use noq_proto::coding::Encodable;
    use proptest::prelude::*;

    use super::*;

    /// The bytes of `pairs`, each varint in the fewest bytes.
    fn encode(pairs: &[(u64, u64)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for &(id, value) in pairs {
            VarInt::from_u64(id).expect("a varint").encode(&mut bytes);
            VarInt::from_u64(value)
                .expect("a varint")
                .encode(&mut bytes);
        }
        bytes
    }

    fn fault(reason: &str) -> Result<Hello, Fault> {
        Err(Fault(reason.to_owned()))
    }

    /// A varint of `value` in 8 bytes.
    fn long(value: u64) -> [u8; 8] {
        (value | 0xc0 << 56).to_be_bytes()
    }

    proptest! {
        #[test]
        fn decode_gives_what_encode_sent(
            a in MESSAGE_BYTES_MIN as u64..=VarInt::MAX.into_inner(),
            b in MESSAGE_BYTES_MIN as u64..=VarInt::MAX.into_inner(),
        ) {
            let hello = Hello {
                window_bytes: usize::try_from(a.max(b)).expect("64 bits"),
                message_bytes_max: usize::try_from(a.min(b)).expect("64 bits"),
            };
            prop_assert_eq!(Hello::decode(&hello.encode()), Ok(hello));
        }

        #[test]
        fn decode_ignores_unknown_ids(
            ids in prop::collection::btree_set(2..=VarInt::MAX.into_inner(), 0..=10),
            value in 0..=VarInt::MAX.into_inner(),
        ) {
            let mut pairs = vec![(0, 2_000), (1, 1_500)];
            pairs.extend(ids.into_iter().map(|id| (id, value)));
            let hello = Hello {
                window_bytes: 2_000,
                message_bytes_max: 1_500,
            };
            prop_assert_eq!(Hello::decode(&encode(&pairs)), Ok(hello));
        }

        #[test]
        fn decode_refuses_each_cut_of_a_hello(
            window in 0..=VarInt::MAX.into_inner(),
            message in 0..=VarInt::MAX.into_inner(),
            unknown in 0..=VarInt::MAX.into_inner(),
        ) {
            let pairs = [(0, window), (1, message), (2, unknown)];
            let bytes = encode(&pairs);
            let first = encode(&pairs[..1]).len();
            let second = encode(&pairs[..2]).len();
            for cut in 0..second {
                let expected = if cut == 0 {
                    fault("a hello with no window_bytes")
                } else if cut == first {
                    fault("a hello with no message_bytes_max")
                } else {
                    fault("a hello that ends inside a pair")
                };
                prop_assert_eq!(Hello::decode(&bytes[..cut]), expected);
            }
            for cut in second + 1..bytes.len() {
                prop_assert_eq!(
                    Hello::decode(&bytes[..cut]),
                    fault("a hello that ends inside a pair")
                );
            }
        }
    }

    #[test]
    fn encode_takes_the_fewest_bytes() {
        let hello = Hello {
            window_bytes: 1 << 20,
            message_bytes_max: 1 << 16,
        };
        assert_eq!(
            hello.encode(),
            [0x00, 0x80, 0x10, 0x00, 0x00, 0x01, 0x80, 0x01, 0x00, 0x00]
        );
    }

    #[test]
    fn encode_sends_a_value_over_a_varint_as_the_largest() {
        let hello = Hello {
            window_bytes: usize::MAX,
            message_bytes_max: 1 << 62,
        };
        let mut expected = vec![0x00];
        expected.extend(long(VarInt::MAX.into_inner()));
        expected.push(0x01);
        expected.extend(long(VarInt::MAX.into_inner()));
        assert_eq!(hello.encode(), expected);
    }

    #[test]
    fn decode_takes_a_value_that_is_not_the_fewest_bytes() {
        let mut bytes = vec![0x40, 0x00];
        bytes.extend(long(1_472));
        bytes.extend([0x01, 0x45, 0xc0]);
        let hello = Hello {
            window_bytes: 1_472,
            message_bytes_max: 1_472,
        };
        assert_eq!(Hello::decode(&bytes), Ok(hello));
    }

    #[test]
    fn decode_takes_a_hello_of_bytes_max() {
        let mut bytes = Vec::new();
        for (id, value) in [(0, 1 << 40), (1, 1 << 30)] {
            bytes.extend(long(id));
            bytes.extend(long(value));
        }
        for id in 2..=29 {
            bytes.extend((id | 0x8000_0000_u32).to_be_bytes());
            bytes.extend(0x8000_0000_u32.to_be_bytes());
        }
        assert_eq!(bytes.len(), BYTES_MAX);
        let hello = Hello {
            window_bytes: 1 << 40,
            message_bytes_max: 1 << 30,
        };
        assert_eq!(Hello::decode(&bytes), Ok(hello));
        bytes.push(0);
        assert_eq!(Hello::decode(&bytes), fault("a hello over 256 bytes"));
    }

    #[test]
    fn decode_reads_each_payload_bit_of_a_varint() {
        let bytes = [0x00, 0xbf, 0xff, 0xff, 0xff, 0x01, 0x7f, 0xff];
        let hello = Hello {
            window_bytes: (1 << 30) - 1,
            message_bytes_max: (1 << 14) - 1,
        };
        assert_eq!(Hello::decode(&bytes), Ok(hello));
    }

    #[test]
    fn decode_gives_the_first_fault_in_the_doc_order() {
        assert_eq!(
            Hello::decode(&encode(&[(0, 1_000), (1, 1_400)])),
            fault("a hello with a message_bytes_max of 1400, below 1472")
        );
        assert_eq!(
            Hello::decode(&[0x00, 0x47, 0xd0, 0x00]),
            fault("a hello that ends inside a pair")
        );
        let mut bytes = encode(&[(0, 4_000), (0, 4_000)]);
        bytes.push(0x01);
        assert_eq!(Hello::decode(&bytes), fault("a hello with id 0 after id 0"));
        assert_eq!(
            Hello::decode(&encode(&[(1, 1_500), (1, 1_500)])),
            fault("a hello with id 1 after id 1")
        );
        assert_eq!(
            Hello::decode(&encode(&[(1, 1_000)])),
            fault("a hello with no window_bytes")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 2_000), (1, 1_500), (5, 0), (3, 0), (2, 0)])),
            fault("a hello with id 3 after id 5")
        );
    }

    #[test]
    fn decode_refuses_an_id_at_or_below_the_one_before() {
        assert_eq!(
            Hello::decode(&encode(&[(0, 2_000), (0, 2_000), (1, 1_500)])),
            fault("a hello with id 0 after id 0")
        );
        assert_eq!(
            Hello::decode(&encode(&[(1, 1_500), (0, 2_000)])),
            fault("a hello with id 0 after id 1")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 2_000), (1, 1_500), (5, 0), (3, 0)])),
            fault("a hello with id 3 after id 5")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 1_472), (1, 1_472), (2, 0), (2, 0)])),
            fault("a hello with id 2 after id 2")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 2_000), (1, 1_500), (1 << 30, 0), (2, 0)])),
            fault("a hello with id 2 after id 1073741824")
        );
    }

    #[test]
    fn decode_refuses_a_hello_with_no_required_id() {
        assert_eq!(Hello::decode(&[]), fault("a hello with no window_bytes"));
        assert_eq!(
            Hello::decode(&encode(&[(1, 1_500), (2, 0)])),
            fault("a hello with no window_bytes")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 2_000), (2, 0)])),
            fault("a hello with no message_bytes_max")
        );
    }

    #[test]
    fn decode_refuses_a_message_bytes_max_below_1472() {
        assert_eq!(
            Hello::decode(&encode(&[(0, 2_000), (1, 0)])),
            fault("a hello with a message_bytes_max of 0, below 1472")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 2_000), (1, 1_471)])),
            fault("a hello with a message_bytes_max of 1471, below 1472")
        );
        let hello = Hello {
            window_bytes: 2_000,
            message_bytes_max: 1_472,
        };
        assert_eq!(Hello::decode(&encode(&[(0, 2_000), (1, 1_472)])), Ok(hello));
    }

    #[test]
    fn decode_refuses_a_window_bytes_below_the_message_bytes_max() {
        assert_eq!(
            Hello::decode(&encode(&[(0, 1_471), (1, 1_472)])),
            fault("a hello with window_bytes 1471 below message_bytes_max 1472")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 1_472), (1, 2_000)])),
            fault("a hello with window_bytes 1472 below message_bytes_max 2000")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 1 << 33), (1, 1 << 34)])),
            fault(
                "a hello with window_bytes 8589934592 below message_bytes_max \
                 17179869184"
            )
        );
        let hello = Hello {
            window_bytes: 1_472,
            message_bytes_max: 1_472,
        };
        assert_eq!(Hello::decode(&encode(&[(0, 1_472), (1, 1_472)])), Ok(hello));
    }
}

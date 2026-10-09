//! The hello: the limits a node sends on its first one-way stream, which its peer
//! obeys. It is (id, value) pairs, both QUIC varints, ids strictly increasing.

use noq_proto::{Dir, ReadError, StreamEvent, StreamId, VarInt};

use super::connection::Fault;
use crate::varint::{self, Varint};
use crate::{MESSAGE_BYTES_MIN, window_min};

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
    /// `message_bytes_max` below 1472, or has a `window_bytes` below twice its
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
        if window_bytes < window_min(message_bytes_max) {
            return Err(Fault(format!(
                "a hello with window_bytes {window_bytes} below twice \
                 message_bytes_max {message_bytes_max}"
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

    /// The bytes of `pairs`, with varint `i` in `lens[i]` bytes when it fits, else
    /// in the fewest bytes. Ids are varints `2k` and values `2k + 1`.
    fn encode_wide(pairs: &[(u64, u64)], lens: &[usize]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let varints = pairs.iter().flat_map(|&(id, value)| [id, value]);
        for (at, varint) in varints.enumerate() {
            match widened(varint, lens.get(at).copied().unwrap_or(1)) {
                Some(wide) => bytes.extend(wide),
                None => VarInt::from_u64(varint)
                    .expect("a varint")
                    .encode(&mut bytes),
            }
        }
        bytes
    }

    /// `varint` in `len` bytes, 2, 4, or 8, when it fits.
    fn widened(varint: u64, len: usize) -> Option<Vec<u8>> {
        match len {
            2 => u16::try_from(varint)
                .ok()
                .filter(|&varint| varint < 1 << 14)
                .map(|varint| (varint | 0x4000).to_be_bytes().to_vec()),
            4 => u32::try_from(varint)
                .ok()
                .filter(|&varint| varint < 1 << 30)
                .map(|varint| (varint | 0x8000_0000).to_be_bytes().to_vec()),
            8 => Some(long(varint).to_vec()),
            _ => None,
        }
    }

    fn fault(reason: &str) -> Result<Hello, Fault> {
        Err(Fault(reason.to_owned()))
    }

    /// A varint of `value` in 8 bytes.
    fn long(value: u64) -> [u8; 8] {
        (value | 0xc0 << 56).to_be_bytes()
    }

    /// What the `decode` doc gives for a hello of `bytes` bytes that holds `pairs`
    /// then `tail`, the start of one more pair when not empty, with the reason of a
    /// fault.
    fn doc_decode(
        pairs: &[(u64, u64)],
        tail: &[u8],
        bytes: usize,
    ) -> Result<Hello, String> {
        if bytes > 256 {
            return Err("a hello over 256 bytes".to_owned());
        }
        let mut after = pairs.iter().zip(pairs.iter().skip(1));
        if let Some(((last, _), (id, _))) =
            after.find(|((last, _), (id, _))| id <= last)
        {
            return Err(format!("a hello with id {id} after id {last}"));
        }
        if !tail.is_empty() {
            return Err("a hello that ends inside a pair".to_owned());
        }
        let value = |id| {
            let pair = pairs.iter().find(|pair| pair.0 == id)?;
            Some(usize::try_from(pair.1).unwrap_or(usize::MAX))
        };
        let Some(window) = value(0) else {
            return Err("a hello with no window_bytes".to_owned());
        };
        let Some(message) = value(1) else {
            return Err("a hello with no message_bytes_max".to_owned());
        };
        if message < 1_472 {
            return Err(format!(
                "a hello with a message_bytes_max of {message}, below 1472"
            ));
        }
        if window / 2 < message {
            return Err(format!(
                "a hello with window_bytes {window} below twice message_bytes_max \
                 {message}"
            ));
        }
        Ok(Hello {
            window_bytes: window,
            message_bytes_max: message,
        })
    }

    /// A varint length over 1 byte.
    fn wide() -> impl Strategy<Value = usize> {
        prop_oneof![Just(2), Just(4), Just(8)]
    }

    /// A value near the limits, or a varint of a bit length drawn evenly from 0 to
    /// 62.
    fn value() -> impl Strategy<Value = u64> {
        prop_oneof![
            0_u64..64,
            0_u64..3_000,
            Just(1_471),
            Just(1_472),
            Just(VarInt::MAX.into_inner()),
            (any::<u64>(), 0_u32..=62).prop_map(|(bits, len)| match len {
                0 => 0,
                _ => bits >> (64 - len) | 1 << (len - 1),
            }),
        ]
    }

    /// An unknown id of each varint length.
    fn unknown() -> impl Strategy<Value = u64> {
        prop_oneof![
            4 => 2_u64..64,
            1 => 64_u64..1 << 14,
            1 => Just(1 << 14),
            1 => (1_u64 << 14) + 1..1 << 30,
            1 => Just(1 << 30),
            // Low bits that match id 0 or 1.
            1 => Just(1 << 61),
            1 => Just((1 << 61) + 1),
            1 => Just(VarInt::MAX.into_inner()),
        ]
    }

    /// Pairs in one of five shapes: up to 31 of any ids in any order; up to 24 ids
    /// that rise, with up to two pairs of any ids after them; ids 0 and 1, then up to
    /// 24 unknown ids that rise; ids 0 and 1, then pairs of 8-byte ids to near 256
    /// bytes; or 60 to 111 pairs of 1-byte and 2-byte ids that rise, from about 124
    /// bytes to past 256, with id 0 or 1 at times left out and up to one pair of any
    /// id after them.
    fn pairs() -> impl Strategy<Value = Vec<(u64, u64)>> {
        let id = || prop_oneof![0_u64..4, unknown()];
        let any = prop::collection::vec((id(), value()), 0..32);
        let rising = (
            prop::collection::btree_set(id(), 0..=24),
            prop::collection::vec(value(), 24),
            prop::collection::vec((id(), value()), 0..=2),
        )
            .prop_map(|(ids, values, more)| {
                let mut pairs: Vec<_> = ids.into_iter().zip(values).collect();
                pairs.extend(more);
                pairs
            });
        let limits = (
            value(),
            value(),
            prop::collection::btree_set(unknown(), 0..=24),
            prop::collection::vec(value(), 24),
        )
            .prop_map(|(window, message, ids, values)| {
                let mut pairs = vec![(0, window), (1, message)];
                pairs.extend(ids.into_iter().zip(values));
                pairs
            });
        let long = (value(), value(), prop::collection::vec(value(), 12..=22))
            .prop_map(|(window, message, values)| {
                let mut pairs = vec![(0, window), (1, message)];
                let max = VarInt::MAX.into_inner();
                let ids = max - u64::try_from(values.len()).expect("22 at most")..max;
                pairs.extend(ids.zip(values));
                pairs
            });
        let many = (
            value(),
            value(),
            prop_oneof![Just(None), Just(Some(0)), Just(Some(1))],
            60_u64..=110,
            prop::collection::vec((id(), value()), 0..=1),
        )
            .prop_map(|(window, message, missing, last, more)| {
                let value = |id| match id {
                    0 => window,
                    1 => message,
                    _ => id % 64,
                };
                let ids = (0..=last).filter(|&id| Some(id) != missing);
                let mut pairs: Vec<_> = ids.map(|id| (id, value(id))).collect();
                pairs.extend(more);
                pairs
            });
        prop_oneof![any, rising, limits, long, many]
    }

    proptest! {
        #[test]
        fn decode_gives_what_encode_sent(
            message in MESSAGE_BYTES_MIN as u64..=VarInt::MAX.into_inner() / 2,
            window in 0..=VarInt::MAX.into_inner(),
        ) {
            let window = window.max(2 * message);
            let hello = Hello {
                window_bytes: usize::try_from(window).expect("64 bits"),
                message_bytes_max: usize::try_from(message).expect("64 bits"),
            };
            prop_assert_eq!(Hello::decode(&hello.encode()), Ok(hello));
        }

        #[test]
        fn decode_refuses_only_a_window_bytes_below_twice_the_message_bytes_max(
            message in value().prop_map(|value| value.max(MESSAGE_BYTES_MIN as u64)),
            offset in -64_i64..=64,
        ) {
            let window = (2 * message)
                .saturating_add_signed(offset)
                .min(VarInt::MAX.into_inner());
            let expected = if window < 2 * message {
                fault(&format!(
                    "a hello with window_bytes {window} below twice message_bytes_max \
                     {message}"
                ))
            } else {
                Ok(Hello {
                    window_bytes: usize::try_from(window).expect("64 bits"),
                    message_bytes_max: usize::try_from(message).expect("64 bits"),
                })
            };
            let bytes = encode(&[(0, window), (1, message)]);
            prop_assert_eq!(Hello::decode(&bytes), expected);
        }

        #[test]
        fn decode_ignores_unknown_ids(
            ids in prop::collection::btree_set(2..=VarInt::MAX.into_inner(), 0..=10),
            value in 0..=VarInt::MAX.into_inner(),
        ) {
            let mut pairs = vec![(0, 3_000), (1, 1_500)];
            pairs.extend(ids.into_iter().map(|id| (id, value)));
            let hello = Hello {
                window_bytes: 3_000,
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

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4_096))]

        #[test]
        fn decode_gives_what_the_doc_gives(
            pairs in pairs(),
            // One more pair, cut inside its id, after its id, or inside its value,
            // with each varint in 1, 2, 4, or 8 bytes when it fits.
            tail in prop_oneof![
                1 => Just(vec![]),
                3 => (
                    prop_oneof![0_u64..4, unknown()],
                    value(),
                    prop::array::uniform2(prop_oneof![Just(1), wide()]),
                    any::<prop::sample::Index>(),
                )
                    .prop_map(|(id, value, lens, at)| {
                        let mut pair = encode_wide(&[(id, value)], &lens);
                        pair.truncate(1 + at.index(pair.len() - 1));
                        pair
                    }),
            ],
            // At times, some varints in more bytes than they need.
            lens in prop_oneof![
                3 => Just(vec![]),
                1 => prop::collection::vec(
                    prop_oneof![9 => Just(1), 1 => wide()],
                    224,
                ),
            ],
        ) {
            let mut bytes = encode_wide(&pairs, &lens);
            bytes.extend(&tail);
            let decoded = Hello::decode(&bytes).map_err(|fault| fault.0);
            prop_assert_eq!(decoded, doc_decode(&pairs, &tail, bytes.len()));
        }
    }

    #[test]
    fn decode_reads_every_pair() {
        let mut pairs = vec![(0, 3_000), (1, 1_500)];
        pairs.extend((2..=63).map(|id| (id, 0)));
        pairs.push((63, 0));
        assert_eq!(
            Hello::decode(&encode(&pairs)),
            fault("a hello with id 63 after id 63")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 3_000), (1, 1_500), (1 << 40, 0), (2, 0)])),
            fault("a hello with id 2 after id 1099511627776")
        );
    }

    #[test]
    fn decode_reads_the_most_pairs_that_fit() {
        let mut pairs = vec![(0, 3_000), (1, 1_500)];
        pairs.extend((2..=104).map(|id| (id, 0)));
        pairs.push((0, 64));
        assert_eq!(encode(&pairs).len(), BYTES_MAX);
        assert_eq!(
            Hello::decode(&encode(&pairs)),
            fault("a hello with id 0 after id 104")
        );
        let mut pairs: Vec<(u64, u64)> = (0..=105).map(|id| (id, 0)).collect();
        pairs.push((2, 0));
        let bytes = encode(&pairs);
        assert_eq!((pairs.len(), bytes.len()), (107, BYTES_MAX));
        assert_eq!(
            Hello::decode(&bytes),
            fault("a hello with id 2 after id 105")
        );
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
    fn decode_reads_each_varint_in_each_length() {
        let cases = [
            (
                [(0, 63), (1, 1_472), (2, 7)],
                fault(
                    "a hello with window_bytes 63 below twice message_bytes_max 1472",
                ),
            ),
            (
                [(0, 2_000), (1, 7), (2, 7)],
                fault("a hello with a message_bytes_max of 7, below 1472"),
            ),
            (
                [(0, 16_000), (1, 3_000), (2, 16_382)],
                Ok(Hello {
                    window_bytes: 16_000,
                    message_bytes_max: 3_000,
                }),
            ),
            (
                [(0, (1 << 30) - 1), (1, 1 << 14), (2, 7)],
                Ok(Hello {
                    window_bytes: (1 << 30) - 1,
                    message_bytes_max: 1 << 14,
                }),
            ),
            (
                [(0, 3_000), (1, 1_500), (63, 0)],
                Ok(Hello {
                    window_bytes: 3_000,
                    message_bytes_max: 1_500,
                }),
            ),
        ];
        for (pairs, expected) in cases {
            for at in 0..1 << 12 {
                let lens: Vec<usize> =
                    (0..6).map(|i| [1, 2, 4, 8][at >> (2 * i) & 3]).collect();
                let bytes = encode_wide(&pairs, &lens);
                assert_eq!(Hello::decode(&bytes), expected, "{pairs:?} {lens:?}");
            }
        }
    }

    #[test]
    fn decode_gives_the_largest_varint_as_its_value() {
        let max = VarInt::MAX.into_inner();
        let hello = Hello {
            window_bytes: usize::try_from(max).expect("64 bits"),
            message_bytes_max: 1_472,
        };
        assert_eq!(Hello::decode(&encode(&[(0, max), (1, 1_472)])), Ok(hello));
    }

    #[test]
    fn decode_refuses_a_cut_inside_a_wide_id() {
        let whole = encode(&[(0, 3_000), (1, 1_500)]);
        for cut in [
            &[0x80][..],
            &[0x80, 0x10, 0x00],
            &[0xc0],
            &[0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        ] {
            let mut bytes = whole.clone();
            bytes.extend_from_slice(cut);
            assert_eq!(
                Hello::decode(&bytes),
                fault("a hello that ends inside a pair"),
                "{cut:?}"
            );
        }
    }

    #[test]
    fn decode_takes_varints_that_are_not_the_fewest_bytes() {
        let mut bytes = vec![0x40, 0x00];
        bytes.extend(long(3_000));
        bytes.extend([0x01, 0x45, 0xc0]);
        let hello = Hello {
            window_bytes: 3_000,
            message_bytes_max: 1_472,
        };
        assert_eq!(Hello::decode(&bytes), Ok(hello));
        // Id 0 and its value in 4 bytes, id 1 in 2 bytes, then unknown id 2^20 with
        // value 5 in 2 bytes.
        let bytes = [
            0x80, 0x00, 0x00, 0x00, 0x80, 0x00, 0x0b, 0xb8, 0x40, 0x01, 0x45, 0xdc,
            0x80, 0x10, 0x00, 0x00, 0x40, 0x05,
        ];
        let hello = Hello {
            window_bytes: 3_000,
            message_bytes_max: 1_500,
        };
        assert_eq!(Hello::decode(&bytes), Ok(hello));
        let mut bytes = vec![0x00, 0x40, 0x05, 0x01];
        bytes.extend(long(7));
        assert_eq!(
            Hello::decode(&bytes),
            fault("a hello with a message_bytes_max of 7, below 1472")
        );
        let bytes = [0x00, 0x40, 0x05, 0x01, 0x45, 0xc0];
        assert_eq!(
            Hello::decode(&bytes),
            fault("a hello with window_bytes 5 below twice message_bytes_max 1472")
        );
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
        // Id 30 alone, then with its value: a cut pair, then a whole one.
        for byte in [0x1e, 0x00] {
            bytes.push(byte);
            assert_eq!(Hello::decode(&bytes), fault("a hello over 256 bytes"));
        }
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
            Hello::decode(&encode(&[(0, 3_000), (1, 1_500), (5, 0), (3, 0), (2, 0)])),
            fault("a hello with id 3 after id 5")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 2_000), (2, 0), (1, 1_500)])),
            fault("a hello with id 1 after id 2")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 2_000), (1, 1_000), (2, 0), (2, 0)])),
            fault("a hello with id 2 after id 2")
        );
        let mut bytes = encode(&[(0, 2_000), (2, 0)]);
        bytes.push(0x03);
        assert_eq!(
            Hello::decode(&bytes),
            fault("a hello that ends inside a pair")
        );
    }

    #[test]
    fn decode_refuses_an_id_at_or_below_the_one_before() {
        assert_eq!(
            Hello::decode(&encode(&[(0, 3_000), (0, 3_000), (1, 1_500)])),
            fault("a hello with id 0 after id 0")
        );
        assert_eq!(
            Hello::decode(&encode(&[(1, 1_500), (0, 2_000)])),
            fault("a hello with id 0 after id 1")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 3_000), (1, 1_500), (5, 0), (3, 0)])),
            fault("a hello with id 3 after id 5")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 1_472), (1, 1_472), (2, 0), (2, 0)])),
            fault("a hello with id 2 after id 2")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 3_000), (1, 1_500), (1 << 30, 0), (2, 0)])),
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
            window_bytes: 3_000,
            message_bytes_max: 1_472,
        };
        assert_eq!(Hello::decode(&encode(&[(0, 3_000), (1, 1_472)])), Ok(hello));
    }

    #[test]
    fn decode_refuses_a_window_bytes_below_twice_the_message_bytes_max() {
        assert_eq!(
            Hello::decode(&encode(&[(0, 2_943), (1, 1_472)])),
            fault("a hello with window_bytes 2943 below twice message_bytes_max 1472")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, 2_000), (1, 2_000)])),
            fault("a hello with window_bytes 2000 below twice message_bytes_max 2000")
        );
        assert_eq!(
            Hello::decode(&encode(&[(0, (1 << 35) - 1), (1, 1 << 34)])),
            fault(
                "a hello with window_bytes 34359738367 below twice message_bytes_max \
                 17179869184"
            )
        );
        let hello = Hello {
            window_bytes: 2_944,
            message_bytes_max: 1_472,
        };
        assert_eq!(Hello::decode(&encode(&[(0, 2_944), (1, 1_472)])), Ok(hello));
    }
}

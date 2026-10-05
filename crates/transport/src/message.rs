//! Messages on a stream: each is a QUIC variable-length integer that counts its
//! bytes, then the bytes.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use std::mem;
use std::ops::Deref;

use block::{Block, Pool, Unique};

use crate::Error;

/// The length prefix of one message, in the fewest bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Prefix {
    bytes: [u8; 8],
    len: usize,
}

impl Prefix {
    /// The prefix of a message of `len` bytes.
    ///
    /// # Panics
    ///
    /// When `len` is 2^62 or more, which no varint holds.
    pub(crate) fn new(len: usize) -> Self {
        let value = u64::try_from(len)
            .ok()
            .filter(|&value| value < 1 << 62)
            .unwrap_or_else(|| {
                panic!("a message of {len} bytes is over the varint limit")
            });
        let (tag, size, rotate) = match value {
            0..64 => (0x00, 1, 7),
            64..16_384 => (0x40, 2, 6),
            16_384..1_073_741_824 => (0x80, 4, 4),
            _ => (0xc0, 8, 0),
        };
        let mut bytes = value.to_be_bytes();
        bytes.rotate_left(rotate);
        let [first, ..] = &mut bytes;
        *first |= tag;
        Self { bytes, len: size }
    }
}

impl Deref for Prefix {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes
            .get(..self.len)
            .expect("invariant: a prefix is at most 8 bytes")
    }
}

/// Splits a stream's bytes into whole messages, each in one block from a pool.
#[derive(Debug)]
pub(crate) struct Reader {
    bytes_max: usize,
    state: State,
}

#[derive(Debug)]
enum State {
    /// A length prefix, with `have` of its `len` bytes. `len` is 1 until the first
    /// byte gives it. The bytes after `have` are zero.
    Prefix {
        bytes: [u8; 8],
        have: usize,
        len: usize,
    },
    /// A message of this many bytes, with no block yet.
    Sized(usize),
    /// A message's block, with `have` of its bytes.
    Body { block: Unique, have: usize },
}

const START: State = State::Prefix {
    bytes: [0; 8],
    have: 0,
    len: 1,
};

impl Reader {
    /// A reader that refuses a message over `bytes_max` bytes.
    pub(crate) fn new(bytes_max: usize) -> Self {
        Self {
            bytes_max,
            state: START,
        }
    }

    /// Reads the next whole message into a block from `pool`. `source(max)` gives
    /// the stream's next bytes, at most `max` of them, or `None` or no bytes when it
    /// has none now. The reader never asks for a byte past the current message, so
    /// the bytes of later messages stay with the source.
    ///
    /// Returns `None` when the source runs out before the message is whole; the next
    /// call goes on where this one stopped.
    ///
    /// # Errors
    ///
    /// - [`Error::TooLarge`] when the message is longer than `bytes_max`. The stream
    ///   cannot go on.
    /// - [`Error::Pool`] when `pool` has no room for the message. Its bytes stay with
    ///   the source; call again after a block frees.
    /// - The source's error.
    ///
    /// # Panics
    ///
    /// When the source gives more bytes than the reader asked for.
    pub(crate) fn read<B: AsRef<[u8]>>(
        &mut self,
        pool: &Pool,
        mut source: impl FnMut(usize) -> Result<Option<B>, Error>,
    ) -> Result<Option<Block>, Error> {
        loop {
            match &mut self.state {
                State::Prefix { bytes, have, len } => {
                    let prefix = bytes.get_mut(..*len).expect("invariant: len <= 8");
                    if !pull(&mut source, prefix, have)? {
                        return Ok(None);
                    }
                    let [first, ..] = *bytes;
                    let (size, shift, mask) = match first >> 6 {
                        0 => (1, 56, 0x3f),
                        1 => (2, 48, 0x3fff),
                        2 => (4, 32, 0x3fff_ffff),
                        _ => (8, 0, 0x3fff_ffff_ffff_ffff),
                    };
                    *len = size;
                    if *have == size {
                        let value =
                            u64::from_be_bytes(*bytes).wrapping_shr(shift) & mask;
                        // On a 32-bit target this saturates, and is still over
                        // `bytes_max`.
                        self.state =
                            State::Sized(usize::try_from(value).unwrap_or(usize::MAX));
                    }
                }
                State::Sized(bytes) => {
                    let bytes = *bytes;
                    if bytes > self.bytes_max {
                        return Err(Error::TooLarge {
                            bytes,
                            bytes_max: self.bytes_max,
                        });
                    }
                    let block = pool.alloc(bytes).map_err(Error::Pool)?;
                    self.state = State::Body { block, have: 0 };
                }
                State::Body { block, have } => {
                    if *have < block.len() && !pull(&mut source, block, have)? {
                        return Ok(None);
                    }
                    if *have == block.len() {
                        let State::Body { block, .. } =
                            mem::replace(&mut self.state, START)
                        else {
                            unreachable!("this arm matched a body");
                        };
                        return Ok(Some(block.freeze()));
                    }
                }
            }
        }
    }

    /// Checks that the stream ended between two messages.
    ///
    /// # Errors
    ///
    /// [`Error::Broken`] when it ended inside a message.
    pub(crate) fn finish(&self) -> Result<(), Error> {
        match self.state {
            State::Prefix { have: 0, .. } => Ok(()),
            _ => Err(Error::Broken {
                reason: "the stream ended inside a message".to_owned(),
            }),
        }
    }
}

/// Copies the source's next bytes into `buffer` after its first `have`. Returns
/// `false` when the source has none now.
fn pull<B: AsRef<[u8]>>(
    source: &mut impl FnMut(usize) -> Result<Option<B>, Error>,
    buffer: &mut [u8],
    have: &mut usize,
) -> Result<bool, Error> {
    let rest = buffer.get_mut(*have..).expect("invariant: have <= len");
    let Some(chunk) = source(rest.len())? else {
        return Ok(false);
    };
    let chunk = chunk.as_ref();
    rest.get_mut(..chunk.len())
        .expect("the source gives at most the bytes asked for")
        .copy_from_slice(chunk);
    *have = have.saturating_add(chunk.len());
    Ok(!chunk.is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use block::{Config, Heap};
    use proptest::prelude::*;

    use super::*;

    fn pool(budget: usize) -> Pool {
        let config = Config { budget };
        let memory = Heap::new(config.reservation());
        Pool::new(config, memory)
    }

    /// The stream bytes of `messages`, each with its prefix.
    fn encode(messages: &[Vec<u8>]) -> Vec<u8> {
        let mut stream = Vec::new();
        for message in messages {
            stream.extend_from_slice(&Prefix::new(message.len()));
            stream.extend_from_slice(message);
        }
        stream
    }

    /// A source over `bytes` that gives at most `split` bytes per call, and records
    /// the most bytes it gave.
    struct Source {
        bytes: VecDeque<u8>,
        split: usize,
        given: usize,
    }

    impl Source {
        fn new(bytes: Vec<u8>, split: usize) -> Self {
            Self {
                bytes: bytes.into(),
                split,
                given: 0,
            }
        }

        fn take(&mut self, max: usize) -> Option<Vec<u8>> {
            let n = max.min(self.split).min(self.bytes.len());
            if n == 0 {
                return None;
            }
            self.given = self.given.saturating_add(n);
            Some(self.bytes.drain(..n).collect())
        }
    }

    /// Every message `reader` reads from `source` until it has none.
    fn read_all(
        reader: &mut Reader,
        pool: &Pool,
        source: &mut Source,
    ) -> Result<Vec<Vec<u8>>, Error> {
        let mut messages = Vec::new();
        while let Some(block) = reader.read(pool, |max| Ok(source.take(max)))? {
            messages.push(block.to_vec());
        }
        Ok(messages)
    }

    mod prefix {
        use super::*;

        #[test]
        fn takes_the_fewest_bytes() {
            let lens = [
                (0, 1),
                (63, 1),
                (64, 2),
                (16_383, 2),
                (16_384, 4),
                ((1 << 30) - 1, 4),
                (1 << 30, 8),
                ((1 << 62) - 1, 8),
            ];
            for (len, bytes) in lens {
                assert_eq!(Prefix::new(len).len(), bytes, "{len}");
            }
        }

        #[test]
        fn matches_rfc_9000() {
            // RFC 9000 appendix A.1.
            assert_eq!(
                &*Prefix::new(151_288_809_941_952_652),
                [0xc2, 0x19, 0x7c, 0x5e, 0xff, 0x14, 0xe8, 0x8c]
            );
            assert_eq!(&*Prefix::new(494_878_333), [0x9d, 0x7f, 0x3e, 0x7d]);
            assert_eq!(&*Prefix::new(15_293), [0x7b, 0xbd]);
            assert_eq!(&*Prefix::new(37), [0x25]);
        }

        #[test]
        #[should_panic(expected = "a message of 4611686018427387904 bytes")]
        fn panics_at_2_to_the_62() {
            let _ = Prefix::new(1 << 62);
        }
    }

    mod reader {
        use super::*;

        proptest! {
            #[test]
            fn gives_whole_messages_in_order(
                messages in prop::collection::vec(
                    prop::collection::vec(any::<u8>(), 0..=300),
                    0..20,
                ),
                split in 1_usize..400,
            ) {
                let pool = pool(1 << 16);
                let mut source = Source::new(encode(&messages), split);
                let mut reader = Reader::new(300);
                let read = read_all(&mut reader, &pool, &mut source);
                prop_assert_eq!(read, Ok(messages));
                prop_assert_eq!(reader.finish(), Ok(()));
            }
        }

        #[test]
        fn gives_messages_at_each_prefix_size() {
            let messages: Vec<_> = [0, 63, 64, 16_383, 16_384, 70_000]
                .into_iter()
                .map(|len| (0..=250).cycle().take(len).collect())
                .collect();
            let pool = pool(1 << 20);
            let mut source = Source::new(encode(&messages), 1_000);
            let mut reader = Reader::new(70_000);
            assert_eq!(read_all(&mut reader, &pool, &mut source), Ok(messages));
        }

        #[test]
        fn reads_a_prefix_that_is_not_the_fewest_bytes() {
            let pool = pool(1 << 16);
            let mut source = Source::new(vec![0x40, 0x03, 1, 2, 3], 1);
            let mut reader = Reader::new(16);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Ok(vec![vec![1, 2, 3]])
            );
        }

        #[test]
        fn never_asks_past_the_message() {
            let pool = pool(1 << 16);
            let mut source = Source::new(encode(&[vec![7; 10], vec![8; 10]]), 64);
            let mut reader = Reader::new(16);
            let first = reader.read(&pool, |max| Ok(source.take(max)));
            assert_eq!(
                first.map(|block| block.map(|b| b.to_vec())),
                Ok(Some(vec![7; 10]))
            );
            assert_eq!(source.given, 11);
        }

        #[test]
        fn when_message_is_too_large_it_fails() {
            let pool = pool(1 << 16);
            let mut source = Source::new(encode(&[vec![0; 17]]), 64);
            let mut reader = Reader::new(16);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Err(Error::TooLarge {
                    bytes: 17,
                    bytes_max: 16
                })
            );
            assert_eq!(source.given, 1);
        }

        #[test]
        fn when_prefix_is_the_largest_it_fails() {
            let pool = pool(1 << 16);
            let mut source = Source::new(vec![0xff; 8], 64);
            let mut reader = Reader::new(16);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Err(Error::TooLarge {
                    bytes: (1 << 62) - 1,
                    bytes_max: 16
                })
            );
        }

        #[test]
        fn when_pool_is_full_it_fails_then_goes_on() {
            // A 100-byte block takes 192 bytes of the budget.
            let pool = pool(300);
            let held = pool.alloc(100).expect("room");
            let mut source = Source::new(encode(&[vec![9; 100]]), 64);
            let mut reader = Reader::new(1_000);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Err(Error::Pool(block::Error::Exhausted {
                    requested: 100,
                    available: 108
                }))
            );
            assert_eq!(source.given, 2);
            drop(held);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Ok(vec![vec![9; 100]])
            );
        }

        #[test]
        fn when_source_fails_it_gives_the_error() {
            let pool = pool(1 << 16);
            let mut reader = Reader::new(16);
            let read = reader
                .read::<Vec<u8>>(&pool, |_| {
                    Err(Error::Reset {
                        code: crate::Code(16),
                    })
                })
                .map(|block| block.map(|block| block.to_vec()));
            assert_eq!(
                read,
                Err(Error::Reset {
                    code: crate::Code(16)
                })
            );
        }

        #[test]
        fn when_stream_ends_inside_a_message_finish_fails() {
            let pool = pool(1 << 16);
            let mut source = Source::new(vec![0x05, 1, 2], 64);
            let mut reader = Reader::new(16);
            assert_eq!(read_all(&mut reader, &pool, &mut source), Ok(vec![]));
            assert_eq!(
                reader.finish(),
                Err(Error::Broken {
                    reason: "the stream ended inside a message".to_owned()
                })
            );
        }

        #[test]
        fn when_stream_ends_inside_a_prefix_finish_fails() {
            let pool = pool(1 << 16);
            let mut source = Source::new(vec![0x40], 64);
            let mut reader = Reader::new(16);
            assert_eq!(read_all(&mut reader, &pool, &mut source), Ok(vec![]));
            assert_eq!(
                reader.finish(),
                Err(Error::Broken {
                    reason: "the stream ended inside a message".to_owned()
                })
            );
        }
    }
}

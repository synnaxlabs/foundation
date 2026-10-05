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
use std::task::Poll;

use block::{Block, Pool, Unique};

use crate::Error;

/// The length prefix of one message, in the fewest bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Prefix {
    bytes: [u8; 8],
    size: usize,
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
        let (tag, size, shift) = match value {
            0..64 => (0, 1, 56),
            64..16_384 => (0x40 << 56, 2, 48),
            16_384..1_073_741_824 => (0x80 << 56, 4, 32),
            _ => (0xc0 << 56, 8, 0),
        };
        let bytes = (value.wrapping_shl(shift) | tag).to_be_bytes();
        Self { bytes, size }
    }
}

impl Deref for Prefix {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes
            .get(..self.size)
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
    /// A length prefix, with `have` of its `size` bytes. `size` is 1 until the first
    /// byte gives it. The bytes after `have` are zero.
    Prefix {
        bytes: [u8; 8],
        have: usize,
        size: usize,
    },
    /// A message of this many bytes, with no block yet.
    Sized(u64),
    /// A message's block, with `have` of its bytes.
    Body { block: Unique, have: usize },
}

const START: State = State::Prefix {
    bytes: [0; 8],
    have: 0,
    size: 1,
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
    /// the stream's next 1 to `max` bytes, `Pending` when it has none now, or `None`
    /// when the stream has ended. The reader never asks for a byte past the current
    /// message, so later messages stay with the source.
    ///
    /// Returns the message, `Pending` when the source has no more bytes now, or
    /// `None` when the stream ended between two messages. After `Pending`, the next
    /// call goes on where this one stopped.
    ///
    /// # Errors
    ///
    /// - [`Error::Broken`] when the peer breaks the framing: a message over
    ///   `bytes_max`, or a stream that ends inside a message. The stream cannot go on.
    /// - [`Error::Pool`] when `pool` has no room for the message now. Its bytes stay
    ///   with the source; call again when the pool has room.
    /// - The source's error.
    ///
    /// # Panics
    ///
    /// When the source gives no bytes or more than `max`, or when `pool` cannot hold
    /// a message of `bytes_max` bytes.
    pub(crate) fn read<B: AsRef<[u8]>>(
        &mut self,
        pool: &Pool,
        mut source: impl FnMut(usize) -> Result<Poll<Option<B>>, Error>,
    ) -> Result<Poll<Option<Block>>, Error> {
        loop {
            match &mut self.state {
                State::Prefix { bytes, have, size } => {
                    let prefix = bytes.get_mut(..*size).expect("invariant: size <= 8");
                    match pull(&mut source, prefix, have)? {
                        Poll::Pending => return Ok(Poll::Pending),
                        Poll::Ready(false) if *have == 0 => {
                            return Ok(Poll::Ready(None));
                        }
                        Poll::Ready(false) => return Err(ended()),
                        Poll::Ready(true) => {}
                    }
                    let [first, ..] = *bytes;
                    let (full, shift, mask) = match first >> 6 {
                        0 => (1, 56, 0x3f),
                        1 => (2, 48, 0x3fff),
                        2 => (4, 32, 0x3fff_ffff),
                        _ => (8, 0, 0x3fff_ffff_ffff_ffff),
                    };
                    *size = full;
                    if *have == full {
                        let value =
                            u64::from_be_bytes(*bytes).wrapping_shr(shift) & mask;
                        self.state = State::Sized(value);
                    }
                }
                State::Sized(value) => {
                    let block = alloc(pool, *value, self.bytes_max)?;
                    self.state = State::Body { block, have: 0 };
                }
                State::Body { block, have } => {
                    if *have < block.len() {
                        match pull(&mut source, block, have)? {
                            Poll::Pending => return Ok(Poll::Pending),
                            Poll::Ready(false) => return Err(ended()),
                            Poll::Ready(true) => {}
                        }
                    }
                    if *have == block.len() {
                        let State::Body { block, .. } =
                            mem::replace(&mut self.state, START)
                        else {
                            unreachable!("this arm matched a body");
                        };
                        return Ok(Poll::Ready(Some(block.freeze())));
                    }
                }
            }
        }
    }
}

/// A block for a message of `value` bytes.
///
/// # Errors
///
/// [`Error::Broken`] when `value` is over `bytes_max`, and [`Error::Pool`] when the
/// pool has no room now.
///
/// # Panics
///
/// When the pool cannot hold a message of `bytes_max` bytes.
fn alloc(pool: &Pool, value: u64, bytes_max: usize) -> Result<Unique, Error> {
    let Some(len) = usize::try_from(value).ok().filter(|&len| len <= bytes_max) else {
        return Err(Error::Broken {
            reason: format!(
                "a message of {value} bytes is over the limit of {bytes_max}"
            ),
        });
    };
    match pool.alloc(len) {
        Ok(block) => Ok(block),
        Err(block::Error::Exhausted {
            requested,
            available,
        }) => Err(Error::Pool {
            bytes: requested,
            available,
        }),
        Err(error @ block::Error::TooLarge { .. }) => {
            panic!("the pool cannot hold a message of `bytes_max`: {error}")
        }
    }
}

fn ended() -> Error {
    Error::Broken {
        reason: "the stream ended inside a message".to_owned(),
    }
}

/// Copies the source's next bytes into `buffer` after its first `have`. Returns
/// `false` when the stream has ended.
fn pull<B: AsRef<[u8]>>(
    source: &mut impl FnMut(usize) -> Result<Poll<Option<B>>, Error>,
    buffer: &mut [u8],
    have: &mut usize,
) -> Result<Poll<bool>, Error> {
    let rest = buffer.get_mut(*have..).expect("invariant: have <= len");
    let Poll::Ready(chunk) = source(rest.len())? else {
        return Ok(Poll::Pending);
    };
    let Some(chunk) = chunk else {
        return Ok(Poll::Ready(false));
    };
    let chunk = chunk.as_ref();
    assert!(!chunk.is_empty(), "the source gives at least one byte");
    rest.get_mut(..chunk.len())
        .expect("the source gives at most the bytes asked for")
        .copy_from_slice(chunk);
    *have = have.saturating_add(chunk.len());
    Ok(Poll::Ready(true))
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
    /// the bytes it gave. When it has no bytes, the stream has ended, or it is
    /// pending when `open`.
    struct Source {
        bytes: VecDeque<u8>,
        split: usize,
        given: usize,
        open: bool,
    }

    impl Source {
        fn new(bytes: Vec<u8>, split: usize) -> Self {
            Self {
                bytes: bytes.into(),
                split,
                given: 0,
                open: false,
            }
        }

        fn take(&mut self, max: usize) -> Poll<Option<Vec<u8>>> {
            let n = max.min(self.split).min(self.bytes.len());
            if n == 0 {
                return if self.open {
                    Poll::Pending
                } else {
                    Poll::Ready(None)
                };
            }
            self.given = self.given.saturating_add(n);
            Poll::Ready(Some(self.bytes.drain(..n).collect()))
        }
    }

    /// One read of `reader` from `source`, with the message as bytes.
    fn read(
        reader: &mut Reader,
        pool: &Pool,
        source: &mut Source,
    ) -> Result<Poll<Option<Vec<u8>>>, Error> {
        let read = reader.read(pool, |max| Ok(source.take(max)))?;
        Ok(read.map(|block| block.map(|block| block.to_vec())))
    }

    /// Every message `reader` reads from `source`, until the stream ends.
    fn read_all(
        reader: &mut Reader,
        pool: &Pool,
        source: &mut Source,
    ) -> Result<Vec<Vec<u8>>, Error> {
        let mut messages = Vec::new();
        loop {
            match read(reader, pool, source)? {
                Poll::Ready(Some(message)) => messages.push(message),
                Poll::Ready(None) => return Ok(messages),
                Poll::Pending => panic!("a source that is not open is never pending"),
            }
        }
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
            assert_eq!(
                read(&mut reader, &pool, &mut source),
                Ok(Poll::Ready(Some(vec![7; 10])))
            );
            assert_eq!(source.given, 11);
        }

        #[test]
        fn when_source_has_no_bytes_now_it_is_pending_then_goes_on() {
            let pool = pool(1 << 16);
            let stream = encode(&[vec![5; 10]]);
            let (now, later) = stream.split_at(4);
            let mut source = Source::new(now.to_vec(), 64);
            source.open = true;
            let mut reader = Reader::new(16);
            assert_eq!(read(&mut reader, &pool, &mut source), Ok(Poll::Pending));
            source.bytes.extend(later);
            source.open = false;
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Ok(vec![vec![5; 10]])
            );
        }

        #[test]
        fn when_message_is_too_large_it_fails() {
            let pool = pool(1 << 16);
            let mut source = Source::new(encode(&[vec![0; 17]]), 64);
            let mut reader = Reader::new(16);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Err(Error::Broken {
                    reason: "a message of 17 bytes is over the limit of 16".to_owned()
                })
            );
            assert_eq!(source.given, 1);
            assert_eq!(
                read(&mut reader, &pool, &mut source),
                Err(Error::Broken {
                    reason: "a message of 17 bytes is over the limit of 16".to_owned()
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
                Err(Error::Broken {
                    reason: "a message of 4611686018427387903 bytes is over the limit \
                        of 16"
                        .to_owned()
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
                Err(Error::Pool {
                    bytes: 100,
                    available: 108
                })
            );
            assert_eq!(source.given, 2);
            drop(held);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Ok(vec![vec![9; 100]])
            );
        }

        #[test]
        #[should_panic(expected = "the pool cannot hold a message of `bytes_max`")]
        fn when_pool_cannot_hold_bytes_max_it_panics() {
            let pool = pool(300);
            let mut source = Source::new(encode(&[vec![9; 200]]), 64);
            let mut reader = Reader::new(1_000);
            drop(read_all(&mut reader, &pool, &mut source));
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
                .map(|read| read.map(|block| block.map(|block| block.to_vec())));
            assert_eq!(
                read,
                Err(Error::Reset {
                    code: crate::Code(16)
                })
            );
        }

        #[test]
        fn when_source_fails_inside_a_message_it_gives_the_error() {
            let pool = pool(1 << 16);
            let mut reader = Reader::new(16);
            let mut given = false;
            let read = reader
                .read(&pool, |_| {
                    if mem::replace(&mut given, true) {
                        return Err(Error::Reset {
                            code: crate::Code(16),
                        });
                    }
                    Ok(Poll::Ready(Some([0x05])))
                })
                .map(|read| read.map(|block| block.map(|block| block.to_vec())));
            assert_eq!(
                read,
                Err(Error::Reset {
                    code: crate::Code(16)
                })
            );
        }

        #[test]
        #[should_panic(expected = "the source gives at least one byte")]
        fn when_source_gives_no_bytes_it_panics() {
            let pool = pool(1 << 16);
            let mut reader = Reader::new(16);
            drop(reader.read(&pool, |_| Ok(Poll::Ready(Some([0_u8; 0])))));
        }

        #[test]
        fn when_stream_ends_after_a_prefix_it_fails() {
            let pool = pool(1 << 16);
            let mut source = Source::new(vec![0x05], 64);
            let mut reader = Reader::new(16);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Err(Error::Broken {
                    reason: "the stream ended inside a message".to_owned()
                })
            );
        }

        #[test]
        fn when_stream_ends_inside_a_message_it_fails() {
            let pool = pool(1 << 16);
            let mut source = Source::new(vec![0x05, 1, 2], 64);
            let mut reader = Reader::new(16);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Err(Error::Broken {
                    reason: "the stream ended inside a message".to_owned()
                })
            );
        }

        #[test]
        fn when_stream_ends_inside_a_prefix_it_fails() {
            let pool = pool(1 << 16);
            let mut source = Source::new(vec![0x40], 64);
            let mut reader = Reader::new(16);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Err(Error::Broken {
                    reason: "the stream ended inside a message".to_owned()
                })
            );
        }
    }
}

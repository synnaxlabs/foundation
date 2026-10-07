//! Messages on a stream: each is a QUIC variable-length integer that counts its
//! bytes, then the bytes.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use std::mem;
use std::task::Poll;

use block::{Block, Pool, Unique};

use crate::Error;
use crate::varint::{self, Varint};

/// The prefix of a message of `len` bytes.
///
/// # Panics
///
/// When `len` is 2^62 or more, which no peer's limit allows.
pub(crate) fn prefix(len: usize) -> Varint {
    Varint::new(len)
        .unwrap_or_else(|| panic!("a message of {len} bytes is over the varint limit"))
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
    /// byte gives it.
    Prefix {
        bytes: [u8; varint::BYTES_MAX],
        have: usize,
        len: usize,
    },
    /// A message of `len` bytes, with no block yet. `miss` says why the last try
    /// to take one failed.
    Sized { len: u64, miss: Option<Miss> },
    /// A message's block, with `have` of its bytes.
    Body { block: Unique, have: usize },
}

/// Why a [`Reader`] has no block for the next message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Miss {
    /// The pool's budget has no room for it now.
    Exhausted,
    /// The system refused memory for its block.
    Refused,
}

const START: State = State::Prefix {
    bytes: [0; varint::BYTES_MAX],
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

    /// Reads the next whole message into a block from `pool`. `admit(len)` gives
    /// whether the next message, of `len` bytes, may take a block now. `source(max)`
    /// gives the stream's next 1 to `max` bytes, `Pending` when it has none now, or
    /// `None` when the stream has ended. The reader never asks for a byte past the
    /// current message, so later messages stay with the source.
    ///
    /// Returns the message, `Pending` when `admit` refuses the message, `pool` has
    /// no block for it now ([`Reader::miss`] says why), or the source has no more
    /// bytes now, or `None` when the stream ended between two messages. After
    /// `Pending`, the next call goes on where this one stopped. After an error inside
    /// a message's body, the reader holds no block.
    ///
    /// # Errors
    ///
    /// - [`Error::Broken`] when the peer breaks the framing: a message over
    ///   `bytes_max`, or a stream that ends inside a message. The stream cannot go on.
    /// - The source's error.
    ///
    /// # Panics
    ///
    /// When the source gives no bytes or more than `max`, or when `pool` cannot hold
    /// a message of `bytes_max` bytes.
    pub(crate) fn read<B: AsRef<[u8]>>(
        &mut self,
        pool: &Pool,
        mut admit: impl FnMut(usize) -> bool,
        mut source: impl FnMut(usize) -> Result<Poll<Option<B>>, Error>,
    ) -> Result<Poll<Option<Block>>, Error> {
        loop {
            match &mut self.state {
                State::Prefix { bytes, have, len } => {
                    let prefix = bytes.get_mut(..*len).expect("invariant: len <= 8");
                    match pull(&mut source, prefix, have)? {
                        Poll::Pending => return Ok(Poll::Pending),
                        Poll::Ready(false) if *have == 0 => {
                            return Ok(Poll::Ready(None));
                        }
                        Poll::Ready(false) => return Err(ended()),
                        Poll::Ready(true) => {}
                    }
                    let [first, ..] = *bytes;
                    *len = varint::len(first);
                    if *have == *len {
                        let prefix = bytes.get(..*len).expect("invariant: len <= 8");
                        self.state = State::Sized {
                            len: varint::value(prefix),
                            miss: None,
                        };
                    }
                }
                State::Sized { len: value, miss } => {
                    *miss = None;
                    let Some(len) = usize::try_from(*value)
                        .ok()
                        .filter(|&len| len <= self.bytes_max)
                    else {
                        return Err(Error::Broken {
                            reason: format!(
                                "a message of {value} bytes is over the limit of {}",
                                self.bytes_max
                            ),
                        });
                    };
                    if !admit(len) {
                        return Ok(Poll::Pending);
                    }
                    match alloc(pool, len) {
                        Ok(block) => self.state = State::Body { block, have: 0 },
                        Err(cause) => {
                            *miss = Some(cause);
                            return Ok(Poll::Pending);
                        }
                    }
                }
                State::Body { block, have } => {
                    if *have < block.len() {
                        let error = match pull(&mut source, block, have) {
                            Ok(Poll::Pending) => return Ok(Poll::Pending),
                            Ok(Poll::Ready(true)) => None,
                            Ok(Poll::Ready(false)) => Some(ended()),
                            Err(error) => Some(error),
                        };
                        if let Some(error) = error {
                            self.state = START;
                            return Err(error);
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

    /// Why the last [`Reader::read`] found no block for the next message, if it
    /// gave `Pending` for that reason.
    pub(crate) fn miss(&self) -> Option<Miss> {
        match self.state {
            State::Sized { miss, .. } => miss,
            State::Prefix { .. } | State::Body { .. } => None,
        }
    }
}

/// A block of `len` bytes from `pool`, or why it has none now.
///
/// # Panics
///
/// When the pool cannot hold `len` bytes.
fn alloc(pool: &Pool, len: usize) -> Result<Unique, Miss> {
    match pool.alloc(len) {
        Ok(block) => Ok(block),
        Err(block::Error::Exhausted { .. }) => Err(Miss::Exhausted),
        Err(block::Error::Refused { .. }) => Err(Miss::Refused),
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

    use block::testing::Scarce;
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
            stream.extend_from_slice(&prefix(message.len()));
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
        let read = reader.read(pool, |_| true, |max| Ok(source.take(max)))?;
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

    #[test]
    #[should_panic(expected = "a message of 4611686018427387904 bytes")]
    fn prefix_panics_at_2_to_the_62() {
        let _ = prefix(1 << 62);
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
        fn when_pool_is_full_it_waits_then_goes_on() {
            // A 100-byte block takes 192 bytes of the budget.
            let pool = pool(300);
            let held = pool.alloc(100).expect("room");
            let mut source = Source::new(encode(&[vec![9; 100]]), 64);
            let mut reader = Reader::new(1_000);
            assert_eq!(read(&mut reader, &pool, &mut source), Ok(Poll::Pending));
            assert_eq!(reader.miss(), Some(Miss::Exhausted));
            assert_eq!(source.given, 2);
            assert_eq!(read(&mut reader, &pool, &mut source), Ok(Poll::Pending));
            assert_eq!(reader.miss(), Some(Miss::Exhausted));
            drop(held);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Ok(vec![vec![9; 100]])
            );
        }

        #[test]
        fn when_the_system_refuses_the_commit_it_waits_then_goes_on() {
            let config = Config { budget: 1 << 16 };
            let (memory, switch) = Scarce::new(config.reservation());
            let pool = Pool::new(config, memory);
            let mut source = Source::new(encode(&[vec![3; 9000]]), 4096);
            let mut reader = Reader::new(10_000);
            switch.refuse();
            assert_eq!(read(&mut reader, &pool, &mut source), Ok(Poll::Pending));
            assert_eq!(reader.miss(), Some(Miss::Refused));
            switch.allow();
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Ok(vec![vec![3; 9000]])
            );
            assert_eq!(reader.miss(), None);
        }

        #[test]
        fn a_later_wait_for_the_budget_or_the_source_is_no_miss() {
            let pool = pool(300);
            let held = pool.alloc(100).expect("room");
            let mut source = Source::new(encode(&[vec![9; 100]]), 64);
            source.open = true;
            let mut reader = Reader::new(1_000);
            assert_eq!(read(&mut reader, &pool, &mut source), Ok(Poll::Pending));
            assert_eq!(reader.miss(), Some(Miss::Exhausted));
            let refused = reader.read(&pool, |_| false, |max| Ok(source.take(max)));
            assert!(matches!(refused, Ok(Poll::Pending)), "{refused:?}");
            assert_eq!(reader.miss(), None);
            drop(held);
            let message = read(&mut reader, &pool, &mut source);
            assert_eq!(message, Ok(Poll::Ready(Some(vec![9; 100]))));
            assert_eq!(read(&mut reader, &pool, &mut source), Ok(Poll::Pending));
            assert_eq!(reader.miss(), None);
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
                .read::<Vec<u8>>(
                    &pool,
                    |_| true,
                    |_| {
                        Err(Error::Reset {
                            code: crate::Code(16),
                        })
                    },
                )
                .map(|read| read.map(|block| block.map(|block| block.to_vec())));
            assert_eq!(
                read,
                Err(Error::Reset {
                    code: crate::Code(16)
                })
            );
        }

        #[test]
        fn when_source_fails_inside_a_message_it_gives_the_error_and_drops_the_block() {
            // A 100-byte block takes 192 bytes of the budget, so the pool holds one.
            let pool = pool(300);
            let mut reader = Reader::new(1_000);
            let part = encode(&[vec![9; 100]]).into_iter().take(10).collect();
            let mut source = Source::new(part, 64);
            source.open = true;
            assert_eq!(read(&mut reader, &pool, &mut source), Ok(Poll::Pending));
            let read = reader
                .read::<Vec<u8>>(
                    &pool,
                    |_| true,
                    |_| {
                        Err(Error::Reset {
                            code: crate::Code(16),
                        })
                    },
                )
                .map(|read| read.map(|block| block.map(|block| block.to_vec())));
            assert_eq!(
                read,
                Err(Error::Reset {
                    code: crate::Code(16)
                })
            );
            drop(pool.alloc(100).expect("the reader dropped its block"));
        }

        #[test]
        fn when_admit_refuses_it_takes_no_body_bytes_then_goes_on() {
            let pool = pool(1 << 16);
            let mut source = Source::new(encode(&[vec![4; 10]]), 64);
            let mut reader = Reader::new(16);
            let read = reader
                .read(&pool, |_| false, |max| Ok(source.take(max)))
                .map(|read| read.map(|block| block.map(|block| block.to_vec())));
            assert_eq!(read, Ok(Poll::Pending));
            assert_eq!(source.given, 1);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Ok(vec![vec![4; 10]])
            );
        }

        #[test]
        #[should_panic(expected = "the source gives at least one byte")]
        fn when_source_gives_no_bytes_it_panics() {
            let pool = pool(1 << 16);
            let mut reader = Reader::new(16);
            drop(reader.read(&pool, |_| true, |_| Ok(Poll::Ready(Some([0_u8; 0])))));
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

        #[test]
        fn when_readers_that_hold_large_blocks_drop_a_small_message_reads() {
            let bytes_max = 1 << 10;
            let pool = pool(2 * block::footprint(bytes_max));
            let mut readers = Vec::new();
            for _ in 0..2 {
                let mut source = Source::new(prefix(bytes_max).to_vec(), 64);
                source.open = true;
                let mut reader = Reader::new(bytes_max);
                assert_eq!(read(&mut reader, &pool, &mut source), Ok(Poll::Pending));
                readers.push(reader);
            }
            let mut source = Source::new(encode(&[vec![1; 10]]), 64);
            let mut reader = Reader::new(bytes_max);
            assert_eq!(read(&mut reader, &pool, &mut source), Ok(Poll::Pending));
            assert_eq!(reader.miss(), Some(Miss::Exhausted));
            drop(readers);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Ok(vec![vec![1; 10]])
            );
        }
    }
}

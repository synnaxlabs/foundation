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

use block::{Block, Unique};
use bytes::Bytes;

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

/// Splits a stream's bytes into whole messages, each in one block.
#[derive(Debug)]
pub(crate) struct Reader {
    bytes_max: usize,
    state: State,
    held: Held,
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
    /// A message of `len` bytes, with no room yet.
    Sized { len: u64 },
    /// A message of `len` bytes that has room, with `have` of its bytes in the
    /// reader's [`Held`].
    Body { len: usize, have: usize },
}

/// The bytes of a message that has no block yet. A chunk of the source can keep its
/// whole receive buffer alive, so a chunk lives only inside the read that took it.
#[derive(Debug, Default)]
struct Held {
    /// The bytes held across reads, in one allocation of the message's length.
    buffer: Vec<u8>,
    /// The chunks that this read took, after `buffer`. Its capacity stays.
    chunks: Vec<Bytes>,
}

/// The most chunks that one read holds. Past it, the read copies them into the
/// buffer, so a peer that sends tiny frames cannot grow the list.
const CHUNKS_MAX: usize = 64;

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
            held: Held::default(),
        }
    }

    /// A reader, as [`Reader::new`], that has read `first`, the first byte of the
    /// stream's first message.
    pub(crate) fn started(bytes_max: usize, first: u8) -> Self {
        let len = varint::len(first);
        let state = if len == 1 {
            State::Sized {
                len: varint::value(&[first]),
            }
        } else {
            let mut bytes = [0; varint::BYTES_MAX];
            let [head, ..] = &mut bytes;
            *head = first;
            State::Prefix {
                bytes,
                have: 1,
                len,
            }
        };
        Self {
            bytes_max,
            state,
            held: Held::default(),
        }
    }

    /// Reads the next whole message into a block. `admit(len)` says whether the
    /// next message, of `len` bytes, may take its bytes now. `take(len)` gives a
    /// block of exactly `len` bytes for a whole message, or `None` when it may not
    /// have one now. `source(max)` gives the stream's next 1 to `max` bytes,
    /// `Pending` when it has none now, or `None` when the stream has ended. The
    /// reader never asks for a byte past the current message, so later messages
    /// stay with the source. It holds the bytes of a message until the message is
    /// whole, and no chunk from `source` outlives the call.
    ///
    /// Returns the message, `Pending` when `admit` or `take` refuses or the source
    /// has no more bytes now, or `None` when the stream ended between two messages.
    /// After `Pending`, the next call goes on where this one stopped: it asks
    /// `admit` again only when it refused, and `take` only for a whole message.
    /// After an error inside a message's body, the reader holds no bytes of the
    /// message. After any other error, it holds at most the 8 bytes of a length
    /// prefix.
    ///
    /// # Errors
    ///
    /// - [`Error::Broken`] when the peer breaks the framing: a message over
    ///   `bytes_max`, or a stream that ends inside a message. The stream cannot go on.
    /// - The source's error.
    ///
    /// # Panics
    ///
    /// When the source gives no bytes or more than `max`.
    pub(crate) fn read(
        &mut self,
        mut admit: impl FnMut(usize) -> bool,
        mut take: impl FnMut(usize) -> Option<Unique>,
        mut source: impl FnMut(usize) -> Result<Poll<Option<Bytes>>, Error>,
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
                        };
                    }
                }
                State::Sized { len: value } => {
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
                    self.state = State::Body { len, have: 0 };
                }
                State::Body { len, have } if *have < *len => {
                    let error = match next(&mut source, len.saturating_sub(*have)) {
                        Ok(Poll::Pending) => {
                            self.held.spill(*len);
                            return Ok(Poll::Pending);
                        }
                        Ok(Poll::Ready(Some(chunk))) => {
                            *have = have.saturating_add(chunk.len());
                            self.held.push(*len, chunk);
                            continue;
                        }
                        Ok(Poll::Ready(None)) => ended(),
                        Err(error) => error,
                    };
                    self.clear();
                    return Err(error);
                }
                State::Body { len, .. } => {
                    let Some(mut block) = take(*len) else {
                        self.held.spill(*len);
                        return Ok(Poll::Pending);
                    };
                    self.held.drain_into(&mut block);
                    self.state = START;
                    return Ok(Poll::Ready(Some(block.freeze())));
                }
            }
        }
    }

    /// Drops the message in hand, so that the reader holds no bytes of it. For a
    /// stream that ended outside [`Reader::read`], as by a reset that `source` did
    /// not give.
    pub(crate) fn clear(&mut self) {
        self.state = START;
        self.held.clear();
    }
}

#[cfg(test)]
impl Reader {
    /// The length and capacity of the buffer that holds bytes across reads, if it
    /// has an allocation, and the count of chunks held.
    pub(crate) fn held(&self) -> (Option<(usize, usize)>, usize) {
        let buffer = &self.held.buffer;
        let held = (buffer.capacity() > 0).then(|| (buffer.len(), buffer.capacity()));
        (held, self.held.chunks.len())
    }
}

impl Held {
    /// Holds `chunk`, the next bytes of a message of `len` bytes.
    fn push(&mut self, len: usize, chunk: Bytes) {
        if self.chunks.len() == CHUNKS_MAX {
            self.spill(len);
        }
        self.chunks.push(chunk);
    }

    /// Copies the chunks into the buffer of a message of `len` bytes, and drops them.
    /// The first chunk makes the buffer, so a message with no chunks holds no heap.
    fn spill(&mut self, len: usize) {
        for chunk in self.chunks.drain(..) {
            if self.buffer.capacity() == 0 {
                self.buffer = Vec::with_capacity(len);
            }
            self.buffer.extend_from_slice(&chunk);
        }
    }

    /// Copies the held bytes into `block`, which holds exactly that many, and drops
    /// them.
    fn drain_into(&mut self, block: &mut [u8]) {
        let (buffered, mut rest) = block.split_at_mut(self.buffer.len());
        buffered.copy_from_slice(&self.buffer);
        for chunk in &self.chunks {
            let (bytes, after) = mem::take(&mut rest).split_at_mut(chunk.len());
            bytes.copy_from_slice(chunk);
            rest = after;
        }
        self.clear();
    }

    fn clear(&mut self) {
        self.buffer = Vec::new();
        self.chunks.clear();
    }
}

fn ended() -> Error {
    Error::Broken {
        reason: "the stream ended inside a message".to_owned(),
    }
}

/// The source's next 1 to `max` bytes, as `source(max)` gives them.
///
/// # Panics
///
/// When the source gives no bytes or more than `max`.
fn next(
    source: &mut impl FnMut(usize) -> Result<Poll<Option<Bytes>>, Error>,
    max: usize,
) -> Result<Poll<Option<Bytes>>, Error> {
    let next = source(max)?;
    if let Poll::Ready(Some(chunk)) = &next {
        assert!(!chunk.is_empty(), "the source gives at least one byte");
        assert!(
            chunk.len() <= max,
            "the source gives at most the bytes asked for"
        );
    }
    Ok(next)
}

/// Copies the source's next bytes into `buffer` after its first `have`. Returns
/// `false` when the stream has ended.
fn pull(
    source: &mut impl FnMut(usize) -> Result<Poll<Option<Bytes>>, Error>,
    buffer: &mut [u8],
    have: &mut usize,
) -> Result<Poll<bool>, Error> {
    let rest = buffer.get_mut(*have..).expect("invariant: have <= len");
    let Poll::Ready(chunk) = next(source, rest.len())? else {
        return Ok(Poll::Pending);
    };
    let Some(chunk) = chunk else {
        return Ok(Poll::Ready(false));
    };
    rest.get_mut(..chunk.len())
        .expect("invariant: the chunk fits")
        .copy_from_slice(&chunk);
    *have = have.saturating_add(chunk.len());
    Ok(Poll::Ready(true))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::{iter, slice};

    use block::{Config, Heap, Pool};
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

        fn take(&mut self, max: usize) -> Poll<Option<Bytes>> {
            let n = max.min(self.split).min(self.bytes.len());
            if n == 0 {
                return if self.open {
                    Poll::Pending
                } else {
                    Poll::Ready(None)
                };
            }
            self.given = self.given.saturating_add(n);
            Poll::Ready(Some(self.bytes.drain(..n).collect::<Vec<_>>().into()))
        }
    }

    /// One read of `reader` from `source`, with the message as bytes.
    fn read(
        reader: &mut Reader,
        pool: &Pool,
        source: &mut Source,
    ) -> Result<Poll<Option<Vec<u8>>>, Error> {
        let take = |len| pool.alloc(len).ok();
        let read = reader.read(|_| true, take, |max| Ok(source.take(max)))?;
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

            #[test]
            fn started_gives_what_new_gives_after_the_first_byte(
                messages in prop::collection::vec(
                    prop::collection::vec(any::<u8>(), 0..=20_000),
                    1..4,
                ),
                split in 1_usize..400,
            ) {
                let pool = pool(1 << 16);
                let mut bytes = encode(&messages);
                let first = bytes.remove(0);
                let mut source = Source::new(bytes, split);
                let mut reader = Reader::started(20_000, first);
                let read = read_all(&mut reader, &pool, &mut source);
                prop_assert_eq!(read, Ok(messages));
            }

            #[test]
            fn when_stream_ends_inside_a_prefix_it_fails(
                // `None` repeats the first byte.
                tail in prop::collection::vec(
                    prop_oneof![
                        Just(None),
                        Just(Some(0)),
                        Just(Some(0xFF)),
                        any::<u8>().prop_map(Some),
                    ],
                    7,
                ),
            ) {
                let pool = pool(1 << 16);
                // A message before the cut prefix leaves state in the reader.
                let starts = [Vec::new(), encode(&[vec![1, 2, 3]])];
                // Each first byte of a prefix of 2, 4, or 8 bytes.
                for first in 0x40_u8..=0xFF {
                    let len = 1_usize << (first >> 6);
                    let tail = tail.iter().map(|byte| byte.unwrap_or(first));
                    let whole: Vec<_> = iter::once(first).chain(tail).collect();
                    for cut in 1..len {
                        for split in 1..len {
                            for start in &starts {
                                let cut = whole.get(..cut).expect("a cut");
                                let bytes = [start.as_slice(), cut].concat();
                                let mut source = Source::new(bytes, split);
                                let mut reader = Reader::new(16);
                                prop_assert_eq!(
                                    read_all(&mut reader, &pool, &mut source),
                                    Err(Error::Broken {
                                        reason: "the stream ended inside a message"
                                            .to_owned()
                                    }),
                                    "{:x?} then {:x?}, {} per chunk",
                                    start,
                                    cut,
                                    split
                                );
                                // Private: only a peer that misframes ends a stream
                                // inside a message, and no heap count is exact in a
                                // binary with a test harness.
                                prop_assert_eq!(reader.held.buffer.capacity(), 0);
                                let slots = reader.held.chunks.capacity();
                                prop_assert!(
                                    slots <= CHUNKS_MAX,
                                    "a list of {} slots",
                                    slots
                                );
                            }
                        }
                    }
                }
            }
        }

        #[test]
        fn when_stream_ends_inside_a_prefix_after_a_long_message_it_fails() {
            let pool = pool(1 << 16);
            // At 1 to 3 bytes per chunk the message fills the list of chunks.
            for split in 1..=8 {
                for cut in [&[0x40][..], &[0x80, 0, 0], &[0xC0; 7]] {
                    let bytes = [encode(&[vec![1; 200]]).as_slice(), cut].concat();
                    let mut source = Source::new(bytes, split);
                    let mut reader = Reader::new(200);
                    assert_eq!(
                        read_all(&mut reader, &pool, &mut source),
                        Err(Error::Broken {
                            reason: "the stream ended inside a message".to_owned()
                        }),
                        "{cut:x?}, {split} per chunk"
                    );
                }
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
        fn when_source_fails_it_gives_the_error() {
            let pool = pool(1 << 16);
            let mut reader = Reader::new(16);
            let read = reader
                .read(
                    |_| true,
                    |len| pool.alloc(len).ok(),
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
        fn when_source_fails_inside_a_message_it_gives_the_error_and_drops_its_bytes() {
            let pool = pool(1 << 16);
            let mut reader = Reader::new(1_000);
            let part = encode(&[vec![9; 100]]).into_iter().take(10).collect();
            let mut source = Source::new(part, 64);
            source.open = true;
            assert_eq!(read(&mut reader, &pool, &mut source), Ok(Poll::Pending));
            // Private: no heap count is exact in a binary with a test harness.
            assert_eq!(reader.held.buffer, vec![9; 8]);
            let read = reader
                .read(
                    |_| true,
                    |len| pool.alloc(len).ok(),
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
            assert_eq!(reader.held.buffer.capacity(), 0);
            let mut next = Source::new(encode(&[vec![5; 20]]), 64);
            let next = super::read(&mut reader, &pool, &mut next);
            assert_eq!(next, Ok(Poll::Ready(Some(vec![5; 20]))));
        }

        #[test]
        fn when_admit_refuses_it_takes_no_body_bytes_then_goes_on() {
            let pool = pool(1 << 16);
            let mut source = Source::new(encode(&[vec![4; 100]]), 64);
            let mut reader = Reader::new(1_000);
            let mut asked = Vec::new();
            for _ in 0..2 {
                let admit = |len| {
                    asked.push(len);
                    false
                };
                let take = |_| panic!("a message with no room takes no block");
                let read = reader.read(admit, take, |max| Ok(source.take(max)));
                assert!(matches!(read, Ok(Poll::Pending)), "{read:?}");
            }
            assert_eq!(asked, [100, 100]);
            assert_eq!(source.given, 2);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Ok(vec![vec![4; 100]])
            );
        }

        #[test]
        fn when_take_gives_no_block_it_holds_the_whole_message_then_goes_on() {
            let pool = pool(1 << 16);
            let mut source = Source::new(encode(&[vec![4; 100], vec![5; 3]]), 64);
            let mut reader = Reader::new(1_000);
            let (mut admitted, mut asked) = (Vec::new(), Vec::new());
            for _ in 0..2 {
                let admit = |len| {
                    admitted.push(len);
                    true
                };
                let take = |len| {
                    asked.push(len);
                    None
                };
                let read = reader.read(admit, take, |max| Ok(source.take(max)));
                assert!(matches!(read, Ok(Poll::Pending)), "{read:?}");
            }
            assert_eq!((admitted, asked), (vec![100], vec![100, 100]));
            assert_eq!(source.given, 102);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Ok(vec![vec![4; 100], vec![5; 3]])
            );
        }

        /// One read of `reader` that takes `batch[*at..end]` one byte per chunk,
        /// each a view into `batch`, then is pending.
        fn read_views(
            reader: &mut Reader,
            pool: &Pool,
            batch: &Bytes,
            at: &mut usize,
            end: usize,
        ) -> Result<Poll<Option<Vec<u8>>>, Error> {
            let source = |_| {
                if *at == end {
                    return Ok(Poll::Pending);
                }
                let chunk = batch.slice(*at..=*at);
                *at = at.saturating_add(1);
                Ok(Poll::Ready(Some(chunk)))
            };
            let read = reader.read(|_| true, |len| pool.alloc(len).ok(), source)?;
            Ok(read.map(|block| block.map(|block| block.to_vec())))
        }

        #[test]
        fn a_read_that_ends_before_the_block_keeps_no_chunk_but_one_buffer() {
            let pool = pool(1 << 16);
            let message: Vec<u8> = (0..=255).cycle().take(1_024).collect();
            let batch = Bytes::from(encode(slice::from_ref(&message)));
            let (mut reader, mut at) = (Reader::new(1_024), 0);
            let read = read_views(&mut reader, &pool, &batch, &mut at, 2 + 10);
            assert_eq!(read, Ok(Poll::Pending));
            assert!(batch.is_unique());
            // Private: no heap count is exact in a binary with a test harness.
            assert_eq!(reader.held.buffer, message[..10]);
            assert_eq!(reader.held.buffer.capacity(), 1_024);
            let buffer = reader.held.buffer.as_ptr();
            let read = read_views(&mut reader, &pool, &batch, &mut at, 2 + 11);
            assert_eq!(read, Ok(Poll::Pending));
            assert!(batch.is_unique());
            assert_eq!(reader.held.buffer, message[..11]);
            assert_eq!(reader.held.buffer.as_ptr(), buffer);
            let read = read_views(&mut reader, &pool, &batch, &mut at, batch.len());
            assert_eq!(read, Ok(Poll::Ready(Some(message))));
            assert!(batch.is_unique());
            assert_eq!(reader.held.buffer.capacity(), 0);
        }

        #[test]
        fn a_whole_message_with_no_block_keeps_no_chunk_but_one_buffer() {
            let pool = pool(block::footprint(1_024));
            let message: Vec<u8> = (0..=255).cycle().take(1_024).collect();
            let batch = Bytes::from(encode(slice::from_ref(&message)));
            let (mut reader, mut at) = (Reader::new(1_024), 0);
            let held = pool.alloc(1_024).expect("a block");
            let read = read_views(&mut reader, &pool, &batch, &mut at, batch.len());
            assert_eq!(read, Ok(Poll::Pending));
            assert!(batch.is_unique());
            // Private: no heap count is exact in a binary with a test harness.
            assert_eq!(reader.held.buffer, message);
            drop(held);
            let read = read_views(&mut reader, &pool, &batch, &mut at, batch.len());
            assert_eq!(read, Ok(Poll::Ready(Some(message))));
            assert_eq!(reader.held.buffer.capacity(), 0);
        }

        /// A chunk that counts itself in `live` while it lives.
        struct Counted {
            bytes: Vec<u8>,
            live: Arc<AtomicUsize>,
        }

        impl AsRef<[u8]> for Counted {
            fn as_ref(&self) -> &[u8] {
                &self.bytes
            }
        }

        impl Drop for Counted {
            fn drop(&mut self) {
                self.live.fetch_sub(1, Ordering::Relaxed);
            }
        }

        /// Each copy of a full list, not only the first two that
        /// `tests/alloc/chunks.rs` pins, keeps the list at the bound.
        #[test]
        fn a_read_of_many_tiny_chunks_never_holds_more_than_chunks_max() {
            let pool = pool(1 << 20);
            let message: Vec<u8> = (0..=250).cycle().take(1 << 18).collect();
            let stream = encode(slice::from_ref(&message));
            let live = Arc::new(AtomicUsize::new(0));
            let (mut at, mut size) = (0, 0);
            let source = |max: usize| {
                assert!(live.load(Ordering::Relaxed) <= 64, "at byte {at}");
                size = size % 3 + 1;
                let end = at + size.min(max);
                if end == stream.len() {
                    // The check in `take` sees a 65th chunk only after a full list.
                    assert_eq!(live.load(Ordering::Relaxed), 64, "a full list");
                }
                live.fetch_add(1, Ordering::Relaxed);
                let bytes = stream[at..end].to_vec();
                at = end;
                let live = Arc::clone(&live);
                Ok(Poll::Ready(Some(Bytes::from_owner(Counted {
                    bytes,
                    live,
                }))))
            };
            let take = |len| {
                assert!(live.load(Ordering::Relaxed) <= 64, "at the last byte");
                pool.alloc(len).ok()
            };
            let mut reader = Reader::new(message.len());
            let read = reader
                .read(|_| true, take, source)
                .map(|read| read.map(|block| block.map(|block| block.to_vec())));
            assert_eq!(read, Ok(Poll::Ready(Some(message))));
            assert_eq!(live.load(Ordering::Relaxed), 0);
        }

        #[test]
        #[should_panic(expected = "the source gives at least one byte")]
        fn when_source_gives_no_bytes_it_panics() {
            let pool = pool(1 << 16);
            let mut reader = Reader::new(16);
            drop(reader.read(
                |_| true,
                |len| pool.alloc(len).ok(),
                |_| Ok(Poll::Ready(Some(Bytes::new()))),
            ));
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
        fn when_stream_ends_inside_a_message_it_fails_and_keeps_no_byte() {
            let pool = pool(1 << 16);
            let batch = Bytes::from(encode(&[vec![3; 1_000]]));
            let end = 2 + 100;
            let mut at = 0;
            let mut reader = Reader::new(1_000);
            let source = |_| {
                let chunk = (at < end).then(|| batch.slice(at..=at));
                at += 1;
                Ok(Poll::Ready(chunk))
            };
            let read = reader
                .read(|_| true, |len| pool.alloc(len).ok(), source)
                .map(|read| read.map(|block| block.map(|block| block.to_vec())));
            assert_eq!(
                read,
                Err(Error::Broken {
                    reason: "the stream ended inside a message".to_owned()
                })
            );
            assert!(batch.is_unique(), "a chunk outlives the read");
            // Private: only a peer that misframes ends a stream inside a message, and
            // no heap count is exact in a binary with a test harness.
            assert_eq!(reader.held.buffer.capacity(), 0);
            let slots = reader.held.chunks.capacity();
            assert!(slots <= CHUNKS_MAX, "a list of {slots} slots");
        }

        #[test]
        fn messages_in_progress_hold_no_block() {
            let bytes_max = 1 << 10;
            let pool = pool(block::footprint(bytes_max));
            let large = vec![2; bytes_max];
            let part: Vec<u8> = encode(slice::from_ref(&large))
                .into_iter()
                .take(500)
                .collect();
            let mut readers = Vec::new();
            for _ in 0..2 {
                let mut source = Source::new(part.clone(), 64);
                source.open = true;
                let mut reader = Reader::new(bytes_max);
                assert_eq!(read(&mut reader, &pool, &mut source), Ok(Poll::Pending));
                readers.push((reader, source));
            }
            let mut source = Source::new(encode(&[vec![1; 10]]), 64);
            let mut reader = Reader::new(bytes_max);
            assert_eq!(
                read_all(&mut reader, &pool, &mut source),
                Ok(vec![vec![1; 10]])
            );
            for (reader, source) in &mut readers {
                source
                    .bytes
                    .extend(encode(slice::from_ref(&large)).into_iter().skip(500));
                source.open = false;
                assert_eq!(read_all(reader, &pool, source), Ok(vec![large.clone()]));
            }
        }
    }
}

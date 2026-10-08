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
    /// The chunks of the message that this read took, after the buffer of
    /// [`State::Body`]. Its capacity stays.
    chunks: Vec<Bytes>,
}

/// What a [`Reader`] needs from its caller to go on.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The stream ended between two messages.
    Ended,
    /// The source has no more bytes now.
    Pending,
    /// The next message has `len` bytes and needs room. Call [`Reader::admit`] once
    /// it has room. Until then, each read gives `Room` again and takes no byte.
    Room(usize),
    /// The message, of `len` bytes, is whole. Call [`Reader::fill`].
    Block(usize),
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
    Sized { len: usize },
    /// A message of `len` bytes, over the limit.
    Over { len: u64 },
    /// A message that the stream's end cut short.
    Cut,
    /// A message of `len` bytes that has room, with `have` of its bytes in `buffer`
    /// and the reader's chunks. A chunk of the source can keep its whole receive
    /// buffer alive, so a chunk lives only inside the read that took it, and
    /// `buffer` holds the bytes across reads.
    Body {
        len: usize,
        have: usize,
        buffer: Vec<u8>,
    },
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
            chunks: Vec::new(),
        }
    }

    /// A reader, as [`Reader::new`], that has read `first`, the first byte of the
    /// stream's first message.
    pub(crate) fn started(bytes_max: usize, first: u8) -> Self {
        let len = varint::len(first);
        let state = if len == 1 {
            sized(varint::value(&[first]), bytes_max)
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
            chunks: Vec::new(),
        }
    }

    /// Reads the stream's bytes from `source` until the reader needs a step from
    /// the caller. `source(max)` gives the stream's next 1 to `max` bytes, `Pending`
    /// when it has none now, or `None` when the stream has ended. The reader never
    /// asks for a byte past the current message, so later messages stay with the
    /// source. No chunk from `source` outlives the call, except those of the whole
    /// message that [`Step::Block`] gives.
    ///
    /// After an error inside a message's body, the reader holds no bytes of the
    /// message. After any other error, it holds at most the 8 bytes of a length
    /// prefix. After the framing breaks, each later read gives its error again.
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
        mut source: impl FnMut(usize) -> Result<Poll<Option<Bytes>>, Error>,
    ) -> Result<Step, Error> {
        loop {
            match &mut self.state {
                State::Prefix { bytes, have, len } => {
                    let prefix = bytes.get_mut(..*len).expect("invariant: len <= 8");
                    match pull(&mut source, prefix, have)? {
                        Poll::Pending => return Ok(Step::Pending),
                        Poll::Ready(false) if *have == 0 => return Ok(Step::Ended),
                        Poll::Ready(false) => return Err(ended()),
                        Poll::Ready(true) => {}
                    }
                    let [first, ..] = *bytes;
                    *len = varint::len(first);
                    if *have == *len {
                        let prefix = bytes.get(..*len).expect("invariant: len <= 8");
                        self.state = sized(varint::value(prefix), self.bytes_max);
                    }
                }
                State::Sized { len } => return Ok(Step::Room(*len)),
                State::Over { len } => {
                    return Err(Error::Broken {
                        reason: format!(
                            "a message of {len} bytes is over the limit of {}",
                            self.bytes_max
                        ),
                    });
                }
                State::Body { len, have, buffer } if *have < *len => {
                    let error = match next(&mut source, len.saturating_sub(*have)) {
                        Ok(Poll::Pending) => {
                            spill(buffer, &mut self.chunks, *len);
                            return Ok(Step::Pending);
                        }
                        Ok(Poll::Ready(Some(chunk))) => {
                            *have = have.saturating_add(chunk.len());
                            if self.chunks.len() == CHUNKS_MAX {
                                spill(buffer, &mut self.chunks, *len);
                            }
                            self.chunks.push(chunk);
                            continue;
                        }
                        Ok(Poll::Ready(None)) => {
                            self.state = State::Cut;
                            self.chunks.clear();
                            return Err(ended());
                        }
                        Err(error) => error,
                    };
                    self.clear();
                    return Err(error);
                }
                State::Cut => return Err(ended()),
                State::Body { len, .. } => return Ok(Step::Block(*len)),
            }
        }
    }

    /// Lets the message that [`Step::Room`] gave take its bytes.
    ///
    /// # Panics
    ///
    /// When the reader has no message that waits for room.
    pub(crate) fn admit(&mut self) {
        let State::Sized { len } = self.state else {
            panic!("a reader admits only a message that waits for room");
        };
        self.state = State::Body {
            len,
            have: 0,
            buffer: Vec::new(),
        };
    }

    /// Copies the whole message that [`Step::Block`] gave into `block` and gives
    /// it, so that the next read starts the next message. With `None`, it copies
    /// the message's chunks into its own buffer and gives `Pending`, and the next
    /// read gives `Block` again.
    ///
    /// # Panics
    ///
    /// When the reader has no whole message, or `block` is not its length.
    pub(crate) fn fill(&mut self, block: Option<Unique>) -> Poll<Block> {
        let State::Body { len, have, buffer } = &mut self.state else {
            panic!("a reader fills only a whole message");
        };
        assert!(have == len, "a reader fills only a whole message");
        let Some(mut block) = block else {
            spill(buffer, &mut self.chunks, *len);
            return Poll::Pending;
        };
        assert_eq!(block.len(), *len, "the block is the message's length");
        let (buffered, mut rest) = block.split_at_mut(buffer.len());
        buffered.copy_from_slice(buffer);
        for chunk in self.chunks.drain(..) {
            let (bytes, after) = mem::take(&mut rest).split_at_mut(chunk.len());
            bytes.copy_from_slice(&chunk);
            rest = after;
        }
        self.state = START;
        Poll::Ready(block.freeze())
    }

    /// Drops the message in hand, so that the reader holds no bytes of it. For a
    /// stream that ended outside [`Reader::read`], as by a reset that `source` did
    /// not give.
    pub(crate) fn clear(&mut self) {
        self.state = START;
        self.chunks.clear();
    }
}

#[cfg(test)]
impl Reader {
    /// The length and capacity of the buffer that holds bytes across reads, if it
    /// has an allocation, and the count of chunks held.
    pub(crate) fn held(&self) -> (Option<(usize, usize)>, usize) {
        let buffer = self.buffer();
        let held = (buffer.capacity() > 0).then(|| (buffer.len(), buffer.capacity()));
        (held, self.chunks.len())
    }

    /// The buffer of the message in hand, or an empty one.
    fn buffer(&self) -> &Vec<u8> {
        const EMPTY: &Vec<u8> = &Vec::new();
        match &self.state {
            State::Body { buffer, .. } => buffer,
            _ => EMPTY,
        }
    }
}

/// The state after a whole prefix that gives `len`.
fn sized(len: u64, bytes_max: usize) -> State {
    match usize::try_from(len).ok().filter(|&len| len <= bytes_max) {
        Some(len) => State::Sized { len },
        None => State::Over { len },
    }
}

/// Copies `chunks` into `buffer`, the buffer of a message of `len` bytes, and drops
/// them. The first chunk makes the buffer, so a message with no chunks holds no
/// heap.
fn spill(buffer: &mut Vec<u8>, chunks: &mut Vec<Bytes>, len: usize) {
    for chunk in chunks.drain(..) {
        if buffer.capacity() == 0 {
            *buffer = Vec::with_capacity(len);
        }
        buffer.extend_from_slice(&chunk);
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
    use std::slice;

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

    /// Drives `reader` to its next message as a stream's read does: `admit(len)`
    /// says whether a message has room, and `take(len)` gives its block or `None`.
    fn drive(
        reader: &mut Reader,
        mut admit: impl FnMut(usize) -> bool,
        mut take: impl FnMut(usize) -> Option<Unique>,
        mut source: impl FnMut(usize) -> Result<Poll<Option<Bytes>>, Error>,
    ) -> Result<Poll<Option<Block>>, Error> {
        loop {
            match reader.read(&mut source)? {
                Step::Ended => return Ok(Poll::Ready(None)),
                Step::Pending => return Ok(Poll::Pending),
                Step::Room(len) => {
                    if !admit(len) {
                        return Ok(Poll::Pending);
                    }
                    reader.admit();
                }
                Step::Block(len) => return Ok(reader.fill(take(len)).map(Some)),
            }
        }
    }

    /// One read of `reader` from `source`, with the message as bytes.
    fn read(
        reader: &mut Reader,
        pool: &Pool,
        source: &mut Source,
    ) -> Result<Poll<Option<Vec<u8>>>, Error> {
        let take = |len| pool.alloc(len).ok();
        let read = drive(reader, |_| true, take, |max| Ok(source.take(max)))?;
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
            let read = drive(
                &mut reader,
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
            // No call shows the heap that the reader keeps.
            assert_eq!(*reader.buffer(), vec![9; 8]);
            let read = drive(
                &mut reader,
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
            assert_eq!(reader.buffer().capacity(), 0);
            assert!(reader.chunks.is_empty());
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
                let read = drive(&mut reader, admit, take, |max| Ok(source.take(max)));
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
                let read = drive(&mut reader, admit, take, |max| Ok(source.take(max)));
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
            let read = drive(reader, |_| true, |len| pool.alloc(len).ok(), source)?;
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
            // No call shows the heap that the reader keeps.
            assert!(reader.chunks.is_empty());
            assert_eq!(*reader.buffer(), message[..10]);
            assert_eq!(reader.buffer().capacity(), 1_024);
            let buffer = reader.buffer().as_ptr();
            let read = read_views(&mut reader, &pool, &batch, &mut at, 2 + 11);
            assert_eq!(read, Ok(Poll::Pending));
            assert!(batch.is_unique());
            assert_eq!(*reader.buffer(), message[..11]);
            assert_eq!(reader.buffer().as_ptr(), buffer);
            let read = read_views(&mut reader, &pool, &batch, &mut at, batch.len());
            assert_eq!(read, Ok(Poll::Ready(Some(message))));
            assert!(batch.is_unique());
            assert_eq!(reader.buffer().capacity(), 0);
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
            // No call shows the heap that the reader keeps.
            assert!(reader.chunks.is_empty());
            assert_eq!(*reader.buffer(), message);
            drop(held);
            let read = read_views(&mut reader, &pool, &batch, &mut at, batch.len());
            assert_eq!(read, Ok(Poll::Ready(Some(message))));
            assert_eq!(reader.buffer().capacity(), 0);
        }

        /// The step of `reader` once it has read a message of `len` bytes, each byte
        /// in its own chunk.
        fn read_bytewise(reader: &mut Reader, len: usize) -> Result<Step, Error> {
            let batch = Bytes::from(encode(&[(0..=255).cycle().take(len).collect()]));
            let mut at = 0;
            let mut source = |_| {
                let chunk = batch.slice(at..=at);
                at = at.saturating_add(1);
                Ok(Poll::Ready(Some(chunk)))
            };
            assert_eq!(reader.read(&mut source), Ok(Step::Room(len)));
            reader.admit();
            reader.read(&mut source)
        }

        #[test]
        fn a_read_holds_at_most_chunks_max_chunks_then_buffers_them() {
            let pool = pool(1 << 16);
            let mut reader = Reader::new(100);
            assert_eq!(read_bytewise(&mut reader, 64), Ok(Step::Block(64)));
            // Private: no call shows the heap that the reader keeps.
            assert_eq!(reader.held(), (None, 64));
            reader.clear();
            assert_eq!(read_bytewise(&mut reader, 65), Ok(Step::Block(65)));
            assert_eq!(reader.held(), (Some((64, 65)), 1));
            let message: Vec<u8> = (0..=255).cycle().take(65).collect();
            let read = reader.fill(pool.alloc(65).ok());
            assert_eq!(read.map(|block| block.to_vec()), Poll::Ready(message));
            assert_eq!(reader.held(), (None, 0));
        }

        #[test]
        fn a_fill_with_no_block_keeps_no_chunk_and_gives_the_step_again() {
            let pool = pool(1 << 16);
            let mut source = Source::new(encode(&[vec![3; 100]]), 10);
            let mut reader = Reader::new(100);
            let read = reader.read(|max| Ok(source.take(max)));
            assert_eq!(read, Ok(Step::Room(100)));
            reader.admit();
            let read = reader.read(|max| Ok(source.take(max)));
            assert_eq!(read, Ok(Step::Block(100)));
            // Private: no call shows the heap that the reader keeps.
            assert_eq!(reader.held(), (None, 10));
            assert!(reader.fill(None).is_pending());
            assert_eq!(reader.held(), (Some((100, 100)), 0));
            let read = reader.read(|_| panic!("a whole message asks for no bytes"));
            assert_eq!(read, Ok(Step::Block(100)));
            let read = reader.fill(pool.alloc(100).ok());
            assert_eq!(read.map(|block| block.to_vec()), Poll::Ready(vec![3; 100]));
            assert_eq!(reader.read(|max| Ok(source.take(max))), Ok(Step::Ended));
        }

        #[test]
        #[should_panic(expected = "a reader admits only a message that waits for room")]
        fn when_no_message_waits_for_room_admit_panics() {
            let mut reader = Reader::new(16);
            reader.admit();
        }

        #[test]
        #[should_panic(expected = "a reader admits only a message that waits for room")]
        fn when_a_message_is_admitted_admit_panics() {
            let mut source = Source::new(encode(&[vec![1; 4]]), 64);
            let mut reader = Reader::new(16);
            let read = reader.read(|max| Ok(source.take(max)));
            assert_eq!(read, Ok(Step::Room(4)));
            reader.admit();
            reader.admit();
        }

        #[test]
        #[should_panic(expected = "a reader fills only a whole message")]
        fn when_a_message_is_not_whole_fill_panics() {
            let mut source = Source::new(encode(&[vec![1; 4]]), 2);
            source.open = true;
            source.bytes.truncate(3);
            let mut reader = Reader::new(16);
            assert_eq!(reader.read(|max| Ok(source.take(max))), Ok(Step::Room(4)));
            reader.admit();
            assert_eq!(reader.read(|max| Ok(source.take(max))), Ok(Step::Pending));
            drop(reader.fill(None));
        }

        #[test]
        #[should_panic(expected = "a reader fills only a whole message")]
        fn when_no_message_is_admitted_fill_panics() {
            let mut reader = Reader::new(16);
            drop(reader.fill(None));
        }

        #[test]
        #[should_panic(expected = "the block is the message's length")]
        fn when_the_block_is_not_the_message_length_fill_panics() {
            let pool = pool(1 << 16);
            let mut source = Source::new(encode(&[vec![1; 4]]), 64);
            let mut reader = Reader::new(16);
            assert_eq!(reader.read(|max| Ok(source.take(max))), Ok(Step::Room(4)));
            reader.admit();
            assert_eq!(reader.read(|max| Ok(source.take(max))), Ok(Step::Block(4)));
            drop(reader.fill(pool.alloc(5).ok()));
        }

        #[test]
        #[should_panic(expected = "the source gives at least one byte")]
        fn when_source_gives_no_bytes_it_panics() {
            let pool = pool(1 << 16);
            let mut reader = Reader::new(16);
            drop(drive(
                &mut reader,
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
        fn after_the_stream_ends_inside_a_message_each_read_fails() {
            let mut source = Source::new(vec![0x05, 1, 2], 64);
            let mut reader = Reader::new(16);
            let ended = Err(Error::Broken {
                reason: "the stream ended inside a message".to_owned(),
            });
            assert_eq!(reader.read(|max| Ok(source.take(max))), Ok(Step::Room(5)));
            reader.admit();
            assert_eq!(reader.read(|max| Ok(source.take(max))), ended);
            assert_eq!(reader.read(|max| Ok(source.take(max))), ended);
            // Private: no call shows the heap that the reader keeps.
            assert_eq!(reader.held(), (None, 0));
        }

        #[test]
        fn after_the_stream_ends_inside_a_prefix_each_read_fails() {
            let mut source = Source::new(vec![0x40], 64);
            let mut reader = Reader::new(16);
            let ended = Err(Error::Broken {
                reason: "the stream ended inside a message".to_owned(),
            });
            assert_eq!(reader.read(|max| Ok(source.take(max))), ended);
            assert_eq!(reader.read(|max| Ok(source.take(max))), ended);
        }

        #[test]
        fn after_the_stream_ends_inside_a_buffered_message_it_holds_no_bytes() {
            let mut source = Source::new(vec![0x05, 1], 64);
            source.open = true;
            let mut reader = Reader::new(16);
            let ended = Err(Error::Broken {
                reason: "the stream ended inside a message".to_owned(),
            });
            assert_eq!(reader.read(|max| Ok(source.take(max))), Ok(Step::Room(5)));
            reader.admit();
            assert_eq!(reader.read(|max| Ok(source.take(max))), Ok(Step::Pending));
            // Private: no call shows the heap that the reader keeps.
            assert_eq!(reader.held(), (Some((1, 5)), 0));
            source.open = false;
            assert_eq!(reader.read(|max| Ok(source.take(max))), ended);
            assert_eq!(reader.held(), (None, 0));
            assert_eq!(
                reader.read(|_| panic!("a cut message asks for no bytes")),
                ended
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

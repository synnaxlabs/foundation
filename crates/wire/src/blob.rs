//! A blob session: one blob stream from a requester to the server that holds the store.
//! After the header, the requester sends [`get`] and [`Put`] messages, and the server
//! sends [`Reply`] messages. The server answers requests in order: the digests of a
//! get in message order, and a put after its body. Each reply names its digest, and a
//! reply that does not answer the oldest open request breaks the session. The caller
//! keeps the open requests and checks this; the decoders do not.
//!
//! A body follows a [`Put`] and a [`Reply::Chunk`]: the bytes of the chunk, as stream
//! messages back to back, with no prefix, each at most the peer's `message_bytes_max`.
//! No message of a body is empty, and the body starts a new message. A chunk of 0
//! bytes has no body message.
//!
//! [`Server`] decodes the messages from the requester, and [`Requester`] those from the
//! server. Each takes from its caller the most bytes a chunk may have, refuses a longer
//! chunk at its head, and checks that a body holds exactly the bytes of its head. The
//! receiver checks the digest over the whole chunk.
//!
//! A message that does not decode, comes from the wrong side, or breaks a rule of this
//! module stops the stream with the code of its error ([`Error::code`]).
//!
//! Fields are little-endian.
//!
//! - [`get`]: kind 1, then one or more digests (32 bytes each). The length gives the
//!   count.
//! - [`Put`]: kind 2, the digest, and `len` (`u32`). The body follows.
//! - [`Reply`]: kind 1 (chunk): the digest and `len` (`u32`), and the body follows.
//!   Kind 2 (absent) and kind 3 (stored): the digest.

use std::fmt;

use types::digest::Digest;

use crate::common::{Fields, Writer};
use crate::header;

const GET: u8 = 1;
const PUT: u8 = 2;

const CHUNK: u8 = 1;
const ABSENT: u8 = 2;
const STORED: u8 = 3;

/// The bytes of a digest.
const DIGEST_LEN: usize = 32;

/// The bytes of a head with a digest and no length: kind and digest.
const DIGEST_HEAD_LEN: usize = 1 + DIGEST_LEN;

/// The bytes of a head with a digest and a length: kind, digest, and `u32`.
const CHUNK_HEAD_LEN: usize = DIGEST_HEAD_LEN + 4;

/// Stop code: the bytes of a chunk do not hash to its digest.
pub const MISMATCH: u32 = 16;
/// Stop code: a chunk is longer than the largest block of this node.
pub const TOO_LARGE: u32 = 17;
/// Stop code: a put would leave the disk under the free floor of the store.
pub const FULL: u32 = 18;

/// A get from the requester: the digests it wants. The server answers each, in message
/// order, with a [`Reply::Chunk`] or a [`Reply::Absent`]. A get is one message, so a requester with
/// more digests than one message holds sends more gets.
pub mod get {
    use std::slice;

    use types::digest::Digest;

    use super::{DIGEST_LEN, Error, GET, Writer};

    /// The bytes of a get of `digests` digests. A get has at least 1.
    #[must_use]
    pub fn encoded_len(digests: usize) -> usize {
        digests.saturating_mul(DIGEST_LEN).saturating_add(1)
    }

    /// Writes a get of `digests` into `out`.
    ///
    /// # Panics
    ///
    /// When `digests` is empty, or `out` is not [`encoded_len`] bytes.
    pub fn encode(digests: &[Digest], out: &mut [u8]) {
        assert!(!digests.is_empty(), "a get names at least one digest");
        let mut out = Writer::new(out, encoded_len(digests.len()));
        out.put(&[GET]);
        for digest in digests {
            out.put(&digest.0);
        }
    }

    /// The digests of a get, in message order.
    #[derive(Clone, Debug)]
    pub struct Digests<'m>(slice::Iter<'m, [u8; DIGEST_LEN]>);

    impl Iterator for Digests<'_> {
        type Item = Digest;

        fn next(&mut self) -> Option<Digest> {
            self.0.next().copied().map(Digest)
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            self.0.size_hint()
        }
    }

    impl ExactSizeIterator for Digests<'_> {}

    /// The digests in `rest`, the bytes after the kind of a message of `len` bytes.
    pub(super) fn decode(rest: &[u8], len: usize) -> Result<Digests<'_>, Error> {
        let (digests, tail) = rest.as_chunks::<DIGEST_LEN>();
        if digests.is_empty() || !tail.is_empty() {
            return Err(Error::Length { len });
        }
        Ok(Digests(digests.iter()))
    }
}

/// A chunk from the requester, which the server stores. Its body follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Put {
    /// The digest of the body.
    pub digest: Digest,
    /// The bytes of the body.
    pub len: u32,
}

impl Put {
    /// The bytes of an encoded put.
    pub const LEN: usize = CHUNK_HEAD_LEN;

    /// Writes the put into `out`.
    ///
    /// # Panics
    ///
    /// When `out` is not [`Put::LEN`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        let mut out = Writer::new(out, Self::LEN);
        out.put(&[PUT]);
        out.put(&self.digest.0);
        out.put(&self.len.to_le_bytes());
    }

    /// The put whose fields are `rest`, the bytes after the kind of a message of
    /// `len` bytes.
    fn fields(rest: &[u8], len: usize) -> Result<Self, Error> {
        let mut fields = Fields::new(rest, Error::Length { len });
        let digest = Digest(fields.take()?);
        let len = u32::from_le_bytes(fields.take()?);
        fields.end()?;
        Ok(Self { digest, len })
    }
}

/// A message from the server to the requester.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// A chunk that the requester asked for. Its body follows.
    Chunk {
        /// The digest of the body.
        digest: Digest,
        /// The bytes of the body.
        len: u32,
    },
    /// The server does not have the chunk.
    Absent {
        /// The digest of the get.
        digest: Digest,
    },
    /// The chunk of a put is durable on the server.
    Stored {
        /// The digest of the put.
        digest: Digest,
    },
}

impl Reply {
    /// The bytes of the encoded reply.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Chunk { .. } => CHUNK_HEAD_LEN,
            Self::Absent { .. } | Self::Stored { .. } => DIGEST_HEAD_LEN,
        }
    }

    /// Writes the reply into `out`.
    ///
    /// # Panics
    ///
    /// When `out` is not [`Reply::encoded_len`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        let mut out = Writer::new(out, self.encoded_len());
        match *self {
            Self::Chunk { digest, len } => {
                out.put(&[CHUNK]);
                out.put(&digest.0);
                out.put(&len.to_le_bytes());
            }
            Self::Absent { digest } => {
                out.put(&[ABSENT]);
                out.put(&digest.0);
            }
            Self::Stored { digest } => {
                out.put(&[STORED]);
                out.put(&digest.0);
            }
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let (&kind, rest) = bytes.split_first().ok_or(Error::Empty)?;
        let mut fields = Fields::new(rest, Error::Length { len: bytes.len() });
        let reply = match kind {
            CHUNK => Self::Chunk {
                digest: Digest(fields.take()?),
                len: u32::from_le_bytes(fields.take()?),
            },
            ABSENT => Self::Absent {
                digest: Digest(fields.take()?),
            },
            STORED => Self::Stored {
                digest: Digest(fields.take()?),
            },
            kind => return Err(Error::Kind { kind }),
        };
        fields.end()?;
        Ok(reply)
    }
}

/// The decoder at the server: it takes each message from the requester, in order, and
/// checks the chunk limit and the body of each put.
#[derive(Debug)]
pub struct Server {
    transit: Transit,
}

/// A message from the requester, decoded.
#[derive(Clone, Debug)]
pub enum FromRequester<'m> {
    /// The digests the requester wants.
    Get(get::Digests<'m>),
    /// A chunk to store. Its body follows, unless `len` is 0.
    Put(Put),
    /// One message of the body of the last put. It starts where [`Server::body`] was
    /// before the call.
    Body {
        /// The bytes of the message.
        bytes: &'m [u8],
        /// The body ends with this message.
        last: bool,
    },
}

impl Server {
    /// A decoder that takes a chunk of at most `chunk_bytes_max` bytes.
    #[must_use]
    pub fn new(chunk_bytes_max: usize) -> Self {
        Self {
            transit: Transit::new(chunk_bytes_max),
        }
    }

    /// Decodes the next message from the requester.
    ///
    /// # Errors
    ///
    /// The [`Error`] of a message that does not decode, or that breaks the session:
    /// [`Error::TooLarge`] for a put longer than the limit, and [`Error::Body`] for a
    /// message longer than the rest of the body. A message of a body has no kind, so a
    /// message where the body continues is read as one. The caller then stops the
    /// session with [`Error::code`].
    pub fn decode<'m>(
        &mut self,
        message: &'m [u8],
    ) -> Result<FromRequester<'m>, Error> {
        if self.transit.in_body() {
            let (bytes, last) = self.transit.part(message)?;
            return Ok(FromRequester::Body { bytes, last });
        }
        let (&kind, rest) = message.split_first().ok_or(Error::Empty)?;
        match kind {
            GET => Ok(FromRequester::Get(get::decode(rest, message.len())?)),
            PUT => {
                let put = Put::fields(rest, message.len())?;
                self.transit.start(put.len)?;
                Ok(FromRequester::Put(put))
            }
            kind => Err(Error::Kind { kind }),
        }
    }

    /// Where in the chunk the next message starts, when the next message is body
    /// bytes. Read it before [`Server::decode`] takes that message.
    #[must_use]
    pub fn body(&self) -> Option<usize> {
        self.transit.body()
    }
}

/// The decoder at the requester: it takes each message from the server, in order, and
/// checks the chunk limit and the body of each chunk.
#[derive(Debug)]
pub struct Requester {
    transit: Transit,
}

/// A message from the server, decoded.
#[derive(Clone, Debug)]
pub enum FromServer<'m> {
    /// A reply. The body of a chunk follows, unless its `len` is 0.
    Reply(Reply),
    /// One message of the body of the last chunk. It starts where [`Requester::body`]
    /// was before the call.
    Body {
        /// The bytes of the message.
        bytes: &'m [u8],
        /// The body ends with this message.
        last: bool,
    },
}

impl Requester {
    /// A decoder that takes a chunk of at most `chunk_bytes_max` bytes.
    #[must_use]
    pub fn new(chunk_bytes_max: usize) -> Self {
        Self {
            transit: Transit::new(chunk_bytes_max),
        }
    }

    /// Decodes the next message from the server.
    ///
    /// # Errors
    ///
    /// The [`Error`] of a message that does not decode, or that breaks the session:
    /// [`Error::TooLarge`] for a chunk longer than the limit, and [`Error::Body`] for
    /// a message longer than the rest of the body. A message of a body has no kind, so
    /// a message where the body continues is read as one. The caller then stops the
    /// session with [`Error::code`].
    pub fn decode<'m>(&mut self, message: &'m [u8]) -> Result<FromServer<'m>, Error> {
        if self.transit.in_body() {
            let (bytes, last) = self.transit.part(message)?;
            return Ok(FromServer::Body { bytes, last });
        }
        let reply = Reply::decode(message)?;
        if let Reply::Chunk { len, .. } = reply {
            self.transit.start(len)?;
        }
        Ok(FromServer::Reply(reply))
    }

    /// Where in the chunk the next message starts, when the next message is body
    /// bytes. Read it before [`Requester::decode`] takes that message.
    #[must_use]
    pub fn body(&self) -> Option<usize> {
        self.transit.body()
    }
}

/// The chunk limit and the body in transit, which both decoders keep.
#[derive(Debug)]
struct Transit {
    chunk_bytes_max: usize,
    len: usize,
    remain: usize,
}

impl Transit {
    fn new(chunk_bytes_max: usize) -> Self {
        Self {
            chunk_bytes_max,
            len: 0,
            remain: 0,
        }
    }

    /// Starts the body of a chunk of `len` bytes, after its head.
    fn start(&mut self, len: u32) -> Result<(), Error> {
        let max = self.chunk_bytes_max;
        let fits = usize::try_from(len).ok().filter(|&len| len <= max);
        self.len = fits.ok_or(Error::TooLarge { len, max })?;
        self.remain = self.len;
        Ok(())
    }

    fn in_body(&self) -> bool {
        self.remain > 0
    }

    /// Takes `message` as the next part of the body.
    fn part<'m>(&mut self, message: &'m [u8]) -> Result<(&'m [u8], bool), Error> {
        let len = message.len();
        if len == 0 {
            return Err(Error::Empty);
        }
        let remain = self.remain;
        self.remain = remain.checked_sub(len).ok_or(Error::Body { len, remain })?;
        Ok((message, self.remain == 0))
    }

    fn body(&self) -> Option<usize> {
        self.in_body().then(|| {
            self.len
                .checked_sub(self.remain)
                .expect("invariant: the rest of the body is no longer than the body")
        })
    }
}

/// A blob message that is not valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The message has no bytes.
    Empty,
    /// The first byte names no message.
    Kind {
        /// The first byte.
        kind: u8,
    },
    /// No message of its kind has this length: a get whose rest is not one or more
    /// whole digests, or another message of a length its kind does not have.
    Length {
        /// The bytes of the message.
        len: usize,
    },
    /// A head names a chunk longer than the decoder takes.
    TooLarge {
        /// The bytes of the chunk.
        len: u32,
        /// The most bytes the decoder takes.
        max: usize,
    },
    /// A message of a body has more bytes than remain in the body.
    Body {
        /// The bytes of the message.
        len: usize,
        /// The bytes that remain in the body.
        remain: usize,
    },
}

impl Error {
    /// The stop code that ends the session for this error: [`TOO_LARGE`] for
    /// [`Error::TooLarge`], and [`MALFORMED`](crate::header::MALFORMED) for each
    /// other.
    #[must_use]
    pub fn code(&self) -> u32 {
        match self {
            Self::TooLarge { .. } => TOO_LARGE,
            Self::Empty
            | Self::Kind { .. }
            | Self::Length { .. }
            | Self::Body { .. } => header::MALFORMED,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the blob message is empty"),
            Self::Kind { kind } => write!(
                f,
                "the blob message has kind {kind}, which this node does not know"
            ),
            Self::Length { len } => write!(
                f,
                "the blob message has {len} bytes, which no message of its kind has"
            ),
            Self::TooLarge { len, max } => write!(
                f,
                "the chunk has {len} bytes, and this node takes at most {max}"
            ),
            Self::Body { len, remain } => write!(
                f,
                "the body message has {len} bytes, and {remain} remain in the body"
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use std::iter;

    use proptest::prelude::*;

    use super::*;

    /// A chunk limit that no test reaches.
    const MAX: usize = 1 << 20;

    fn digest(byte: u8) -> Digest {
        Digest([byte; DIGEST_LEN])
    }

    fn put(byte: u8, len: u32) -> Put {
        Put {
            digest: digest(byte),
            len,
        }
    }

    fn chunk(byte: u8, len: u32) -> Reply {
        Reply::Chunk {
            digest: digest(byte),
            len,
        }
    }

    fn encode_get(digests: &[Digest]) -> Vec<u8> {
        let mut out = vec![0xaa; get::encoded_len(digests.len())];
        get::encode(digests, &mut out);
        out
    }

    fn encode_put(put: Put) -> Vec<u8> {
        let mut out = vec![0xaa; Put::LEN];
        put.encode(&mut out);
        out
    }

    fn encode_reply(reply: Reply) -> Vec<u8> {
        let mut out = vec![0xaa; reply.encoded_len()];
        reply.encode(&mut out);
        out
    }

    /// `kind`, then `len - 1` zeros.
    fn zeros(kind: u8, len: usize) -> Vec<u8> {
        iter::once(kind).chain(iter::repeat(0)).take(len).collect()
    }

    /// A message of either side, with the bytes it borrows copied out.
    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Get(Vec<Digest>),
        Put(Put),
        Reply(Reply),
        Body(Vec<u8>, bool),
    }

    fn from_requester(server: &mut Server, message: &[u8]) -> Result<Event, Error> {
        server.decode(message).map(|event| match event {
            FromRequester::Get(digests) => Event::Get(digests.collect()),
            FromRequester::Put(put) => Event::Put(put),
            FromRequester::Body { bytes, last } => Event::Body(bytes.to_vec(), last),
        })
    }

    fn from_server(requester: &mut Requester, message: &[u8]) -> Result<Event, Error> {
        requester.decode(message).map(|event| match event {
            FromServer::Reply(reply) => Event::Reply(reply),
            FromServer::Body { bytes, last } => Event::Body(bytes.to_vec(), last),
        })
    }

    /// The event of `message` at a server between puts.
    fn server(message: &[u8]) -> Result<Event, Error> {
        from_requester(&mut Server::new(MAX), message)
    }

    /// The event of `message` at a requester between chunks.
    fn requester(message: &[u8]) -> Result<Event, Error> {
        from_server(&mut Requester::new(MAX), message)
    }

    /// `decode` gives [`Error::Length`] for a message of `kind` with each length in
    /// `lens`.
    fn check(decode: fn(&[u8]) -> Result<Event, Error>, kind: u8, lens: &[usize]) {
        for &len in lens {
            let bytes = zeros(kind, len);
            assert_eq!(decode(&bytes), Err(Error::Length { len }), "kind {kind}");
        }
    }

    /// `decode` gives [`Error::Kind`] for each kind byte outside `known`, at each
    /// length in `lens`.
    fn check_kinds(
        decode: fn(&[u8]) -> Result<Event, Error>,
        known: &[u8],
        lens: &[usize],
    ) {
        for kind in (0..=u8::MAX).filter(|kind| !known.contains(kind)) {
            for &len in lens {
                let bytes = zeros(kind, len);
                assert_eq!(decode(&bytes), Err(Error::Kind { kind }), "{len}");
            }
        }
    }

    mod get_message {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            let digests = [digest(1), digest(2)];
            let expected: Vec<u8> = [[1].as_slice(), &[1; 32], &[2; 32]].concat();
            assert_eq!(encode_get(&digests), expected);
            assert_eq!(get::encoded_len(2), 65);
        }

        #[test]
        fn decodes_each_digest_in_order() {
            let digests = [digest(1), digest(2), digest(3)];
            assert_eq!(
                server(&encode_get(&digests)),
                Ok(Event::Get(digests.to_vec()))
            );
        }

        #[test]
        fn counts_its_digests() {
            let bytes = encode_get(&[digest(1), digest(2)]);
            let mut server = Server::new(MAX);
            match server.decode(&bytes) {
                Ok(FromRequester::Get(digests)) => assert_eq!(digests.len(), 2),
                other => panic!("the get did not decode: {other:?}"),
            }
        }

        #[test]
        fn refuses_a_get_of_no_digest() {
            assert_eq!(server(&[GET]), Err(Error::Length { len: 1 }));
        }

        #[test]
        fn refuses_a_get_that_splits_a_digest() {
            check(server, GET, &[2, 32, 34, 64, 66]);
        }

        #[test]
        #[should_panic(expected = "a get names at least one digest")]
        fn panics_on_an_encode_of_no_digest() {
            get::encode(&[], &mut [0; 1]);
        }

        #[test]
        #[should_panic(expected = "out has 34 bytes, and the message has 33")]
        fn panics_on_an_out_of_the_wrong_length() {
            get::encode(&[digest(1)], &mut [0; 34]);
        }
    }

    mod put_message {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            let bytes = encode_put(put(7, 0x0102_0304));
            let expected: Vec<u8> = [[2].as_slice(), &[7; 32], &[4, 3, 2, 1]].concat();
            assert_eq!(bytes, expected);
            assert_eq!(bytes.len(), Put::LEN);
        }

        #[test]
        fn decodes_a_put_and_its_body() {
            let mut server = Server::new(MAX);
            assert_eq!(server.body(), None);
            let bytes = encode_put(put(7, 13));
            assert_eq!(
                from_requester(&mut server, &bytes),
                Ok(Event::Put(put(7, 13)))
            );
            assert_eq!(server.body(), Some(0));
            assert_eq!(
                from_requester(&mut server, &[1; 9]),
                Ok(Event::Body(vec![1; 9], false))
            );
            assert_eq!(server.body(), Some(9));
            assert_eq!(
                from_requester(&mut server, &[2; 4]),
                Ok(Event::Body(vec![2; 4], true))
            );
            assert_eq!(server.body(), None);
            let bytes = encode_put(put(8, 1));
            assert_eq!(
                from_requester(&mut server, &bytes),
                Ok(Event::Put(put(8, 1)))
            );
        }

        #[test]
        fn takes_a_body_in_one_message() {
            let mut server = Server::new(MAX);
            server
                .decode(&encode_put(put(7, 3)))
                .expect("the put decodes");
            assert_eq!(
                from_requester(&mut server, &[1, 2, 3]),
                Ok(Event::Body(vec![1, 2, 3], true))
            );
            assert_eq!(server.body(), None);
        }

        #[test]
        fn a_put_of_0_bytes_has_no_body() {
            let mut server = Server::new(MAX);
            assert_eq!(
                from_requester(&mut server, &encode_put(put(7, 0))),
                Ok(Event::Put(put(7, 0)))
            );
            assert_eq!(server.body(), None);
            let get = encode_get(&[digest(1)]);
            assert_eq!(
                from_requester(&mut server, &get),
                Ok(Event::Get(vec![digest(1)]))
            );
        }

        #[test]
        fn takes_a_chunk_at_the_limit_and_refuses_one_over_it() {
            let mut server = Server::new(16);
            assert_eq!(
                from_requester(&mut server, &encode_put(put(7, 16))),
                Ok(Event::Put(put(7, 16)))
            );
            assert_eq!(
                from_requester(&mut server, &[0; 16]),
                Ok(Event::Body(vec![0; 16], true))
            );
            assert_eq!(
                from_requester(&mut server, &encode_put(put(7, 17))),
                Err(Error::TooLarge { len: 17, max: 16 })
            );
            assert_eq!(server.body(), None);
        }

        #[test]
        fn refuses_a_body_message_past_the_rest() {
            let mut server = Server::new(MAX);
            server
                .decode(&encode_put(put(7, 4)))
                .expect("the put decodes");
            server.decode(&[0; 2]).expect("the first part decodes");
            assert_eq!(
                from_requester(&mut server, &[0; 3]),
                Err(Error::Body { len: 3, remain: 2 })
            );
            assert_eq!(server.body(), Some(2));
        }

        #[test]
        fn refuses_an_empty_body_message() {
            let mut server = Server::new(MAX);
            server
                .decode(&encode_put(put(7, 4)))
                .expect("the put decodes");
            assert_eq!(from_requester(&mut server, &[]), Err(Error::Empty));
            assert_eq!(server.body(), Some(0));
        }

        #[test]
        fn reads_a_head_in_a_body_as_body_bytes() {
            let mut server = Server::new(MAX);
            server
                .decode(&encode_put(put(7, 40)))
                .expect("the put decodes");
            let head = encode_put(put(8, 1));
            assert_eq!(
                from_requester(&mut server, &head),
                Ok(Event::Body(head.clone(), false))
            );
        }

        #[test]
        fn refuses_each_wrong_length() {
            check(server, PUT, &[1, 33, 36, 38]);
        }

        #[test]
        #[should_panic(expected = "out has 36 bytes, and the message has 37")]
        fn panics_on_an_out_of_the_wrong_length() {
            put(7, 1).encode(&mut [0; 36]);
        }
    }

    mod requester_messages {
        use super::*;

        #[test]
        fn refuses_an_empty_message() {
            assert_eq!(server(&[]), Err(Error::Empty));
        }

        #[test]
        fn refuses_unknown_kinds_before_the_length() {
            check_kinds(server, &[GET, PUT], &[1, 33, 37]);
        }
    }

    mod reply_message {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            let expected: Vec<u8> = [[1].as_slice(), &[7; 32], &[4, 3, 2, 1]].concat();
            assert_eq!(encode_reply(chunk(7, 0x0102_0304)), expected);
            let absent = Reply::Absent { digest: digest(8) };
            let expected: Vec<u8> = [[2].as_slice(), &[8; 32]].concat();
            assert_eq!(encode_reply(absent), expected);
            let stored = Reply::Stored { digest: digest(9) };
            let expected: Vec<u8> = [[3].as_slice(), &[9; 32]].concat();
            assert_eq!(encode_reply(stored), expected);
        }

        #[test]
        fn decodes_a_chunk_and_its_body() {
            let mut requester = Requester::new(MAX);
            assert_eq!(requester.body(), None);
            let bytes = encode_reply(chunk(7, 13));
            assert_eq!(
                from_server(&mut requester, &bytes),
                Ok(Event::Reply(chunk(7, 13)))
            );
            assert_eq!(requester.body(), Some(0));
            assert_eq!(
                from_server(&mut requester, &[1; 9]),
                Ok(Event::Body(vec![1; 9], false))
            );
            assert_eq!(requester.body(), Some(9));
            assert_eq!(
                from_server(&mut requester, &[2; 4]),
                Ok(Event::Body(vec![2; 4], true))
            );
            assert_eq!(requester.body(), None);
        }

        #[test]
        fn absent_and_stored_have_no_body() {
            let mut requester = Requester::new(MAX);
            for reply in [
                Reply::Absent { digest: digest(1) },
                Reply::Stored { digest: digest(2) },
                chunk(3, 0),
            ] {
                assert_eq!(
                    from_server(&mut requester, &encode_reply(reply)),
                    Ok(Event::Reply(reply))
                );
                assert_eq!(requester.body(), None);
            }
        }

        #[test]
        fn takes_a_chunk_at_the_limit_and_refuses_one_over_it() {
            let mut requester = Requester::new(16);
            assert_eq!(
                from_server(&mut requester, &encode_reply(chunk(7, 16))),
                Ok(Event::Reply(chunk(7, 16)))
            );
            assert_eq!(
                from_server(&mut requester, &[0; 16]),
                Ok(Event::Body(vec![0; 16], true))
            );
            assert_eq!(
                from_server(&mut requester, &encode_reply(chunk(7, 17))),
                Err(Error::TooLarge { len: 17, max: 16 })
            );
            assert_eq!(requester.body(), None);
        }

        #[test]
        fn refuses_a_body_message_past_the_rest() {
            let mut requester = Requester::new(MAX);
            requester
                .decode(&encode_reply(chunk(7, 4)))
                .expect("the chunk decodes");
            requester.decode(&[0; 2]).expect("the first part decodes");
            assert_eq!(
                from_server(&mut requester, &[0; 3]),
                Err(Error::Body { len: 3, remain: 2 })
            );
            assert_eq!(requester.body(), Some(2));
        }

        #[test]
        fn refuses_an_empty_body_message() {
            let mut requester = Requester::new(MAX);
            requester
                .decode(&encode_reply(chunk(7, 4)))
                .expect("the chunk decodes");
            assert_eq!(from_server(&mut requester, &[]), Err(Error::Empty));
            assert_eq!(requester.body(), Some(0));
        }

        #[test]
        fn refuses_an_empty_message() {
            assert_eq!(requester(&[]), Err(Error::Empty));
        }

        #[test]
        fn refuses_unknown_kinds_before_the_length() {
            check_kinds(requester, &[CHUNK, ABSENT, STORED], &[1, 33, 37]);
        }

        #[test]
        fn refuses_each_wrong_length() {
            check(requester, CHUNK, &[1, 33, 36, 38]);
            check(requester, ABSENT, &[1, 32, 34, 37]);
            check(requester, STORED, &[1, 32, 34, 37]);
        }

        #[test]
        #[should_panic(expected = "out has 37 bytes, and the message has 33")]
        fn panics_on_an_out_of_the_wrong_length() {
            Reply::Absent { digest: digest(1) }.encode(&mut [0; 37]);
        }
    }

    #[test]
    fn pins_the_stop_codes() {
        assert_eq!((MISMATCH, TOO_LARGE, FULL), (16, 17, 18));
    }

    #[test]
    fn gives_the_stop_code_of_each_error() {
        let errors = [
            Error::Empty,
            Error::Kind { kind: 9 },
            Error::Length { len: 4 },
            Error::TooLarge { len: 17, max: 16 },
            Error::Body {
                len: 11,
                remain: 10,
            },
        ];
        for error in errors {
            let code = match error {
                Error::TooLarge { .. } => 17,
                Error::Empty
                | Error::Kind { .. }
                | Error::Length { .. }
                | Error::Body { .. } => 2,
            };
            assert_eq!(error.code(), code, "{error}");
        }
    }

    #[test]
    fn names_each_error() {
        let cases = [
            (Error::Empty, "the blob message is empty"),
            (
                Error::Kind { kind: 9 },
                "the blob message has kind 9, which this node does not know",
            ),
            (
                Error::Length { len: 4 },
                "the blob message has 4 bytes, which no message of its kind has",
            ),
            (
                Error::TooLarge { len: 17, max: 16 },
                "the chunk has 17 bytes, and this node takes at most 16",
            ),
            (
                Error::Body {
                    len: 11,
                    remain: 10,
                },
                "the body message has 11 bytes, and 10 remain in the body",
            ),
        ];
        for (error, text) in cases {
            assert_eq!(error.to_string(), text);
        }
    }

    fn any_digest() -> impl Strategy<Value = Digest> {
        any::<[u8; DIGEST_LEN]>().prop_map(Digest)
    }

    fn reply() -> impl Strategy<Value = Reply> {
        prop_oneof![
            (any_digest(), any::<u32>())
                .prop_map(|(digest, len)| Reply::Chunk { digest, len }),
            any_digest().prop_map(|digest| Reply::Absent { digest }),
            any_digest().prop_map(|digest| Reply::Stored { digest }),
        ]
    }

    /// A kind byte from 0 to 4, then random bytes, most of them a length that a
    /// message of some kind has.
    fn bytes() -> impl Strategy<Value = Vec<u8>> {
        let rest = prop_oneof![Just(32), Just(36), Just(64), 0..70_usize];
        (0..5_u8, rest).prop_flat_map(|(kind, rest)| {
            proptest::collection::vec(any::<u8>(), rest)
                .prop_map(move |rest| [[kind].as_slice(), &rest].concat())
        })
    }

    proptest! {
        #[test]
        fn round_trips_a_get(digests in proptest::collection::vec(any_digest(), 1..8)) {
            prop_assert_eq!(server(&encode_get(&digests)), Ok(Event::Get(digests)));
        }

        #[test]
        fn round_trips_a_put(digest in any_digest(), len in any::<u32>()) {
            let put = Put { digest, len };
            let mut server = Server::new(usize::MAX);
            prop_assert_eq!(
                from_requester(&mut server, &encode_put(put)),
                Ok(Event::Put(put))
            );
        }

        #[test]
        fn round_trips_a_reply(reply in reply()) {
            let mut requester = Requester::new(usize::MAX);
            prop_assert_eq!(
                from_server(&mut requester, &encode_reply(reply)),
                Ok(Event::Reply(reply))
            );
        }

        #[test]
        fn decodes_only_the_messages_it_encodes(bytes in bytes()) {
            match Server::new(usize::MAX).decode(&bytes) {
                Ok(FromRequester::Get(digests)) => {
                    let digests: Vec<_> = digests.collect();
                    prop_assert_eq!(&encode_get(&digests), &bytes);
                }
                Ok(FromRequester::Put(put)) => {
                    prop_assert_eq!(&encode_put(put), &bytes);
                }
                Ok(FromRequester::Body { .. }) => {
                    prop_assert!(false, "a body with no put");
                }
                Err(_) => {}
            }
            match Requester::new(usize::MAX).decode(&bytes) {
                Ok(FromServer::Reply(reply)) => {
                    prop_assert_eq!(&encode_reply(reply), &bytes);
                }
                Ok(FromServer::Body { .. }) => {
                    prop_assert!(false, "a body with no chunk");
                }
                Err(_) => {}
            }
        }
    }
}

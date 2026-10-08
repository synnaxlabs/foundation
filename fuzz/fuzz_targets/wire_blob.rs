//! `wire::blob::Server` and `wire::blob::Requester` never panic, each head encodes to
//! its message, each body message is where `body` says and no longer than the rest of
//! the body, each refusal is the one the state gives, and each valid message made from
//! the input decodes to itself.
//!
//! Input: the chunk limit (one byte), then the stream messages (`fuzz::messages`),
//! which each side reads.

#![no_main]

use libfuzzer_sys::{
    arbitrary::{self, Unstructured},
    fuzz_target,
};
use types::digest::Digest;
use wire::blob::{
    Error, FromRequester, FromServer, Put, Reply, Requester, Server, get,
};

/// The most digests of a written get.
const GET_MAX: usize = 4;

/// The bytes of a digest.
const DIGEST_LEN: usize = 32;

/// The bytes of a chunk length.
const LEN_LEN: usize = 4;

/// The rest of a body, kept apart from the decoder.
#[derive(Clone, Copy, Debug)]
struct Body {
    len: usize,
    remain: usize,
}

impl Body {
    /// The body of a chunk of `len` bytes whose head a decoder with `max` took. `None`
    /// when the chunk has no body message.
    ///
    /// # Panics
    ///
    /// When the chunk is over the limit.
    fn start(len: u32, max: usize) -> Option<Self> {
        let len = usize::try_from(len)
            .ok()
            .filter(|&len| len <= max)
            .expect("a head over the limit decoded");
        (len > 0).then_some(Self { len, remain: len })
    }

    /// Where in the chunk the next body message starts.
    fn at(self) -> usize {
        self.len - self.remain
    }

    /// The rest of the body after a message of `bytes` bytes that the decoder took and
    /// called `last`. `None` when the body ended.
    ///
    /// # Panics
    ///
    /// When the message is empty or longer than the rest, or when `last` is not where
    /// the body ends.
    fn take(self, bytes: usize, last: bool) -> Option<Self> {
        assert!(bytes > 0, "a body message has no byte");
        let remain = self
            .remain
            .checked_sub(bytes)
            .expect("a body message is longer than the rest of the body");
        assert_eq!(last, remain == 0, "the body ends at another message");
        (!last).then_some(Self { remain, ..self })
    }

    /// Whether `error` is the refusal of `message` where the body continues.
    fn refused(self, message: &[u8], error: Error) -> bool {
        let (len, remain) = (message.len(), self.remain);
        if len == 0 {
            error == Error::Empty
        } else {
            len > remain && error == Error::Body { len, remain }
        }
    }
}

/// What a decoder gave for a message, the same for both sides.
#[derive(Debug)]
enum Event<'m> {
    /// A head, encoded again, and the length of its chunk. `None` when no body follows.
    Head(Vec<u8>, Option<u32>),
    /// A body message.
    Body(&'m [u8], bool),
}

/// One side of a blob stream.
trait Side {
    fn decode<'m>(&mut self, message: &'m [u8]) -> Result<Event<'m>, Error>;

    fn body(&self) -> Option<usize>;

    /// The error that the side gives for `message` where it takes a head, or `None`
    /// when the message is a head it takes.
    fn refusal(message: &[u8], max: usize) -> Option<Error>;
}

impl Side for Server {
    fn decode<'m>(&mut self, message: &'m [u8]) -> Result<Event<'m>, Error> {
        Ok(match Server::decode(self, message)? {
            FromRequester::Get(digests) => {
                let digests: Vec<_> = digests.collect();
                let mut out = vec![0; get::encoded_len(digests.len())];
                get::encode(&digests, &mut out);
                Event::Head(out, None)
            }
            FromRequester::Put(put) => {
                let mut out = vec![0; Put::LEN];
                put.encode(&mut out);
                Event::Head(out, Some(put.len))
            }
            FromRequester::Body { bytes, last } => Event::Body(bytes, last),
        })
    }

    fn body(&self) -> Option<usize> {
        Server::body(self)
    }

    fn refusal(message: &[u8], max: usize) -> Option<Error> {
        let len = message.len();
        let Some((&kind, rest)) = message.split_first() else {
            return Some(Error::Empty);
        };
        let kinds = kinds();
        if kind == kinds.get {
            (rest.is_empty() || !rest.len().is_multiple_of(DIGEST_LEN))
                .then_some(Error::Length { len })
        } else if kind == kinds.put {
            chunk_refusal(rest, max)
        } else {
            Some(Error::Kind { kind })
        }
    }
}

impl Side for Requester {
    fn decode<'m>(&mut self, message: &'m [u8]) -> Result<Event<'m>, Error> {
        Ok(match Requester::decode(self, message)? {
            FromServer::Reply(reply) => {
                let mut out = vec![0; reply.encoded_len()];
                reply.encode(&mut out);
                let len = match reply {
                    Reply::Chunk { len, .. } => Some(len),
                    Reply::Absent { .. } | Reply::Stored { .. } => None,
                };
                Event::Head(out, len)
            }
            FromServer::Body { bytes, last } => Event::Body(bytes, last),
        })
    }

    fn body(&self) -> Option<usize> {
        Requester::body(self)
    }

    fn refusal(message: &[u8], max: usize) -> Option<Error> {
        let len = message.len();
        let Some((&kind, rest)) = message.split_first() else {
            return Some(Error::Empty);
        };
        let kinds = kinds();
        if kind == kinds.chunk {
            chunk_refusal(rest, max)
        } else if kind == kinds.absent || kind == kinds.stored {
            (rest.len() != DIGEST_LEN).then_some(Error::Length { len })
        } else {
            Some(Error::Kind { kind })
        }
    }
}

/// The error for the fields `rest` of a head with a body (a digest and a length), or
/// `None` when a decoder with `max` takes it.
fn chunk_refusal(rest: &[u8], max: usize) -> Option<Error> {
    if rest.len() != DIGEST_LEN + LEN_LEN {
        return Some(Error::Length {
            len: rest.len() + 1,
        });
    }
    let len = rest[DIGEST_LEN..]
        .try_into()
        .expect("the length has four bytes");
    let len = u32::from_le_bytes(len);
    let fits = usize::try_from(len).is_ok_and(|len| len <= max);
    (!fits).then_some(Error::TooLarge { len, max })
}

/// The kind bytes that the encoders write.
#[derive(Clone, Copy, Debug)]
struct Kinds {
    get: u8,
    put: u8,
    chunk: u8,
    absent: u8,
    stored: u8,
}

fn kinds() -> Kinds {
    let digest = Digest([0; DIGEST_LEN]);
    let mut get_out = vec![0; get::encoded_len(1)];
    get::encode(&[digest], &mut get_out);
    let mut put_out = [0; Put::LEN];
    Put { digest, len: 0 }.encode(&mut put_out);
    Kinds {
        get: get_out[0],
        put: put_out[0],
        chunk: reply_kind(Reply::Chunk { digest, len: 0 }),
        absent: reply_kind(Reply::Absent { digest }),
        stored: reply_kind(Reply::Stored { digest }),
    }
}

/// The kind byte that the encoder writes for `reply`.
fn reply_kind(reply: Reply) -> u8 {
    let mut out = vec![0; reply.encoded_len()];
    reply.encode(&mut out);
    out[0]
}

/// Each event that `side` gives for the messages in `bytes` must encode to its message
/// and be where the body says, and each refusal must be the one the state gives.
fn read<S: Side + std::fmt::Debug>(mut side: S, max: usize, bytes: &[u8]) {
    let mut body: Option<Body> = None;
    for message in fuzz::messages(bytes) {
        body = match (body, side.decode(message)) {
            (None, Ok(Event::Head(out, len))) => {
                assert_eq!(out, message, "the head changed");
                len.and_then(|len| Body::start(len, max))
            }
            (Some(body), Ok(Event::Body(bytes, last))) => {
                assert_eq!(bytes, message, "the body message changed");
                body.take(bytes.len(), last)
            }
            (body, Err(error)) => {
                let refused = match body {
                    None => S::refusal(message, max) == Some(error),
                    Some(body) => body.refused(message, error),
                };
                assert!(
                    refused,
                    "{error:?} is not the refusal of {message:?} for {body:?}"
                );
                body
            }
            (body, Ok(event)) => panic!("{event:?} came, not {body:?}"),
        };
        assert_eq!(side.body(), body.map(Body::at), "the body is elsewhere");
    }
}

/// Each valid message made from `input` must decode to itself. A get of no digest is
/// not valid, so an input that ends early writes a get of one.
fn write(input: &mut Unstructured) -> arbitrary::Result<()> {
    let count = input.int_in_range(1..=GET_MAX)?;
    let digests = (0..count)
        .map(|_| input.arbitrary().map(Digest))
        .collect::<arbitrary::Result<Vec<_>>>()?;
    let mut out = vec![0; get::encoded_len(digests.len())];
    get::encode(&digests, &mut out);
    match Server::new(0).decode(&out) {
        Ok(FromRequester::Get(decoded)) => {
            assert_eq!(decoded.collect::<Vec<_>>(), digests, "the get changed");
        }
        other => panic!("a get did not read back: {other:?}"),
    }

    let digest = Digest(input.arbitrary()?);
    let len = input.arbitrary()?;
    let put = Put { digest, len };
    let mut out = [0; Put::LEN];
    put.encode(&mut out);
    match Server::new(usize::MAX).decode(&out) {
        Ok(FromRequester::Put(decoded)) => assert_eq!(decoded, put, "the put changed"),
        other => panic!("a put did not read back: {other:?}"),
    }

    for reply in [
        Reply::Chunk { digest, len },
        Reply::Absent { digest },
        Reply::Stored { digest },
    ] {
        let mut out = vec![0; reply.encoded_len()];
        reply.encode(&mut out);
        match Requester::new(usize::MAX).decode(&out) {
            Ok(FromServer::Reply(decoded)) => {
                assert_eq!(decoded, reply, "the reply changed");
            }
            other => panic!("a reply did not read back: {other:?}"),
        }
    }
    Ok(())
}

fuzz_target!(|bytes: &[u8]| {
    if let Some((&max, messages)) = bytes.split_first() {
        let max = usize::from(max);
        read(Server::new(max), max, messages);
        read(Requester::new(max), max, messages);
    }
    write(&mut Unstructured::new(bytes)).expect("an input that ends gives zeros");
});

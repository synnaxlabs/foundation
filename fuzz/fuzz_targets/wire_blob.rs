//! `wire::blob::Server`, `wire::blob::Requester`, and the `Body` of each head never
//! panic, each head encodes to its message, each body counts the bytes that its head
//! names and no more, each refusal is the one the state gives, a body ends unfinished
//! while bytes remain, and each valid message made from the input decodes to itself.
//!
//! Input: the chunk limit (one byte), then the stream messages (`fuzz::messages`),
//! which each side reads.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::{
    arbitrary::{self, Unstructured},
    fuzz_target,
};
use types::digest::Digest;
use wire::blob::{Body, Error, FromRequester, Put, Reply, Requester, Server, get};

/// The most digests of a written get.
const GET_MAX: usize = 4;

/// The bytes of a digest.
const DIGEST_LEN: usize = 32;

/// The bytes of a chunk length.
const LEN_LEN: usize = 4;

/// The rest of a body, kept apart from `wire`.
#[derive(Clone, Copy, Debug)]
struct Count {
    remain: usize,
}

impl Count {
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
        (len > 0).then_some(Self { remain: len })
    }

    /// The rest of the body after a message of `bytes` bytes that `wire` took.
    ///
    /// # Panics
    ///
    /// When the message is empty or longer than the rest.
    fn take(self, bytes: usize) -> Self {
        assert!(bytes > 0, "a body message has no byte");
        let remain = self
            .remain
            .checked_sub(bytes)
            .expect("a body message is longer than the rest of the body");
        Self { remain }
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

/// A head that a decoder gave, the same for both sides.
#[derive(Debug)]
struct Head {
    /// The head, encoded again.
    out: Vec<u8>,
    /// The length of its chunk. `None` when the head names no chunk.
    len: Option<u32>,
    /// The body that follows it. `None` for a get.
    body: Option<Body>,
}

/// One side of a blob stream.
trait Side {
    fn head(&self, message: &[u8]) -> Result<Head, Error>;

    /// The error that the side gives for `message` where it takes a head, or `None`
    /// when the message is a head it takes.
    fn refusal(message: &[u8], max: usize) -> Option<Error>;
}

impl Side for Server {
    fn head(&self, message: &[u8]) -> Result<Head, Error> {
        Ok(match Server::decode(self, message)? {
            FromRequester::Get(digests) => {
                let digests: Vec<_> = digests.collect();
                let mut out = vec![0; get::encoded_len(digests.len())];
                get::encode(&digests, &mut out);
                Head {
                    out,
                    len: None,
                    body: None,
                }
            }
            FromRequester::Put(put) => {
                let mut out = vec![0; Put::LEN];
                put.encode(&mut out);
                Head {
                    out,
                    len: Some(put.len),
                    body: Some(put.body()),
                }
            }
        })
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
    fn head(&self, message: &[u8]) -> Result<Head, Error> {
        let reply = Requester::decode(self, message)?;
        let mut out = vec![0; reply.encoded_len()];
        reply.encode(&mut out);
        let len = match reply {
            Reply::Chunk { len, .. } => Some(len),
            Reply::Absent { .. } | Reply::Stored { .. } => None,
        };
        Ok(Head {
            out,
            len,
            body: Some(reply.body()),
        })
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

/// Each head that `side` gives for the messages in `bytes` must encode to its message,
/// each body must count what the target counts, and each refusal must be the one the
/// state gives.
fn read<S: Side>(side: &S, max: usize, bytes: &[u8]) {
    let mut body: Option<(Body, Count)> = None;
    for message in fuzz::messages(bytes) {
        body = match body {
            None => match side.head(message) {
                Ok(head) => {
                    assert_eq!(head.out, message, "the head changed");
                    let count = head.len.and_then(|len| Count::start(len, max));
                    let remain = count.map_or(0, |count| count.remain);
                    let body = head.body.filter(|body| body.remain() > 0);
                    assert_eq!(
                        body.as_ref().map_or(0, Body::remain),
                        remain,
                        "the body"
                    );
                    body.zip(count)
                }
                Err(error) => {
                    assert_eq!(
                        S::refusal(message, max),
                        Some(error),
                        "{error:?} is not the refusal of {message:?}"
                    );
                    None
                }
            },
            Some((mut body, count)) => {
                let count = match body.take(message) {
                    Ok(bytes) => {
                        assert_eq!(bytes, message, "the body message changed");
                        count.take(bytes.len())
                    }
                    Err(error) => {
                        assert!(
                            count.refused(message, error),
                            "{error:?} is not the refusal of {message:?} for {count:?}"
                        );
                        count
                    }
                };
                assert_eq!(body.remain(), count.remain, "the body is elsewhere");
                if count.remain == 0 {
                    assert_eq!(body.end(), Ok(()), "a body that ended is unfinished");
                    None
                } else {
                    Some((body, count))
                }
            }
        };
    }
    if let Some((body, count)) = body {
        let remain = count.remain;
        assert_eq!(
            body.end(),
            Err(Error::Unfinished { remain }),
            "the body ended"
        );
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
            Ok(decoded) => assert_eq!(decoded, reply, "the reply changed"),
            other => panic!("a reply did not read back: {other:?}"),
        }
    }
    Ok(())
}

fuzz_target!(|bytes: &[u8]| {
    if let Some((&max, messages)) = bytes.split_first() {
        let max = usize::from(max);
        read(&Server::new(max), max, messages);
        read(&Requester::new(max), max, messages);
    }
    write(&mut Unstructured::new(bytes)).expect("an input that ends gives zeros");
});

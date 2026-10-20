//! The blob protocol between this node's store and a peer, over one stream per call.

use std::fmt;

use block::Block;
use env::files;
use transport::stream::{Incoming, Part, Receiver, Sender};
use transport::{Class, Code, Session};
use types::digest::Digest;
use wire::Protocol;
use wire::blob::{
    Body, FULL, FromRequester, MISMATCH, Put, Reply, Requester, Server, TOO_LARGE, get,
};
use wire::header::MALFORMED;

use crate::{Receipt, Store};

/// The longest message that `transport` copies when its carrier takes it, so that it
/// holds no block of the message until the ack
/// ([`transport::Config::window_bytes`]).
const COPIED_MAX: usize = 1452;

/// Serves `incoming`, a blob stream whose header the caller read, from `store`: it
/// answers each get and stores each put, in order, until the peer's half ends. A put
/// gets its reply only once the chunk is durable. A put head waits while 8 puts of
/// `serve` on `store` are open. A drop of the future ends both halves with code 0. A
/// put dropped in its body or its end leaves its part file until the floor check of a
/// later put on `store`, or the next open of `store`. One dropped in its end after the
/// start of its remove of an earlier file of the chunk leaves its part, then the only
/// copy of the chunk, until the next floor check or put of its digest on `store` makes
/// it the file of the digest, or the next open. One dropped in its end ends as a
/// dropped [`Store::put`].
///
/// # Errors
///
/// The [`Error`] that ended the stream. The stream stops with the code of a refusal
/// ([`Error::Wire`], [`Error::OneWay`], and [`Error::Store`] with `Mismatch`, `Floor`,
/// or a full disk), and with code 0 after another store error.
pub async fn serve(store: &Store, incoming: Incoming) -> Result<(), Error> {
    let Incoming {
        mut receiver,
        sender,
        ..
    } = incoming;
    let Some(mut sender) = sender else {
        receiver.stop(Code(MALFORMED));
        return Err(Error::OneWay);
    };
    let answered = answer(store, &mut receiver, &mut sender).await;
    match &answered {
        Ok(()) => sender.finish()?,
        // With no code, both halves drop, which ends them with code 0.
        Err(error) => {
            let code = match error {
                Error::Store(crate::Error::Mismatch { .. }) => Some(Code(MISMATCH)),
                Error::Store(
                    crate::Error::Floor { .. }
                    | crate::Error::Files(files::Error::Full { .. }),
                ) => Some(Code(FULL)),
                Error::Store(_) => None,
                error => error.code(),
            };
            if let Some(code) = code {
                stop(sender, receiver, code);
            }
        }
    }
    answered
}

/// Ends both halves of a stream with `code`.
fn stop(sender: Sender, receiver: Receiver, code: Code) {
    receiver.stop(code);
    sender.reset(code);
}

/// Gets each of `digests` from the peer of `session` and puts each chunk it gives in
/// `store`. Gives each digest that the peer did not give, with its cause, in the
/// order of `digests`. A refused chunk stops its stream, and the digests still open
/// go again on a new stream to the same peer. A chunk is stored only when its bytes
/// hash to its digest. A drop of the future ends the stream with code 0, a chunk not
/// yet stored is not stored, and a put in flight ends as a dropped [`Store::put`].
///
/// # Errors
///
/// [`Error::Wire`] for a message that does not decode, [`Error::Unexpected`] for a
/// reply that does not answer the oldest open digest, [`Error::Unanswered`] when the
/// peer ends its half with digests open, [`Error::Stream`] when the stream or its
/// session fails, and [`Error::Store`] when the store refuses or fails a put or the
/// pool has no block. The digests that this call did not answer are not in the
/// result.
pub async fn get(
    store: &Store,
    session: &Session,
    digests: &[Digest],
) -> Result<Vec<Miss>, Error> {
    let mut misses = Vec::new();
    let mut open = digests;
    while !open.is_empty() {
        let (mut sender, mut receiver) = session.open(Class::Complete).await?;
        let fetched = fetch(store, &mut sender, &mut receiver, &mut open, &mut misses);
        let fetched = fetched.await;
        let code = match &fetched {
            Ok(refused) => refused.and_then(|miss| miss.cause.code()),
            Err(error) => error.code(),
        };
        if let Some(code) = code {
            stop(sender, receiver, code);
        }
        misses.extend(fetched?);
    }
    Ok(misses)
}

/// Gets the digests of `open` on one stream, oldest first, and takes each one that
/// the peer answers off `open`, and each absent one onto `misses`. Gives the miss of
/// a refused chunk, whose stream then stops, or `None` once `open` is empty and both
/// halves ended.
async fn fetch(
    store: &Store,
    sender: &mut Sender,
    receiver: &mut Receiver,
    open: &mut &[Digest],
    misses: &mut Vec<Miss>,
) -> Result<Option<Miss>, Error> {
    header(store, sender).await?;
    let decoder = Requester::new(store.pool.largest());
    // A get comes from the pool of the store, and must free its block before a late
    // ack, for a reply to take. A peer takes 1472 bytes or more. The header takes a
    // block first, so `room` is at least 64 and `batch` at least 1.
    let room = COPIED_MAX.min(store.pool.largest());
    let batch = (room - get::encoded_len(0)) / size_of::<Digest>();
    while !open.is_empty() {
        let digests = &open[..open.len().min(batch)];
        let mut message = store
            .pool
            .alloc(get::encoded_len(digests.len()))
            .map_err(crate::Error::Pool)?;
        get::encode(digests, &mut message);
        sender.send(message.freeze()).await?;
        for &digest in digests {
            let Some(message) = receiver.recv().await? else {
                return Err(Error::Unanswered);
            };
            let cause = match decoder.decode(&message) {
                // The decoder checks the limit before it gives the digest of the head.
                Err(wire::blob::Error::TooLarge { len, max }) => {
                    match unread(&message)? {
                        Reply::Chunk { digest: of, .. } if of == digest => {
                            Some(Cause::TooLarge { len, max })
                        }
                        reply => return Err(Error::Unexpected { reply }),
                    }
                }
                Ok(Reply::Absent { digest: of }) if of == digest => Some(Cause::Absent),
                Ok(reply @ Reply::Chunk { digest: of, .. }) if of == digest => {
                    drop(message);
                    let chunk = body(store, receiver, reply.body()).await?;
                    match store.put(digest, &chunk).await {
                        Err(crate::Error::Mismatch { found, .. }) => {
                            Some(Cause::Mismatch { found })
                        }
                        put => {
                            put?;
                            None
                        }
                    }
                }
                Ok(reply) => return Err(Error::Unexpected { reply }),
                Err(error) => return Err(error.into()),
            };
            *open = &open[1..];
            match cause {
                None => {}
                Some(Cause::Absent) => misses.push(Miss {
                    digest,
                    cause: Cause::Absent,
                }),
                Some(cause) => return Ok(Some(Miss { digest, cause })),
            }
        }
    }
    sender.finish()?;
    end(receiver).await?;
    Ok(None)
}

/// Puts each of `digests` from `store` on the peer of `session`, in order, one at a
/// time, on one [`Push`]. It returns once the peer stored each one, so each is durable
/// on the peer. A drop of the future ends the stream with code 0.
///
/// # Errors
///
/// [`Error::Absent`] when `store` does not hold a digest, [`Error::Refused`] when the
/// peer stops the stream with a code, [`Error::Unexpected`], [`Error::Unanswered`],
/// and [`Error::Stream`] as for [`get()`], [`Error::Wire`] for a reply that does not
/// decode, and [`Error::Store`] when the store fails to read a chunk or the pool has
/// no block.
pub async fn put(
    store: &Store,
    session: &Session,
    digests: &[Digest],
) -> Result<(), Error> {
    if digests.is_empty() {
        return Ok(());
    }
    let mut push = Push::open(store, session).await?;
    for &digest in digests {
        push = push.put(digest).await?;
    }
    push.finish().await
}

/// An open put of chunks on one stream to a peer, for a caller that bounds each step
/// on its own. A drop ends the stream with code 0.
#[derive(Debug)]
pub struct Push<'a> {
    store: &'a Store,
    sender: Sender,
    receiver: Receiver,
}

impl<'a> Push<'a> {
    /// Opens a stream on `session` and sends the blob header.
    ///
    /// # Errors
    ///
    /// [`Error::Stream`] when the stream or its session fails, and [`Error::Store`]
    /// when the pool has no block.
    pub async fn open(store: &'a Store, session: &Session) -> Result<Self, Error> {
        let (mut sender, receiver) = session.open(Class::Complete).await?;
        header(store, &mut sender).await?;
        Ok(Self {
            store,
            sender,
            receiver,
        })
    }

    /// Puts the chunk of `digest` from the store on the peer, and returns once the
    /// peer stored it, so it is durable on the peer. A drop of the future ends the
    /// stream with code 0.
    ///
    /// # Errors
    ///
    /// As for [`put()`]. The stream ends, with the code of a fault of the peer.
    pub async fn put(mut self, digest: Digest) -> Result<Self, Error> {
        match self.send(digest).await {
            Ok(()) => Ok(self),
            Err(error) => Err(self.stop(error)),
        }
    }

    /// Ends this half of the stream and waits for the end of the peer's half.
    ///
    /// # Errors
    ///
    /// [`Error::Unexpected`] for a reply after the last put, which stops the stream
    /// with `MALFORMED`, [`Error::Wire`] for such a reply that does not decode, which
    /// stops it with its code, and [`Error::Stream`] when the stream or its session
    /// fails.
    pub async fn finish(mut self) -> Result<(), Error> {
        let ended = async {
            self.sender.finish()?;
            end(&mut self.receiver).await
        };
        match ended.await {
            Ok(()) => Ok(()),
            Err(error) => Err(self.stop(error)),
        }
    }

    /// Sends the chunk of `digest` and waits for its `Stored`.
    async fn send(&mut self, digest: Digest) -> Result<(), Error> {
        let Some(chunk) = self.store.get(digest).await? else {
            return Err(Error::Absent { digest });
        };
        let refusal = |error| refused(digest, error);
        let len =
            u32::try_from(chunk.len()).expect("invariant: a block holds at most 2 GiB");
        let mut head = self
            .store
            .pool
            .alloc(Put::LEN)
            .map_err(crate::Error::Pool)?;
        Put { digest, len }.encode(&mut head);
        self.sender.send(head.freeze()).await.map_err(refusal)?;
        send_chunk(&mut self.sender, chunk).await.map_err(refusal)?;
        let Some(message) = self.receiver.recv().await.map_err(refusal)? else {
            return Err(Error::Unanswered);
        };
        match unread(&message)? {
            Reply::Stored { digest: of } if of == digest => Ok(()),
            reply => Err(Error::Unexpected { reply }),
        }
    }

    /// Ends the stream with the code of `error`, when it has one, and gives `error`.
    fn stop(self, error: Error) -> Error {
        if let Some(code) = error.code() {
            stop(self.sender, self.receiver, code);
        }
        error
    }
}

/// The error of a put of `digest` that `error` ended: the peer's refusal when the
/// peer stopped or reset the stream with a code that is not 0.
fn refused(digest: Digest, error: transport::Error) -> Error {
    match error {
        transport::Error::Stopped { code } | transport::Error::Reset { code }
            if code != Code(0) =>
        {
            Error::Refused { digest, code }
        }
        error => Error::Stream(error),
    }
}

/// Sends the blob header.
async fn header(store: &Store, sender: &mut Sender) -> Result<(), Error> {
    let header = wire::header::encode(Protocol::Blob);
    let block = store.pool.copy(&header).map_err(crate::Error::Pool)?;
    sender.send(block).await?;
    Ok(())
}

/// Waits for the end of the peer's half, once no request is open.
async fn end(receiver: &mut Receiver) -> Result<(), Error> {
    match receiver.recv().await? {
        None => Ok(()),
        Some(message) => Err(Error::Unexpected {
            reply: unread(&message)?,
        }),
    }
}

/// Decodes a reply whose body the requester does not read, so a chunk head of any
/// length decodes.
fn unread(message: &[u8]) -> Result<Reply, Error> {
    Ok(Requester::new(usize::MAX).decode(message)?)
}

/// Answers each request of `receiver` on `sender` until the peer's half ends.
async fn answer(
    store: &Store,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<(), Error> {
    let server = Server::new(store.pool.largest());
    while let Some(message) = receiver.recv().await? {
        match server.decode(&message)? {
            FromRequester::Get(digests) => {
                for digest in digests {
                    give(store, sender, digest).await?;
                }
            }
            FromRequester::Put(put) => {
                // A head that waits for a receipt holds no block.
                drop(message);
                let len =
                    usize::try_from(put.len).expect("invariant: a u32 fits a usize");
                let receipt = store.receive(put.digest, len).await?;
                receive(receipt, receiver, put.body()).await?;
                let stored = Reply::Stored { digest: put.digest };
                reply(store, sender, stored).await?;
            }
        }
    }
    Ok(())
}

/// Sends the chunk of `digest`, or that `store` does not hold it.
async fn give(store: &Store, sender: &mut Sender, digest: Digest) -> Result<(), Error> {
    let Some(chunk) = store.get(digest).await? else {
        return reply(store, sender, Reply::Absent { digest }).await;
    };
    let len =
        u32::try_from(chunk.len()).expect("invariant: a block holds at most 2 GiB");
    reply(store, sender, Reply::Chunk { digest, len }).await?;
    send_chunk(sender, chunk).await?;
    Ok(())
}

/// Sends `chunk` as messages of at most [`COPIED_MAX`] bytes, each a range of its
/// block, which `transport` copies, so it holds no block of the chunk after the last
/// send.
async fn send_chunk(sender: &mut Sender, chunk: Block) -> Result<(), transport::Error> {
    let mut at = 0;
    while at < chunk.len() {
        let end = chunk.len().min(at + COPIED_MAX);
        let part = Part {
            range: at..end,
            zeros: 0,
        };
        sender.send_parts(chunk.clone(), &[part]).await?;
        at = end;
    }
    Ok(())
}

/// Writes `body` through `receipt` as each message comes, and ends the put. A body
/// that does not come whole discards the put.
async fn receive(
    mut receipt: Receipt<'_>,
    receiver: &mut Receiver,
    mut body: Body,
) -> Result<(), Error> {
    let read = async {
        while body.remain() > 0 {
            let Some(message) = receiver.recv().await? else {
                break;
            };
            body.take(&message)?;
            receipt.write(message).await?;
        }
        body.end()?;
        Ok(())
    }
    .await;
    match read {
        Ok(()) => Ok(receipt.end().await?),
        Err(error) => {
            receipt.discard().await;
            Err(error)
        }
    }
}

/// Reads `body` into one block.
async fn body(
    store: &Store,
    receiver: &mut Receiver,
    mut body: Body,
) -> Result<Block, Error> {
    let mut chunk = store
        .pool
        .alloc(body.remain())
        .map_err(crate::Error::Pool)?;
    let mut at = 0;
    while body.remain() > 0 {
        let Some(message) = receiver.recv().await? else {
            break;
        };
        let bytes = body.take(&message)?;
        let end = at + bytes.len();
        chunk[at..end].copy_from_slice(bytes);
        at = end;
    }
    body.end()?;
    Ok(chunk.freeze())
}

async fn reply(store: &Store, sender: &mut Sender, reply: Reply) -> Result<(), Error> {
    let mut head = store
        .pool
        .alloc(reply.encoded_len())
        .map_err(crate::Error::Pool)?;
    reply.encode(&mut head);
    sender.send(head.freeze()).await?;
    Ok(())
}

/// A digest that a get did not give, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Miss {
    /// The digest of the get.
    pub digest: Digest,
    /// Why the peer did not give it.
    pub cause: Cause,
}

/// Why the peer did not give a chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The peer does not hold it.
    Absent,
    /// The peer's chunk is longer than the largest block of the store.
    TooLarge {
        /// The bytes of the chunk, as the peer claimed.
        len: u32,
        /// The largest block of the store.
        max: usize,
    },
    /// The peer's bytes hash to `found`.
    Mismatch {
        /// The digest of the bytes.
        found: Digest,
    },
}

impl Cause {
    /// The code that the stream of a refused chunk stops with, or `None` when the
    /// stream goes on.
    fn code(self) -> Option<Code> {
        match self {
            Self::Absent => None,
            Self::TooLarge { .. } => Some(Code(TOO_LARGE)),
            Self::Mismatch { .. } => Some(Code(MISMATCH)),
        }
    }
}

/// Why a blob stream ended with an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A message of the peer that does not decode. The stream stops with
    /// [`wire::blob::Error::code`].
    Wire(wire::blob::Error),
    /// The peer opened the stream one way. The stream stops with code `MALFORMED`.
    OneWay,
    /// The store refused a put, or failed. `serve` stops the stream with `MISMATCH`
    /// for [`crate::Error::Mismatch`], with `FULL` for [`crate::Error::Floor`] and a
    /// full disk, and with code 0 for each other error. [`get()`], [`put()`], and
    /// [`Push`] end it with code 0.
    Store(crate::Error),
    /// The stream or its session failed. No code.
    Stream(transport::Error),
    /// The store of a put does not hold the digest. Nothing of it was sent.
    Absent {
        /// The digest of the put.
        digest: Digest,
    },
    /// The peer stopped the stream with `code` in the put of `digest`: `FULL`,
    /// `MISMATCH`, `TOO_LARGE`, or another code that is not 0.
    Refused {
        /// The digest of the put.
        digest: Digest,
        /// The code of the stop.
        code: Code,
    },
    /// A reply that does not answer the oldest open request. The stream stops with
    /// code `MALFORMED`.
    Unexpected {
        /// The reply.
        reply: Reply,
    },
    /// The peer ended its half with requests open. The stream stops with code
    /// `MALFORMED`.
    Unanswered,
}

impl Error {
    /// The code of a fault of the peer that the stream stops with, or `None` when it
    /// drops with code 0. A store error is a failure of this node: only `serve` takes
    /// a code for it.
    fn code(&self) -> Option<Code> {
        match self {
            Self::Wire(error) => Some(Code(error.code())),
            Self::OneWay | Self::Unexpected { .. } | Self::Unanswered => {
                Some(Code(MALFORMED))
            }
            Self::Store(_)
            | Self::Stream(_)
            | Self::Absent { .. }
            | Self::Refused { .. } => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wire(error) => write!(f, "a blob message is not valid: {error}"),
            Self::OneWay => f.write_str("a blob stream needs a two-way stream"),
            Self::Store(error) => error.fmt(f),
            Self::Stream(error) => write!(f, "the blob stream failed: {error}"),
            Self::Absent { digest } => write!(f, "the store does not hold {digest}"),
            Self::Refused { digest, code } => {
                write!(
                    f,
                    "the peer refused the put of {digest} with code {}",
                    code.0
                )
            }
            Self::Unexpected { reply } => {
                write!(f, "the reply {reply:?} does not answer the oldest request")
            }
            Self::Unanswered => {
                f.write_str("the peer ended its half with requests open")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<wire::blob::Error> for Error {
    fn from(error: wire::blob::Error) -> Self {
        Self::Wire(error)
    }
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        Self::Store(error)
    }
}

impl From<transport::Error> for Error {
    fn from(error: transport::Error) -> Self {
        Self::Stream(error)
    }
}

#[cfg(test)]
mod tests;

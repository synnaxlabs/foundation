//! The blob protocol between this node's store and a peer, over one stream per call.

use std::fmt;

use block::Block;
use env::files;
use transport::Code;
use transport::stream::{Incoming, Part, Receiver, Sender};
use types::digest::Digest;
use wire::blob::{FULL, FromRequester, MISMATCH, Put, Reply, Server};
use wire::header::MALFORMED;

use crate::Store;

/// Serves `incoming`, a blob stream whose header the caller read, from `store`: it
/// answers each get and stores each put, in order, until the peer's half ends. A put
/// gets its reply only once the chunk is durable.
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
            if let Some(code) = error.code() {
                receiver.stop(code);
                sender.reset(code);
            }
        }
    }
    answered
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
                let chunk = body(store, receiver, put).await?;
                store.put(put.digest, &chunk).await?;
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
    let step = sender.bytes_max();
    let mut at = 0;
    while at < chunk.len() {
        let end = chunk.len().min(at.saturating_add(step));
        let part = Part {
            range: at..end,
            zeros: 0,
        };
        sender.send_parts(chunk.clone(), &[part]).await?;
        at = end;
    }
    Ok(())
}

/// Reads the body of `put` into one block.
async fn body(
    store: &Store,
    receiver: &mut Receiver,
    put: Put,
) -> Result<Block, Error> {
    let mut body = put.body();
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

/// Why a blob stream ended with an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A message of the peer that does not decode. The stream stops with
    /// [`wire::blob::Error::code`].
    Wire(wire::blob::Error),
    /// The peer opened the stream one way. The stream stops with code `MALFORMED`.
    OneWay,
    /// The store refused a put, or failed. The stream stops with `MISMATCH` for
    /// [`crate::Error::Mismatch`], with `FULL` for [`crate::Error::Floor`] and a full
    /// disk, and with code 0 for each other error.
    Store(crate::Error),
    /// The stream or its session failed. No code.
    Stream(transport::Error),
}

impl Error {
    /// The code that the stream stops with, or `None` when it drops with code 0.
    fn code(&self) -> Option<Code> {
        match self {
            Self::Wire(error) => Some(Code(error.code())),
            Self::OneWay => Some(Code(MALFORMED)),
            Self::Store(crate::Error::Mismatch { .. }) => Some(Code(MISMATCH)),
            Self::Store(
                crate::Error::Floor { .. }
                | crate::Error::Files(files::Error::Full { .. }),
            ) => Some(Code(FULL)),
            Self::Store(_) | Self::Stream(_) => None,
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

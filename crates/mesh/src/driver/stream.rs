//! Serves the streams of the mesh protocol that a peer opened.

use transport::Code;
use transport::stream::{Incoming, Receiver, Sender};
use types::ed25519::PublicKey;

use super::{Mesh, REFUSED, REMOVED};
use crate::bytes::block;
use crate::error::Error;
use crate::message::Message;

/// The code of a stream that carried a message that is not valid for it.
const MALFORMED: Code = Code(wire::header::MALFORMED);

impl Mesh {
    /// Serves one stream of a session whose peer proved the key `peer`. The caller
    /// has read the header of the stream, which is its whole first message. `serve`
    /// returns when the stream ends, or at the first message it refuses. A refused
    /// message changes nothing, and the stream stops with code 16, or with code 17
    /// for a request from a node that a committed configuration removed.
    ///
    /// # Errors
    ///
    /// - [`Error::Malformed`] when a message is the byte form of no message, or is not
    ///   one that its stream carries. The stream stops with code 2, but after the
    ///   answer only the half that `serve` reads stops.
    /// - [`Error::Spoofed`], [`Error::NotVoter`], [`Error::Removed`],
    ///   [`Error::PeerNotVoter`], [`Error::Claim`], and [`Error::Raft`] when the group
    ///   refuses a message.
    /// - [`Error::Pool`] when the pool has no block: while the group waits to write its
    ///   log, which refuses the message, or for the answer to a proposal, which the
    ///   peer then does not get, and the group can hold the entry of the proposal. A
    ///   `raft` message that gets it on a one-way stream is dropped, and the stream
    ///   goes on.
    /// - [`Error::Stream`] when the stream or its session fails.
    /// - [`Error::Stopped`] when the group stopped. On a stream that goes both ways,
    ///   the reply half then ends with no mesh code: the group can hold the entry.
    pub async fn serve(
        &self,
        peer: PublicKey,
        incoming: Incoming,
    ) -> Result<(), Error> {
        let Incoming {
            mut receiver,
            sender,
            ..
        } = incoming;
        let Some(sender) = sender else {
            let delivered = self.deliver(peer, &mut receiver).await;
            if let Some(code) = delivered.as_ref().err().and_then(code) {
                receiver.stop(code);
            }
            return delivered;
        };
        self.exchange(peer, receiver, sender).await
    }

    /// Gives the group each `raft` message of a stream that only `peer` sends on.
    async fn deliver(
        &self,
        peer: PublicKey,
        receiver: &mut Receiver,
    ) -> Result<(), Error> {
        while let Some(bytes) = receiver.recv().await? {
            let Some(Message::Raft(message)) = Message::decode(&bytes) else {
                return Err(Error::Malformed);
            };
            match self.receive(peer, message) {
                // `raft` sends the message again.
                Ok(()) | Err(Error::Pool(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Answers the one proposal of a stream that goes both ways, and reads to its end.
    async fn exchange(
        &self,
        peer: PublicKey,
        mut receiver: Receiver,
        mut sender: Sender,
    ) -> Result<(), Error> {
        let answer = match self.ask(peer, &mut receiver).await {
            Ok(answer) => answer.encode(),
            // The group can stop in the write of the entry, so a stop is no refusal.
            Err(error @ Error::Stopped(_)) => return Err(error),
            Err(error) => {
                if let Some(code) = code(&error) {
                    sender.reset(code);
                    receiver.stop(code);
                }
                return Err(error);
            }
        };
        // The group can hold the entry of the proposal, so no error from here is a
        // refusal: a sender that drops ends the reply half with no code of the mesh.
        let answer = block(&self.pool, &answer).map_err(Error::Pool)?;
        sender.send(answer).await?;
        sender.finish()?;
        // A reset takes back an answer that the peer does not have yet, so from here
        // only the receiver stops.
        if receiver.recv().await?.is_some() {
            receiver.stop(MALFORMED);
            return Err(Error::Malformed);
        }
        Ok(())
    }

    /// Reads the proposal, and gives the answer of the group to it.
    async fn ask(
        &self,
        peer: PublicKey,
        receiver: &mut Receiver,
    ) -> Result<Message, Error> {
        // The block of the proposal drops with this statement: the group writes the
        // entry from the same pool.
        let Some(Message::Propose { change }) =
            receiver.recv().await?.as_deref().and_then(Message::decode)
        else {
            return Err(Error::Malformed);
        };
        self.answer(peer, change).await
    }
}

/// The code that stops a stream after `error`, or `None` when the stream failed. A
/// stream that goes both ways takes no code for a group that stopped.
fn code(error: &Error) -> Option<Code> {
    match error {
        Error::Stream(_) => None,
        Error::Malformed => Some(MALFORMED),
        Error::Removed { .. } => Some(REMOVED),
        Error::Log(_)
        | Error::Raft(_)
        | Error::Spoofed { .. }
        | Error::NotVoter { .. }
        | Error::PeerNotVoter { .. }
        | Error::Claim(_)
        | Error::NotMember(_)
        | Error::NoVote
        | Error::Member(_)
        | Error::WrongKey
        | Error::Pool(_)
        | Error::Stopped(_)
        | Error::Stale { .. }
        | Error::Large { .. }
        | Error::Problems(_)
        | Error::Quorum { .. }
        | Error::Blob(_)
        | Error::NotIndex(_)
        | Error::UnknownNode(_)
        | Error::Homes { .. } => Some(REFUSED),
    }
}

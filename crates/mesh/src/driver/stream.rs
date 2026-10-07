//! Serves the streams of the mesh protocol that a peer opened.

use transport::Code;
use transport::stream::{Incoming, Receiver, Sender};
use types::node::PublicKey;

use super::Mesh;
use crate::error::Error;
use crate::message::{ANSWER_MAX, Message};

/// The code of a stream that carried a message that is not valid for it.
const MALFORMED: Code = Code(wire::header::MALFORMED);
/// The code of a stream with a message that the mesh refused.
const REFUSED: Code = Code(16);

impl Mesh {
    /// Serves one stream that `peer` opened, after `node` read its header. It returns
    /// when the stream ends, or at the first message it refuses. A refused message
    /// changes nothing, and the stream stops with the mesh code `REFUSED`.
    ///
    /// # Errors
    ///
    /// - [`Error::Malformed`] when a message is the byte form of no message, or is not
    ///   one that its stream carries. The stream stops with code 2.
    /// - [`Error::Spoofed`], [`Error::NotVoter`], [`Error::PeerNotVoter`],
    ///   [`Error::Grant`], and [`Error::Raft`] when the group refuses a message.
    /// - [`Error::Pool`] when the pool has no block for a proposal: for its answer, or
    ///   while the group waits to write its log. A `raft` message that gets it on a
    ///   one-way stream is dropped, and the stream goes on.
    /// - [`Error::Stream`] when the stream or its session fails.
    /// - [`Error::Stopped`] when the group stopped.
    pub(crate) async fn serve(
        &self,
        peer: PublicKey,
        incoming: Incoming,
    ) -> Result<(), Error> {
        let Incoming {
            mut receiver,
            sender,
            ..
        } = incoming;
        let served = match sender {
            None => self.take(peer, &mut receiver).await,
            Some(sender) => self.exchange(peer, &mut receiver, sender).await,
        };
        if let Some(code) = served.as_ref().err().and_then(code) {
            receiver.stop(code);
        }
        served
    }

    /// Gives the group each `raft` message of a stream that only `peer` sends on.
    async fn take(
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
        receiver: &mut Receiver,
        mut sender: Sender,
    ) -> Result<(), Error> {
        if let Err(error) = self.reply(peer, receiver, &mut sender).await {
            if let Some(code) = code(&error) {
                sender.reset(code);
            }
            return Err(error);
        }
        // A reset takes back an answer that the peer does not have yet, so from here
        // only the receiver stops.
        match receiver.recv().await? {
            None => Ok(()),
            Some(_) => Err(Error::Malformed),
        }
    }

    /// Reads the proposal, gives it to the group, and sends its answer whole.
    async fn reply(
        &self,
        peer: PublicKey,
        receiver: &mut Receiver,
        sender: &mut Sender,
    ) -> Result<(), Error> {
        let first = receiver.recv().await?;
        let Some(Message::Propose { change }) =
            first.as_deref().and_then(Message::decode)
        else {
            return Err(Error::Malformed);
        };
        // The block comes first, so a refusal for memory changes nothing.
        let mut block = self.pool.alloc(ANSWER_MAX).map_err(Error::Pool)?;
        let answer = self.answer(peer, change).await?.encode();
        // The answer ends the block, and the stream skips the bytes before it.
        let start = ANSWER_MAX
            .checked_sub(answer.len())
            .expect("invariant: ANSWER_MAX bounds an answer");
        block
            .get_mut(start..)
            .expect("invariant: the block has ANSWER_MAX bytes")
            .copy_from_slice(&answer);
        sender.send(block.freeze().skip(start)).await?;
        Ok(sender.finish()?)
    }
}

/// The code that stops a stream after `error`, or `None` when the stream failed.
fn code(error: &Error) -> Option<Code> {
    match error {
        Error::Stream(_) => None,
        Error::Malformed => Some(MALFORMED),
        Error::Log(_)
        | Error::Raft(_)
        | Error::Spoofed { .. }
        | Error::NotVoter { .. }
        | Error::PeerNotVoter { .. }
        | Error::Grant(_)
        | Error::NotMember(_)
        | Error::Member(_)
        | Error::WrongKey
        | Error::Pool(_)
        | Error::Stopped(_) => Some(REFUSED),
    }
}

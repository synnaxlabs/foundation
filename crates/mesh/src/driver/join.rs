//! Admits a node that asks a voter to join by a ticket.

use transport::stream::Sender;
use types::ed25519::PublicKey;

use super::Mesh;
use crate::error::Error;
use crate::message::{FOUNDING_HEAD, Message, put_founding};
use crate::refusal::Refusal;
use crate::region::Request;

impl Mesh {
    // Admits the node of `request`, from a peer that proved the key `peer`.
    // `Ok(Ok(()))` once this node applied the join, also when it applied it before the
    // call. `Unled` when one try did not commit: at once when a new leader replaced
    // its entry, and a tick after the try when no leader took it. `Spoofed` when
    // `peer` is not the key of the card.
    pub(super) async fn admit(
        &self,
        peer: PublicKey,
        request: Request,
    ) -> Result<Result<(), Refusal>, Error> {
        let card = &request.card;
        if card.card.public_key != peer {
            return Err(Error::Spoofed { from: card.key });
        }
        if let Err(error) = self.check_proposer() {
            return refusal(error);
        }
        if self.admitted(&request) {
            return Ok(Ok(()));
        }
        let change = match self.stamp(request.clone()) {
            Ok(change) => change,
            Err(refused) => return Ok(Err(refused)),
        };
        let settled = match self.attempt() {
            Ok(attempt) => attempt.settle(change).await,
            Err(error) => Err(error),
        };
        match settled {
            // The entry of an earlier answer can apply first, and then this one is a
            // duplicate.
            Ok(Some(Err(_))) if self.admitted(&request) => Ok(Ok(())),
            Ok(Some(applied)) => Ok(applied.map_err(Refusal::Refused)),
            Ok(None) => Ok(Err(Refusal::Unled)),
            Err(error) => refusal(error),
        }
    }

    /// Sends a `Joined`, then the founding in parts that each fit one message.
    pub(super) async fn send_founding(&self, sender: &mut Sender) -> Result<(), Error> {
        let founding = self.founding.get().await?;
        let body = founding.body();
        let joined = Message::Joined { bytes: body.len() }.encode();
        sender
            .send(self.pool.copy(&joined).map_err(Error::Pool)?)
            .await?;
        let part_max = sender
            .bytes_max()
            .min(self.pool.largest())
            .checked_sub(FOUNDING_HEAD)
            .expect("invariant: a peer takes messages of at least 1,472 bytes");
        for part in body.chunks(part_max) {
            let bytes = part.len().checked_add(FOUNDING_HEAD);
            let bytes = bytes.expect("invariant: a part is shorter than a message");
            let mut message = self.pool.alloc(bytes).map_err(Error::Pool)?;
            put_founding(part, &mut message);
            sender.send(message.freeze()).await?;
        }
        Ok(())
    }

    // Whether this node applied the join of `request`: a member holds its node key
    // with its public key and its admission.
    fn admitted(&self, request: &Request) -> bool {
        let group = self.group.borrow();
        group.state.member(request.card.key).is_some_and(|member| {
            member.card.card().public_key == request.card.card.public_key
                && member.admission == request.admission
        })
    }
}

// The refusal that the joining node gets for `error`, or `error` when it is no refusal.
fn refusal(error: Error) -> Result<Result<(), Refusal>, Error> {
    match error {
        Error::Contested { .. } => Ok(Err(Refusal::Contested)),
        Error::NoVote => Ok(Err(Refusal::NoVote)),
        Error::Stopped(_) => Ok(Err(Refusal::Stopped)),
        error => Err(error),
    }
}

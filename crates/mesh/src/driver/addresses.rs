//! Sets the addresses of the card of this node through the leader of the group.

use super::{Mesh, TICK};
use crate::card::addresses::Addresses;
use crate::card::{Card, Signed};
use crate::change::Change;
use crate::error::Error;
use crate::region::Refused;

impl Mesh {
    /// Makes `addresses` the addresses of the card of this node, which
    /// [`Config::private_key`](super::Config::private_key) signs. It does nothing when
    /// the card holds them at or above each version that this node signed before. Else
    /// it proposes the card with them, at a version above the held one and above each
    /// version that this node signed before, and returns when an entry that holds it
    /// has committed and this node applied it. It tries again as [`Mesh::set_home`]
    /// does, and also when a card of a later version applied first. A node that joined
    /// and has not applied its own join yet waits until it has.
    ///
    /// # Errors
    ///
    /// - [`Error::NoVote`] when this node is not a voter.
    /// - [`Error::Soon`] when the leader, after its own start, proposed another card
    ///   of this node less than a minute before. Call again after a minute.
    /// - [`Error::Files`] when a read or write of the last signed version fails.
    /// - [`Error::CardFile`] when the file of the last signed version is not valid.
    /// - [`Error::Stopped`] when the group stopped.
    ///
    /// # Panics
    ///
    /// On a broken invariant of the region state: a refusal of the card other than
    /// for its version.
    pub async fn set_addresses(&self, addresses: Addresses) -> Result<(), Error> {
        let mut signed: Option<Signed> = None;
        loop {
            let attempt = self.attempt()?;
            let Some(held) = self.own() else {
                drop(attempt);
                self.clock.sleep(TICK).await;
                continue;
            };
            let card = match signed.take() {
                Some(card) if card.card().version > held.version => card,
                _ => match self.cards.sign(&held, &addresses).await {
                    Ok(Some(card)) => card,
                    Ok(None) => return Ok(()),
                    // The file takes a block again after a tick, as a proposal does.
                    Err(Error::Pool(_)) => {
                        drop(attempt);
                        self.clock.sleep(TICK).await;
                        continue;
                    }
                    Err(error) => return Err(error),
                },
            };
            signed = Some(card.clone());
            match attempt.settle(Change::Card(Box::new(card.into()))).await? {
                Some(Ok(())) => return Ok(()),
                Some(Err(Refused::Behind { .. })) | None => {}
                Some(Err(refused)) => {
                    panic!(
                        "invariant: an own card is refused only as behind: {refused}"
                    )
                }
            }
        }
    }

    // The card of this node in its applied state, or `None` before it applied its
    // join.
    fn own(&self) -> Option<Card> {
        let group = self.group.borrow();
        let own = group.state.member(group.raft.key())?;
        Some(own.card.card().clone())
    }
}

//! Makes a join ticket and records it through the leader of the group.

use types::ed25519::PrivateKey;

use super::Mesh;
use crate::change::Change;
use crate::error::Error;
use crate::region::Refused;
use crate::ticket::{Options, Ticket, Voter};

impl Mesh {
    /// Makes a join ticket for the region, records it with `options`, and gives its
    /// secret. The key pair comes from [`Config::entropy`](super::Config::entropy).
    /// The ticket lists each voter of the configuration that this node has applied, of
    /// both halves of a joint one, in node key order, with the addresses of its card.
    /// A follower forwards the change to the leader. It returns when an entry that
    /// records the ticket has committed and this node applied it, and proposes again
    /// as [`Mesh::set_home`] does. An entry of an earlier try of this call that
    /// recorded the ticket counts as the record.
    ///
    /// # Errors
    ///
    /// - [`Error::Outside`] when `options.prefix` is not under the region's prefix.
    /// - [`Error::NoVote`] and [`Error::Stopped`] as for [`Mesh::set_home`].
    ///
    /// # Panics
    ///
    /// On a broken invariant of the region state: a refusal of the change, or a voter
    /// that is not a member.
    pub async fn ticket(&self, options: Options) -> Result<Ticket, Error> {
        let region = self.group.borrow().state.prefix().clone();
        if !region.contains(&options.prefix) {
            return Err(Error::Outside {
                prefix: options.prefix,
                region,
            });
        }
        let mut private_key = [0; 32];
        self.entropy.fill(&mut private_key);
        let private_key = PrivateKey(private_key);
        let public_key = private_key.public();
        loop {
            let attempt = self.attempt()?;
            let change = Change::Ticket {
                public_key,
                options: options.clone(),
            };
            match attempt.settle(change).await? {
                Some(Ok(()) | Err(Refused::Recorded { .. })) => break,
                Some(Err(refused)) => {
                    panic!(
                        "invariant: a ticket in the region is never refused: {refused}"
                    )
                }
                None => {}
            }
        }
        let state = &self.group.borrow().state;
        let voters = state
            .voters()
            .nodes()
            .map(|key| {
                let member = state.member(key);
                Voter::of(member.expect("invariant: each applied voter is a member"))
            })
            .collect();
        Ok(Ticket::new(private_key, region, voters))
    }
}

//! Changes the voters of the region through the leader of the group.

use std::collections::{BTreeMap, BTreeSet};

use raft::{Position, Voters};
use spec::tree::{self, Chunks};
use types::digest::Digest;
use types::ed25519::PublicKey;
use types::node;

use super::used::RETRY;
use super::{Group, Mesh, check_voters};
use crate::error::Error;

impl Mesh {
    /// Proposes `voters` as the next set of voters, and returns once this node applied
    /// the joint entry and a majority of the set holds each chunk of the tree of the
    /// spec in force. `raft` moves to the set through a joint configuration (RAFT
    /// VOTERS), whose entry binds the card key of each voter that it adds. Before the
    /// proposal, it puts each chunk of the tree of the pointer in force on each other
    /// voter of the set, all at once, in the calls of [`Mesh::apply`], and proposes
    /// once the holders are a majority of the set. After the joint entry applies, it
    /// puts the tree of the pointer then in force on the set again when that pointer
    /// moved, and tries again each second until a majority holds it. No failed put
    /// after the joint entry fails the call: the change of voters is in force.
    ///
    /// # Errors
    ///
    /// None of these propose anything, other than `Stopped`, `Blob`, and `NotLeader`
    /// after the write: the set is in force after the joint entry.
    ///
    /// - [`Error::Voters`] when the set holds more voters than one set can hold.
    /// - [`Error::NotMember`] when a node of the set is not a member in the applied
    ///   state of this node.
    /// - [`Error::NoVote`] when this node is not a voter.
    /// - [`Error::Stopped`] when the group stopped, or stops before the call ends.
    /// - [`Error::Lacking`] when this node's store cannot give a chunk of the tree in
    ///   force.
    /// - [`Error::Blob`] when a call of the store fails.
    /// - [`Error::Quorum`] when the holders are not a majority of the set once each
    ///   put ended, with each voter whose put failed and why.
    /// - [`Error::Raft`] when `raft` refuses the set ([`Raft::propose_voters`]), or
    ///   with [`raft::Error::NotLeader`] when a new leader replaces the entry before
    ///   a write holds it or before it applies.
    ///
    /// # Panics
    ///
    /// On a broken invariant of the region state: a refusal of the entry.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "a later PR of #336 is the first user")
    )]
    pub(crate) async fn propose_voters(
        &self,
        voters: BTreeSet<node::Key>,
    ) -> Result<Position, Error> {
        check_voters(voters.len())?;
        keyed(&self.group.borrow(), &voters)?;
        self.check_proposer()?;
        let set = Voters {
            incoming: voters.clone(),
            outgoing: BTreeSet::new(),
        };
        let pointer = self.pointer();
        self.spread(&set, &self.tree(pointer.root).await?).await?;
        let at = {
            let attempt = self.attempt()?;
            let at = self
                .proposed(|group| {
                    Ok(group.raft.propose_voters(keyed(group, &voters)?)?)
                })
                .await?;
            match attempt.applied(at).await? {
                Some(Ok(())) => at,
                Some(Err(refused)) => {
                    panic!("invariant: a change of voters is never refused: {refused}")
                }
                None => {
                    let leader = self.group.borrow().raft.leader();
                    return Err(raft::Error::NotLeader { leader }.into());
                }
            }
        };
        loop {
            let moved = self.pointer();
            if moved == pointer {
                return Ok(at);
            }
            let put = match self.tree(moved.root).await {
                Ok(digests) => self.spread(&set, &digests).await.map(|_| ()),
                Err(error) => Err(error),
            };
            match put {
                Ok(()) => return Ok(at),
                Err(Error::Quorum { .. } | Error::Lacking(_)) => {}
                Err(error) => return Err(error),
            }
            self.clock.sleep(RETRY).await;
        }
    }

    // Each chunk of the tree at `root`, from this node's store, in the order of a walk
    // from the root.
    async fn tree(&self, root: Digest) -> Result<Vec<Digest>, Error> {
        let mut chunks = Chunks::default();
        let mut digests = Vec::new();
        loop {
            let digest = match tree::diff(&chunks, tree::empty(), root) {
                Ok(_) => return Ok(digests),
                Err(tree::Error::Missing(digest)) => digest,
                Err(tree::Error::Corrupt(digest)) => {
                    return Err(Error::Lacking(digest));
                }
            };
            match self.store.get(digest).await.map_err(Error::Blob)? {
                Some(chunk) => chunks.insert(chunk.to_vec()),
                None => return Err(Error::Lacking(digest)),
            };
            digests.push(digest);
        }
    }
}

// The public key of each of `voters` in the applied state of `group`, or `NotMember`
// of the first that is not a member.
fn keyed(
    group: &Group,
    voters: &BTreeSet<node::Key>,
) -> Result<BTreeMap<node::Key, PublicKey>, Error> {
    voters
        .iter()
        .map(|&key| match group.state.member(key) {
            Some(member) => Ok((key, member.public_key())),
            None => Err(Error::NotMember(key)),
        })
        .collect()
}

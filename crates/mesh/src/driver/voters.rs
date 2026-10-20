//! Changes the voters of the region.

use std::collections::{BTreeMap, BTreeSet};

use raft::{Position, Role, Voters};
use spec::tree::{self, Chunks};
use types::digest::Digest;
use types::ed25519::PublicKey;
use types::node;
use types::time::{Monotonic, Span};

use super::used::RETRY;
use super::{Group, Mesh, check_voters, fetched};
use crate::error::Error;

/// The least span between two warnings of a put that runs again after the joint
/// entry.
const WARN: Span = Span::MINUTE;

impl Mesh {
    /// Proposes `voters` as the next set of voters, and returns once this node applied
    /// the joint entry and a majority of the set holds each chunk of the tree of the
    /// spec in force. `raft` moves to the set through a joint configuration (RAFT
    /// VOTERS), whose entry binds the card key of each voter that it adds. The call
    /// puts the tree on the set before the proposal, and again after the joint entry
    /// applies when the pointer moved in between, for as long as it takes a majority
    /// of the set to hold it. It logs a warning at the first of those puts that
    /// fails, and at most once a minute after, which names each voter whose put
    /// failed.
    ///
    /// # Errors
    ///
    /// Nothing is proposed, and the voters stay as they were:
    ///
    /// - [`Error::Voters`] when the set holds more voters than one set can hold.
    /// - [`Error::NotMember`] when a node of the set is not a member in the applied
    ///   state of this node.
    /// - [`Error::NoVote`] when this node is not a voter.
    /// - [`Error::Raft`] with [`raft::Error::NotLeader`] when this node does not lead,
    ///   or when a new leader replaces the entry before a write holds it or before it
    ///   applies, or when `raft` refuses the set ([`Raft::propose_voters`]).
    /// - [`Error::Quorum`] when the holders are not a majority of the set once each
    ///   put ended, with each voter whose put failed and why.
    /// - [`Error::Pool`] from an earlier write of the log that finds no block, until
    ///   that write ends.
    /// - [`Error::Stopped`], [`Error::Tree`], and [`Error::Blob`] before the proposal.
    ///
    /// The set is in force, and a majority of it may not hold the tree in force:
    ///
    /// - [`Error::Stopped`] when the group stops after the joint entry applied.
    /// - [`Error::Tree`] with [`tree::Error::Corrupt`] when a chunk of the tree in
    ///   force does not fit in a tree.
    /// - [`Error::Blob`] when a call of the store fails after the joint entry.
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
        public_keys(&self.group.borrow(), &voters)?;
        self.check_leader()?;
        let set = Voters {
            incoming: voters,
            outgoing: BTreeSet::new(),
        };
        let pointer = self.pointer();
        self.spread_tree(&set, pointer.root).await?;
        let at = {
            let attempt = self.attempt()?;
            let at = self
                .proposed(|group| {
                    let keys = public_keys(group, &set.incoming)?;
                    Ok(group.raft.propose_voters(keys)?)
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
        if self.pointer().root != pointer.root {
            self.spread_in_force(&set).await?;
        }
        Ok(at)
    }

    // `Stopped` when the group stopped, `NoVote` when this node is not a voter, and
    // `NotLeader` when it does not lead.
    fn check_leader(&self) -> Result<(), Error> {
        self.check_proposer()?;
        let group = self.group.borrow();
        if group.raft.role() != Role::Leader {
            let leader = group.raft.leader();
            return Err(raft::Error::NotLeader { leader }.into());
        }
        Ok(())
    }

    // Puts the tree of the pointer in force on each other voter of `set`, each
    // `RETRY`, until a majority of each half holds it. Ends on `Stopped`, `Blob`, or
    // `Tree(Corrupt)`. Warns at the first put that fails, then at most each `WARN`.
    pub(super) async fn spread_in_force(&self, set: &Voters) -> Result<(), Error> {
        // No warning goes before this time.
        let mut quiet: Option<Monotonic> = None;
        loop {
            let error = match self.spread_tree(set, self.pointer().root).await {
                Ok(()) => return Ok(()),
                Err(
                    error @ (Error::Quorum { .. }
                    | Error::Tree(tree::Error::Missing(_))),
                ) => error,
                Err(error) => return Err(error),
            };
            let now = self.clock.now();
            if quiet.is_none_or(|until| now >= until) {
                tracing::warn!(
                    "the new voters are in force, and the put of the spec tree in \
                     force on them runs again: {error}"
                );
                quiet = now.checked_add(WARN);
            }
            self.clock.sleep(RETRY).await;
            self.group.borrow().running()?;
        }
    }

    // Puts each chunk of the tree at `root`, from this node's store, on each other
    // voter of `set`.
    async fn spread_tree(&self, set: &Voters, root: Digest) -> Result<(), Error> {
        let mut chunks = Chunks::default();
        let read = |chunks: &Chunks| {
            tree::diff(chunks, tree::empty(), root).map(|diff| diff.chunks)
        };
        let listed = fetched(&self.store, &mut chunks, read)
            .await?
            .map_err(Error::Tree)?;
        self.spread(set, &listed).await.map(|_| ())
    }
}

// The public key of each of `voters` in the applied state of `group`, or `NotMember`
// of the first that is not a member.
fn public_keys(
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

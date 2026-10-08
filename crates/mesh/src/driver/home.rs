//! Sets the home of an index through the leader of the group.

use types::{channel, node};

use super::Mesh;
use crate::change::Change;
use crate::error::Error;

impl Mesh {
    /// Makes `home` the home of `index`. A follower forwards it to the leader. It
    /// returns when an entry that sets it has committed and this node applied it; a
    /// later entry may change it again. It proposes again when a new leader replaces
    /// the entry. It has no time limit: while no leader takes the change, it tries
    /// again after each tick. A drop of the future ends the call, but a leader that
    /// took the change can still commit it. A try that gives up resets its stream,
    /// but a proposal that the network delivers late, before the reset, can still
    /// apply after a later call returned, and set the older home again.
    ///
    /// # Errors
    ///
    /// [`Error::NoVote`] when this node is not a voter, [`Error::NotMember`] when
    /// `home` is not a member of the region, and [`Error::Stopped`] when the group
    /// stopped. `NoVote` reads the configuration of the log of this node, which
    /// changes when the node appends a change of voters, before the commit: a new
    /// leader that replaces that entry changes the result back. `NotMember` reads
    /// what this node applied, so a node that has not applied a join yet gives it.
    pub async fn set_home(
        &self,
        index: channel::Key,
        home: node::Key,
    ) -> Result<(), Error> {
        loop {
            let attempt = self.attempt()?;
            if self.group.borrow().state.member(home).is_none() {
                return Err(Error::NotMember(home));
            }
            if attempt
                .settle(Change::Home { index, home })
                .await?
                .is_some()
            {
                return Ok(());
            }
        }
    }
}

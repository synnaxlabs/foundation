//! Sets the home of an index through the leader of the group.

use std::future::poll_fn;
use std::pin::pin;
use std::task::Poll;

use block::Pool;
use raft::Position;
use transport::{Class, Session};
use types::{channel, node};

use super::{Mesh, TICK, header};
use crate::applied::{Floor, Outcome};
use crate::bytes::block;
use crate::change::Change;
use crate::error::Error;
use crate::message::Message;

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
            let Some(at) = attempt.place(Change::Home { index, home }).await? else {
                drop(attempt);
                self.clock.sleep(TICK).await;
                continue;
            };
            if attempt.applied(at).await? {
                return Ok(());
            }
        }
    }

    // Starts one try of a proposal, when the group runs and this node is a voter.
    fn attempt(&self) -> Result<Try<'_>, Error> {
        let mut group = self.group.borrow_mut();
        group.running()?;
        if !group.voter(group.raft.key()) {
            return Err(Error::NoVote);
        }
        Ok(Try {
            mesh: self,
            slot: group.slot(),
            floor: group.applied.open(),
        })
    }
}

// Sends `change` to the leader on a new stream of `session`, and gives the position
// in its answer. `None` when the peer did not propose it, or the stream failed.
async fn forward(session: &Session, pool: &Pool, change: Change) -> Option<Position> {
    let (mut sender, mut receiver) = session.open(Class::Command).await.ok()?;
    sender.send(header(pool).ok()?).await.ok()?;
    let proposal = Message::Propose { change };
    sender
        .send(block(pool, &proposal.encode()).ok()?)
        .await
        .ok()?;
    // The stream ends only after the answer. A try that drops before it resets the
    // stream, so the transport sends a proposal that the leader did not get no more.
    let answer = receiver.recv().await.ok()??;
    // The answer stands, whatever the end of the stream gives.
    drop(sender.finish());
    match Message::decode(&answer)? {
        Message::Proposed { at } => Some(at),
        Message::Raft(_) | Message::Propose { .. } | Message::NotLeader { .. } => None,
    }
}

// One try of a call. It holds its floor in the group until it drops.
struct Try<'a> {
    mesh: &'a Mesh,
    // The key of the waker of the call in the group.
    slot: u64,
    floor: Floor,
}

impl Try<'_> {
    // Gives `change` to the leader, and gives the position of its entry there, or
    // `None` when no leader took it.
    async fn place(&self, change: Change) -> Result<Option<Position>, Error> {
        let mesh = self.mesh;
        match mesh.propose(change.clone()).await {
            Ok(at) => return Ok(Some(at)),
            Err(Error::Raft(raft::Error::NotLeader { .. })) => {}
            // The group takes a proposal again when the write of its log ends.
            Err(Error::Pool(_)) => return Ok(None),
            Err(error) => return Err(error),
        }
        // Only the task that sends to the leader dials it. A follower sends to its
        // leader in each tick, so with no session the leader cannot be reached.
        let (lead, session) = {
            let group = mesh.group.borrow();
            let lead = (group.raft.leader(), group.raft.term());
            let session = lead.0.and_then(|leader| group.sessions.get(&leader));
            let Some(session) = session.cloned() else {
                return Ok(None);
            };
            (lead, session)
        };
        let mut forward = pin!(forward(&session, &mesh.pool, change));
        // The leader answers or ends the stream while it leads, so only a change of
        // the lead ends the wait with no answer. The answer is polled first, so a
        // change of the lead in the same poll drops no answer.
        poll_fn(|cx| {
            if let Poll::Ready(at) = forward.as_mut().poll(cx) {
                return Poll::Ready(Ok(at));
            }
            let mut group = mesh.group.borrow_mut();
            group.running()?;
            if (group.raft.leader(), group.raft.term()) != lead {
                return Poll::Ready(Ok(None));
            }
            group.calls.insert(self.slot, cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    // Waits until this node knows what became of the entry at `at`, and gives
    // whether it applied that entry.
    async fn applied(&self, at: Position) -> Result<bool, Error> {
        poll_fn(|cx| {
            let mut group = self.mesh.group.borrow_mut();
            match group.applied.outcome(&self.floor, at) {
                Outcome::Applied => return Poll::Ready(Ok(true)),
                Outcome::Replaced => return Poll::Ready(Ok(false)),
                Outcome::Pending => {}
            }
            group.running()?;
            group.calls.insert(self.slot, cx.waker().clone());
            Poll::Pending
        })
        .await
    }
}

impl Drop for Try<'_> {
    fn drop(&mut self) {
        let mut group = self.mesh.group.borrow_mut();
        group.calls.remove(&self.slot);
        group.applied.close(&self.floor);
    }
}

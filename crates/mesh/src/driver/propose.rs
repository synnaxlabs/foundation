//! Proposes a change through the leader of the group, one try at a time.

use std::future::poll_fn;
use std::pin::pin;
use std::task::Poll;

use block::Pool;
use raft::Position;
use transport::{Class, Session};

use super::{Mesh, TICK, header};
use crate::applied::{Floor, Outcome};
use crate::bytes::block;
use crate::change::Change;
use crate::error::Error;
use crate::message::Message;
use crate::region::Refused;

impl Mesh {
    // Starts one try of a proposal, when the group runs and this node is a voter.
    pub(super) fn attempt(&self) -> Result<Try<'_>, Error> {
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
pub(super) struct Try<'a> {
    mesh: &'a Mesh,
    // The key of the waker of the call in the group.
    slot: u64,
    floor: Floor,
}

impl Try<'_> {
    // Gives `change` to the leader, and waits until this node knows what became of
    // its entry. Gives what the apply of that entry gave, or `None` when the call
    // tries again: a new leader replaced the entry, or no leader took it, and then
    // only after a tick.
    pub(super) async fn settle(
        self,
        change: Change,
    ) -> Result<Option<Result<(), Refused>>, Error> {
        let Some(at) = self.place(change).await? else {
            let mesh = self.mesh;
            // The floor closes before the sleep, so the group can trim.
            drop(self);
            mesh.clock.sleep(TICK).await;
            return Ok(None);
        };
        self.applied(at).await
    }

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

    // Waits until this node knows what became of the entry at `at`. Gives what the
    // apply of that entry gave, or `None` when a new leader replaced it.
    async fn applied(
        &self,
        at: Position,
    ) -> Result<Option<Result<(), Refused>>, Error> {
        poll_fn(|cx| {
            let mut group = self.mesh.group.borrow_mut();
            match group.applied.outcome(&self.floor, at) {
                Outcome::Applied(applied) => return Poll::Ready(Ok(Some(applied))),
                Outcome::Replaced => return Poll::Ready(Ok(None)),
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

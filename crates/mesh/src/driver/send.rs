//! Sends the `raft` messages of a group to each member.

use std::cell::RefCell;
use std::future::poll_fn;
use std::mem;
use std::pin::pin;
use std::rc::{Rc, Weak};
use std::task::Poll;

use block::Pool;
use env::tasks::Tasks;
use transport::stream::Sender;
use transport::{Class, Session, Transport};
use types::node;

use super::{Group, header};
use crate::bytes::block;
use crate::message::Message;

/// What sends the messages of one group: one task for each member, so a member that
/// is slow holds only its own messages. Each task holds the group weakly, and ends
/// when the group stops or drops.
#[derive(Clone)]
pub(super) struct Senders {
    pub(super) group: Weak<RefCell<Group>>,
    pub(super) transport: Rc<Transport>,
    pub(super) pool: Rc<Pool>,
    pub(super) tasks: Tasks,
}

// Why a message did not go.
enum Failure {
    // The pool had no block for the message, or for the header of its stream.
    Pool,
    // The group has no record of the member, so no address to dial.
    Unknown,
    Transport(transport::Error),
}

impl From<block::Error> for Failure {
    fn from(_: block::Error) -> Self {
        Self::Pool
    }
}

impl From<transport::Error> for Failure {
    fn from(error: transport::Error) -> Self {
        Self::Transport(error)
    }
}

impl Senders {
    /// Spawns the task of each member at the first message for it.
    pub(super) async fn run(self) {
        loop {
            let fresh = poll_fn(|cx| {
                let Some(group) = self.running() else {
                    return Poll::Ready(None);
                };
                let mut group = group.borrow_mut();
                if group.fresh.is_empty() {
                    group.starter = Some(cx.waker().clone());
                    return Poll::Pending;
                }
                Poll::Ready(Some(mem::take(&mut group.fresh)))
            });
            let Some(fresh) = fresh.await else { return };
            for to in fresh {
                self.tasks.spawn(self.clone().send(to));
            }
        }
    }

    // Sends each message for `to`. A message that fails drops, with the part that
    // failed, the stream or the session of the group: `raft` sends again.
    async fn send(self, to: node::Key) {
        let mut stream = None;
        while let Some(message) = self.next(to).await {
            let pass = self.pass(to, &mut stream, message);
            let Some(sent) = self.alive(to, pass).await else {
                return;
            };
            let Err(failure) = sent else { continue };
            match failure {
                Failure::Pool | Failure::Unknown => {}
                Failure::Transport(error) => match error {
                    transport::Error::TooLarge { .. } => {}
                    transport::Error::Stopped { .. } => stream = None,
                    // The session failed, or no dial gave one. Other protocols can
                    // use the session, so only the handle drops.
                    transport::Error::Unreachable { .. }
                    | transport::Error::Closed { .. }
                    | transport::Error::PeerClosed { .. }
                    | transport::Error::TimedOut
                    | transport::Error::Broken { .. }
                    | transport::Error::Network { .. } => {
                        stream = None;
                        self.group().borrow_mut().sessions.remove(&to);
                    }
                    // A stream gives `Reset` only after a dropped send, and this
                    // task ends when it drops one. Only a bind gives `Config`. Only
                    // an attempt of a dial gives `Unroutable` or `Authentication`,
                    // inside `Unreachable`.
                    transport::Error::Reset { .. }
                    | transport::Error::Config { .. }
                    | transport::Error::Unroutable
                    | transport::Error::Authentication { .. } => {
                        unreachable!("invariant: a send of the mesh gives no {error}")
                    }
                },
            }
        }
    }

    // The next message for `to`, or `None` when the group stopped or dropped.
    async fn next(&self, to: node::Key) -> Option<raft::Message> {
        poll_fn(|cx| {
            let Some(group) = self.running() else {
                return Poll::Ready(None);
            };
            group.borrow_mut().outgoing(to, cx).map(Some)
        })
        .await
    }

    // What `future` gives, or `None` as soon as the group stops or drops: a send can
    // wait for its peer with no bound.
    async fn alive<F: Future>(&self, to: node::Key, future: F) -> Option<F::Output> {
        let mut future = pin!(future);
        poll_fn(|cx| {
            let Some(group) = self.running() else {
                return Poll::Ready(None);
            };
            group.borrow_mut().queue(to).waker = Some(cx.waker().clone());
            future.as_mut().poll(cx).map(Some)
        })
        .await
    }

    // The group, while it lives and runs.
    fn running(&self) -> Option<Rc<RefCell<Group>>> {
        let group = self.group.upgrade()?;
        let running = group.borrow().running().is_ok();
        running.then_some(group)
    }

    // The group, in a step of the send of a message: `alive` found it at the last
    // poll, and no other task ran since.
    fn group(&self) -> Rc<RefCell<Group>> {
        let group = self.group.upgrade();
        group.expect("invariant: the group lives in each step of a send")
    }

    // Sends `message` on `stream`. With no stream it opens one, and with no session
    // to `to` in the group it dials first. The header of the protocol goes first on
    // a stream, which `stream` holds only after. Each block is taken just before its
    // send: a dial can wait until it times out.
    async fn pass(
        &self,
        to: node::Key,
        stream: &mut Option<Sender>,
        message: raft::Message,
    ) -> Result<(), Failure> {
        let sender = if let Some(sender) = stream {
            sender
        } else {
            let held = self.group().borrow().sessions.get(&to).cloned();
            let session = match held {
                Some(session) => session,
                None => self.dial(to).await?,
            };
            let mut sender = session.open_sender(Class::Command).await?;
            sender.send(header(&self.pool)?).await?;
            stream.insert(sender)
        };
        let block = block(&self.pool, &Message::Raft(message).encode())?;
        Ok(sender.send(block).await?)
    }

    // Dials `to` at the addresses of its card, as the group holds it now, and gives
    // the session to the group.
    async fn dial(&self, to: node::Key) -> Result<Session, Failure> {
        let (public_key, addresses) = {
            let group = self.group();
            let group = group.borrow();
            let member = group.state.member(to).ok_or(Failure::Unknown)?;
            let addresses = member.card.card().addresses.as_slice().to_vec();
            (member.public_key(), addresses)
        };
        let session = self.transport.dial(public_key, &addresses).await?;
        self.group()
            .borrow_mut()
            .sessions
            .insert(to, session.clone());
        Ok(session)
    }
}

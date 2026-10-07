//! Sends the `raft` messages of a group to each member.

use std::cell::RefCell;
use std::future::poll_fn;
use std::mem;
use std::pin::pin;
use std::rc::{Rc, Weak};
use std::task::Poll;

use block::{Block, Pool};
use env::tasks::Tasks;
use transport::stream::Sender;
use transport::{Class, Session, Transport};
use types::node;
use wire::Protocol;

use super::Group;
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

// The way to one member: a session to it, and one stream of that session.
#[derive(Default)]
struct Link {
    session: Option<Session>,
    stream: Option<Sender>,
}

// Why a message did not go.
enum Failure {
    // The pool had no block for the message, or for the header of its stream.
    Pool,
    // The group has no record of the member, so no address to dial.
    Unknown,
    Transport(transport::Error),
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

    // Sends each message for `to`. A message that fails drops, with the part of the
    // link that failed: `raft` sends again.
    async fn send(self, to: node::Key) {
        let mut link = Link::default();
        while let Some(message) = self.next(to).await {
            let pass = self.pass(to, &mut link, message);
            let Some(sent) = self.alive(to, pass).await else {
                return;
            };
            let Err(failure) = sent else { continue };
            match failure {
                Failure::Pool | Failure::Unknown => {}
                Failure::Transport(error) => match error {
                    transport::Error::TooLarge { .. } => {}
                    transport::Error::Stopped { .. } => link.stream = None,
                    // The session failed, or no dial gave one. Other protocols can
                    // use the session, so only the handle drops.
                    transport::Error::Unreachable { .. }
                    | transport::Error::Unroutable
                    | transport::Error::Authentication { .. }
                    | transport::Error::Closed { .. }
                    | transport::Error::PeerClosed { .. }
                    | transport::Error::TimedOut
                    | transport::Error::Broken { .. }
                    | transport::Error::Network { .. } => link = Link::default(),
                    // A stream gives `Reset` only after a dropped send, and this
                    // task ends when it drops one. Only a bind gives `Config`.
                    transport::Error::Reset { .. }
                    | transport::Error::Config { .. } => {
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

    // Sends `message` on the stream of `link`. With no stream it opens one, and with
    // no session it dials first. The header of the protocol goes first on a stream,
    // which `link` holds only after. Each block is taken just before its send: a
    // dial can wait until it times out.
    async fn pass(
        &self,
        to: node::Key,
        link: &mut Link,
        message: raft::Message,
    ) -> Result<(), Failure> {
        let sender = if let Some(sender) = &mut link.stream {
            sender
        } else {
            let session = match &link.session {
                Some(session) => session,
                None => link.session.insert(self.dial(to).await?),
            };
            let mut sender = session.open_sender(Class::Command).await?;
            let header = self.block(&wire::header::encode(Protocol::Mesh))?;
            sender.send(header).await?;
            link.stream.insert(sender)
        };
        let block = self.block(&Message::Raft(message).encode())?;
        Ok(sender.send(block).await?)
    }

    // A block of the pool that holds `bytes`.
    fn block(&self, bytes: &[u8]) -> Result<Block, Failure> {
        let Ok(mut block) = self.pool.alloc(bytes.len()) else {
            return Err(Failure::Pool);
        };
        block.copy_from_slice(bytes);
        Ok(block.freeze())
    }

    // Dials `to` at the addresses of its card, as the group holds it now.
    async fn dial(&self, to: node::Key) -> Result<Session, Failure> {
        let (public_key, addresses) = {
            let group = self.group.upgrade();
            let group = group.expect("invariant: `alive` holds the group for a dial");
            let group = group.borrow();
            let member = group.state.member(to).ok_or(Failure::Unknown)?;
            let addresses = member.card.card().addresses.as_slice().to_vec();
            (member.public_key(), addresses)
        };
        Ok(self.transport.dial(public_key, &addresses).await?)
    }
}

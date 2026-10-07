//! Sends the `raft` messages of a group to each member.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::future::poll_fn;
use std::rc::{Rc, Weak};
use std::task::Poll;

use block::{Block, Pool};
use env::tasks::Tasks;
use transport::stream::Sender;
use transport::{Class, Session, Transport};
use types::node;
use wire::Protocol;

use super::Group;
use crate::error::{Error, Stopped};
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
    sender: Option<Sender>,
}

impl Senders {
    /// Spawns the task of each member at the first message for it.
    pub(super) async fn run(self) {
        let mut started = BTreeSet::new();
        loop {
            let fresh = poll_fn(|cx| {
                let Some(group) = self.group.upgrade() else {
                    return Poll::Ready(None);
                };
                let mut group = group.borrow_mut();
                if group.running().is_err() {
                    return Poll::Ready(None);
                }
                let members = group.queues.keys().copied();
                let fresh: Vec<_> =
                    members.filter(|to| !started.contains(to)).collect();
                if fresh.is_empty() {
                    group.senders = Some(cx.waker().clone());
                    return Poll::Pending;
                }
                Poll::Ready(Some(fresh))
            });
            let Some(fresh) = fresh.await else { return };
            for to in fresh {
                started.insert(to);
                self.tasks.spawn(self.clone().send(to));
            }
        }
    }

    // Sends each message for `to`. A message that fails drops, with the part of the
    // link that failed: `raft` sends again.
    async fn send(self, to: node::Key) {
        let mut link = Link::default();
        while let Some(message) = self.next(to).await {
            let Err(error) = self.forward(to, &mut link, message).await else {
                continue;
            };
            match error {
                Error::Pool(_) | Error::Stream(transport::Error::TooLarge { .. }) => {}
                Error::Stream(
                    transport::Error::Reset { .. } | transport::Error::Stopped { .. },
                ) => link.sender = None,
                // The session failed, or no dial gave one. Other protocols can use
                // the session, so only the handle drops.
                _ => link = Link::default(),
            }
        }
    }

    // The next message for `to`, or `None` when the group stopped or dropped.
    async fn next(&self, to: node::Key) -> Option<raft::Message> {
        poll_fn(|cx| {
            let Some(group) = self.group.upgrade() else {
                return Poll::Ready(None);
            };
            group.borrow_mut().outgoing(to, cx).map(Result::ok)
        })
        .await
    }

    // Sends `message` on the stream of `link`. With no stream it opens one and sends
    // the header of the protocol, and with no session it dials first.
    async fn forward(
        &self,
        to: node::Key,
        link: &mut Link,
        message: raft::Message,
    ) -> Result<(), Error> {
        let block = self.block(&Message::Raft(message).encode())?;
        let sender = if let Some(sender) = &mut link.sender {
            sender
        } else {
            let header = self.block(&wire::header::encode(Protocol::Mesh))?;
            let session = match &link.session {
                Some(session) => session,
                None => link.session.insert(self.dial(to).await?),
            };
            let mut sender = session.open_sender(Class::Command).await?;
            sender.send(header).await?;
            link.sender.insert(sender)
        };
        Ok(sender.send(block).await?)
    }

    // A block of the pool that holds `bytes`.
    fn block(&self, bytes: &[u8]) -> Result<Block, Error> {
        let mut block = self.pool.alloc(bytes.len()).map_err(Error::Pool)?;
        block.copy_from_slice(bytes);
        Ok(block.freeze())
    }

    // Dials `to` at the addresses of its card, as the group holds it now.
    async fn dial(&self, to: node::Key) -> Result<Session, Error> {
        let (public_key, addresses) = {
            let group = self.group.upgrade().ok_or(Stopped::Dropped);
            let group = group.map_err(Error::Stopped)?;
            let group = group.borrow();
            let member = group.state.member(to).ok_or(Error::NotMember(to))?;
            let addresses = member.card.card().addresses.as_slice().to_vec();
            (member.public_key(), addresses)
        };
        Ok(self.transport.dial(public_key, &addresses).await?)
    }
}

//! The sessions of a group to its members.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use transport::{Session, Transport};
use types::node;

use super::Group;
use super::send::Failure;

/// The session of the group to `to`, or else a new one that it dials at the
/// addresses of the card of `to` and keeps. The group lives in each poll.
pub(super) async fn session(
    group: &Weak<RefCell<Group>>,
    transport: &Transport,
    to: node::Key,
) -> Result<Session, Failure> {
    let (public_key, addresses) = {
        let group = live(group);
        let group = group.borrow();
        if let Some(session) = group.sessions.get(&to) {
            return Ok(session.clone());
        }
        let member = group.state.member(to).ok_or(Failure::Unknown)?;
        let addresses = member.card.card().addresses.as_slice().to_vec();
        (member.public_key(), addresses)
    };
    let session = transport.dial(public_key, &addresses).await?;
    live(group)
        .borrow_mut()
        .sessions
        .insert(to, session.clone());
    Ok(session)
}

fn live(group: &Weak<RefCell<Group>>) -> Rc<RefCell<Group>> {
    let group = group.upgrade();
    group.expect("invariant: the group lives in each poll of a call on a peer")
}

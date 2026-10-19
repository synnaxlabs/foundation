//! The sessions of a group to its members, and the calls of `blob::peer` on them.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::future::poll_fn;
use std::pin::{Pin, pin};
use std::rc::{Rc, Weak};
use std::task::Poll;

use env::clock::Clock;
use raft::Voters;
use transport::{Session, Transport};
use types::digest::Digest;
use types::node;
use types::time::Span;

use super::send::Failure;
use super::{Group, Mesh};

/// How long one get or put of chunks on a peer may take, its dial included.
pub(super) const DEADLINE: Span = Span::from_nanos(10 * Span::SECOND.nanos());

/// The most chunks that one put on a peer carries, so that a call ends within
/// [`DEADLINE`] on a slow link.
pub(super) const CALL: usize = 32;

impl Mesh {
    /// Puts each chunk of `digests` of the store on each voter of `voters` but this
    /// node, all at once, and gives the holders: this node when it is a voter, and
    /// each voter whose put ended `Ok` within [`DEADLINE`]. Returns once the holders
    /// are a majority of each half of `voters`, and drops the puts still open, or once
    /// each put has ended. A voter counts only while a half that holds it lacks a
    /// majority, so the holders are at most the sum of the majorities of the halves.
    pub(super) async fn spread(
        &self,
        voters: &Voters,
        digests: &[Digest],
    ) -> BTreeSet<node::Key> {
        let own = self.group.borrow().raft.key();
        let mut holders = BTreeSet::new();
        if voters.contains(own) {
            holders.insert(own);
        }
        let group = Rc::downgrade(&self.group);
        let mut puts: Vec<_> = voters
            .nodes()
            .filter(|&key| key != own)
            .map(|key| (key, Box::pin(self.put_on(&group, key, digests))))
            .collect();
        poll_fn(|cx| {
            puts.retain_mut(|(key, put)| {
                if quorate(voters, &holders) {
                    return true;
                }
                let Poll::Ready(stored) = put.as_mut().poll(cx) else {
                    return true;
                };
                if stored && short(voters, &holders, *key) {
                    holders.insert(*key);
                }
                false
            });
            if puts.is_empty() || quorate(voters, &holders) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        holders
    }

    // Puts `digests` on `to` in calls of at most `CALL` chunks, in order, and gives
    // whether each call ended `Ok` within `DEADLINE`, the first with the dial. Any
    // failure, of the dial, the stream, or the peer, leaves `to` no holder.
    async fn put_on(
        &self,
        group: &Weak<RefCell<Group>>,
        to: node::Key,
        digests: &[Digest],
    ) -> bool {
        for call in digests.chunks(CALL) {
            let put = async {
                let session = session(group, &self.transport, to).await.ok()?;
                blob::peer::put(&self.store, &session, call).await.ok()
            };
            if !matches!(deadline(&self.clock, put).await, Some(Some(()))) {
                return false;
            }
        }
        true
    }
}

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

// The output of `future`, or `None` when `DEADLINE` passes first.
async fn deadline<F: Future>(clock: &Clock, future: F) -> Option<F::Output> {
    let (mut future, mut sleep) = (pin!(future), clock.sleep(DEADLINE));
    poll_fn(|cx| {
        if let Poll::Ready(output) = future.as_mut().poll(cx) {
            return Poll::Ready(Some(output));
        }
        Pin::new(&mut sleep).poll(cx).map(|()| None)
    })
    .await
}

fn quorate(voters: &Voters, holders: &BTreeSet<node::Key>) -> bool {
    voters.quorum(|key| holders.contains(&key)).is_ok()
}

// Whether `key` is in a half of `voters` that lacks a majority of `holders`.
fn short(voters: &Voters, holders: &BTreeSet<node::Key>, key: node::Key) -> bool {
    let lacks = |half: &BTreeSet<node::Key>| {
        let alone = Voters {
            incoming: half.clone(),
            outgoing: BTreeSet::new(),
        };
        half.contains(&key) && !quorate(&alone, holders)
    };
    lacks(&voters.incoming) || lacks(&voters.outgoing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::key;

    fn keys(ids: &[u8]) -> BTreeSet<node::Key> {
        ids.iter().copied().map(key).collect()
    }

    #[test]
    fn a_voter_counts_only_while_a_half_that_holds_it_lacks_a_majority() {
        let voters = Voters {
            incoming: keys(&[1, 2, 3]),
            outgoing: keys(&[3, 4, 5]),
        };
        assert!(short(&voters, &keys(&[1]), key(2)));
        assert!(short(&voters, &keys(&[1, 2]), key(3)));
        assert!(!short(&voters, &keys(&[1, 2, 4, 5]), key(3)));
        assert!(!short(&voters, &keys(&[1, 2, 4]), key(9)));
        let alone = Voters {
            incoming: keys(&[1, 2, 3]),
            outgoing: BTreeSet::new(),
        };
        assert!(short(&alone, &keys(&[1]), key(2)));
        assert!(!short(&alone, &keys(&[1, 2]), key(3)));
    }
}

//! The calls of `blob::peer` on the members of a group.

use std::collections::{BTreeMap, BTreeSet};
use std::future::poll_fn;
use std::pin::{Pin, pin};
use std::task::Poll;

use env::clock::Clock;
use raft::Voters;
use transport::Session;
use types::digest::Digest;
use types::node;
use types::time::Span;

use super::{Call, Mesh};
use crate::error::{Error, Miss, Missed};

/// How long one get or put of chunks on a peer may take, its dial included.
pub(super) const DEADLINE: Span = Span::from_nanos(10 * Span::SECOND.nanos());

/// The most chunks that one put on a peer carries, so that a call of the largest
/// chunks ends within [`DEADLINE`] over links of 100 ms.
pub(crate) const CHUNKS: usize = 16;

impl Mesh {
    /// Puts each chunk of `digests` of the store on each voter of `voters` but this
    /// node, all at once, and gives the holders: this node when it is a voter, and
    /// each voter whose put ended `Ok` within [`DEADLINE`]. Returns once the holders
    /// are a majority of each half of `voters`, and drops the puts still open. A
    /// voter counts only while a half that holds it lacks a majority, so the holders
    /// are at most the sum of the majorities of the halves.
    ///
    /// `Stopped` as soon as the group stops, and `Quorum` for the first half that
    /// lacks a majority once each put ended, with each voter whose put failed.
    pub(super) async fn spread(
        &self,
        voters: &Voters,
        digests: &[Digest],
    ) -> Result<BTreeSet<node::Key>, Error> {
        let own = self.group.borrow().raft.key();
        let mut holders = BTreeSet::new();
        if voters.contains(own) {
            holders.insert(own);
        }
        let mut puts: Vec<_> = voters
            .nodes()
            .filter(|&key| key != own)
            .map(|key| (key, Box::pin(self.put_on(key, digests))))
            .collect();
        let mut missed = BTreeMap::new();
        let call = Call::new(&self.group);
        poll_fn(|cx| -> Poll<Result<(), Error>> {
            self.group.borrow().running()?;
            puts.retain_mut(|(key, put)| {
                let Poll::Ready(stored) = put.as_mut().poll(cx) else {
                    return true;
                };
                match stored {
                    Ok(()) if short(voters, &holders, *key) => {
                        holders.insert(*key);
                    }
                    Ok(()) => {}
                    Err(miss) => {
                        missed.insert(*key, miss);
                    }
                }
                false
            });
            if puts.is_empty() || quorate(voters, &holders) {
                return Poll::Ready(Ok(()));
            }
            let mut group = self.group.borrow_mut();
            group.calls.insert(call.slot, cx.waker().clone());
            Poll::Pending
        })
        .await?;
        if let Err(short) = voters.quorum(|key| holders.contains(&key)) {
            let group = self.group.borrow();
            let missed = missed.into_iter().map(|(node, cause)| Missed {
                node,
                name: group
                    .state
                    .member(node)
                    .map(|member| member.card.card().name.clone()),
                cause,
            });
            return Err(Error::Quorum {
                held: short.counted,
                voters: short.voters,
                missed: missed.collect(),
            });
        }
        Ok(holders)
    }

    // Puts `digests` on `to` in calls of at most `CHUNKS` chunks, in order, and gives
    // why the first call that did not end `Ok` within `DEADLINE`, its dials included,
    // failed. A call that failed on a session that ended dials once more: a session
    // to a process of `to` that restarted ends only at its first use.
    async fn put_on(&self, to: node::Key, digests: &[Digest]) -> Result<(), Miss> {
        for call in digests.chunks(CHUNKS) {
            let put = async {
                let session = self.dial(to).await?;
                match blob::peer::put(&self.store, &session, call).await {
                    Err(_) if session.ended().is_some() => {}
                    first => return first.map_err(Miss::Put),
                }
                let session = self.dial(to).await?;
                let put = blob::peer::put(&self.store, &session, call).await;
                put.map_err(Miss::Put)
            };
            within(&self.clock, DEADLINE, put)
                .await
                .unwrap_or(Err(Miss::Deadline))?;
        }
        Ok(())
    }

    // The session of the transport to member `to`, which it dials at the addresses
    // of the card of `to` when it has none open.
    async fn dial(&self, to: node::Key) -> Result<Session, Miss> {
        let contact = self.group.borrow().contact(to);
        let (public_key, addresses) = contact.ok_or(Miss::Unknown)?;
        let dialed = self.transport.dial(public_key, &addresses).await;
        dialed.map_err(Miss::Dial)
    }
}

/// The output of `future`, or `None` when `span` passes first.
pub(super) async fn within<F: Future>(
    clock: &Clock,
    span: Span,
    future: F,
) -> Option<F::Output> {
    let (mut future, mut sleep) = (pin!(future), clock.sleep(span));
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

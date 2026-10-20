//! The calls of `blob::peer` on the members of a group.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::future::poll_fn;
use std::pin::{Pin, pin};
use std::task::Poll;

use env::clock::Clock;
use raft::Voters;
use transport::{Address, Session, Transport};
use types::digest::Digest;
use types::ed25519::PublicKey;
use types::node;
use types::time::Span;

use super::{Call, Mesh};
use crate::error::{Error, Miss, Missed};

/// How long one get of chunks from a peer may take, its dial included, and how long
/// each step of a put on a peer may take.
pub(crate) const DEADLINE: Span = Span::from_nanos(10 * Span::SECOND.nanos());

/// The most chunks that one get from a peer asks for, so that a call of the largest
/// chunks ends within [`DEADLINE`] over links of 100 ms.
pub(crate) const CHUNKS: usize = 16;

impl Mesh {
    /// Puts each chunk of `digests` of the store on each voter of `voters` but this
    /// node, all at once, and gives the holders: this node when it is a voter, and
    /// each voter whose put ended `Ok`, each step within [`DEADLINE`]. Returns once
    /// the holders are a majority of each half of `voters`, and drops the puts still
    /// open. A voter counts only while a half that holds it lacks a majority, so the
    /// holders are at most the sum of the majorities of the halves.
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

    // Puts `digests` on `to` on one stream, in order, and gives why it failed. Each
    // step fails after `DEADLINE`: the dial with the open of the stream, and the
    // `Stored` of each chunk. A step that fails on a session that ended dials once
    // more and goes on from its chunk. A session that stored a chunk earns one more
    // redial. So does one session that lost to a dial of the peer, at the start and
    // after each session that stored a chunk. So a peer that ends each session with
    // code 0 and stores nothing ends the put after at most two redials. No chunk
    // opens no stream. The card of `to` is read once.
    async fn put_on(&self, to: node::Key, digests: &[Digest]) -> Result<(), Miss> {
        let (public_key, addresses) =
            self.group.borrow().contact(to).ok_or(Miss::Unknown)?;
        if digests.is_empty() {
            return Ok(());
        }
        let mut left = digests;
        let (mut redial, mut raced) = (true, false);
        loop {
            let dialed = step(&self.clock, async {
                let dial = self.transport.dial(public_key, &addresses);
                let session = dial.await.map_err(Failed::Dial)?;
                let push = blob::peer::Push::open(&self.store, &session).await;
                Ok::<_, Failed>((session, push))
            });
            let (session, push) = dialed.await??;
            let before = left.len();
            let pushed = match push {
                Ok(push) => self.push(push, &mut left).await,
                Err(error) => Err(Failed::Call(error)),
            };
            let Err(failed) = pushed else {
                return Ok(());
            };
            match (&failed, session.ended()) {
                (Failed::Call(_), Some(_)) if left.len() < before => {
                    (redial, raced) = (false, false);
                }
                (Failed::Call(_), Some(ended)) if !raced && replaced(&ended) => {
                    raced = true;
                }
                (Failed::Call(_), Some(_)) if redial => redial = false,
                _ => return Err(failed.into()),
            }
        }
    }

    // Puts `left` on `push`, each step within `DEADLINE`, and takes each chunk that
    // the peer stored off `left`. Each chunk that got `Stored` is durable on the peer,
    // so the end of the stream, also past `DEADLINE`, cannot fail the put.
    async fn push(
        &self,
        mut push: blob::peer::Push<'_>,
        left: &mut &[Digest],
    ) -> Result<(), Failed> {
        while let Some((&digest, rest)) = left.split_first() {
            push = step(&self.clock, push.put(digest))
                .await?
                .map_err(Failed::Call)?;
            *left = rest;
        }
        match within(&self.clock, DEADLINE, push.finish()).await {
            Some(Ok(()) | Err(_)) | None => Ok(()),
        }
    }
}

/// The public key of a member and the addresses of its card.
pub(super) type Contact = (PublicKey, Vec<Address>);

/// Gets each chunk of `digests` from `peers`, in order, and puts it in `store`. Each
/// peer gets the chunks still missing in calls of at most [`CHUNKS`], each within
/// [`DEADLINE`]. A chunk that a peer does not give goes to the next peer, and a call
/// that fails or passes the deadline gives the rest to the next peer. `progress`
/// holds where the get is at each step, so that a caller that drops it knows.
///
/// # Errors
///
/// The error of `store` when it fails or refuses the put of a chunk. No later peer
/// is asked.
pub(super) async fn get(
    store: &blob::Store,
    transport: &Transport,
    clock: &Clock,
    digests: &[Digest],
    peers: &[(node::Key, Contact)],
    progress: &Cell<Progress>,
) -> Result<(), blob::Error> {
    let got = walk(store, transport, clock, digests, peers, progress).await;
    progress.set(Progress {
        asking: None,
        ..progress.get()
    });
    got
}

/// Where a get is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Progress {
    /// The peer that it asks, or `None` before the first and after the get ends.
    pub(super) asking: Option<node::Key>,
    /// The last peer that gave it a chunk, or `None` while no peer gave one.
    pub(super) gave: Option<node::Key>,
}

// The body of `get`, which leaves `asking` set when it returns.
async fn walk(
    store: &blob::Store,
    transport: &Transport,
    clock: &Clock,
    digests: &[Digest],
    peers: &[(node::Key, Contact)],
    progress: &Cell<Progress>,
) -> Result<(), blob::Error> {
    let mut missing = digests.to_vec();
    for (key, contact) in peers {
        let asking = Some(*key);
        progress.set(Progress {
            asking,
            ..progress.get()
        });
        let mut left = Vec::new();
        let mut calls = missing.chunks(CHUNKS);
        for chunks in calls.by_ref() {
            let get =
                async |session: &Session| blob::peer::get(store, session, chunks).await;
            match ask(transport, clock, contact, get).await {
                Ok(misses) => {
                    if misses.len() != chunks.len() {
                        progress.set(Progress {
                            asking,
                            gave: asking,
                        });
                    }
                    left.extend(misses.iter().map(|miss| miss.digest));
                }
                Err(Failed::Call(blob::peer::Error::Store(error))) => {
                    return Err(error);
                }
                Err(_) => {
                    left.extend(chunks);
                    break;
                }
            }
        }
        left.extend(calls.flatten());
        missing = left;
        if missing.is_empty() {
            break;
        }
    }
    Ok(())
}

// Why a call on a peer gave no result.
enum Failed {
    Dial(transport::Error),
    Call(blob::peer::Error),
    Deadline,
}

impl From<Failed> for Miss {
    fn from(failed: Failed) -> Self {
        match failed {
            Failed::Dial(error) => Self::Dial(error),
            Failed::Call(error) => Self::Put(error),
            Failed::Deadline => Self::Deadline,
        }
    }
}

// Runs `call` on the session of `transport` to the peer of `contact`, which it dials
// when it has none open, and fails when `DEADLINE` passes first, its dials included. A
// call that failed on a session that ended dials once more: a session to a process
// that restarted ends only at its first use.
async fn ask<T>(
    transport: &Transport,
    clock: &Clock,
    (public_key, addresses): &Contact,
    call: impl AsyncFn(&Session) -> Result<T, blob::peer::Error>,
) -> Result<T, Failed> {
    let redialed = async {
        let dial = async || transport.dial(*public_key, addresses).await;
        let session = dial().await.map_err(Failed::Dial)?;
        match call(&session).await {
            Err(_) if session.ended().is_some() => {}
            first => return first.map_err(Failed::Call),
        }
        let session = dial().await.map_err(Failed::Dial)?;
        call(&session).await.map_err(Failed::Call)
    };
    within(clock, DEADLINE, redialed)
        .await
        .unwrap_or(Err(Failed::Deadline))
}

// Whether a session that ended with `ended` lost to a session that the peer dialed at
// the same time, which the next dial gives.
fn replaced(ended: &transport::Error) -> bool {
    use transport::{Code, Error};
    matches!(
        ended,
        Error::Closed { code: Code(0) } | Error::PeerClosed { code: Code(0) }
    )
}

// The output of `future`, or `Failed::Deadline` when `DEADLINE` passes first.
async fn step<F: Future>(clock: &Clock, future: F) -> Result<F::Output, Failed> {
    within(clock, DEADLINE, future)
        .await
        .ok_or(Failed::Deadline)
}

/// The output of `future`, or `None` when `span` passes first.
pub(crate) async fn within<F: Future>(
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

    // It calls `short`, as `apply` cannot reach a joint configuration until #336.
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

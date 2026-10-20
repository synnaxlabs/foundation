//! Asks the voters of a ticket to admit this node to their region.

use std::fmt;
use std::rc::Rc;

use block::{Block, Pool};
use env::clock::Clock;
use transport::stream::Receiver;
use transport::{Class, Code, Transport};
use types::time::{Monotonic, Span};

use crate::card;
use crate::driver::{header, within};
use crate::error::Error;
use crate::message::Message;
use crate::refusal::Refusal;
use crate::region::{self, Request};
use crate::status;
use crate::ticket::{Ticket, Voter};

/// The most rounds of asks of the voters of a ticket.
const ROUNDS: u32 = 8;
/// The pause between two rounds: one election timeout.
const PAUSE: Span = Span::SECOND;
/// The most time that the dial of a voter and its answer take, and that each message
/// of a founding takes after the one before it.
const DEADLINE: Span = Span::from_nanos(10 * Span::SECOND.nanos());
/// The least rate, in bytes each second, at which a founding comes: the rate that the
/// chunk puts of a peer assume.
const RATE: i64 = 100 << 10;
/// The code of a reply half that carried a message that a join stream does not carry.
const MALFORMED: Code = Code(wire::header::MALFORMED);

/// What a node gives to ask to join a region by a ticket.
#[derive(Debug)]
pub struct Config {
    /// The ticket that admits the node.
    pub ticket: Ticket,
    /// The node's first card, which its private key signs.
    pub card: card::Signed,
    /// The names of the node's status channels.
    pub status: status::Names,
    /// Times the deadline of each ask and the pause between rounds.
    pub clock: Clock,
    /// Gives the blocks of the request.
    pub pool: Rc<Pool>,
    /// The transport of this shard. It proves the public key of `card`.
    pub transport: Rc<Transport>,
}

/// The most time that [`ask`] takes when no voter starts a founding: 8 x (10 s for
/// each voter of `ticket` + 1 s).
#[must_use]
pub fn bound(ticket: &Ticket) -> Span {
    let voters = i64::try_from(ticket.voters().len()).unwrap_or(i64::MAX);
    let round = DEADLINE.nanos().saturating_mul(voters);
    let rounds = i64::from(ROUNDS).saturating_mul(round.saturating_add(PAUSE.nanos()));
    Span::from_nanos(rounds)
}

/// Asks the voters of `config.ticket` to admit the node of `config.card`, and gives
/// the founding of the region once one admits it. [`Mesh::open`](crate::Mesh::open)
/// with it opens the node. It asks each voter in the ticket's order, in at most 8
/// rounds with a pause of 1 s between them. The dial of a voter, its answer, and the
/// end of a refusal come within 10 s. So voters that do not answer give
/// [`Error::Unjoined`] within [`bound`]. Each transfer of a founding that fails adds
/// at most 10 s + its bytes / (100 KiB/s), which is 665.36 s for the largest. A voter
/// admits a join once this node's `Join` has applied there, so a repeat of the same
/// request is admitted again.
///
/// # Errors
///
/// - [`Error::Unjoined`] when no voter admitted the node in the last round.
/// - [`Error::Pool`] when the pool has no block for the request.
///
/// # Panics
///
/// When `config.transport` proves a key that is not the public key of
/// `config.card`.
pub async fn ask(config: Config) -> Result<region::Founding, Error> {
    let Config {
        ticket,
        card,
        status,
        clock,
        pool,
        transport,
    } = config;
    assert_eq!(
        transport.public_key(),
        card.card().public_key,
        "invariant: the transport proves the public key of the card"
    );
    let request = Request {
        ticket: ticket.public_key(),
        admission: ticket.admission(&card),
        card: card.into(),
        status,
    };
    let join = Message::Join {
        request: Box::new(request),
    };
    let blocks = [header(&pool), pool.copy(&join.encode())];
    let [header, join] = blocks.map(|block| block.map_err(Error::Pool));
    let (header, join) = (header?, join?);
    let mut missed = Vec::new();
    for round in 0..ROUNDS {
        if round > 0 {
            clock.sleep(PAUSE).await;
        }
        missed.clear();
        for voter in ticket.voters() {
            let asked = ask_voter(&clock, &transport, voter, [&header, &join]).await;
            match asked {
                Ok(founding) => return Ok(founding),
                Err(miss) => missed.push((voter.key, miss)),
            }
        }
    }
    Err(Error::Unjoined { missed })
}

// Sends the header and the join on a new stream to `voter`, and reads the answer.
async fn ask_voter(
    clock: &Clock,
    transport: &Transport,
    voter: &Voter,
    messages: [&Block; 2],
) -> Result<region::Founding, Miss> {
    let end = after(clock, DEADLINE);
    let first = async {
        let addresses = voter.addresses.as_slice();
        let session = transport.dial(voter.public_key, addresses).await?;
        let (mut sender, mut receiver) = session.open(Class::Complete).await?;
        for message in messages {
            sender.send(message.clone()).await?;
        }
        sender.finish()?;
        let first = receiver.recv().await?;
        Ok((receiver, first))
    };
    let first = before(clock, end, first).await.ok_or(Miss::Deadline)?;
    let (mut receiver, first) = first.map_err(Miss::Stream)?;
    let Some(first) = first else {
        return Err(Miss::Malformed);
    };
    match Message::decode(&first) {
        Some(Message::Joined { bytes }) => founding(clock, receiver, bytes).await,
        Some(Message::Unjoined { refusal }) => {
            // The answer came, so only a message after it hides the refusal.
            match before(clock, end, receiver.recv()).await {
                Some(Ok(Some(_))) => {
                    receiver.stop(MALFORMED);
                    return Err(Miss::Malformed);
                }
                None => receiver.stop(MALFORMED),
                Some(Ok(None) | Err(_)) => {}
            }
            Err(Miss::from(refusal))
        }
        _ => {
            receiver.stop(MALFORMED);
            Err(Miss::Malformed)
        }
    }
}

// Reads the founding of `bytes` bytes in parts, each a `Founding` that is not empty,
// then the end of the reply half, all within 10 s + `bytes` / `RATE`.
async fn founding(
    clock: &Clock,
    mut receiver: Receiver,
    bytes: usize,
) -> Result<region::Founding, Miss> {
    let end = after(clock, transfer(bytes));
    let mut body = Vec::new();
    while body.len() < bytes {
        let next = before(clock, end, receiver.recv()).await;
        let next = next.ok_or(Miss::Deadline)?.map_err(Miss::Stream)?;
        let Some(next) = next else {
            return Err(Miss::Malformed);
        };
        match Message::decode(&next) {
            Some(Message::Founding { part })
                if !part.is_empty()
                    && (body.len().checked_add(part.len()))
                        .is_some_and(|len| len <= bytes) =>
            {
                body.extend(part);
            }
            _ => {
                receiver.stop(MALFORMED);
                return Err(Miss::Malformed);
            }
        }
    }
    let next = before(clock, end, receiver.recv()).await;
    if next.ok_or(Miss::Deadline)?.map_err(Miss::Stream)?.is_some() {
        receiver.stop(MALFORMED);
        return Err(Miss::Malformed);
    }
    region::Founding::decode(&body).ok_or(Miss::Malformed)
}

// The most time that a founding of `bytes` and the end of its half take: 10 s, and the
// time of `bytes` at `RATE`.
fn transfer(bytes: usize) -> Span {
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
    let nanos = bytes
        .saturating_mul(Span::SECOND.nanos())
        .saturating_div(RATE);
    Span::from_nanos(nanos.saturating_add(DEADLINE.nanos()))
}

// The time `span` after now.
fn after(clock: &Clock, span: Span) -> Monotonic {
    Monotonic(clock.now().0.saturating_add(span.nanos().unsigned_abs()))
}

// The output of `future`, or `None` when `end` or 10 s from now passes first.
async fn before<F: Future>(
    clock: &Clock,
    end: Monotonic,
    future: F,
) -> Option<F::Output> {
    let left = i64::try_from(end.0.saturating_sub(clock.now().0));
    let left = left.map_or(DEADLINE, Span::from_nanos).min(DEADLINE);
    within(clock, left, future).await
}

/// Why one ask of a voter gave no founding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Miss {
    /// The dial or the stream failed.
    Stream(transport::Error),
    /// The dial, an answer, or the end of the reply half did not come within its
    /// deadline.
    Deadline,
    /// The answer is not one that a join stream carries. The reply half stopped
    /// with code 2, or had ended.
    Malformed,
    /// No leader committed the join in the voter's one try.
    Uncommitted,
    /// The voter has no mesh time that can stamp a join.
    Unsynced,
    /// The node asked is not a voter.
    NoVote,
    /// The group of the voter stopped.
    Stopped,
    /// A member or an earlier join of the log holds the node key with another public
    /// key. Join with a new key.
    Contested,
    /// The region refuses the join, for its card, its ticket, or its fit.
    Refused(Refused),
}

impl From<Refusal> for Miss {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Refused(refused) => Self::Refused(Refused(refused)),
            Refusal::Contested => Self::Contested,
            Refusal::Unsynced => Self::Unsynced,
            Refusal::Unled => Self::Uncommitted,
            Refusal::NoVote => Self::NoVote,
            Refusal::Stopped => Self::Stopped,
        }
    }
}

impl fmt::Display for Miss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stream(error) => write!(f, "the stream failed: {error}"),
            Self::Deadline => f.write_str("no answer came within its deadline"),
            Self::Malformed => f.write_str("the answer is not one of a join stream"),
            Self::Uncommitted => f.write_str("no leader committed the join"),
            Self::Unsynced => f.write_str("the voter has no mesh time to stamp a join"),
            Self::NoVote => f.write_str("the node is not a voter"),
            Self::Stopped => f.write_str("the group of the voter stopped"),
            Self::Contested => f.write_str(
                "a member or an earlier join holds the node key with another public key",
            ),
            Self::Refused(refused) => write!(f, "the region refuses the join: {refused}"),
        }
    }
}

/// A refusal of the region, which gives its text (`Display`) and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused(region::Refused);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

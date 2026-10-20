//! Asks the voters of a ticket to admit this node to their region.

pub(crate) mod file;

use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;

use block::{Block, Pool};
use env::clock::Clock;
use env::files::{Files, Mode};
use transport::stream::Receiver;
use transport::{Class, Code, Transport};
use types::node;
use types::time::{Monotonic, Span};

use crate::card;
use crate::driver::{header, within};
use crate::error::Error;
use crate::file::Blocks;
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
/// The least rate at which a founding must come.
const RATE_BYTES_PER_S: i64 = 100 << 10;
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
    /// The file seam. `os` or `sim` implements it.
    pub files: Files,
    /// The mesh directory that `Mesh::open` takes as `Config::dir`. Its parent must
    /// be there and durable.
    pub dir: PathBuf,
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
/// Before its first ask, `ask` makes `config.dir` and keeps the request in the file
/// `join` in it, durable. The file holds the card and the ticket's admission, a
/// signature over this card only, and never the private key of the ticket. When the
/// file holds a request for the public key of `config.ticket` and the public key that
/// `config.transport` proves, `ask` sends that request, and does not send `config.card`
/// or `config.status`. Else it writes the new request over it. So a node that stops
/// after a voter applied its join, and before the answer came, is admitted at the next
/// call. When a round ends with a voter that refuses the stored request, and no voter
/// admitted it in this call, `ask` writes the request of `config.card` and
/// `config.status` over it and asks the next round with that, at once. The file stays
/// for the life of `config.dir`: each [`Mesh::open`](crate::Mesh::open) of a founding
/// that lacks this node reads it.
///
/// # Errors
///
/// - [`Error::Founded`] when `config.dir` holds a founding file.
/// - [`Error::Unjoined`] when no voter admitted the node in the last round.
/// - [`Error::JoinFile`] when `config.dir` holds a join file that does not read back
///   whole.
/// - [`Error::Pool`] when the pool has no block for the request or its file.
/// - [`Error::Files`] when a file call fails.
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
        files,
        dir,
    } = config;
    let own = transport.public_key();
    assert_eq!(
        own,
        card.card().public_key,
        "invariant: the transport proves the public key of the card"
    );
    if let Some(path) = crate::driver::founded(&files, &dir).await? {
        return Err(Error::Founded { path });
    }
    let blocks = Blocks::new(Rc::clone(&pool)).map_err(Error::Pool)?;
    file::durable(&files, &dir).await?;
    let fresh = Request {
        ticket: ticket.public_key(),
        admission: ticket.admission(&card),
        card: card.into(),
        status,
    };
    // The request that a round with a refusal of the stored one writes over it.
    let mut later = None;
    let request = match file::read(&files, &dir, &blocks, Mode::Write).await? {
        Some(stored)
            if stored.ticket == fresh.ticket && stored.card.card.public_key == own =>
        {
            later = Some(fresh).filter(|fresh| *fresh != stored);
            stored
        }
        _ => {
            file::write(&files, &dir, &blocks, &fresh).await?;
            fresh
        }
    };
    let header = header(&pool).map_err(Error::Pool)?;
    let mut join = encoded(&pool, request)?;
    let mut missed = Vec::new();
    for round in 0..ROUNDS {
        if round > 0 {
            let refused = missed.iter().any(|(_, m)| matches!(m, Miss::Refused(_)));
            match later.take_if(|_| refused) {
                Some(fresh) => {
                    file::write(&files, &dir, &blocks, &fresh).await?;
                    join = encoded(&pool, fresh)?;
                }
                None => clock.sleep(PAUSE).await,
            }
        }
        let messages = [&header, &join];
        match ask_round(&clock, &transport, &ticket, messages).await {
            Round::Founded(founding) => return Ok(founding),
            Round::Missed {
                missed: now,
                joined,
            } => {
                // The stored request applied, so no later round writes over it.
                later = later.filter(|_| !joined);
                missed = now;
            }
        }
    }
    Err(Error::Unjoined { missed })
}

// How a round of asks ends.
enum Round {
    Founded(region::Founding),
    // The miss of each voter. `joined`: a voter answered `Joined`.
    Missed {
        missed: Vec<(node::Key, Miss)>,
        joined: bool,
    },
}

// Asks each voter of `ticket` once with `messages`, and gives the first founding that
// comes.
async fn ask_round(
    clock: &Clock,
    transport: &Transport,
    ticket: &Ticket,
    messages: [&Block; 2],
) -> Round {
    let mut missed = Vec::new();
    let mut joined = false;
    for voter in ticket.voters() {
        let miss = match ask_voter(clock, transport, voter, messages).await {
            Ok((receiver, bytes)) => match founding(clock, receiver, bytes).await {
                Ok(founding) => return Round::Founded(founding),
                Err(miss) => {
                    joined = true;
                    miss
                }
            },
            Err(miss) => miss,
        };
        missed.push((voter.key, miss));
    }
    Round::Missed { missed, joined }
}

// The join message of `request`, in a block of `pool`.
fn encoded(pool: &Pool, request: Request) -> Result<Block, Error> {
    let join = Message::Join {
        request: Box::new(request),
    };
    pool.copy(&join.encode()).map_err(Error::Pool)
}

// Sends the header and the join on a new stream to `voter`, and reads the answer. Gives
// the reply half and the bytes of the founding when the answer is `Joined`.
async fn ask_voter(
    clock: &Clock,
    transport: &Transport,
    voter: &Voter,
    messages: [&Block; 2],
) -> Result<(Receiver, usize), Miss> {
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
        Some(Message::Joined { bytes }) => Ok((receiver, bytes)),
        Some(Message::Unjoined { refusal }) => {
            // The answer came, so only a message after it hides the refusal.
            if let Some(Ok(Some(_))) = before(clock, end, receiver.recv()).await {
                receiver.stop(MALFORMED);
                return Err(Miss::Malformed);
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
// then the end of the reply half, all within `transfer(bytes)`, and each within 10 s
// of the one before it.
async fn founding(
    clock: &Clock,
    mut receiver: Receiver,
    bytes: usize,
) -> Result<region::Founding, Miss> {
    let end = after(clock, transfer(bytes));
    let next = || end.min(after(clock, DEADLINE));
    let mut body = Vec::new();
    while body.len() < bytes {
        let part = before(clock, next(), receiver.recv()).await;
        let part = part.ok_or(Miss::Slow)?.map_err(Miss::Stream)?;
        let Some(part) = part else {
            return Err(Miss::Malformed);
        };
        match Message::decode(&part) {
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
    let last = before(clock, next(), receiver.recv()).await;
    if last.ok_or(Miss::Slow)?.map_err(Miss::Stream)?.is_some() {
        receiver.stop(MALFORMED);
        return Err(Miss::Malformed);
    }
    region::Founding::decode(&body).ok_or(Miss::Malformed)
}

// The most time that a founding of `bytes` and the end of its half take: 10 s, and the
// time of `bytes` at `RATE_BYTES_PER_S`.
fn transfer(bytes: usize) -> Span {
    let bytes = i64::try_from(bytes).expect("invariant: a founding is at most 64 MiB");
    let nanos = bytes.saturating_mul(Span::SECOND.nanos()) / RATE_BYTES_PER_S;
    Span::from_nanos(DEADLINE.nanos().saturating_add(nanos))
}

// The time `span` after now.
fn after(clock: &Clock, span: Span) -> Monotonic {
    let end = clock.now().checked_add(span);
    end.expect("invariant: a deadline is far from the end of the clock")
}

// The output of `future`, or `None` when `end` passes first.
async fn before<F: Future>(
    clock: &Clock,
    end: Monotonic,
    future: F,
) -> Option<F::Output> {
    let left = end.0.saturating_sub(clock.now().0);
    let left = i64::try_from(left).expect("invariant: an end is at most 665.36 s away");
    within(clock, Span::from_nanos(left), future).await
}

/// Why one ask of a voter gave no founding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Miss {
    /// The dial or the stream failed.
    Stream(transport::Error),
    /// The dial or the answer did not come within 10 s.
    Deadline,
    /// After `Joined`, the founding or the end of the reply half did not come within
    /// 10 s + its bytes / (100 KiB/s), or a message or the end of the half did not
    /// come within 10 s of the message before it.
    Slow,
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
            Self::Deadline => f.write_str("no answer came within 10 s"),
            Self::Slow => f.write_str(
                "the founding came under 100 KiB/s or with a gap over 10 s",
            ),
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

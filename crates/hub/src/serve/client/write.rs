//! The write stream of a client session.

mod group;

use std::cell::RefCell;
use std::future::poll_fn;
use std::mem;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::Arc;
use std::task::Poll;

use access::proof;
use block::Unique;
use transport::stream::{Receiver, Sender};
use types::frame::key_set::{Entry, KeySet};
use types::frame::{Draft, Layout};
use types::name::Name;
use wire::hub::client::Place;
use wire::hub::client::write::{Open, Write};
use wire::hub::write::{Due, Frames, Outcome, Reply, Step, Time};

use self::group::{Frame, Group, Parts};
use super::{Reserved, body};
use crate::State;
use crate::link::Session;
use crate::serve::{Error, alloc, unless};
use crate::writer::{self, Homed, Split, Writer};

/// Serves the write of `subject` that `write` starts: verifies its open, opens its
/// writer, and sends its places, then writes each frame of the program and sends its
/// outcome, and `Stored` after each commit, until the program finishes the stream.
pub(super) async fn serve(
    session: &Session,
    subject: Name,
    write: Write,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<(), Error> {
    let state = &session.state;
    let open = open(session, subject.clone(), write, receiver).await?;
    let decoded = Reserved::new(state, subject.clone(), held(&open.channels))?;
    let Some(homed) = here(state, subject.clone(), open, receiver).await? else {
        sender.finish()?;
        return Ok(());
    };
    let ends = Frames::bytes(homed.places());
    let ends = u64::try_from(ends).expect("invariant: a u64 holds a usize");
    let _reserved = Reserved::new(state, subject, ends)?;
    let split = homed
        .split()
        .map_err(|error| Error::Writer(Box::new(error)))?;
    let Split {
        set,
        entries,
        writers,
        channels,
    } = split;
    start(state, sender, &set, &entries, channels).await?;
    drop(decoded);
    let writes = Writes::new(&set, writers);
    let writes = relay(state, set, writes, receiver, sender).await?;
    writes.finish(state, sender).await
}

/// Reads, verifies, and decodes the open, and holds its body in the budget until then.
async fn open(
    session: &Session,
    subject: Name,
    write: Write,
    receiver: &mut Receiver,
) -> Result<Open, Error> {
    let _reserved = Reserved::new(&session.state, subject, write.length)?;
    let bytes = body(receiver, write.body()).await?;
    session.client.admission.verify(
        &session.state,
        proof::Kind::Write,
        &bytes,
        &write.signature,
    )?;
    Ok(Open::decode(&bytes)?)
}

/// The bytes that `names` hold once decoded: 16 for each name, and its length.
fn held(names: &[Name]) -> u64 {
    let bytes: usize = names
        .iter()
        .map(|name| size_of::<Name>() + name.as_str().len())
        .sum();
    u64::try_from(bytes).expect("invariant: a u64 holds a usize")
}

/// Resolves the channels of `open` and waits for this node to be the home of each
/// index, or gives `None` when the program finishes first.
async fn here(
    state: &Rc<RefCell<State>>,
    subject: Name,
    open: Open,
    receiver: &mut Receiver,
) -> Result<Option<Homed>, Error> {
    let config = writer::Config {
        subject,
        authority: open.authority,
        lease: open.lease,
        channels: open.channels,
    };
    let program = pin!(async {
        match receiver.recv().await? {
            None => Ok(()),
            Some(_) => Err(Error::Client(wire::hub::client::Error::Trailing)),
        }
    });
    let Some(homed) = unless(program, Homed::new(state, config)).await? else {
        return Ok(None);
    };
    homed
        .map(Some)
        .map_err(|error| Error::Writer(Box::new(error)))
}

/// Sends the places of the write and their run: place `n` is entry `n` of `set`, with
/// its name from `names`, the channels of the open, whose entries are `opened`, or
/// from the table for its index. Call it with no wait after the writers open, while
/// each channel of the open has the key that it opened with.
async fn start(
    state: &RefCell<State>,
    sender: &mut Sender,
    set: &KeySet,
    opened: &[usize],
    names: Vec<Name>,
) -> Result<(), Error> {
    let places: Vec<Place> = {
        let channels = &state.borrow().channels;
        let entries = set.entries();
        let mut of = vec![None; entries.len()];
        for (&n, name) in opened.iter().zip(&names) {
            of[n] = Some(name);
        }
        for &index in set.groups() {
            of[index].get_or_insert_with(|| channels.index(entries[index].key).0);
        }
        let place = |(at, (entry, name)): (usize, (&Entry, Option<&Name>))| Place {
            key: entry.key,
            data_type: channels.known(entry.key).sample(),
            name: name
                .expect("invariant: each entry is a channel of the open or its index")
                .clone(),
            index: set.index(at),
        };
        entries.iter().zip(of).enumerate().map(place).collect()
    };
    super::places(state, sender, &places).await
}

/// Writes each frame of the program with `writes`, a write of `set`, and sends its
/// outcomes, until the program finishes the stream, and gives `writes`. The blocks of
/// the outcomes of a frame come before its draft, so a pool with no block for them
/// stops the stream before a home takes the frame.
async fn relay(
    state: &RefCell<State>,
    set: Arc<KeySet>,
    mut writes: Writes,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<Writes, Error> {
    let mut frames = Frames::new(set);
    loop {
        let step = match frames.due().map_err(Error::Frame)? {
            Due::Message => {
                let Some(message) = writes.race(state, sender, receiver.recv()).await?
                else {
                    frames.finish()?;
                    return Ok(writes);
                };
                frames.decode(&message)?
            }
            Due::Body(rest) => {
                match writes.race(state, sender, receiver.recv_into(rest)).await {
                    Ok(Some(len)) => frames.received(len)?,
                    Ok(None) => {
                        frames.finish()?;
                        unreachable!("invariant: a stream that ends in a body is cut")
                    }
                    // The message stays queued, and its decode gives its error.
                    Err(Error::Stream(transport::Error::TooLarge { .. })) => {
                        let message =
                            writes.race(state, sender, receiver.recv()).await?;
                        let message =
                            message.expect("invariant: the message is queued");
                        frames.decode(&message)?
                    }
                    Err(error) => return Err(error),
                }
            }
            Due::Draft { layout, form } => {
                writes.head(state, &layout)?;
                let drafted = layout.draft(state.borrow().home.pool(), form);
                match drafted {
                    Ok(draft) => frames.draft(draft),
                    Err(error) => {
                        frames.skip();
                        writes.refuse(sender, &error).await?;
                        Step::More
                    }
                }
            }
        };
        if let Step::Frame { draft, time } = step {
            writes.write(state, sender, draft, time).await?;
        }
    }
}

/// The writes of the groups of a stream, and the outcome blocks of its last frame.
///
/// A group that takes a frame waits for the next commit. One commit at a time is due:
/// when none is, the stream calls one for the frames of each waiting group, and when
/// it ends, sends the `Stored` or `Ended` of each group that it covers, in the order
/// in which the groups took their first frame after the commit before.
struct Writes {
    /// The count of frames of the stream whose head came.
    heads: u64,
    /// The write of each group of the stream, in group order.
    groups: Vec<Group>,
    /// How a frame reaches the groups.
    spread: Spread,
    /// The commit that is due, of the taken frames of each group of `covered`.
    due: Option<::home::Commit>,
    /// Each group that the commit in `due` covers.
    covered: Vec<u32>,
    /// Each group that took a frame since the call of the commit in `due`.
    waiting: Vec<u32>,
}

/// How a frame of a stream reaches its groups.
enum Spread {
    /// One group, which takes each frame whole, and the block of the outcome of the
    /// frame whose head came.
    One(Option<Unique>),
    /// Many groups, each of which takes a copy of its series of a frame.
    Many {
        /// Each group of the frame whose head came, with the block of its outcome.
        pending: Vec<(u32, Unique)>,
        /// The series of each group of the last frame.
        parts: Parts,
    },
}

/// The panic of a frame of one group that has no block for its outcome.
const TAKEN: &str = "invariant: the head of a frame takes the block of its outcome";

impl Writes {
    /// The replies of a write of `set` with `writers`, one for each group, in group
    /// order.
    fn new(set: &KeySet, writers: Vec<Writer>) -> Self {
        let spread = if writers.len() > 1 {
            Spread::Many {
                pending: Vec::new(),
                parts: Parts::new(set),
            }
        } else {
            Spread::One(None)
        };
        let groups = (0..)
            .zip(writers)
            .map(|(number, writer)| Group::new(number, writer))
            .collect();
        Self {
            heads: 0,
            groups,
            spread,
            due: None,
            covered: Vec::new(),
            waiting: Vec::new(),
        }
    }

    /// Counts the frame of `layout`, whose head came, and takes the block of the
    /// outcome of each group that it holds.
    fn head(
        &mut self,
        state: &RefCell<State>,
        layout: &Layout<'_>,
    ) -> Result<(), Error> {
        self.heads += 1;
        match &mut self.spread {
            // A head names at least one series and each series its index, so the
            // frame of a stream of one group holds it. This skips the walk of the
            // series.
            Spread::One(block) => *block = Some(alloc(state, Outcome::LEN_MAX)?),
            Spread::Many { pending, .. } => {
                for group in layout.groups() {
                    pending.push((group, alloc(state, Outcome::LEN_MAX)?));
                }
            }
        }
        Ok(())
    }

    /// Sends the outcome of each group of the frame whose draft found no block with
    /// `error`.
    async fn refuse(
        &mut self,
        sender: &mut Sender,
        error: &block::Error,
    ) -> Result<(), Error> {
        let frame = self.heads;
        match &mut self.spread {
            Spread::One(block) => {
                let block = block.take().expect(TAKEN);
                let outcome = self.groups[0].refuse(error);
                send(sender, block, reply(frame, 0, outcome)).await?;
            }
            Spread::Many { pending, .. } => {
                for (group, block) in pending.drain(..) {
                    let outcome = self.groups[to_usize(group)].refuse(error);
                    send(sender, block, reply(frame, group, outcome)).await?;
                }
            }
        }
        Ok(())
    }

    /// Writes each group of `draft`, the frame whose head came last, and sends the
    /// outcome of each, in group order, after the `Ended` of a group that ends at it.
    /// A frame of a stream of one group goes to its group as it is; else each group
    /// gets a copy of its series.
    async fn write(
        &mut self,
        state: &RefCell<State>,
        sender: &mut Sender,
        draft: Draft,
        time: Time,
    ) -> Result<(), Error> {
        let Self {
            heads,
            groups,
            spread,
            waiting,
            ..
        } = self;
        match spread {
            Spread::One(block) => {
                let block = block.take().expect(TAKEN);
                let frame = Frame::Whole(draft);
                let group = &mut groups[0];
                put(state, sender, group, waiting, *heads, block, frame, time).await?;
            }
            Spread::Many { pending, parts } => {
                parts.split(&draft, pending.iter().map(|&(group, _)| group));
                for (number, block) in pending.drain(..) {
                    let frame = parts.part(&draft, number);
                    let group = &mut groups[to_usize(number)];
                    put(state, sender, group, waiting, *heads, block, frame, time)
                        .await?;
                }
            }
        }
        self.call(state);
        Ok(())
    }

    /// Calls a commit for the taken frames of each waiting group when none is due.
    fn call(&mut self, state: &RefCell<State>) {
        if self.due.is_some() {
            return;
        }
        let groups = &mut self.groups;
        self.waiting
            .retain(|&group| groups[to_usize(group)].cover());
        if !self.waiting.is_empty() {
            mem::swap(&mut self.covered, &mut self.waiting);
            self.due = Some(state.borrow().home.committed());
        }
    }

    /// Sends the reply of each group that the due commit covered, which ended with
    /// `committed`, then calls the next commit.
    async fn committed(
        &mut self,
        state: &RefCell<State>,
        sender: &mut Sender,
        committed: &Result<(), env::files::Error>,
    ) -> Result<(), Error> {
        let mut covered = mem::take(&mut self.covered);
        for group in covered.drain(..) {
            if let Some((block, reply)) =
                self.groups[to_usize(group)].committed(committed)
            {
                send(sender, block, reply).await?;
            }
        }
        self.covered = covered;
        self.call(state);
        Ok(())
    }

    /// Waits for `received`, and sends the replies of each commit that ends meanwhile.
    async fn race<T>(
        &mut self,
        state: &RefCell<State>,
        sender: &mut Sender,
        received: impl Future<Output = Result<T, transport::Error>>,
    ) -> Result<T, Error> {
        let mut received = pin!(received);
        while let Some(due) = &mut self.due {
            let committed = poll_fn(|cx| {
                if let Poll::Ready(committed) = Pin::new(&mut *due).poll(cx) {
                    return Poll::Ready(Err(committed));
                }
                received.as_mut().poll(cx).map(Ok)
            });
            match committed.await {
                Ok(received) => return Ok(received?),
                Err(committed) => {
                    self.due = None;
                    self.committed(state, sender, &committed).await?;
                }
            }
        }
        Ok(received.await?)
    }

    /// Drops each writer, so that control ends, then waits for the commit of each
    /// taken frame, sends its reply, and finishes the stream.
    async fn finish(
        mut self,
        state: &RefCell<State>,
        sender: &mut Sender,
    ) -> Result<(), Error> {
        for group in &mut self.groups {
            group.close();
        }
        while let Some(due) = self.due.take() {
            let committed = due.await;
            self.committed(state, sender, &committed).await?;
        }
        sender.finish()?;
        Ok(())
    }
}

/// Writes `frame`, the frame `heads` of the stream, to `group`, which adds its number
/// to `waiting` when it starts to wait for a commit, and sends the `Ended` of the group
/// when it ends at the frame, then the outcome of the frame in `block`.
#[expect(
    clippy::too_many_arguments,
    reason = "the parts of a frame that both paths of `Writes::write` give"
)]
async fn put(
    state: &RefCell<State>,
    sender: &mut Sender,
    group: &mut Group,
    waiting: &mut Vec<u32>,
    heads: u64,
    block: Unique,
    frame: Frame<'_>,
    time: Time,
) -> Result<(), Error> {
    let (ended, outcome) = group.write(state, frame, time, heads, waiting);
    if let Some((block, reply)) = ended {
        send(sender, block, reply).await?;
    }
    send(sender, block, reply(heads, group.number(), outcome)).await
}

/// Sends `reply` in the last bytes of `block`.
async fn send(
    sender: &mut Sender,
    mut block: Unique,
    reply: Reply,
) -> Result<(), Error> {
    let start = block.len() - reply.encoded_len();
    reply.encode(&mut block[start..]);
    sender.send(block.freeze().skip(start)).await?;
    Ok(())
}

/// The reply of `outcome` of `group` in `frame`.
fn reply(frame: u64, group: u32, outcome: Outcome) -> Reply {
    Reply::Outcome {
        frame,
        group,
        outcome,
    }
}

fn to_usize(value: u32) -> usize {
    usize::try_from(value).expect("invariant: a usize holds a u32")
}

/// The refusal that stops a write whose writer did not open with `error`.
///
/// # Panics
///
/// For `Empty`, as an open names at least one channel, and for a `Home` error of
/// `Unsynced`, as the writer opens once the node has mesh time.
pub(in crate::serve) fn refused(error: &writer::Error) -> wire::hub::client::Refusal {
    use wire::hub::client::Refusal as Stop;
    match error {
        writer::Error::Unknown(_) => Stop::Unknown,
        writer::Error::Remote { .. } => Stop::NotHome,
        writer::Error::Mesh(_) => Stop::Failed,
        writer::Error::Home(::home::writer::Error::Lease { .. }) => Stop::Malformed,
        writer::Error::Empty | writer::Error::Home(::home::writer::Error::Unsynced) => {
            panic!("invariant: a write does not give {error:?}")
        }
    }
}

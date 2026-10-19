//! The write stream of a client session.

use std::cell::RefCell;
use std::future::poll_fn;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::Arc;
use std::task::Poll;

use access::proof;
use block::Unique;
use transport::stream::{Receiver, Sender};
use types::frame::key_set::Entry;
use types::frame::{Draft, Label, Path};
use types::name::Name;
use types::time::{Span, Stamp};
use wire::hub::client::Place;
use wire::hub::client::write::{Open, Write};
use wire::hub::write::{Due, Frames, Outcome, Refusal, Reply, Step, Time};

use super::{Reserved, body};
use crate::State;
use crate::link::Session;
use crate::serve::{Error, alloc, unless};
use crate::writer::{self, Failure, Homed, Writer};

/// The bytes of the block of a `Stored` reply.
const STORED_BYTES: usize = Reply::Stored { frames: 0 }.encoded_len();

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
    let ends = Frames::bytes(open.channels.len() + 1);
    let ends = u64::try_from(ends).expect("invariant: a u64 holds a usize");
    let _reserved = Reserved::new(state, subject.clone(), ends)?;
    let decoded = Reserved::new(state, subject.clone(), held(&open.channels))?;
    let Some((writer, names)) = here(state, subject, open, receiver).await? else {
        sender.finish()?;
        return Ok(());
    };
    start(state, sender, &writer, names).await?;
    drop(decoded);
    let replies = relay(state, writer, receiver, sender).await?;
    replies.finish(state, sender).await
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

/// Opens the writer of `open` at this node, the home of its index, and gives it with
/// the channels of `open`, or gives `None` when the program finishes first. A writer
/// on more than one index gives [`Error::ManyIndexes`] before it waits for a home, so
/// it takes no control.
async fn here(
    state: &Rc<RefCell<State>>,
    subject: Name,
    open: Open,
    receiver: &mut Receiver,
) -> Result<Option<(Writer, Vec<Name>)>, Error> {
    let refused = |error| Error::Writer(Box::new(error));
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
    let Some(homed) = unless(program, Homed::one(state, config)).await? else {
        return Ok(None);
    };
    let Some(homed) = homed.map_err(refused)? else {
        return Err(Error::ManyIndexes);
    };
    homed.open().map(Some).map_err(refused)
}

/// Sends the places of `writer` and their run: place `n` is entry `n` of its key set,
/// with its name from `names`, the channels of its open, or from the table for its
/// index. Call it with no wait after the writer opens, while each channel of the open
/// has the key that it opened with.
async fn start(
    state: &RefCell<State>,
    sender: &mut Sender,
    writer: &Writer,
    names: Vec<Name>,
) -> Result<(), Error> {
    let set = writer.set();
    let places: Vec<Place> = {
        let channels = &state.borrow().channels;
        let entries = set.entries();
        let mut of = vec![None; entries.len()];
        for (&n, name) in writer.entries().iter().zip(&names) {
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

/// Writes each frame of the program with `writer` and sends its outcome, until the
/// program finishes the stream. Then drops `writer`, so that control ends, and gives
/// the commits that are due. The block of each outcome comes before the draft of its
/// frame, so a pool with no block for it stops the stream before the home takes the
/// frame. The block of the next `Stored` comes after the body, right before the home
/// takes the frame, and a frame that finds none is refused as `Full`, as a frame whose
/// draft finds none is.
async fn relay(
    state: &RefCell<State>,
    mut writer: Writer,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<Replies, Error> {
    let mut frames = Frames::new(Arc::clone(writer.set()));
    let mut replies = Replies::default();
    let mut stamped: Option<Stamp> = None;
    let mut reply = None;
    loop {
        let step = match frames.due().map_err(Error::Frame)? {
            Due::Message => {
                let Some(message) =
                    replies.race(state, sender, receiver.recv()).await?
                else {
                    frames.finish()?;
                    return Ok(replies);
                };
                frames.decode(&message)?
            }
            Due::Body(rest) => {
                match replies.race(state, sender, receiver.recv_into(rest)).await {
                    Ok(Some(len)) => frames.received(len)?,
                    Ok(None) => {
                        frames.finish()?;
                        unreachable!("invariant: a stream that ends in a body is cut")
                    }
                    // The message stays queued, and its decode gives its error.
                    Err(Error::Stream(transport::Error::TooLarge { .. })) => {
                        let message =
                            replies.race(state, sender, receiver.recv()).await?;
                        let message =
                            message.expect("invariant: the message is queued");
                        frames.decode(&message)?
                    }
                    Err(error) => return Err(error),
                }
            }
            Due::Draft { layout, form } => {
                let pending = replies.head(state)?;
                let drafted = layout.draft(state.borrow().home.pool(), form);
                match drafted {
                    Ok(draft) => {
                        reply = Some(pending);
                        frames.draft(draft)
                    }
                    Err(error) => {
                        frames.skip();
                        let refusal = match error {
                            block::Error::TooLarge { .. } => Refusal::Large,
                            block::Error::Exhausted { .. }
                            | block::Error::Refused { .. } => Refusal::Full,
                        };
                        let outcome = Outcome::Refused(refusal);
                        let (block, reply) = replies.outcome(state, pending, outcome);
                        send(sender, block, reply).await?;
                        Step::More
                    }
                }
            }
        };
        let Step::Frame { draft, time } = step else {
            continue;
        };
        let pending = reply
            .take()
            .expect("invariant: a draft comes with its block");
        let (block, reply) = replies.write(state, pending, || {
            write(&mut writer, draft, time, &mut stamped)
        })?;
        send(sender, block, reply).await?;
    }
}

/// Writes `draft`, a frame of `time`, with `writer`, and gives its outcome. A frame of
/// [`Time::Node`] gets its stamp from [`next`], which `stamped` keeps when the home
/// takes or loses the frame.
fn write(
    writer: &mut Writer,
    mut draft: Draft,
    time: Time,
    stamped: &mut Option<Stamp>,
) -> Result<Outcome, Error> {
    let (label, stamp) = match time {
        Time::Frame(path) => (Label::Path(path), None),
        Time::Node => {
            let stamp = next(writer.now(), *stamped);
            let index = draft
                .series_mut(writer.set().groups()[0])
                .expect("invariant: a frame of a write holds its index");
            let stamp = stamp_index(index, stamp);
            (Label::Path(Path::Live), stamp)
        }
    };
    let outcome = outcome(writer.write(label, draft))?;
    // The home spends the stamp of a lost frame too.
    if let Outcome::Taken { .. } | Outcome::Lost { .. } = outcome {
        *stamped = stamp.or(*stamped);
    }
    Ok(outcome)
}

/// The stamp of a frame of [`Time::Node`] at mesh time `now`: `now`, or 1 ns after
/// `stamped`, the last stamp of the stream that the home took or lost, when `now` is
/// not after it.
fn next(now: Stamp, stamped: Option<Stamp>) -> Stamp {
    match stamped {
        Some(last) if now <= last => last + Span::from_nanos(1),
        _ => now,
    }
}

/// Writes `stamp` over the one sample of `index`, the index series of a frame, and
/// gives it. Gives `None` and writes nothing when that series is not one stamp, a frame
/// of one sample that the home refuses as `Codec` after its check of control.
fn stamp_index(index: &mut [u8], stamp: Stamp) -> Option<Stamp> {
    let sample = <&mut [u8; 8]>::try_from(index).ok()?;
    *sample = stamp.nanos().to_le_bytes();
    Some(stamp)
}

/// The outcome of a frame from what the write gave, or the error that stops the
/// stream.
fn outcome(written: Result<&[::home::Outcome], Failure>) -> Result<Outcome, Error> {
    let outcomes = match written {
        Ok(outcomes) => outcomes,
        Err(Failure::Removed(key)) => return Err(Error::Removed(key)),
        Err(Failure::Home(::home::Error::Disk(error))) => {
            return Err(Error::Buffer(error));
        }
        Err(Failure::Home(::home::Error::Full)) => {
            return Ok(Outcome::Refused(Refusal::Full));
        }
        Err(Failure::Home(::home::Error::Large)) => {
            return Ok(Outcome::Refused(Refusal::Large));
        }
        Err(Failure::Home(::home::Error::Resend)) => {
            unreachable!("invariant: each frame of a write is labeled with a path")
        }
    };
    let [outcome] = outcomes else {
        unreachable!("invariant: a frame of a write holds one group")
    };
    Ok(match *outcome {
        ::home::Outcome::Applied { range, .. } => Outcome::Taken { range },
        ::home::Outcome::Lost { range, .. } => Outcome::Lost { range },
        ::home::Outcome::Refused { ref refusal, .. } => {
            Outcome::Refused(match refusal {
                ::home::Refusal::Waiting => Refusal::Waiting,
                ::home::Refusal::Reserved => Refusal::Reserved,
                ::home::Refusal::Expired => Refusal::Expired,
                ::home::Refusal::Order(_) => Refusal::Order,
                ::home::Refusal::Codec { .. } => Refusal::Codec,
            })
        }
    })
}

/// The replies of a write: the outcome of each frame, and the `Stored` of each commit
/// of the frames that it took.
#[derive(Default)]
struct Replies {
    /// The count of frames of the stream whose head came.
    heads: u64,
    /// The count of frames through the last one taken since the call of the commit in
    /// `due`.
    taken: Option<u64>,
    /// The block of the next `Stored`. Outside [`Replies::write`], it is `Some` exactly
    /// when `taken` is.
    next: Option<Unique>,
    /// The commit that is due, the count of frames that it covers, and the block of its
    /// `Stored`.
    due: Option<(::home::Commit, u64, Unique)>,
}

/// The block of the outcome of a frame whose head came, and the count of that frame.
struct Pending {
    frame: u64,
    block: Unique,
}

impl Replies {
    /// Counts the frame whose head came, and gives the block for its outcome.
    fn head(&mut self, state: &RefCell<State>) -> Result<Pending, Error> {
        self.heads += 1;
        let block = alloc(state, Outcome::LEN_MAX)?;
        Ok(Pending {
            frame: self.heads,
            block,
        })
    }

    /// Writes the frame of `pending` with `write`, and gives its block and reply as
    /// [`Replies::outcome`] does. First takes the block of the next `Stored` from the
    /// home's pool when the stream holds none, so each taken frame has the block of its
    /// `Stored`. A frame that finds none gets `Refused(Full)`, and `write` is not
    /// called.
    ///
    /// # Errors
    ///
    /// The error of `write`.
    fn write(
        &mut self,
        state: &RefCell<State>,
        pending: Pending,
        write: impl FnOnce() -> Result<Outcome, Error>,
    ) -> Result<(Unique, Reply), Error> {
        if self.next.is_none() {
            self.next = state.borrow().alloc(STORED_BYTES).ok();
        }
        let outcome = match self.next {
            Some(_) => write()?,
            None => Outcome::Refused(Refusal::Full),
        };
        Ok(self.outcome(state, pending, outcome))
    }

    /// Gives the block of `pending` and the reply of `outcome`, labeled with its frame.
    /// A taken frame is counted for the next commit, which this calls when none is due.
    /// A frame that the home did not take gives back the block of the next `Stored`
    /// while no taken frame waits for one. Sync, so the relay's future holds no nested
    /// one.
    fn outcome(
        &mut self,
        state: &RefCell<State>,
        pending: Pending,
        outcome: Outcome,
    ) -> (Unique, Reply) {
        let Pending { frame, block } = pending;
        if let Outcome::Taken { .. } = outcome {
            self.taken = Some(frame);
            self.call(state);
        } else if self.taken.is_none() {
            self.next = None;
        }
        (block, Reply::Outcome { frame, outcome })
    }

    /// Calls a commit for the taken frames when none is due.
    fn call(&mut self, state: &RefCell<State>) {
        if self.due.is_none()
            && let Some(frames) = self.taken.take()
        {
            let block = self
                .next
                .take()
                .expect("invariant: a taken frame has the block of its `Stored`");
            self.due = Some((state.borrow().home.committed(), frames, block));
        }
    }

    /// Waits for `received`, and sends `Stored` at each commit that ends meanwhile.
    async fn race<T>(
        &mut self,
        state: &RefCell<State>,
        sender: &mut Sender,
        received: impl Future<Output = Result<T, transport::Error>>,
    ) -> Result<T, Error> {
        let mut received = pin!(received);
        loop {
            let Some((commit, ..)) = &mut self.due else {
                return Ok(received.await?);
            };
            let committed = poll_fn(|cx| {
                if let Poll::Ready(committed) = Pin::new(&mut *commit).poll(cx) {
                    return Poll::Ready(Err(committed));
                }
                received.as_mut().poll(cx).map(Ok)
            });
            match committed.await {
                Ok(received) => return Ok(received?),
                Err(committed) => {
                    committed.map_err(Error::Buffer)?;
                    self.committed(state, sender).await?;
                }
            }
        }
    }

    /// Calls the next commit and sends the `Stored` of the due one, which ended.
    async fn committed(
        &mut self,
        state: &RefCell<State>,
        sender: &mut Sender,
    ) -> Result<(), Error> {
        let (_, frames, block) = self.due.take().expect("invariant: a commit is due");
        self.call(state);
        send(sender, block, Reply::Stored { frames }).await
    }

    /// Waits for the commit of each taken frame, sends its `Stored`, and finishes the
    /// stream.
    async fn finish(
        mut self,
        state: &RefCell<State>,
        sender: &mut Sender,
    ) -> Result<(), Error> {
        while let Some((commit, ..)) = &mut self.due {
            commit.await.map_err(Error::Buffer)?;
            self.committed(state, sender).await?;
        }
        sender.finish()?;
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use types::channel::Slot;
    use types::frame::Range;

    use super::*;

    /// Each outcome and error of a write at the home gives the outcome of the same
    /// meaning.
    #[test]
    fn gives_the_outcome_of_each_outcome_and_error_of_the_home() {
        let (slot, range) = (Slot::new(3), Range { seq: 4, count: 2 });
        let stamp = Stamp::from_nanos(9);
        let refused = |refusal| ::home::Outcome::Refused { slot, refusal };
        let codec = ::home::Refusal::Codec {
            channel: types::channel::Key::from_u128(2),
            error: codec::Error::Tag { vector: 0, tag: 9 },
        };
        let order = ::home::order::Error::Backwards {
            path: Path::Live,
            before: stamp,
            stamp,
        };
        let cases = [
            (
                ::home::Outcome::Applied { slot, range },
                Outcome::Taken { range },
            ),
            (
                ::home::Outcome::Lost { slot, range },
                Outcome::Lost { range },
            ),
            (
                refused(::home::Refusal::Waiting),
                Outcome::Refused(Refusal::Waiting),
            ),
            (
                refused(::home::Refusal::Reserved),
                Outcome::Refused(Refusal::Reserved),
            ),
            (
                refused(::home::Refusal::Expired),
                Outcome::Refused(Refusal::Expired),
            ),
            (
                refused(::home::Refusal::Order(order)),
                Outcome::Refused(Refusal::Order),
            ),
            (refused(codec), Outcome::Refused(Refusal::Codec)),
        ];
        for (home, want) in cases {
            assert_eq!(
                outcome(Ok(std::slice::from_ref(&home))),
                Ok(want),
                "{home:?}"
            );
        }
        for (error, refusal) in [
            (::home::Error::Full, Refusal::Full),
            (::home::Error::Large, Refusal::Large),
        ] {
            let got = outcome(Err(Failure::Home(error.clone())));
            assert_eq!(got, Ok(Outcome::Refused(refusal)), "{error:?}");
        }
    }
}

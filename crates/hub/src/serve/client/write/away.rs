//! A program's write whose home is another node: the forward from the program's node.

use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::ops::Range;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker, ready};

use block::Block;
use transport::stream::{Receiver, Sender};
use transport::{Class, Code};
use types::node;
use wire::Protocol;
use wire::header::MALFORMED;
use wire::hub::client::write::{Open, Opening, Start, Write};
use wire::hub::client::{Refusal, Signed};
use wire::hub::forward::{Forward, places};
use wire::hub::write::{Due, End, Frames, Outcome, Replies, Reply, Step};

use super::{Reserved, bytes, send};
use crate::serve::admission::Admission;
use crate::serve::{Error, alloc, unless};
use crate::{State, dial};

/// The most frames sent to the home with an outcome due. The node reads no more of the
/// program while so many are due.
const IN_FLIGHT: u64 = 64;

/// Serves the write of the subject of `admission` that `write` starts with `open`,
/// whose home is `home`, another node: forwards it to `home`, sends the program the
/// places of `home`, then sends `home` each message of the program unchanged, and each
/// renewal of the hello of `admission` between frames, and sends the program each
/// reply of `home` unchanged.
/// When `home` ends the write after its places, ends group 0 with [`Reply::Ended`],
/// and each frame that has no outcome then, or that comes later, with
/// [`Outcome::Ended`]. Finishes the program's stream once the program and `home`
/// finish.
pub(super) async fn serve(
    state: &Rc<RefCell<State>>,
    admission: &Admission,
    home: node::Key,
    write: Write,
    open: Open,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<(), Error> {
    let subject = admission
        .subject()
        .expect("invariant: an admitted hello has a subject");
    let channels = open.channels.len();
    let _ends =
        Reserved::new(state, subject.clone(), bytes(Frames::bytes(channels + 1)))?;
    let reserved = Reserved::new(state, subject, write.length)?;
    // Each field of an open has one encoding, so these are the bytes the program
    // signed.
    let mut body = vec![0; open.encoded_len()];
    open.encode(&mut body);
    drop(open);
    let channels = u32::try_from(channels)
        .expect("invariant: an open names fewer than u32::MAX channels");
    let started = {
        let program = pin!(trailing(receiver));
        let start = start(state, admission, home, write, &body, channels);
        unless(program, start).await?
    };
    drop((body, reserved));
    let Some(started) = started else {
        sender.finish()?;
        return Ok(());
    };
    let (mut to, mut from, Start { places, set }, seen) = started?;
    super::super::places(state, sender, &places).await?;
    let mut frames = Frames::new(set);
    let flight = Flight::new();
    let (cut, finished) = {
        let mut up = pin!(up(
            state,
            admission,
            seen,
            &mut frames,
            &flight,
            receiver,
            &mut to
        ));
        let mut down = pin!(down(&flight, &mut from, sender));
        let mut finished = false;
        let cut = poll_fn(|cx| {
            if !finished {
                match up.as_mut().poll(cx) {
                    Poll::Ready(Ok(None)) => finished = true,
                    Poll::Pending => {}
                    up @ Poll::Ready(_) => return up,
                }
            }
            down.as_mut().poll(cx).map(|cut| match cut {
                Ok(None) if !finished => Ok(Some(Cut::Broken)),
                cut => cut,
            })
        })
        .await?;
        (cut, finished)
    };
    let Some(cut) = cut else {
        sender.finish()?;
        return Ok(());
    };
    to.reset(cut.code());
    from.stop(cut.code());
    let program = if finished { None } else { Some(receiver) };
    after(state, cut.end(), &mut frames, &flight, program, sender).await
}

/// Waits for the program to finish while the write opens at its home. Each message of
/// the program before the places breaks the client wire.
async fn trailing(receiver: &mut Receiver) -> Result<(), Error> {
    match receiver.recv().await? {
        Some(_) => Err(Error::Client(wire::hub::client::Error::Trailing)),
        None => Ok(()),
    }
}

/// Opens the forward at `home` of the write that `write` starts with `body`, an open of
/// `channels` channels, with the hello of `admission`. Gives its halves, the places of
/// `home`, and the count of hellos admitted through the hello it sent. A session that
/// closes with `Code(0)` before the places gets another try, as [`dial::retry`] states.
async fn start(
    state: &Rc<RefCell<State>>,
    admission: &Admission,
    home: node::Key,
    write: Write,
    body: &[u8],
    channels: u32,
) -> Result<(Sender, Receiver, Start, u64), Error> {
    let closed: fn(&Error) -> Option<&transport::Error> = |error| match error {
        Error::Stream(error) => Some(error),
        _ => None,
    };
    let attempt = async || {
        let (signed, seen) = admission.signed();
        let (to, from, start) =
            attempt(state, home, write, body, channels, &signed).await?;
        Ok((to, from, start, seen))
    };
    dial::retry(attempt, closed)
        .await
        .map_err(|error| match error {
            Error::Stream(error) => Error::Forward {
                home,
                refusal: refusal(&error).unwrap_or(Refusal::Failed),
            },
            error => error,
        })
}

/// One try of [`start`].
async fn attempt(
    state: &Rc<RefCell<State>>,
    home: node::Key,
    write: Write,
    body: &[u8],
    channels: u32,
    signed: &Signed,
) -> Result<(Sender, Receiver, Start), Error> {
    let (mut to, mut from) =
        dial::dial(state, home).await?.open(Class::Complete).await?;
    let failed = Error::Forward {
        home,
        refusal: Refusal::Failed,
    };
    let (taken, largest) = {
        let state = state.borrow();
        let region = &state
            .region
            .as_ref()
            .expect("invariant: only a node in a region forwards")
            .region;
        (
            region.transport.message_bytes_max(),
            state.home.pool().largest(),
        )
    };
    // Each message of the program goes to the home as it came.
    let home_max = to.bytes_max();
    if home_max < taken {
        to.reset(Code(Refusal::Failed.code()));
        from.stop(Code(Refusal::Failed.code()));
        return Err(Error::Limit {
            home,
            home_max,
            max: taken,
        });
    }
    let header = wire::header::encode(Protocol::Hub);
    message(state, &mut to, header.len(), |out| {
        out.copy_from_slice(&header);
    })
    .await?;
    let forward = Forward { places: channels };
    message(state, &mut to, Forward::LEN, |out| forward.encode(out)).await?;
    message(state, &mut to, signed.encoded_len(), |out| {
        signed.encode(out);
    })
    .await?;
    message(state, &mut to, Write::LEN, |out| write.encode(out)).await?;
    let most = to.bytes_max().min(largest);
    for chunk in body.chunks(most) {
        message(state, &mut to, chunk.len(), |out| {
            out.copy_from_slice(chunk);
        })
        .await?;
    }
    for run in runs(channels, most) {
        let len = run.len() * places::LEN;
        message(state, &mut to, len, |out| places::encode(run, out)).await?;
    }
    let mut opening = Opening::default();
    loop {
        let message = from.recv().await?;
        match opening.decode(message.as_deref()) {
            Ok(Some(start)) => return Ok((to, from, start)),
            Ok(None) => {}
            Err(_) => {
                to.reset(Code(MALFORMED));
                from.stop(Code(MALFORMED));
                return Err(failed);
            }
        }
    }
}

/// The places `0..channels` in runs whose places fit `max` bytes.
fn runs(channels: u32, max: usize) -> impl Iterator<Item = Range<u32>> {
    let most = u32::try_from(max / places::LEN).unwrap_or(u32::MAX);
    let mut start = 0_u32;
    std::iter::from_fn(move || {
        let run = start..channels.min(start.saturating_add(most));
        start = run.end;
        (!run.is_empty()).then_some(run)
    })
}

/// Sends a message of `len` bytes that `fill` writes, in a block of the home's pool.
async fn message(
    state: &RefCell<State>,
    sender: &mut Sender,
    len: usize,
    fill: impl FnOnce(&mut [u8]),
) -> Result<(), Error> {
    let mut block = alloc(state, len)?;
    fill(&mut block);
    sender.send(block.freeze()).await?;
    Ok(())
}

/// The refusal of the client wire that `error` carries, when the home stopped or reset
/// the stream with one.
fn refusal(error: &transport::Error) -> Option<Refusal> {
    match error {
        transport::Error::Reset { code } | transport::Error::Stopped { code } => {
            Refusal::from_code(code.0)
        }
        _ => None,
    }
}

/// Why the forward ended before the program finished it.
enum Cut {
    /// The stream to the home failed with this error.
    Stream(transport::Error),
    /// The home broke HUB WIRE: a reply that [`Replies::decode`] refuses, or a finish
    /// before the program's.
    Broken,
}

impl Cut {
    /// The end of the group.
    fn end(&self) -> End {
        let Self::Stream(error) = self else {
            return End::Failed;
        };
        match refusal(error) {
            Some(Refusal::Refused | Refusal::Via | Refusal::Changed) => End::Refused,
            Some(Refusal::Unknown) => End::Removed,
            _ => End::Failed,
        }
    }

    /// The code that stops each half of the stream to the home.
    fn code(&self) -> Code {
        match self {
            Self::Stream(_) => Code(Refusal::Failed.code()),
            Self::Broken => Code(MALFORMED),
        }
    }
}

/// The frames of a forward: those whose head went to the home, and those with their
/// outcome.
struct Flight {
    replies: RefCell<Replies>,
    heads: Cell<u64>,
    answered: Cell<u64>,
    /// The end of the [`Reply::Ended`] of the home.
    ended: Cell<Option<End>>,
    /// The task that waits for an outcome while [`IN_FLIGHT`] frames are due.
    waiting: Cell<Option<Waker>>,
}

impl Flight {
    fn new() -> Self {
        Self {
            replies: RefCell::new(Replies::new(1)),
            heads: Cell::default(),
            answered: Cell::default(),
            ended: Cell::default(),
            waiting: Cell::default(),
        }
    }

    /// Counts a frame whose head goes to the home. The home answers a frame whose draft
    /// finds no block before its body, so it counts at the head.
    fn head(&self) {
        self.heads.set(self.heads.get() + 1);
        self.replies.borrow_mut().sent([0]);
    }

    /// Checks `message`, the next reply of the home, and counts an outcome.
    fn reply(&self, message: &[u8]) -> Result<(), wire::hub::Error> {
        match self.replies.borrow_mut().decode(message)? {
            Reply::Outcome { .. } => {
                self.answered.set(self.answered.get() + 1);
                if let Some(waker) = self.waiting.take() {
                    waker.wake();
                }
            }
            Reply::Ended { end, .. } => self.ended.set(Some(end)),
            Reply::Stored { .. } => {}
        }
        Ok(())
    }

    /// Ready while fewer than [`IN_FLIGHT`] frames have an outcome due.
    fn poll_room(&self, cx: &Context<'_>) -> Poll<()> {
        if self.heads.get() - self.answered.get() < IN_FLIGHT {
            return Poll::Ready(());
        }
        self.waiting.set(Some(cx.waker().clone()));
        Poll::Pending
    }
}

/// Sends `to` each message of the program after `frames` checks it, and between
/// frames the hello of `admission` at each renewal after the first `seen`. Gives
/// `None` once the program finishes and it finishes `to`, or the cut when the stream
/// to the home fails.
async fn up(
    state: &RefCell<State>,
    admission: &Admission,
    mut seen: u64,
    frames: &mut Frames,
    flight: &Flight,
    receiver: &mut Receiver,
    to: &mut Sender,
) -> Result<Option<Cut>, Error> {
    loop {
        let between = frames.between();
        let message = match frames.due().map_err(Error::Frame)? {
            Due::Draft { .. } => {
                frames.skip();
                continue;
            }
            Due::Body(_) => unreachable!("invariant: a forward takes no draft"),
            Due::Message if between => {
                match next(admission.renewed(seen), flight, receiver).await? {
                    Next::Renewed => {
                        let signed;
                        (signed, seen) = admission.signed();
                        let mut block = alloc(state, signed.encoded_len())?;
                        signed.encode(&mut block);
                        block.freeze()
                    }
                    Next::Message(Some(message)) => {
                        if let Step::Head(_) = frames.decode(&message)? {
                            flight.head();
                        }
                        message
                    }
                    Next::Message(None) => {
                        frames.finish()?;
                        if let Err(error) = to.finish() {
                            return Ok(Some(Cut::Stream(error)));
                        }
                        return Ok(None);
                    }
                }
            }
            Due::Message => {
                let Some(message) = receiver.recv().await? else {
                    frames.finish()?;
                    unreachable!("invariant: a stream that ends in a frame is cut")
                };
                frames.decode(&message)?;
                message
            }
        };
        if let Err(error) = to.send(message).await {
            return Ok(Some(Cut::Stream(error)));
        }
    }
}

/// What comes between frames.
enum Next {
    /// The session admitted a hello.
    Renewed,
    /// The next message of the program, or `None` when it finished.
    Message(Option<Block>),
}

/// Waits for `renewed`, or for the next message of the program while `flight` has room.
async fn next(
    renewed: impl Future<Output = ()>,
    flight: &Flight,
    receiver: &mut Receiver,
) -> Result<Next, Error> {
    let mut renewed = pin!(renewed);
    let mut message = pin!(receiver.recv());
    poll_fn(|cx| {
        if renewed.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Ok(Next::Renewed));
        }
        ready!(flight.poll_room(cx));
        message
            .as_mut()
            .poll(cx)
            .map(|message| Ok(Next::Message(message?)))
    })
    .await
}

/// Sends the program each reply of the home after `flight` checks it. Gives `None`
/// once the home finishes, or the cut when the stream to the home fails or a reply
/// breaks HUB WIRE.
async fn down(
    flight: &Flight,
    from: &mut Receiver,
    sender: &mut Sender,
) -> Result<Option<Cut>, Error> {
    loop {
        let message = match from.recv().await {
            Ok(Some(message)) => message,
            Ok(None) => return Ok(None),
            Err(error) => return Ok(Some(Cut::Stream(error))),
        };
        if flight.reply(&message).is_err() {
            return Ok(Some(Cut::Broken));
        }
        sender.send(message).await?;
    }
}

/// Ends group 0 with `end` once the home cut the forward, unless the home ended it
/// first: sends the program [`Reply::Ended`], then [`Outcome::Ended`] for each frame of
/// `flight` with no outcome, and for each later frame of `program` until it finishes,
/// when it has not. Then finishes the program's stream.
async fn after(
    state: &RefCell<State>,
    end: End,
    frames: &mut Frames,
    flight: &Flight,
    mut program: Option<&mut Receiver>,
    sender: &mut Sender,
) -> Result<(), Error> {
    let end = if let Some(ended) = flight.ended.get() {
        ended
    } else {
        reply(state, sender, Reply::Ended { group: 0, end }).await?;
        end
    };
    let (mut answered, mut heads) = (flight.answered.get(), flight.heads.get());
    loop {
        while answered < heads {
            answered += 1;
            let outcome = Reply::Outcome {
                frame: answered,
                group: 0,
                outcome: Outcome::Ended(end),
            };
            reply(state, sender, outcome).await?;
        }
        let Some(receiver) = program.as_deref_mut() else {
            break;
        };
        match frames.due().map_err(Error::Frame)? {
            Due::Draft { .. } => frames.skip(),
            Due::Body(_) => unreachable!("invariant: a forward takes no draft"),
            Due::Message => {
                let Some(message) = receiver.recv().await? else {
                    frames.finish()?;
                    break;
                };
                if let Step::Head(_) = frames.decode(&message)? {
                    heads += 1;
                }
            }
        }
    }
    sender.finish()?;
    Ok(())
}

/// Sends `reply` to the program.
async fn reply(
    state: &RefCell<State>,
    sender: &mut Sender,
    reply: Reply,
) -> Result<(), Error> {
    let block = alloc(state, reply.encoded_len())?;
    send(sender, block, reply).await
}

// A run of more places than a message holds needs over 12000 channels in one open, so
// this test calls the private `runs`.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_cut_the_places_at_the_bytes_of_a_message() {
        let runs = |channels, max| {
            let ends = |run: Range<u32>| (run.start, run.end);
            runs(channels, max).map(ends).collect::<Vec<_>>()
        };
        assert_eq!(runs(400, 1472), [(0, 368), (368, 400)]);
        assert_eq!(runs(368, 1472), [(0, 368)]);
        assert_eq!(runs(369, 1475), [(0, 368), (368, 369)]);
        assert_eq!(runs(0, 1472), []);
    }
}

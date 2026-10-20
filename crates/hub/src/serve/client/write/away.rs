//! A program's write whose home is another node: the forward from the program's node.

use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::ops::Range;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use block::Block;
use transport::stream::{Receiver, Sender};
use transport::{Class, Code};
use types::name::Name;
use types::node;
use wire::Protocol;
use wire::header::MALFORMED;
use wire::hub::client::write::{Open, Opening, Start, Write};
use wire::hub::client::{Place, Places, Refusal, Signed};
use wire::hub::forward::{Forward, places};
use wire::hub::write::{Due, Frames, Replies, Reply, Step};

use super::{Reserved, bytes};
use crate::serve::admission::{Admission, Renewal};
use crate::serve::{Error, alloc, unless};
use crate::{State, dial};

/// The most frames sent to the home with an outcome due. The node reads no more of the
/// program while so many are due.
const IN_FLIGHT: u64 = 64;

/// Serves the write of the subject of `admission` that `write` starts with `open`,
/// whose home is `home`, another node: forwards it to `home`, sends the program the
/// places of `home`, then sends `home` each message of the program unchanged, and each
/// renewal of the hello of `admission`, and sends the program each reply of `home`
/// unchanged. Finishes the program's stream once the program and `home` finish.
///
/// # Errors
///
/// [`Error::Forward`] when the forward ends before the program and `home` finish it,
/// and the errors of the program's stream.
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
    let reserved = Reserved::new(state, subject.clone(), write.length)?;
    // Each field of an open has one encoding, so these are the bytes the program
    // signed.
    let mut body = vec![0; open.encoded_len()];
    open.encode(&mut body);
    drop(open);
    let channels = u32::try_from(channels)
        .expect("invariant: an open names fewer than u32::MAX channels");
    let renewal = admission.renewal();
    let forward = Forward { places: channels };
    let started = {
        let program = pin!(trailing(receiver));
        let start = start(state, &renewal, &subject, home, write, &body, forward);
        unless(program, start).await?
    };
    drop((body, reserved));
    let Some(started) = started else {
        sender.finish()?;
        return Ok(());
    };
    let Started {
        mut to,
        mut from,
        start: Start { places, set },
        held,
    } = started?;
    super::super::places(state, sender, &places).await?;
    drop((places, held));
    let mut frames = Frames::new(set);
    let flight = Flight::new();
    let cut = {
        let mut up = pin!(up(state, &renewal, &mut frames, &flight, receiver, &mut to));
        let mut down = pin!(down(&flight, &mut from, sender));
        let mut finished = false;
        poll_fn(|cx| {
            if !finished {
                match up.as_mut().poll(cx) {
                    Poll::Ready(Ok(None)) => finished = true,
                    Poll::Pending => {}
                    up @ Poll::Ready(_) => return up,
                }
            }
            down.as_mut().poll(cx).map(|cut| match cut {
                Ok(None) if !finished || flight.due() => Ok(Some(Cut::Broken)),
                cut => cut,
            })
        })
        .await?
    };
    let Some(cut) = cut else {
        sender.finish()?;
        return Ok(());
    };
    to.reset(cut.code());
    from.stop(cut.code());
    Err(Error::Forward {
        home,
        refusal: cut.refusal(),
    })
}

/// Waits for the program to finish while the write opens at its home. Each message of
/// the program before the places breaks the client wire.
async fn trailing(receiver: &mut Receiver) -> Result<(), Error> {
    match receiver.recv().await? {
        Some(_) => Err(Error::Client(wire::hub::client::Error::Trailing)),
        None => Ok(()),
    }
}

/// A forward that its home started.
struct Started {
    to: Sender,
    from: Receiver,
    start: Start,
    /// The hold of the places of `start` in the share of the subject.
    held: Reserved,
}

/// Opens `forward` at `home`, the forward of the write that `write` starts with
/// `body`, with the hello of `renewal`, and holds the places of `home` in the share of
/// `subject`. A session that closes with `Code(0)` before the places gets another
/// try, as [`dial::retry`] states.
async fn start(
    state: &Rc<RefCell<State>>,
    renewal: &Renewal<'_>,
    subject: &Name,
    home: node::Key,
    write: Write,
    body: &[u8],
    forward: Forward,
) -> Result<Started, Error> {
    let closed: fn(&Error) -> Option<&transport::Error> = |error| match error {
        Error::Stream(error) => Some(error),
        _ => None,
    };
    let attempt =
        async || attempt(state, renewal, subject, home, write, body, forward).await;
    dial::retry(attempt, closed)
        .await
        .map_err(|error| match error {
            Error::Stream(error) => Error::Forward {
                home,
                refusal: refusal(&error),
            },
            error => error,
        })
}

/// One try of [`start`].
async fn attempt(
    state: &Rc<RefCell<State>>,
    renewal: &Renewal<'_>,
    subject: &Name,
    home: node::Key,
    write: Write,
    body: &[u8],
    forward: Forward,
) -> Result<Started, Error> {
    let (mut to, from) = dial::dial(state, home).await?.open(Class::Complete).await?;
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
    message(state, &mut to, Forward::LEN, |out| forward.encode(out)).await?;
    let signed = renewal.signed();
    message(state, &mut to, signed.encoded_len(), |out| {
        signed.encode(out);
    })
    .await?;
    drop(signed);
    message(state, &mut to, Write::LEN, |out| write.encode(out)).await?;
    let most = to.bytes_max().min(largest);
    for chunk in body.chunks(most) {
        message(state, &mut to, chunk.len(), |out| {
            out.copy_from_slice(chunk);
        })
        .await?;
    }
    for run in runs(forward.places, most) {
        let len = run.len() * places::LEN;
        message(state, &mut to, len, |out| places::encode(run, out)).await?;
    }
    opened(state, renewal, subject, home, forward.places, to, from).await
}

/// Waits for the places and the start of `home` on `from` after an open, with each
/// renewal of `renewal` sent on `to`, and holds the places in the share of `subject`.
/// Each channel of the open, `places` in all, has one place, and its one index another.
async fn opened(
    state: &Rc<RefCell<State>>,
    renewal: &Renewal<'_>,
    subject: &Name,
    home: node::Key,
    places: u32,
    mut to: Sender,
    mut from: Receiver,
) -> Result<Started, Error> {
    let mut opening = Opening::default();
    let mut held: Option<Reserved> = None;
    loop {
        let message = match next(renewal, |_| Poll::Ready(()), &mut from).await? {
            Next::Renewed(signed) => {
                message(state, &mut to, signed.encoded_len(), |out| {
                    signed.encode(out);
                })
                .await?;
                continue;
            }
            Next::Message(message) => message,
        };
        if let Some(message) = message.as_deref() {
            if let Some(held) = held.as_mut() {
                // A place holds no more than its bytes, past its fixed size.
                held.add(bytes(message.len()))?;
            } else if let Ok(head) = Places::decode(message) {
                if head.count > places + 1 {
                    return Err(broken(home, to, from));
                }
                let count = usize::try_from(head.count)
                    .expect("invariant: a usize holds a u32");
                let fixed = bytes(count * size_of::<Place>());
                held = Some(Reserved::new(state, subject.clone(), fixed)?);
            }
        }
        match opening.decode(message.as_deref()) {
            Ok(Some(start)) => {
                let held = held.expect("invariant: a start follows its places");
                return Ok(Started {
                    to,
                    from,
                    start,
                    held,
                });
            }
            Ok(None) => {}
            Err(_) => return Err(broken(home, to, from)),
        }
    }
}

/// Stops each half of the stream to `home`, which broke HUB WIRE, and gives the error
/// of the program.
fn broken(home: node::Key, to: Sender, from: Receiver) -> Error {
    to.reset(Code(MALFORMED));
    from.stop(Code(MALFORMED));
    Error::Forward {
        home,
        refusal: Refusal::Failed,
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

/// The refusal that the program gets when `error` ends the stream to the home: the
/// code that the home stopped or reset the stream with, else `FAILED`. A `MALFORMED`
/// of the home gives `FAILED` too: it tells of a message of this node, not of the
/// program.
fn refusal(error: &transport::Error) -> Refusal {
    let (transport::Error::Reset { code } | transport::Error::Stopped { code }) = error
    else {
        return Refusal::Failed;
    };
    match Refusal::from_code(code.0) {
        Some(Refusal::Malformed) | None => Refusal::Failed,
        Some(refusal) => refusal,
    }
}

/// Why the forward ended before the program and the home finished it.
enum Cut {
    /// The stream to the home failed with this error.
    Stream(transport::Error),
    /// The home broke HUB WIRE: a reply that [`Replies::decode`] refuses, or a finish
    /// before the program's or with an outcome due.
    Broken,
}

impl Cut {
    /// The refusal that the program gets.
    fn refusal(&self) -> Refusal {
        match self {
            Self::Stream(error) => refusal(error),
            Self::Broken => Refusal::Failed,
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
    /// The task that waits for an outcome while [`IN_FLIGHT`] frames are due.
    waiting: Cell<Option<Waker>>,
}

impl Flight {
    fn new() -> Self {
        Self {
            replies: RefCell::new(Replies::new(1)),
            heads: Cell::default(),
            answered: Cell::default(),
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
        if let Reply::Outcome { .. } = self.replies.borrow_mut().decode(message)? {
            self.answered.set(self.answered.get() + 1);
            if let Some(waker) = self.waiting.take() {
                waker.wake();
            }
        }
        Ok(())
    }

    /// Whether a frame has an outcome due.
    fn due(&self) -> bool {
        self.heads.get() > self.answered.get()
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
/// frames each hello that `renewal` gives. Gives `None` once the program finishes and
/// it finishes `to`, or the cut when the stream to the home fails.
async fn up(
    state: &RefCell<State>,
    renewal: &Renewal<'_>,
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
                match next(renewal, |cx| flight.poll_room(cx), receiver).await? {
                    Next::Renewed(signed) => {
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

/// What comes next on a stream that takes renewals.
enum Next {
    /// The hello that the session admitted.
    Renewed(Signed),
    /// The next message, or `None` when the stream finished.
    Message(Option<Block>),
}

/// Waits for a hello that `renewal` gives, or, once `room` is ready, for the next
/// message of `receiver`. A renewal goes first.
async fn next(
    renewal: &Renewal<'_>,
    room: impl Fn(&Context<'_>) -> Poll<()>,
    receiver: &mut Receiver,
) -> Result<Next, Error> {
    let mut message = pin!(receiver.recv());
    poll_fn(|cx| {
        if let Some(signed) = renewal.renewed() {
            return Poll::Ready(Ok(Next::Renewed(signed)));
        }
        if room(cx).is_ready()
            && let Poll::Ready(message) = message.as_mut().poll(cx)
        {
            return Poll::Ready(message.map(Next::Message).map_err(Error::from));
        }
        renewal.register(cx);
        Poll::Pending
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

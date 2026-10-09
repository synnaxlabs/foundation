//! A reader session at another node's home: one hub stream to that home (HUB WIRE).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt;
use std::future::poll_fn;
use std::pin::{Pin, pin};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::task::{Context, Poll, Waker, ready};

use block::Block;
use transport::stream::{Receiver, Sender};
use transport::{Class, Code};
use types::frame::key_set::KeySet;
use types::frame::{Draft, Form, Frame, Layout, Mask};
use wire::Protocol;
use wire::hub::{Credit, FromHome, Head, Open, Refusal, keys};

use super::{Ended, Error, Mode, Streak, WINDOW};
use crate::State;

/// A reader session on one stream to the home. A task takes each frame off the stream
/// as it arrives, so an idle caller never holds the window of the session. Each credit
/// on its way lives in it, so a dropped [`Remote::take`] loses nothing.
#[derive(Debug)]
pub(super) struct Remote {
    state: Rc<RefCell<State>>,
    queue: Rc<RefCell<Queue>>,
    /// `None` once the session ended.
    out: Option<Out>,
    /// The charge of each frame given back, for a complete reader.
    credit: Option<u64>,
    /// The grant that the home has, which the task checks.
    limit: Rc<Cell<u64>>,
    /// The reader's key set. Place `n` of the open is entry `n`.
    set: Arc<KeySet>,
    /// The mask of every entry of `set`.
    mask: Mask,
    streak: Streak,
}

/// The frames that the task took and the caller has not, and why the session ended.
#[derive(Debug, Default)]
struct Queue {
    frames: VecDeque<Frame>,
    /// Why the session ended. The caller gets it after each frame in `frames`.
    ended: Option<Ended>,
    /// The waker of a [`Remote::take`] that waits for a frame.
    taker: Option<Waker>,
    /// The waker of the task, which ends once the [`Remote`] drops.
    task: Option<Waker>,
}

/// The receiving half of the stream, and the frame that arrives on it.
#[derive(Debug)]
struct Inbound {
    state: Rc<RefCell<State>>,
    /// `None` once the session ended.
    receiver: Option<Receiver>,
    decoder: wire::hub::Reader,
    set: Arc<KeySet>,
    head: Option<Head>,
    /// The entry and end of each series of the frame that arrives.
    ends: Vec<(usize, usize)>,
    draft: Option<Draft>,
    /// The grant that the home has, for a complete reader.
    limit: Option<Rc<Cell<u64>>>,
    /// The charges of the frames that arrived.
    arrived: u64,
}

impl Remote {
    /// Opens a session of `mode` on `set` at `home`, another node, and waits for the
    /// home to open it. Then spawns the task that takes its frames.
    pub(super) async fn open(
        state: &Rc<RefCell<State>>,
        home: types::node::Key,
        set: Arc<KeySet>,
        mode: Mode,
    ) -> Result<Self, Error> {
        let (class, wire_mode, credit) = match mode {
            Mode::Complete => (
                Class::Complete,
                wire::hub::Mode::Complete {
                    limit_bytes: WINDOW,
                },
                Some(0),
            ),
            Mode::Latest => (Class::Latest, wire::hub::Mode::Latest, None),
        };
        let open = Open {
            mode: wire_mode,
            channels: u32::try_from(set.entries().len())
                .expect("invariant: a key set holds at most 2^32 entries"),
        };
        let (mut sender, mut receiver) = dial(state, home)
            .await?
            .open(class)
            .await
            .map_err(Error::Transport)?;
        let mut decoder = wire::hub::Reader::new(&open);
        let opened = handshake(
            state,
            (&mut sender, &mut receiver),
            &mut decoder,
            &open,
            &set,
        )
        .await;
        if let Err(error) = opened {
            let refusal = match error {
                Error::Message(_) => Some(Refusal::Malformed),
                Error::Pool(_) => Some(Refusal::Busy),
                _ => None,
            };
            if let Some(refusal) = refusal {
                receiver.stop(Code(refusal.code()));
                sender.reset(Code(refusal.code()));
            }
            return Err(error);
        }
        let limit = Rc::new(Cell::new(WINDOW));
        let queue = Rc::new(RefCell::new(Queue::default()));
        let checked = credit.map(|_| Rc::clone(&limit));
        let inbound = Inbound::new(state, receiver, decoder, &set, checked);
        let latest = mode == Mode::Latest;
        let task = run(Rc::downgrade(&queue), inbound, latest);
        state.borrow().tasks.spawn(task);
        let mask = Mask::new(&set, set.entries().iter().map(|entry| entry.slot));
        Ok(Self {
            state: Rc::clone(state),
            queue,
            out: Some(Out::Idle(sender)),
            credit,
            limit,
            set,
            mask,
            streak: Streak::default(),
        })
    }

    /// Adds the charge of `frame`, which the reader gave back, to the credit.
    pub(super) fn give_back(&mut self, frame: &Frame) {
        if let Some(taken) = &mut self.credit {
            *taken += frame.charge();
        }
    }

    /// The next frame, its key set, and the mask of every entry in it. After
    /// [`STREAK`](super::STREAK) frames in a row, it yields once.
    ///
    /// # Errors
    ///
    /// The [`Ended`] that ended the session, after each frame that arrived before it,
    /// on this and every later call.
    pub(super) async fn take(&mut self) -> Result<(Frame, &Arc<KeySet>, &Mask), Ended> {
        if let Err(ended) = self.grant() {
            self.queue.borrow_mut().end(ended);
        }
        let Self {
            queue,
            out,
            credit,
            limit,
            streak,
            ..
        } = &mut *self;
        let next = poll_fn(|cx| {
            ready!(streak.poll(cx));
            if queue.borrow().ended.is_none() {
                poll_credit(out, credit, limit, cx);
            }
            let polled = queue.borrow_mut().poll_take(cx);
            streak.count(&polled);
            polled
        })
        .await;
        match next {
            Ok(frame) => Ok((frame, &self.set, &self.mask)),
            Err(ended) => {
                // A credit on its way drops with its sender, which resets with code
                // 0. The home still sees the stop.
                if let (Some(refusal), Some(Out::Idle(sender))) =
                    (refusal(&ended), self.out.take())
                {
                    sender.reset(Code(refusal.code()));
                }
                Err(ended)
            }
        }
    }

    /// Sends the credit once the home's grant is half a window short of the frames
    /// given back plus a window, unless the session ended or a credit is on its
    /// way. A credit that finds no room goes on its way, and [`poll_credit`] sends
    /// it. A send that fails sends no more credits: the home stopped reading them,
    /// and the frames that it sent still arrive.
    fn grant(&mut self) -> Result<(), Ended> {
        let Some(taken) = self.credit else {
            return Ok(());
        };
        if self.queue.borrow().ended.is_some() {
            return Ok(());
        }
        let limit_bytes = taken + WINDOW;
        let Some(Out::Idle(sender)) = &mut self.out else {
            return Ok(());
        };
        if limit_bytes - self.limit.get() < WINDOW / 2 {
            return Ok(());
        }
        let mut block = self
            .state
            .borrow()
            .alloc(Credit::LEN)
            .map_err(Ended::Pool)?;
        Credit { limit_bytes }.encode(&mut block);
        let block = match sender.try_send(block.freeze()) {
            Ok(None) => {
                self.limit.set(limit_bytes);
                return Ok(());
            }
            Ok(Some(block)) => block,
            Err(_) => {
                (self.out, self.credit) = (None, None);
                return Ok(());
            }
        };
        let Some(Out::Idle(mut sender)) = self.out.take() else {
            unreachable!("invariant: no credit is on its way");
        };
        let sending = async move {
            let sent = sender.send(block).await;
            (sender, limit_bytes, sent)
        };
        self.out = Some(Out::Sending(Box::pin(sending)));
        Ok(())
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let task = self.queue.borrow_mut().task.take();
        if let Some(task) = task {
            task.wake();
        }
    }
}

impl Queue {
    /// The oldest frame, else the end, else `Pending` with the waker kept.
    fn poll_take(&mut self, cx: &Context<'_>) -> Poll<Result<Frame, Ended>> {
        if let Some(frame) = self.frames.pop_front() {
            return Poll::Ready(Ok(frame));
        }
        if let Some(ended) = &self.ended {
            return Poll::Ready(Err(ended.clone()));
        }
        keep(&mut self.taker, cx);
        Poll::Pending
    }

    /// Ends the session with `ended` after the frames that wait, unless it ended.
    /// Wakes the task, which stops the stream.
    fn end(&mut self, ended: Ended) {
        if self.ended.is_none() {
            self.ended = Some(ended);
            if let Some(task) = self.task.take() {
                task.wake();
            }
        }
    }
}

/// Keeps the waker of `cx` in `slot`, unless the one there wakes the same task.
fn keep(slot: &mut Option<Waker>, cx: &Context<'_>) {
    if !slot
        .as_ref()
        .is_some_and(|waker| waker.will_wake(cx.waker()))
    {
        *slot = Some(cx.waker().clone());
    }
}

/// Takes each frame off the stream of `inbound` as it arrives into `queue`, until the
/// session ends or the [`Remote`] drops. A latest reader keeps only the newest frame.
/// An end that the task finds stops the stream with its refusal, if it has one.
async fn run(queue: Weak<RefCell<Queue>>, mut inbound: Inbound, latest: bool) {
    let mut streak = Streak::default();
    loop {
        let next = {
            let mut next = pin!(inbound.next());
            poll_fn(|cx| {
                let Some(queue) = queue.upgrade() else {
                    return Poll::Ready(None);
                };
                let mut queue = queue.borrow_mut();
                if let Some(ended) = &queue.ended {
                    return Poll::Ready(Some(Err(ended.clone())));
                }
                keep(&mut queue.task, cx);
                drop(queue);
                ready!(streak.poll(cx));
                let polled = next.as_mut().poll(cx);
                streak.count(&polled);
                polled.map(Some)
            })
            .await
        };
        let (Some(next), Some(queue)) = (next, queue.upgrade()) else {
            return;
        };
        let mut queue = queue.borrow_mut();
        match next {
            Ok(frame) => {
                if latest {
                    queue.frames.clear();
                }
                queue.frames.push_back(frame);
            }
            Err(ended) => {
                if let Some(refusal) = refusal(&ended) {
                    inbound
                        .receiver
                        .take()
                        .expect("invariant: a session holds its stream until it ends")
                        .stop(Code(refusal.code()));
                }
                queue.ended.get_or_insert(ended);
            }
        }
        if let Some(taker) = queue.taker.take() {
            taker.wake();
        }
        if queue.ended.is_some() {
            return;
        }
    }
}

impl Inbound {
    fn new(
        state: &Rc<RefCell<State>>,
        receiver: Receiver,
        decoder: wire::hub::Reader,
        set: &Arc<KeySet>,
        limit: Option<Rc<Cell<u64>>>,
    ) -> Self {
        Self {
            state: Rc::clone(state),
            receiver: Some(receiver),
            decoder,
            set: Arc::clone(set),
            head: None,
            ends: Vec::new(),
            draft: None,
            limit,
            arrived: 0,
        }
    }

    async fn next(&mut self) -> Result<Frame, Ended> {
        let receiver = self
            .receiver
            .as_mut()
            .expect("invariant: a session holds its stream until it ends");
        loop {
            if let Some(start) = self.decoder.body() {
                let draft = self
                    .draft
                    .as_mut()
                    .expect("invariant: a body arrives into its draft");
                let body = &mut draft.body_mut()[start..];
                let last = match receiver.recv_into(body).await {
                    Ok(Some(len)) => last(self.decoder.decode(&body[..len]))?,
                    Ok(None) => return Err(finished(&self.decoder)),
                    Err(transport::Error::TooLarge { .. }) => {
                        let message = recv(receiver, &self.decoder).await?;
                        last(self.decoder.decode(&message))?
                    }
                    Err(error) => return Err(ended(error)),
                };
                if last {
                    return Ok(self.freeze());
                }
                continue;
            }
            let message = recv(receiver, &self.decoder).await?;
            match self.decoder.decode(&message).map_err(Ended::Message)? {
                FromHome::Head(head) => {
                    if let Some(limit) = &self.limit
                        && self.arrived >= limit.get()
                    {
                        return Err(Ended::Credit {
                            limit_bytes: limit.get(),
                        });
                    }
                    self.head = Some(head);
                    self.ends.clear();
                }
                FromHome::Ends { ends, last } => {
                    self.ends.extend(
                        ends.map(|(place, end)| (to_usize(place), to_usize(end))),
                    );
                    if last {
                        let layout = Layout::from_ends(&self.set, &self.ends)
                            .map_err(Ended::Frame)?;
                        let state = self.state.borrow();
                        let draft = layout
                            .draft(state.home.pool(), Form::Encoded)
                            .map_err(Ended::Pool)?;
                        self.draft = Some(draft);
                        if self.decoder.body().is_none() {
                            drop(state);
                            return Ok(self.freeze());
                        }
                    }
                }
                FromHome::Behind => return Err(Ended::Behind),
                FromHome::Opened | FromHome::Body { .. } => {
                    unreachable!("invariant: the decoder gives no opened or body here")
                }
            }
        }
    }

    /// The frame of the head, the ends, and the draft that arrived.
    fn freeze(&mut self) -> Frame {
        let head = self.head.take().expect("invariant: a frame has a head");
        let mut draft = self.draft.take().expect("invariant: a frame has a draft");
        draft.set_count(0, head.range.count);
        draft.set_seq(0, head.range.seq);
        let frame = draft.freeze(head.path);
        self.arrived += frame.charge();
        frame
    }
}

/// The sending half of the stream to the home.
enum Out {
    /// No credit is on its way.
    Idle(Sender),
    /// A credit on its way, which owns the sender until the stream holds all of the
    /// credit. A dropped [`Remote::take`] never cuts it: a cut send resets the stream.
    /// Once the session ended, nothing polls it, so it never reaches the home.
    Sending(Pin<Box<dyn Future<Output = Sent>>>),
}

/// The sender, the limit of the credit that it sent, and the result of the send.
type Sent = (Sender, u64, Result<(), transport::Error>);

impl fmt::Debug for Out {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle(sender) => f.debug_tuple("Idle").field(sender).finish(),
            Self::Sending(_) => f.write_str("Sending"),
        }
    }
}

/// Polls the credit that `out` has on its way, and raises the grant in `limit` once
/// the stream holds it. A send that fails clears both `out` and `credit`, as
/// [`Remote::grant`] says.
fn poll_credit(
    out: &mut Option<Out>,
    credit: &mut Option<u64>,
    limit: &Cell<u64>,
    cx: &mut Context<'_>,
) {
    if let Some(Out::Sending(sending)) = out
        && let Poll::Ready((sender, limit_bytes, sent)) = sending.as_mut().poll(cx)
    {
        if sent.is_err() {
            (*out, *credit) = (None, None);
            return;
        }
        *out = Some(Out::Idle(sender));
        limit.set(limit_bytes);
    }
}

/// The session to `home`, another node, that the shard's transport holds or dials.
async fn dial(
    state: &Rc<RefCell<State>>,
    home: types::node::Key,
) -> Result<transport::Session, Error> {
    let (transport, card) = {
        let state = state.borrow();
        let region = state
            .region
            .as_ref()
            .expect("invariant: only a node in a region has a remote home");
        let member = region
            .mesh
            .member(home)
            .expect("invariant: the mesh names only a member as a home");
        (Rc::clone(&region.transport), member.card.card().clone())
    };
    transport
        .dial(card.public_key, card.addresses.as_slice())
        .await
        .map_err(Error::Transport)
}

/// Sends the header, `open`, and the keys of `set` on `sender`, and waits for the
/// home's `Opened`.
async fn handshake(
    state: &RefCell<State>,
    (sender, receiver): (&mut Sender, &mut Receiver),
    decoder: &mut wire::hub::Reader,
    open: &Open,
    set: &KeySet,
) -> Result<(), Error> {
    let header = wire::header::encode(Protocol::Hub);
    send(state, sender, header.len(), |out| {
        out.copy_from_slice(&header);
    })
    .await?;
    send(state, sender, open.encoded_len(), |out| {
        open.encode(out);
    })
    .await?;
    let keys: Vec<_> = set.entries().iter().map(|entry| entry.key).collect();
    let most = sender.bytes_max().min(state.borrow().home.pool().largest());
    for run in keys.chunks(most / keys::LEN) {
        send(state, sender, run.len() * keys::LEN, |out| {
            keys::encode(run, out);
        })
        .await?;
    }
    match receiver.recv().await {
        Ok(Some(message)) => match decoder.decode(&message) {
            Ok(FromHome::Opened) => Ok(()),
            Ok(_) => unreachable!("invariant: the decoder gives opened first"),
            Err(error) => Err(Error::Message(error)),
        },
        Ok(None) => {
            Err(Error::Message(decoder.end().expect_err(
                "invariant: a stream before opened may not end",
            )))
        }
        Err(error) => Err(refused(error).map_or_else(Error::Transport, Error::Refused)),
    }
}

/// Sends a message of `len` bytes that `fill` writes, in a block of the home's pool.
async fn send(
    state: &RefCell<State>,
    sender: &mut Sender,
    len: usize,
    fill: impl FnOnce(&mut [u8]),
) -> Result<(), Error> {
    let mut block = state.borrow().alloc(len).map_err(Error::Pool)?;
    fill(&mut block);
    sender
        .send(block.freeze())
        .await
        .map_err(|error| refused(error).map_or_else(Error::Transport, Error::Refused))
}

/// Whether `message`, a body message, is the last of its frame.
fn last(message: Result<FromHome<'_>, wire::hub::Error>) -> Result<bool, Ended> {
    match message.map_err(Ended::Message)? {
        FromHome::Body { last, .. } => Ok(last),
        _ => unreachable!("invariant: a body message follows a body"),
    }
}

async fn recv(
    receiver: &mut Receiver,
    decoder: &wire::hub::Reader,
) -> Result<Block, Ended> {
    match receiver.recv().await {
        Ok(Some(message)) => Ok(message),
        Ok(None) => Err(finished(decoder)),
        Err(error) => Err(ended(error)),
    }
}

/// The refusal of HUB WIRE that `error` carries, or `error` when it carries none.
fn refused(error: transport::Error) -> Result<Refusal, transport::Error> {
    match error {
        transport::Error::Reset { code } | transport::Error::Stopped { code } => {
            Refusal::from_code(code.0).ok_or(error)
        }
        error => Err(error),
    }
}

fn ended(error: transport::Error) -> Ended {
    refused(error).map_or_else(Ended::Stream, Ended::Refused)
}

/// The home finished the stream where `decoder` is.
fn finished(decoder: &wire::hub::Reader) -> Ended {
    Ended::Message(
        decoder
            .end()
            .expect_err("invariant: a session reads no message after Behind"),
    )
}

/// The refusal that stops the stream when `ended` ends the session, if it is this
/// node's to send.
fn refusal(ended: &Ended) -> Option<Refusal> {
    match ended {
        Ended::Message(_) | Ended::Frame(_) | Ended::Credit { .. } => {
            Some(Refusal::Malformed)
        }
        Ended::Pool(_) => Some(Refusal::Busy),
        Ended::Buffer(_) | Ended::Behind | Ended::Stream(_) | Ended::Refused(_) => None,
    }
}

fn to_usize(value: u32) -> usize {
    usize::try_from(value).expect("invariant: a usize holds a u32")
}

#[cfg(test)]
mod tests {
    use std::future;

    use super::*;

    #[test]
    fn a_credit_on_its_way_shows_as_sending() {
        let out = Out::Sending(Box::pin(future::pending()));
        assert_eq!(format!("{out:?}"), "Sending");
    }
}

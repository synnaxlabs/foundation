//! A reader session at another node's home: one hub stream to that home (HUB WIRE).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::{Pin, pin};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::task::{Context, Poll, Waker, ready};

use transport::stream::{Receiver, Sender};
use transport::{Class, Code};
use types::channel;
use types::frame::key_set::{Group, KeySet};
use types::frame::{Form, Frame};
use types::hash;
use types::sample::Type;
use wire::Protocol;
use wire::hub::{Credit, Due, Frames, Open, Refusal, Step, keys};

use super::{Ended, Error, Lens, Matched, Mode, Streak, WINDOW};
use crate::{State, dial};

/// A reader session on one stream to the home. A task opens the stream, takes each
/// frame off it as it arrives, and sends each credit, so an idle caller never holds
/// the window or the send turn of the session.
#[derive(Debug)]
pub(super) struct Remote {
    queue: Rc<RefCell<Queue>>,
    /// The charge of each frame given back, for a complete reader.
    credit: Option<u64>,
    /// The grant that the reader last asked the task to send.
    asked: u64,
    /// The lens of the reader's key set, whose mask holds every entry. Place `n` of
    /// the open is entry `n`.
    lens: Lens,
    streak: Streak,
}

/// Each remote reader that may be open, so that a removal of one of its channels ends
/// it.
#[derive(Debug, Default)]
pub(crate) struct Sessions(Vec<Opened>);

/// The channels of a remote reader, and its queue.
#[derive(Debug)]
struct Opened {
    keys: Box<[channel::Key]>,
    queue: Weak<RefCell<Queue>>,
}

impl Sessions {
    /// Adds `queue`, the queue of a session on `keys`, and drops each queue that went.
    fn add(&mut self, keys: Box<[channel::Key]>, queue: &Rc<RefCell<Queue>>) {
        self.0.retain(|opened| opened.queue.strong_count() > 0);
        self.0.push(Opened {
            keys,
            queue: Rc::downgrade(queue),
        });
    }

    /// Ends each session on a channel of `removed` with [`Ended::Removed`] and the
    /// first such channel.
    pub(crate) fn end(&mut self, removed: &hash::Set<channel::Key>) {
        self.0.retain(|opened| {
            let Some(queue) = opened.queue.upgrade() else {
                return false;
            };
            let Some(&key) = opened.keys.iter().find(|key| removed.contains(key))
            else {
                return true;
            };
            queue.borrow_mut().remove(key);
            false
        });
    }
}

/// The frames that the task took and the caller has not, and why the session ended.
#[derive(Debug, Default)]
struct Queue {
    /// The result of the open, the open position on success, until [`Remote::open`]
    /// takes it.
    opened: Option<Result<u64, Error>>,
    frames: VecDeque<Frame>,
    /// Why the session ended. The caller gets it after each frame in `frames`.
    ended: Option<Ended>,
    /// The grant that the task sends next, for a complete reader.
    due: Option<u64>,
    /// The waker of a [`Remote::take`] that waits for a frame.
    taker: Option<Waker>,
    /// The waker of the task of the session.
    task: Option<Waker>,
}

/// The receiving half of the stream, and the frame that arrives on it.
#[derive(Debug)]
struct Inbound {
    state: Rc<RefCell<State>>,
    /// `None` once the session ended.
    receiver: Option<Receiver>,
    frames: Frames,
    /// The grant that the home has, for a complete reader.
    limit: Option<Rc<Cell<u64>>>,
    /// The charges of the frames that arrived.
    arrived: u64,
}

impl Remote {
    /// Opens a session of `mode` on the channels of `matched`, each with its sample
    /// type, at `home`, another node, the home of their index. Spawns the task of the
    /// session, which dials the home, opens the session, and then takes its frames,
    /// and waits until the home opened it. A removal of a channel during the open ends
    /// the session at its first take. Returns the session, the slot of each channel of
    /// `matched` in its order, and the open position that the home gave, or `None`
    /// when the session ended before the home opened it.
    pub(super) async fn open(
        state: &Rc<RefCell<State>>,
        home: types::node::Key,
        matched: Matched<(channel::Key, Type)>,
        mode: Mode,
    ) -> Result<(Self, Box<[channel::Slot]>, Option<u64>), Error> {
        let (index, _) = matched.items[matched.index];
        let data: Vec<_> = matched
            .items
            .iter()
            .copied()
            .filter(|&(key, _)| key != index)
            .collect();
        let keys = data.iter().map(|&(key, _)| key).chain([index]).collect();
        let queue = Rc::new(RefCell::new(Queue::default()));
        state.borrow_mut().remotes.add(keys, &queue);
        let group = Group { index, data: &data };
        let set = state.borrow_mut().interner.intern(&[group]);
        let task = session(
            Rc::downgrade(&queue),
            Rc::clone(state),
            home,
            mode,
            Arc::clone(&set),
        );
        state.borrow().tasks.spawn(task);
        let slot = set.entries()[set.groups()[0]].slot;
        let slots = {
            let mut state = state.borrow_mut();
            let assigned = state.interner.slots();
            let slot_of = |&(key, sample)| {
                if key == index {
                    slot
                } else {
                    assigned.data(key, sample)
                }
            };
            matched.items.iter().map(slot_of).collect()
        };
        let lens = Lens::new(&set, set.entries().iter().map(|entry| entry.slot), slot);
        let remote = Self {
            queue,
            credit: (mode == Mode::Complete).then_some(0),
            asked: WINDOW,
            lens,
            streak: Streak::default(),
        };
        let live = poll_fn(|cx| remote.queue.borrow_mut().poll_open(cx)).await?;
        Ok((remote, slots, live))
    }

    /// Adds the charge of `frame`, which the reader gave back, to the credit.
    pub(super) fn give_back(&mut self, frame: &Frame) {
        if let Some(taken) = &mut self.credit {
            *taken += frame.charge();
        }
    }

    /// The next frame, and the lens of its key set. After [`STREAK`](super::STREAK)
    /// frames in a row, it yields once.
    ///
    /// # Errors
    ///
    /// The [`Ended`] that ended the session, after each frame that arrived before it,
    /// on this and every later call.
    pub(super) async fn take(&mut self) -> Result<(Frame, &Lens), Ended> {
        self.ask();
        let Self { queue, streak, .. } = &mut *self;
        let next = poll_fn(|cx| {
            ready!(streak.poll(cx));
            let polled = queue.borrow_mut().poll_take(cx);
            streak.count(&polled);
            polled
        })
        .await;
        next.map(|frame| (frame, &self.lens))
    }

    /// Asks the task to send a credit once the grant asked is half a window short of
    /// the frames given back plus a window.
    fn ask(&mut self) {
        let Some(taken) = self.credit else {
            return;
        };
        let limit_bytes = taken + WINDOW;
        if limit_bytes - self.asked < WINDOW / 2 {
            return;
        }
        self.asked = limit_bytes;
        let mut queue = self.queue.borrow_mut();
        queue.due = Some(limit_bytes);
        if let Some(task) = &queue.task {
            task.wake_by_ref();
        }
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
    /// The result of the open once the task has it, else `Ok(None)` once the session
    /// ended, else `Pending` with the waker kept.
    fn poll_open(&mut self, cx: &Context<'_>) -> Poll<Result<Option<u64>, Error>> {
        if let Some(opened) = self.opened.take() {
            return Poll::Ready(opened.map(Some));
        }
        if self.ended.is_some() {
            return Poll::Ready(Ok(None));
        }
        keep(&mut self.taker, cx);
        Poll::Pending
    }

    /// Keeps `opened`, the result of the open, and wakes the caller.
    fn open(&mut self, opened: Result<u64, Error>) {
        self.opened = Some(opened);
        if let Some(taker) = self.taker.take() {
            taker.wake();
        }
    }

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

    /// Ends the session with [`Ended::Removed`] and `key` before the frames that
    /// wait, which it drops. Wakes the caller, and the task, which drops the stream.
    fn remove(&mut self, key: channel::Key) {
        self.frames.clear();
        self.ended = Some(Ended::Removed(key));
        for waker in [self.taker.take(), self.task.take()].into_iter().flatten() {
            waker.wake();
        }
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

/// Opens the session of `queue`, a reader of `mode` on `set`, at `home`, and gives
/// `queue` the result. Then takes its frames and sends its credits. Stops with no
/// result once the session ended or the [`Remote`] dropped.
async fn session(
    queue: Weak<RefCell<Queue>>,
    state: Rc<RefCell<State>>,
    home: types::node::Key,
    mode: Mode,
    set: Arc<KeySet>,
) {
    let (class, wire_mode) = match mode {
        Mode::Complete => (
            Class::Complete,
            wire::hub::Mode::Complete {
                limit_bytes: WINDOW,
            },
        ),
        Mode::Latest => (Class::Latest, wire::hub::Mode::Latest),
    };
    let open = Open {
        mode: wire_mode,
        channels: u32::try_from(set.entries().len())
            .expect("invariant: a key set holds at most 2^32 entries"),
    };
    let opened = {
        let mut opening = pin!(connect(&state, home, class, &set, &open));
        poll_fn(|cx| match poll_end(&queue, cx) {
            Poll::Ready(_) => Poll::Ready(None),
            Poll::Pending => opening.as_mut().poll(cx).map(Some),
        })
        .await
    };
    let (Some(opened), Some(waiting)) = (opened, queue.upgrade()) else {
        return;
    };
    let (sender, receiver, frames) = match opened {
        Ok((sender, receiver, frames)) => {
            waiting.borrow_mut().open(Ok(frames.live()));
            (sender, receiver, frames)
        }
        Err(error) => {
            waiting.borrow_mut().open(Err(error));
            return;
        }
    };
    drop(waiting);
    let limit = Rc::new(Cell::new(WINDOW));
    let checked = (mode == Mode::Complete).then(|| Rc::clone(&limit));
    let inbound = Inbound::new(&state, receiver, frames, checked);
    let receiving = pin!(receive(Weak::clone(&queue), inbound, mode == Mode::Latest));
    run(receiving, pin!(grant(queue, state, sender, limit))).await;
}

/// Polls `granting` and `receiving`, the two halves of the task of a session, until
/// both end. In the poll in which `receiving` ends, polls `granting` again if it has
/// not ended, which ends it.
async fn run(
    mut receiving: Pin<&mut impl Future<Output = ()>>,
    mut granting: Pin<&mut impl Future<Output = ()>>,
) {
    let mut granted = false;
    poll_fn(|cx| {
        // Each end reaches both halves in this poll, before the caller can drop the
        // queue: `receive` follows `grant`, and `grant` runs again after an end of
        // `receive`. `receive` runs once, so its yield after a streak holds.
        granted = granted || granting.as_mut().poll(cx).is_ready();
        ready!(receiving.as_mut().poll(cx));
        granted = granted || granting.as_mut().poll(cx).is_ready();
        assert!(
            granted,
            "invariant: grant ends in the poll that ends receive"
        );
        Poll::Ready(())
    })
    .await;
}

/// Takes each frame off the stream of `inbound` as it arrives into `queue`, until the
/// session ends or the [`Remote`] drops. A latest reader keeps only the newest frame.
/// An end that the task finds stops the stream with its refusal, if it has one.
async fn receive(queue: Weak<RefCell<Queue>>, mut inbound: Inbound, latest: bool) {
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
        frames: Frames,
        limit: Option<Rc<Cell<u64>>>,
    ) -> Self {
        Self {
            state: Rc::clone(state),
            receiver: Some(receiver),
            frames,
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
            let step = match self.frames.due().map_err(Ended::Frame)? {
                Due::Message => {
                    let message = receiver.recv().await.map_err(ended)?;
                    self.frames.decode(message.as_deref())
                }
                Due::Body(body) => match receiver.recv_into(body).await {
                    Ok(Some(len)) => self.frames.received(len),
                    Ok(None) => self.frames.decode(None),
                    Err(transport::Error::TooLarge { .. }) => {
                        let message = receiver.recv().await.map_err(ended)?;
                        self.frames.decode(message.as_deref())
                    }
                    Err(error) => return Err(ended(error)),
                },
                Due::Draft(layout) => {
                    let draft = layout
                        .draft(self.state.borrow().home.pool(), Form::Encoded)
                        .map_err(Ended::Pool)?;
                    Ok(self.frames.draft(draft))
                }
            };
            match step.map_err(Ended::Message)? {
                Step::Head(_) => {
                    if let Some(limit) = &self.limit
                        && self.arrived >= limit.get()
                    {
                        return Err(Ended::Credit {
                            limit_bytes: limit.get(),
                        });
                    }
                }
                Step::More => {}
                Step::Frame(frame) => return Ok(self.arrived(frame)),
            }
        }
    }

    /// Counts the charge of `frame`, which arrived.
    fn arrived(&mut self, frame: Frame) -> Frame {
        self.arrived += frame.charge();
        frame
    }
}

/// Sends each credit that `queue` asks for on `sender`, and raises the grant in `limit`
/// once the stream holds it, until the session ends or the [`Remote`] drops. Then
/// resets the stream with the refusal of the end, or with code 0 when it has none or
/// a credit is still on its way. A send that fails sends no more credits: the home
/// stopped reading them, and the frames that it sent still arrive. A pool with no
/// block for a credit ends the session with [`Ended::Pool`].
async fn grant(
    queue: Weak<RefCell<Queue>>,
    state: Rc<RefCell<State>>,
    mut sender: Sender,
    limit: Rc<Cell<u64>>,
) {
    let refusal = loop {
        let limit_bytes = match poll_fn(|cx| poll_due(&queue, cx)).await {
            Ok(limit_bytes) => limit_bytes,
            Err(refusal) => break refusal,
        };
        let block = state.borrow().alloc(Credit::LEN);
        let mut block = match block {
            Ok(block) => block,
            Err(error) => {
                if let Some(queue) = queue.upgrade() {
                    queue.borrow_mut().end(Ended::Pool(error));
                }
                continue;
            }
        };
        Credit { limit_bytes }.encode(&mut block);
        let mut sending = pin!(sender.send(block.freeze()));
        let sent = poll_fn(|cx| match poll_end(&queue, cx) {
            Poll::Ready(_) => Poll::Ready(None),
            Poll::Pending => sending.as_mut().poll(cx).map(Some),
        })
        .await;
        match sent {
            Some(Ok(())) => limit.set(limit_bytes),
            Some(Err(_)) | None => return,
        }
    };
    if let Some(refusal) = refusal {
        sender.reset(Code(refusal.code()));
    }
}

/// The grant that `queue` asks the task to send, else the end as [`poll_end`] gives it.
fn poll_due(
    queue: &Weak<RefCell<Queue>>,
    cx: &Context<'_>,
) -> Poll<Result<u64, Option<Refusal>>> {
    let Some(queue) = queue.upgrade() else {
        return Poll::Ready(Err(None));
    };
    let mut queue = queue.borrow_mut();
    if let Some(ended) = &queue.ended {
        return Poll::Ready(Err(refusal(ended)));
    }
    if let Some(limit_bytes) = queue.due.take() {
        return Poll::Ready(Ok(limit_bytes));
    }
    keep(&mut queue.task, cx);
    Poll::Pending
}

/// The refusal of the end once the session ended, or `None` once the [`Remote`]
/// dropped, else `Pending` with the waker kept in `queue`.
fn poll_end(queue: &Weak<RefCell<Queue>>, cx: &Context<'_>) -> Poll<Option<Refusal>> {
    let Some(queue) = queue.upgrade() else {
        return Poll::Ready(None);
    };
    let mut queue = queue.borrow_mut();
    if let Some(ended) = &queue.ended {
        return Poll::Ready(refusal(ended));
    }
    keep(&mut queue.task, cx);
    Poll::Pending
}

/// Opens a stream of `class` to `home`, and opens the session of `open` on `set`, whose
/// frames it gives. A reply that breaks HUB WIRE, or a pool with no block for a
/// message, stops the stream with its refusal. A session that closes with `Code(0)`
/// before `Opened` gets another open on the session that the next dial gives, as
/// [`dial::retry`] states.
async fn connect(
    state: &Rc<RefCell<State>>,
    home: types::node::Key,
    class: Class,
    set: &Arc<KeySet>,
    open: &Open,
) -> Result<(Sender, Receiver, Frames), Error> {
    dial::retry(
        async || attempt(state, home, class, set, open).await,
        closed,
    )
    .await
}

/// The transport error that `error` carries.
fn closed(error: &Error) -> Option<&transport::Error> {
    match error {
        Error::Transport(error) => Some(error),
        _ => None,
    }
}

/// One try of [`connect`], on the session that the dial gives.
async fn attempt(
    state: &Rc<RefCell<State>>,
    home: types::node::Key,
    class: Class,
    set: &Arc<KeySet>,
    open: &Open,
) -> Result<(Sender, Receiver, Frames), Error> {
    let (mut sender, mut receiver) = dial::dial(state, home)
        .await
        .map_err(Error::Transport)?
        .open(class)
        .await
        .map_err(Error::Transport)?;
    let opened = handshake(state, (&mut sender, &mut receiver), set, open).await;
    match opened {
        Ok(frames) => Ok((sender, receiver, frames)),
        Err(error) => {
            let refusal = match error {
                Error::Message(_) => Some(Refusal::Malformed),
                Error::Pool(_) => Some(Refusal::Busy),
                _ => None,
            };
            if let Some(refusal) = refusal {
                receiver.stop(Code(refusal.code()));
                sender.reset(Code(refusal.code()));
            }
            Err(error)
        }
    }
}

/// Sends the header, `open`, and the keys of `set` on `sender`, and gives the frames
/// that the home's `Opened` opens.
async fn handshake(
    state: &RefCell<State>,
    (sender, receiver): (&mut Sender, &mut Receiver),
    set: &Arc<KeySet>,
    open: &Open,
) -> Result<Frames, Error> {
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
    let reader = wire::hub::Reader::new(open);
    match receiver.recv().await {
        Ok(message) => Frames::open(reader, Arc::clone(set), message.as_deref())
            .map_err(Error::Message),
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

/// The refusal that stops the stream when `ended` ends the session, if it is this
/// node's to send.
fn refusal(ended: &Ended) -> Option<Refusal> {
    match ended {
        Ended::Message(_) | Ended::Frame(_) | Ended::Credit { .. } => {
            Some(Refusal::Malformed)
        }
        Ended::Pool(_) => Some(Refusal::Busy),
        Ended::Buffer(_)
        | Ended::Removed(_)
        | Ended::Replaced
        | Ended::Stream(_)
        | Ended::Refused(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Only the length of the list shows that an add drops each queue that went.
    #[test]
    fn an_add_drops_each_queue_that_went_and_keeps_each_that_lives() {
        let [a, b, c] = [1, 2, 3].map(channel::Key::from_u128);
        let mut sessions = Sessions::default();
        let first = Rc::new(RefCell::new(Queue::default()));
        sessions.add(Box::new([a]), &first);
        sessions.add(Box::new([b]), &Rc::new(RefCell::new(Queue::default())));
        let third = Rc::new(RefCell::new(Queue::default()));
        sessions.add(Box::new([c]), &third);
        assert_eq!(sessions.0.len(), 2);
        sessions.end(&[a].into_iter().collect());
        assert_eq!(first.borrow().ended, Some(Ended::Removed(a)));
        assert_eq!(third.borrow().ended, None);
        assert_eq!(sessions.0.len(), 1);
    }

    // A run gets this order only when the caller asks and the session ends between
    // two polls of the task.
    #[test]
    fn an_end_with_a_refusal_comes_before_a_grant_that_is_due() {
        let queue = Rc::new(RefCell::new(Queue {
            due: Some(2 * WINDOW),
            ended: Some(Ended::Credit {
                limit_bytes: WINDOW,
            }),
            ..Queue::default()
        }));
        let cx = Context::from_waker(Waker::noop());
        assert_eq!(
            poll_due(&Rc::downgrade(&queue), &cx),
            Poll::Ready(Err(Some(Refusal::Malformed)))
        );
        assert_eq!(queue.borrow().due, Some(2 * WINDOW));
    }
}

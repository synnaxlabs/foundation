//! A reader session at another node's home: one hub stream to that home (HUB WIRE).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use block::Block;
use transport::stream::{Receiver, Sender};
use transport::{Class, Code};
use types::frame::key_set::KeySet;
use types::frame::{Draft, Form, Frame, Layout, Mask};
use wire::Protocol;
use wire::header::MALFORMED;
use wire::hub::{BUSY, Credit, FAILED, FromHome, Head, NOT_HOME, Open, UNKNOWN, keys};

use super::{Ended, Error, Mode, WINDOW};
use crate::State;

/// The stop and reset codes of HUB WIRE that refuse or end a session.
const REFUSALS: [u32; 5] = [MALFORMED, UNKNOWN, NOT_HOME, FAILED, BUSY];

/// A reader session on one stream to the home. Each partial frame lives in it, so a
/// dropped [`Remote::take`] loses nothing.
#[derive(Debug)]
pub(super) struct Remote {
    state: Rc<RefCell<State>>,
    /// `None` once the session ended.
    stream: Option<(Sender, Receiver)>,
    decoder: wire::hub::Reader,
    /// The reader's key set. Place `n` of the open is entry `n`.
    set: Arc<KeySet>,
    /// The mask of every entry of `set`.
    mask: Mask,
    /// The grant that the home has, and the charge of each frame given back, for a
    /// complete reader.
    credit: Option<(u64, u64)>,
    head: Option<Head>,
    /// The entry and end of each series of the frame that arrives.
    ends: Vec<(usize, usize)>,
    draft: Option<Draft>,
    ended: Option<Ended>,
}

impl Remote {
    /// Opens a session of `mode` on `set` at `home`, another node, and waits for the
    /// home to open it.
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
                Some((WINDOW, 0)),
            ),
            Mode::Latest => (Class::Latest, wire::hub::Mode::Latest, None),
        };
        let open = Open {
            mode: wire_mode,
            channels: u32::try_from(set.entries().len())
                .expect("invariant: a key set holds at most 2^32 entries"),
        };
        let (mut sender, mut receiver) = crate::stream(state, home, class)
            .await
            .map_err(Error::Transport)?;
        let header = wire::header::encode(Protocol::Hub);
        send(state, &mut sender, header.len(), |out| {
            out.copy_from_slice(&header);
        })
        .await?;
        send(state, &mut sender, open.encoded_len(), |out| {
            open.encode(out);
        })
        .await?;
        let keys: Vec<_> = set.entries().iter().map(|entry| entry.key).collect();
        for run in keys.chunks(sender.bytes_max() / keys::LEN) {
            send(state, &mut sender, run.len() * keys::LEN, |out| {
                keys::encode(run, out);
            })
            .await?;
        }
        let mut decoder = wire::hub::Reader::new(&open);
        let opened = match receiver.recv().await {
            Ok(Some(message)) => match decoder.decode(&message) {
                Ok(FromHome::Opened) => Ok(()),
                Ok(_) => unreachable!("invariant: the decoder gives opened first"),
                Err(error) => Err(Error::Message(error)),
            },
            Ok(None) => Err(Error::Message(wire::hub::Error::Unfinished { remain: 0 })),
            Err(error) => {
                Err(refused(error).map_or_else(Error::Transport, Error::Refused))
            }
        };
        if let Err(error) = opened {
            if let Error::Message(_) = error {
                receiver.stop(Code(MALFORMED));
                sender.reset(Code(MALFORMED));
            }
            return Err(error);
        }
        let mask = Mask::new(&set, set.entries().iter().map(|entry| entry.slot));
        Ok(Self {
            state: Rc::clone(state),
            stream: Some((sender, receiver)),
            decoder,
            set,
            mask,
            credit,
            head: None,
            ends: Vec::new(),
            draft: None,
            ended: None,
        })
    }

    /// Adds the charge of `frame`, which the reader gave back, to the credit.
    pub(super) fn give_back(&mut self, frame: &Frame) {
        if let Some((_, taken)) = &mut self.credit {
            *taken += frame.charge();
        }
    }

    /// The next frame, its key set, and the mask of every entry in it.
    ///
    /// # Errors
    ///
    /// The [`Ended`] that ended the session, on this and every later call.
    pub(super) async fn take(&mut self) -> Result<(Frame, &Arc<KeySet>, &Mask), Ended> {
        if let Some(ended) = &self.ended {
            return Err(ended.clone());
        }
        match self.next().await {
            Ok(frame) => Ok((frame, &self.set, &self.mask)),
            Err(ended) => {
                if let Some((sender, receiver)) = self.stream.take()
                    && let Some(code) = code(&ended)
                {
                    receiver.stop(code);
                    sender.reset(code);
                }
                Err(self.ended.insert(ended).clone())
            }
        }
    }

    async fn next(&mut self) -> Result<Frame, Ended> {
        self.grant()?;
        let (_, receiver) = self
            .stream
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
                    Ok(None) => return Err(unfinished(body.len())),
                    Err(transport::Error::TooLarge { .. }) => {
                        let message = recv(receiver).await?;
                        last(self.decoder.decode(&message))?
                    }
                    Err(error) => return Err(stream(error)),
                };
                if last {
                    return Ok(self.freeze());
                }
                continue;
            }
            let message = recv(receiver).await?;
            match self.decoder.decode(&message).map_err(Ended::Message)? {
                FromHome::Head(head) => {
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

    /// Sends the credit once the home's grant is half a window short of the frames
    /// given back plus a window. A credit that finds no room goes at a later call.
    fn grant(&mut self) -> Result<(), Ended> {
        let Some((granted, taken)) = &mut self.credit else {
            return Ok(());
        };
        let limit_bytes = *taken + WINDOW;
        if limit_bytes - *granted < WINDOW / 2 {
            return Ok(());
        }
        let mut block = self
            .state
            .borrow()
            .home
            .pool()
            .alloc(Credit::LEN)
            .map_err(Ended::Pool)?;
        Credit { limit_bytes }.encode(&mut block);
        let (sender, _) = self
            .stream
            .as_mut()
            .expect("invariant: a session holds its stream until it ends");
        if sender.try_send(block.freeze()).map_err(stream)?.is_none() {
            *granted = limit_bytes;
        }
        Ok(())
    }

    /// The frame of the head, the ends, and the draft that arrived.
    fn freeze(&mut self) -> Frame {
        let head = self.head.take().expect("invariant: a frame has a head");
        let mut draft = self.draft.take().expect("invariant: a frame has a draft");
        draft.set_count(0, head.range.count);
        draft.set_seq(0, head.range.seq);
        draft.freeze(head.path)
    }
}

/// Sends a message of `len` bytes that `fill` writes, in a block of the home's pool.
async fn send(
    state: &RefCell<State>,
    sender: &mut Sender,
    len: usize,
    fill: impl FnOnce(&mut [u8]),
) -> Result<(), Error> {
    let mut block = state.borrow().home.pool().alloc(len).map_err(Error::Pool)?;
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

async fn recv(receiver: &mut Receiver) -> Result<Block, Ended> {
    match receiver.recv().await {
        Ok(Some(message)) => Ok(message),
        Ok(None) => Err(unfinished(0)),
        Err(error) => Err(stream(error)),
    }
}

/// The code of HUB WIRE that `error` carries, or `error` when it carries none.
fn refused(error: transport::Error) -> Result<Code, transport::Error> {
    match error {
        transport::Error::Reset { code } | transport::Error::Stopped { code }
            if REFUSALS.contains(&code.0) =>
        {
            Ok(code)
        }
        error => Err(error),
    }
}

fn stream(error: transport::Error) -> Ended {
    refused(error).map_or_else(Ended::Stream, Ended::Refused)
}

/// The home finished the stream with `remain` bytes of a body to come.
fn unfinished(remain: usize) -> Ended {
    Ended::Message(wire::hub::Error::Unfinished { remain })
}

/// The code that stops the stream when `ended` ends the session, if it is this node's
/// to send.
fn code(ended: &Ended) -> Option<Code> {
    match ended {
        Ended::Message(_) | Ended::Frame(_) => Some(Code(MALFORMED)),
        Ended::Pool(_) => Some(Code(BUSY)),
        Ended::Buffer(_) | Ended::Behind | Ended::Stream(_) | Ended::Refused(_) => None,
    }
}

fn to_usize(value: u32) -> usize {
    usize::try_from(value).expect("invariant: a usize holds a u32")
}

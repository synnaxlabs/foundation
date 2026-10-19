//! The read stream of a client session.

use std::cell::RefCell;
use std::pin::{Pin, pin};
use std::rc::Rc;

use access::proof;
use transport::Class;
use transport::stream::{Receiver, Sender};
use types::name::{Name, Selector};
use wire::hub::client::{Mode, Open, PLACES_MAX, Place, Read, Refusal};

use super::{Reserved, body};
use crate::State;
use crate::channel::Channel;
use crate::link::Session;
use crate::reader::{self, Home, Reader};
use crate::serve::{Error, Layout, reply, unless};

/// Serves the read of `subject` that `read` starts: verifies its open, opens its
/// reader, and sends its places, then each frame of the reader until the program
/// finishes the stream or the reader ends.
pub(super) async fn serve(
    session: &Session,
    class: Class,
    subject: Name,
    read: Read,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<(), Error> {
    let (open, mode) = open(session, class, subject, read, receiver).await?;
    let mut program = pin!(async {
        match receiver.recv().await? {
            None => Ok(()),
            Some(_) => Err(Error::Client(wire::hub::client::Error::Trailing)),
        }
    });
    let state = &session.state;
    let Some((reader, live, index, places, layout)) =
        here(state, open, mode, program.as_mut()).await?
    else {
        sender.finish()?;
        return Ok(());
    };
    start(state, sender, index, places, live).await?;
    relay(state, reader, layout, program, sender).await
}

/// The verified open of a read, which holds its reservation until it drops.
struct Held {
    bytes: Vec<u8>,
    _reserved: Reserved,
}

impl Held {
    /// The selector of the open, decoded at each call, so that no decoded selector
    /// lives across a wait.
    fn select(&self) -> Selector {
        Open::decode(&self.bytes)
            .expect("invariant: the open decoded once")
            .select
    }
}

/// Reads, verifies, and decodes the open, and gives it with its mode.
async fn open(
    session: &Session,
    class: Class,
    subject: Name,
    read: Read,
    receiver: &mut Receiver,
) -> Result<(Held, reader::Mode), Error> {
    // A read takes no turn.
    let reserved = Reserved::new(&session.state, subject, read.length)?;
    let bytes = body(receiver, read.body()).await?;
    session.client.admission.verify(
        &session.state,
        proof::Kind::Read,
        &bytes,
        &read.signature,
    )?;
    let (wanted, mode) = match Open::decode(&bytes)?.mode {
        Mode::Latest => (Class::Latest, reader::Mode::Latest),
        Mode::Complete => (Class::Complete, reader::Mode::Complete),
    };
    if class != wanted {
        return Err(Error::Class(class));
    }
    let held = Held {
        bytes,
        _reserved: reserved,
    };
    Ok((held, mode))
}

/// Opens the reader of `open` at this node, the home of its index, and gives it with
/// its open position, the place of its index, its places, and their layout, or `None`
/// when `program` finishes first. Holds the reservation of `open` until the open
/// decodes after the wait for the home. Places over [`PLACES_MAX`] give
/// [`Error::Places`], also when the home is another node, and this node then
/// carries no index and gives no slot.
async fn here<F>(
    state: &Rc<RefCell<State>>,
    open: Held,
    mode: reader::Mode,
    program: Pin<&mut F>,
) -> Result<Option<(Reader, u64, usize, Vec<Place>, Layout)>, Error>
where
    F: Future<Output = Result<(), Error>>,
{
    let refused = |error| Error::Reader(Box::new(error));
    let place = |&(name, channel): &(&Name, &Channel)| Place {
        key: channel.key(),
        data_type: channel.sample(),
        name: name.clone(),
    };
    let places = |found: &[(&Name, &Channel)]| found.iter().map(place).collect();
    let found = unless(program, reader::find(state, || open.select(), places)).await;
    drop(open);
    let Some(found) = found? else {
        return Ok(None);
    };
    let (places, home) = found.map_err(refused)?;
    let count = places.items.len();
    if count > usize::try_from(PLACES_MAX).expect("invariant: a usize holds a u32") {
        return Err(Error::Places { count });
    }
    let here = match home {
        Home::Here(here) => here,
        Home::Away(home) => return Err(refused(reader::Error::Remote { home })),
    };
    let channels = here.carry(&mut state.borrow_mut());
    let layout = Layout::new(channels.slots.clone(), channels.index);
    let (reader, live) = Reader::here(state, channels, mode);
    Ok(Some((reader, live, places.index, places.items, layout)))
}

/// Sends `places`, whose index is place `index`, and `Opened` with `live`, the open
/// position, and
/// drops the places.
async fn start(
    state: &RefCell<State>,
    sender: &mut Sender,
    index: usize,
    places: Vec<Place>,
    live: u64,
) -> Result<(), Error> {
    super::places(state, sender, index, &places).await?;
    sender
        .send(reply(state, wire::hub::Reply::Opened { live })?)
        .await?;
    Ok(())
}

/// Sends each frame of `reader` until `program` finishes.
async fn relay<F>(
    state: &RefCell<State>,
    mut reader: Reader,
    mut layout: Layout,
    mut program: Pin<&mut F>,
    sender: &mut Sender,
) -> Result<(), Error>
where
    F: Future<Output = Result<(), Error>>,
{
    loop {
        let Some(next) = unless(program.as_mut(), reader.next()).await? else {
            sender.finish()?;
            return Ok(());
        };
        match next {
            Ok(taken) => {
                layout
                    .send(state, sender, taken.view().frame(), taken.set())
                    .await?;
            }
            Err(ended) => return Err(Error::Ended(Box::new(ended))),
        }
    }
}

/// The refusal that stops a read whose reader did not open with `error`.
///
/// # Panics
///
/// For `Unsynced`, `Full`, and `Unrecorded`, which only a named reader gives.
pub(in crate::serve) fn refused(error: &reader::Error) -> Refusal {
    match error {
        reader::Error::Empty => Refusal::Unknown,
        reader::Error::Remote { .. } => Refusal::NotHome,
        reader::Error::ManyIndexes { .. } => Refusal::Malformed,
        reader::Error::Pool(_) => Refusal::Busy,
        reader::Error::Refused(refusal) => from_home(*refusal),
        reader::Error::Mesh(_)
        | reader::Error::Transport(_)
        | reader::Error::Message(_) => Refusal::Failed,
        reader::Error::Unsynced | reader::Error::Full | reader::Error::Unrecorded => {
            panic!("invariant: an unnamed reader does not give {error:?}")
        }
    }
}

/// The refusal that stops a read whose reader ended as `ended`.
///
/// # Panics
///
/// For `Replaced`, which only a named reader gives.
pub(in crate::serve) fn ended(ended: &reader::Ended) -> Refusal {
    match ended {
        reader::Ended::Removed(_) => Refusal::Unknown,
        reader::Ended::Pool(_) => Refusal::Busy,
        reader::Ended::Refused(refusal) => from_home(*refusal),
        reader::Ended::Buffer(_)
        | reader::Ended::Stream(_)
        | reader::Ended::Message(_)
        | reader::Ended::Frame(_)
        | reader::Ended::Credit { .. } => Refusal::Failed,
        reader::Ended::Replaced => {
            panic!("invariant: a read does not end as {ended:?}")
        }
    }
}

/// The refusal of a read whose remote home refused it with `refusal`: the same code,
/// but `FAILED` for `MALFORMED`, as the program's open was well formed.
fn from_home(refusal: wire::hub::Refusal) -> Refusal {
    match refusal {
        wire::hub::Refusal::Malformed | wire::hub::Refusal::Failed => Refusal::Failed,
        wire::hub::Refusal::Unknown => Refusal::Unknown,
        wire::hub::Refusal::NotHome => Refusal::NotHome,
        wire::hub::Refusal::Busy => Refusal::Busy,
    }
}

//! A forward at the home: the write of a program that its node carries.

use std::cell::RefCell;
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;

use transport::stream::{Receiver, Sender};
use types::node;
use wire::hub::client::Signed;
use wire::hub::client::write::{Open, Write};
use wire::hub::forward::{Forward, places};

use super::Reserved;
use super::write::{self, Origin};
use crate::State;
use crate::serve::Error;
use crate::serve::admission::Admission;

/// Serves the forward from `via` that `forward` starts: admits the program's hello
/// that `via` carries, verifies the open, takes the group at its places, and serves
/// it as a program's write, until the hello ends or `via` finishes the stream.
pub(in crate::serve) async fn serve(
    state: &Rc<RefCell<State>>,
    via: node::Key,
    forward: Forward,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<(), Error> {
    let admission = Admission::new(via);
    let Some(message) = receiver.recv().await? else {
        sender.finish()?;
        return Ok(());
    };
    admission.admit(state, Signed::decode(&message)?)?;
    let mut expiry = pin!(admission.expiry(state));
    // The write polls first, so a renewal moves the end in the poll that admits it.
    let mut served = pin!(group(state, &admission, forward, receiver, sender));
    poll_fn(|cx| match served.as_mut().poll(cx) {
        Poll::Ready(written) => Poll::Ready(written),
        Poll::Pending => expiry.as_mut().poll(cx).map(Err),
    })
    .await
}

/// Serves the write of the group of `forward`, once `admission` admitted its hello.
async fn group(
    state: &Rc<RefCell<State>>,
    admission: &Admission,
    forward: Forward,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<(), Error> {
    let subject = admission
        .subject()
        .expect("invariant: a forward is served after its hello");
    let Some(message) = receiver.recv().await? else {
        sender.finish()?;
        return Ok(());
    };
    let write = Write::decode(&message)?;
    let open = write::open(state, admission, subject.clone(), write, receiver).await?;
    let places = write::bytes(places::LEN * open.channels.len());
    let decoded = write::held(&open.channels) + places;
    let decoded = Reserved::new(state, subject.clone(), decoded)?;
    let Some(open) = take(forward, open, receiver).await? else {
        sender.finish()?;
        return Ok(());
    };
    let _ends = write::ends(state, subject.clone(), &open)?;
    let origin = Origin::Forward(admission);
    write::opened(state, subject, open, origin, decoded, receiver, sender).await
}

/// The open of the group at the places of the run of `forward`, or `None` when the
/// stream ends before the last place.
async fn take(
    forward: Forward,
    mut open: Open,
    receiver: &mut Receiver,
) -> Result<Option<Open>, Error> {
    let channels = u32::try_from(open.channels.len())
        .expect("invariant: an open names fewer than u32::MAX channels");
    let mut run = places::Run::new(forward, channels);
    // The run gives at most one place for each channel.
    let mut taken = Vec::with_capacity(open.channels.len());
    while !run.ended() {
        let Some(message) = receiver.recv().await? else {
            return Ok(None);
        };
        taken.extend(run.decode(&message)?);
    }
    let mut taken = taken.into_iter().peekable();
    open.channels = open
        .channels
        .into_iter()
        .zip(0..)
        .filter_map(|(name, at)| taken.next_if_eq(&at).map(|_| name))
        .collect();
    Ok(Some(open))
}

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

use super::write::{self, Origin};
use crate::State;
use crate::serve::Error;
use crate::serve::admission::Admission;

/// Serves the forward from `via` that `forward` starts: admits the program's hello
/// that `via` carries, and serves the write of the group of `forward` as a program's
/// write, until the hello ends or `via` finishes the stream.
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
    let subject = admission
        .subject()
        .expect("invariant: an admitted hello has a subject");
    let mut expiry = pin!(admission.expiry(state));
    let mut served = pin!(async {
        let Some(message) = receiver.recv().await? else {
            sender.finish()?;
            return Ok(());
        };
        let write = Write::decode(&message)?;
        let origin = Origin::Forward { via, forward };
        write::serve(state, &admission, origin, subject, write, receiver, sender).await
    });
    // The write polls first, so a renewal moves the end in the poll that admits it.
    poll_fn(|cx| match served.as_mut().poll(cx) {
        Poll::Ready(written) => Poll::Ready(written),
        Poll::Pending => expiry.as_mut().poll(cx).map(Err),
    })
    .await
}

/// The open of the group at the places of the run of `forward`, or `None` when the
/// stream ends before the last place.
pub(super) async fn take(
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

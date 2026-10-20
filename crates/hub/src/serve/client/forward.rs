//! A forward at the home: the write of a program that its node carries.

use std::cell::RefCell;
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;

use transport::stream::{Receiver, Sender};
use types::node;
use wire::hub::client::Signed;
use wire::hub::client::write::Write;
use wire::hub::forward::Forward;

use super::write;
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
    drop(message);
    let mut expiry = pin!(admission.expiry(state));
    let mut served = pin!(async {
        let Some(message) = receiver.recv().await? else {
            sender.finish()?;
            return Ok(());
        };
        let write = Write::decode(&message)?;
        drop(message);
        write::forward(state, &admission, via, forward, write, receiver, sender).await
    });
    // The write polls first, so a renewal moves the end in the poll that admits it.
    poll_fn(|cx| match served.as_mut().poll(cx) {
        Poll::Ready(written) => Poll::Ready(written),
        Poll::Pending => expiry.as_mut().poll(cx).map(Err),
    })
    .await
}

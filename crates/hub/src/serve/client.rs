//! The hello stream and the request streams of a client session.

use std::cell::RefCell;
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;

use access::proof::Error as Refusal;
use transport::Code;
use transport::stream::{Incoming, Receiver, Sender};
use wire::hub::client::{
    CAPPED, CHANGED, Challenge, EXPIRED, REFUSED, Response, Signed, UNSYNCED, VIA,
};

use super::{Error, alloc, halves, stop};
use crate::State;
use crate::link::{Served, Shared};

/// A request of a client, which `access` verified.
#[derive(Debug)]
pub struct Request {
    /// The admitted hello of the connection and its signature. Its subject made the
    /// request.
    pub admitted: Signed,
    /// The body, exactly as the program sent it. `hub` does not read it.
    pub body: Vec<u8>,
    /// The signature of the hello's key over the request.
    pub signature: [u8; 64],
    /// Where the response goes.
    pub reply: Reply,
}

/// The response half of a request stream. A drop with no send resets the stream. The
/// link takes its next request from the start of the send, or from the drop.
#[derive(Debug)]
pub struct Reply {
    state: Rc<RefCell<State>>,
    sender: Sender,
    receiver: Receiver,
    open: Open,
}

impl Reply {
    /// Sends `body` as the response, and finishes the stream.
    ///
    /// # Errors
    ///
    /// [`Error::Pool`] when the home's pool has no block for a message, and
    /// [`Error::Stream`] when the stream failed.
    ///
    /// # Panics
    ///
    /// When `body` is over [`BODY_BYTES_MAX`](wire::hub::client::BODY_BYTES_MAX).
    pub async fn send(self, body: &[u8]) -> Result<(), Error> {
        let Self {
            state,
            mut sender,
            receiver,
            open,
        } = self;
        drop(open);
        let sent = respond(&state, &mut sender, body).await;
        if let Err(error) = &sent {
            stop(receiver, sender, error);
        }
        sent
    }
}

/// Holds the one open request of a link, until it drops.
#[derive(Debug)]
struct Open(Rc<Shared>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.open.set(false);
    }
}

/// The code that `error` stops a client stream with. The errors that tell about the
/// spec share `REFUSED`.
pub(crate) fn code(error: &Refusal) -> u32 {
    match error {
        Refusal::Unknown { .. } | Refusal::Unlisted { .. } | Refusal::Signature => {
            REFUSED
        }
        Refusal::Unsynced => UNSYNCED,
        Refusal::Via { .. } => VIA,
        Refusal::Expired { .. } => EXPIRED,
        Refusal::Capped { .. } => CAPPED,
        Refusal::Changed { .. } => CHANGED,
    }
}

/// Serves the hello stream of `shared`, and closes the session when it ends.
pub(crate) async fn hello(
    shared: &Rc<Shared>,
    incoming: Incoming,
) -> Result<Served, Error> {
    let served = match halves(incoming) {
        Ok((mut receiver, mut sender)) => {
            let served = renew(shared, &mut receiver, &mut sender).await;
            if let Err(error) = &served {
                stop(receiver, sender, error);
            }
            served
        }
        Err(error) => Err(error),
    };
    let code = served.as_ref().err().and_then(Error::code);
    shared.session.close(code.unwrap_or(Code(0)));
    served
}

/// Admits the first hello, then each renewal, until the program finishes the stream
/// or the hello expires.
async fn renew(
    shared: &Shared,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<Served, Error> {
    if !take(shared, receiver, sender).await? {
        return Ok(Served::Ended);
    }
    let mut renewals = pin!(async {
        while take(shared, receiver, sender).await? {}
        Ok(Served::Ended)
    });
    let mut expiry = pin!(expiry(shared));
    poll_fn(|cx| match renewals.as_mut().poll(cx) {
        Poll::Ready(served) => Poll::Ready(served),
        Poll::Pending => expiry.as_mut().poll(cx).map(Err),
    })
    .await
}

/// Sends a challenge, then checks the hello that answers it and keeps it. Gives
/// `false` when the program finished the stream first.
async fn take(
    shared: &Shared,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<bool, Error> {
    let nonce = challenge(shared, sender).await?;
    let Some(message) = receiver.recv().await? else {
        return Ok(false);
    };
    let Signed { hello, signature } = Signed::decode(&message)?;
    if hello.nonce != nonce {
        return Err(Error::Stale);
    }
    let state = shared.state.borrow();
    let now = state.time.now().mesh;
    let mut admitted = shared.admitted.borrow_mut();
    let next = match admitted.as_ref() {
        None => state.rules.admit(now, state.node, hello, &signature),
        Some((first, _)) => state.rules.renew(first, now, hello, &signature),
    }
    .map_err(Error::Access)?;
    *admitted = Some((next, signature));
    Ok(true)
}

/// Sends a challenge with a fresh nonce and mesh time, and gives the nonce.
async fn challenge(shared: &Shared, sender: &mut Sender) -> Result<[u8; 16], Error> {
    let mut nonce = [0; 16];
    let message = {
        let state = shared.state.borrow();
        let now = state
            .time
            .now()
            .mesh
            .ok_or(Error::Access(Refusal::Unsynced))?;
        state.entropy.fill(&mut nonce);
        let mut block = alloc(&shared.state, Challenge::LEN)?;
        Challenge { nonce, now }.encode(&mut block);
        block.freeze()
    };
    sender.send(message).await?;
    Ok(nonce)
}

/// Waits until the hello that the link holds expires, and gives the refusal.
async fn expiry(shared: &Shared) -> Error {
    loop {
        let wait = {
            let state = shared.state.borrow();
            let admitted = shared.admitted.borrow();
            let (admitted, _) = admitted
                .as_ref()
                .expect("invariant: the hello stream admitted a hello first");
            let expires = admitted.hello().expires;
            let now = state
                .time
                .now()
                .mesh
                .expect("invariant: mesh time stays once the clock has synced");
            if now.latest >= expires {
                return Error::Access(Refusal::Expired {
                    expires,
                    now: now.latest,
                });
            }
            state.time.reach(expires)
        };
        wait.await;
    }
}

/// Serves a request stream of `shared`: reads and verifies the request.
pub(crate) async fn request(
    shared: &Rc<Shared>,
    incoming: Incoming,
) -> Result<Served, Error> {
    let (mut receiver, sender) = halves(incoming)?;
    match read(shared, &mut receiver).await {
        Ok(Some((admitted, body, signature, open))) => {
            Ok(Served::Request(Box::new(Request {
                admitted,
                body,
                signature,
                reply: Reply {
                    state: Rc::clone(&shared.state),
                    sender,
                    receiver,
                    open,
                },
            })))
        }
        Ok(None) => Ok(Served::Ended),
        Err(error) => {
            stop(receiver, sender, &error);
            Err(error)
        }
    }
}

/// Reads the request and its body, and verifies it. Gives `None` when the program
/// finished the stream before the request.
async fn read(
    shared: &Rc<Shared>,
    receiver: &mut Receiver,
) -> Result<Option<(Signed, Vec<u8>, [u8; 64], Open)>, Error> {
    if shared.admitted.borrow().is_none() {
        return Err(Error::Unadmitted);
    }
    let Some(message) = receiver.recv().await? else {
        return Ok(None);
    };
    let request = wire::hub::client::Request::decode(&message)?;
    if shared.open.replace(true) {
        return Err(Error::Pending);
    }
    let open = Open(Rc::clone(shared));
    let mut rest = request.body();
    let mut body = Vec::with_capacity(rest.remain());
    while rest.remain() > 0 {
        let Some(message) = receiver.recv().await? else {
            break;
        };
        body.extend_from_slice(rest.take(&message)?);
    }
    rest.end()?;
    let (admitted, hello) = shared
        .admitted
        .borrow()
        .clone()
        .expect("invariant: a link keeps the hello that it admitted");
    let state = shared.state.borrow();
    state
        .rules
        .verify(&admitted, state.time.now().mesh, &body, &request.signature)
        .map_err(Error::Access)?;
    let admitted = Signed {
        hello: admitted.hello().clone(),
        signature: hello,
    };
    Ok(Some((admitted, body, request.signature, open)))
}

/// Sends the response and its body, then finishes the stream.
async fn respond(
    state: &RefCell<State>,
    sender: &mut Sender,
    body: &[u8],
) -> Result<(), Error> {
    let length = u64::try_from(body.len()).unwrap_or(u64::MAX);
    let mut message = alloc(state, Response::LEN)?;
    Response { length }.encode(&mut message);
    sender.send(message.freeze()).await?;
    let most = sender.bytes_max().min(state.borrow().home.pool().largest());
    for chunk in body.chunks(most) {
        let mut message = alloc(state, chunk.len())?;
        message.copy_from_slice(chunk);
        sender.send(message.freeze()).await?;
    }
    sender.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use types::time::Stamp;

    use super::*;

    #[test]
    fn each_refusal_has_its_code() {
        let subject: types::name::Name = "ops.agent".parse().expect("a name");
        let key = types::ed25519::PrivateKey([3; 32]).public();
        let node = types::node::Key::from_u128(1);
        let stamp = Stamp::from_nanos(1);
        let cases = [
            (
                Refusal::Unknown {
                    subject: subject.clone(),
                },
                REFUSED,
            ),
            (Refusal::Unlisted { subject, key }, REFUSED),
            (Refusal::Signature, REFUSED),
            (Refusal::Unsynced, UNSYNCED),
            (
                Refusal::Via {
                    via: node,
                    peer: node,
                },
                VIA,
            ),
            (
                Refusal::Expired {
                    expires: stamp,
                    now: stamp,
                },
                EXPIRED,
            ),
            (
                Refusal::Capped {
                    expires: stamp,
                    cap: stamp,
                },
                CAPPED,
            ),
            (
                Refusal::Changed {
                    field: access::proof::Field::Key,
                },
                CHANGED,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(code(&error), expected, "{error:?}");
        }
    }
}

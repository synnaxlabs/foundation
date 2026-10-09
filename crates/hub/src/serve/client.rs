//! The hello stream and the request streams of a client session.

use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;

use access::proof::{self, Admitted};
use transport::Code;
use transport::stream::{Incoming, Receiver, Sender};
use wire::hub::client::{Challenge, Refusal, Response, Signed};

use super::{BODIES_BYTES_MAX, Error, alloc, halves, stop};
use crate::State;
use crate::link::{Served, Session};

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
            mut sender,
            receiver,
            open,
        } = self;
        let state = Rc::clone(&open.session.state);
        drop(open);
        let sent = respond(&state, &mut sender, body).await;
        if let Err(error) = &sent {
            stop(receiver, sender, error);
        }
        sent
    }
}

/// Which streams a client session may give next: the hello stream first, then a
/// request once a hello is admitted, one at a time.
#[derive(Debug, Default)]
pub(crate) struct Gate {
    hello_taken: Cell<bool>,
    /// The hello that the link admitted last, and its signature.
    admitted: RefCell<Option<(Admitted, [u8; 64])>>,
    /// A request is open: read, and its reply not yet sent or dropped.
    open: Cell<bool>,
}

impl Gate {
    /// Gives `true` for the first stream of the session only: the hello stream.
    pub(crate) fn first(&self) -> bool {
        !self.hello_taken.replace(true)
    }
}

/// Holds the one open request of a link and the bytes its body reserved, until it
/// drops.
#[derive(Debug)]
struct Open {
    session: Rc<Session>,
    bytes: u64,
}

impl Open {
    /// Opens the request of `session` and reserves `bytes` for its body.
    fn new(session: &Rc<Session>, bytes: u64) -> Result<Self, Error> {
        if session.client.open.get() {
            return Err(Error::Pending);
        }
        {
            let state = session.state.borrow();
            let held = state.bodies.get();
            if held + bytes > BODIES_BYTES_MAX {
                return Err(Error::Bodies {
                    length: bytes,
                    held,
                });
            }
            state.bodies.set(held + bytes);
        }
        session.client.open.set(true);
        Ok(Self {
            session: Rc::clone(session),
            bytes,
        })
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.session.client.open.set(false);
        let state = self.session.state.borrow();
        state.bodies.set(state.bodies.get() - self.bytes);
    }
}

/// The refusal that `error` stops a client stream with. The errors that tell about
/// the spec share `Refusal::Refused`.
pub(crate) fn refusal(error: &proof::Error) -> Refusal {
    match error {
        proof::Error::Unknown { .. }
        | proof::Error::Unlisted { .. }
        | proof::Error::Signature => Refusal::Refused,
        proof::Error::Unsynced => Refusal::Unsynced,
        proof::Error::Via { .. } => Refusal::Via,
        proof::Error::Expired { .. } => Refusal::Expired,
        proof::Error::Capped { .. } => Refusal::Capped,
        proof::Error::Changed { .. } => Refusal::Changed,
    }
}

/// Serves the hello stream of `session`, and closes the session when it ends.
pub(crate) async fn hello(
    session: &Rc<Session>,
    incoming: Incoming,
) -> Result<Served, Error> {
    let served = match halves(incoming) {
        Ok((mut receiver, mut sender)) => {
            renew(session, &mut receiver, &mut sender).await
        }
        Err(error) => Err(error),
    };
    let code = served.as_ref().err().and_then(Error::code);
    session.transport.close(code.unwrap_or(Code(0)));
    served
}

/// Admits the first hello, then each renewal, until the program finishes the stream
/// or the hello expires.
async fn renew(
    session: &Session,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<Served, Error> {
    if !take(session, receiver, sender).await? {
        return Ok(Served::Ended);
    }
    loop {
        // A fresh wait for each renewal, since a renewal can move the expiry earlier.
        let mut renewal = pin!(take(session, receiver, sender));
        let mut expiry = pin!(expiry(session));
        let renewed = poll_fn(|cx| match renewal.as_mut().poll(cx) {
            Poll::Ready(renewed) => Poll::Ready(renewed),
            Poll::Pending => expiry.as_mut().poll(cx).map(Err),
        })
        .await?;
        if !renewed {
            return Ok(Served::Ended);
        }
    }
}

/// Sends a challenge, then checks the hello that answers it and keeps it. Gives
/// `false` when the program finished the stream first.
async fn take(
    session: &Session,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<bool, Error> {
    let nonce = challenge(session, sender).await?;
    let Some(message) = receiver.recv().await? else {
        return Ok(false);
    };
    let Signed { hello, signature } = Signed::decode(&message)?;
    if hello.nonce != nonce {
        return Err(Error::Stale);
    }
    let state = session.state.borrow();
    let now = state.time.now().mesh;
    let mut admitted = session.client.admitted.borrow_mut();
    let next = match admitted.as_ref() {
        None => state.rules.admit(now, state.node, hello, &signature),
        Some((first, _)) => state.rules.renew(first, now, hello, &signature),
    }
    .map_err(Error::Access)?;
    *admitted = Some((next, signature));
    Ok(true)
}

/// Sends a challenge with a fresh nonce and mesh time, and gives the nonce.
async fn challenge(session: &Session, sender: &mut Sender) -> Result<[u8; 16], Error> {
    let mut nonce = [0; 16];
    let message = {
        let state = session.state.borrow();
        let now = state
            .time
            .now()
            .mesh
            .ok_or(Error::Access(proof::Error::Unsynced))?;
        state.entropy.fill(&mut nonce);
        let mut block = alloc(&session.state, Challenge::LEN)?;
        Challenge { nonce, now }.encode(&mut block);
        block.freeze()
    };
    sender.send(message).await?;
    Ok(nonce)
}

/// Waits until the hello that the link holds expires, and gives the refusal.
async fn expiry(session: &Session) -> Error {
    loop {
        let wait = {
            let state = session.state.borrow();
            let admitted = session.client.admitted.borrow();
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
                return Error::Access(proof::Error::Expired {
                    expires,
                    now: now.latest,
                });
            }
            state.time.reach(expires)
        };
        wait.await;
    }
}

/// Serves a request stream of `session`: reads and verifies the request.
pub(crate) async fn request(
    session: &Rc<Session>,
    incoming: Incoming,
) -> Result<Served, Error> {
    let (mut receiver, mut sender) = halves(incoming)?;
    match read(session, &mut receiver).await {
        Ok(Some((admitted, body, signature, open))) => {
            Ok(Served::Request(Box::new(Request {
                admitted,
                body,
                signature,
                reply: Reply {
                    sender,
                    receiver,
                    open,
                },
            })))
        }
        Ok(None) => {
            sender.finish()?;
            Ok(Served::Ended)
        }
        Err(error) => {
            stop(receiver, sender, &error);
            Err(error)
        }
    }
}

/// Reads the request and its body, and verifies it. Gives `None` when the program
/// finished the stream before the request.
async fn read(
    session: &Rc<Session>,
    receiver: &mut Receiver,
) -> Result<Option<(Signed, Vec<u8>, [u8; 64], Open)>, Error> {
    if session.client.admitted.borrow().is_none() {
        return Err(Error::Unadmitted);
    }
    let Some(message) = receiver.recv().await? else {
        return Ok(None);
    };
    let request = wire::hub::client::Request::decode(&message)?;
    let open = Open::new(session, request.length)?;
    let mut rest = request.body();
    let mut body = Vec::with_capacity(rest.remain());
    while rest.remain() > 0 {
        let Some(message) = receiver.recv().await? else {
            break;
        };
        body.extend_from_slice(rest.take(&message)?);
    }
    rest.end()?;
    let (admitted, hello) = session
        .client
        .admitted
        .borrow()
        .clone()
        .expect("invariant: a link keeps the hello that it admitted");
    let state = session.state.borrow();
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
    use wire::hub::client::{CAPPED, CHANGED, EXPIRED, REFUSED, UNSYNCED, VIA};

    use super::*;

    #[test]
    fn each_refusal_has_its_code() {
        let subject: types::name::Name = "ops.agent".parse().expect("a name");
        let key = types::ed25519::PrivateKey([3; 32]).public();
        let node = types::node::Key::from_u128(1);
        let stamp = Stamp::from_nanos(1);
        let cases = [
            (
                proof::Error::Unknown {
                    subject: subject.clone(),
                },
                REFUSED,
            ),
            (proof::Error::Unlisted { subject, key }, REFUSED),
            (proof::Error::Signature, REFUSED),
            (proof::Error::Unsynced, UNSYNCED),
            (
                proof::Error::Via {
                    via: node,
                    peer: node,
                },
                VIA,
            ),
            (
                proof::Error::Expired {
                    expires: stamp,
                    now: stamp,
                },
                EXPIRED,
            ),
            (
                proof::Error::Capped {
                    expires: stamp,
                    cap: stamp,
                },
                CAPPED,
            ),
            (
                proof::Error::Changed {
                    field: access::proof::Field::Key,
                },
                CHANGED,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(refusal(&error).code(), expected, "{error:?}");
        }
    }
}

//! Shard 0's sessions: each stream goes to the server of the protocol its header
//! names.

use std::cell::Cell;
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;

use hub::{Hub, Link};
use mesh::Mesh;
use transport::stream::Incoming;
use transport::{Code, Error, Peer, Session, Transport};
use types::time::Span;
use wire::Protocol;

use crate::scope::Scope;

/// How long a stream may take to give its header, a patch as [`crate::WINDOW`] is.
const HEADER: Span = Span::from_nanos(10_000_000_000);

/// How many sessions of peers outside the region the node holds at once, a patch as
/// [`HEADER`] is.
const SESSIONS: usize = 256;

/// Serves each session of `transport` in its own future on `tasks`, until the
/// transport stops, and gives the error that stopped it. Admits each session of a
/// member of `mesh`'s region, and of another peer while fewer than [`SESSIONS`] of
/// them are open; closes each other session with `wire::session::REFUSED`. `route`
/// decides each stream.
pub(crate) async fn accept(
    transport: Rc<Transport>,
    mesh: Option<Mesh>,
    hub: Hub,
    clock: env::clock::Clock,
    tasks: env::tasks::Tasks,
) -> Error {
    let mut sessions = Scope::new(tasks.clone());
    let outside = Rc::new(Cell::new(0));
    loop {
        let session = match transport.accept().await {
            Ok(session) => session,
            Err(error) => return error,
        };
        let member = match (session.peer(), &mesh) {
            (Peer::Node(key), Some(mesh)) => mesh.holder(key).is_some(),
            (Peer::Node(_), None) | (Peer::Client, _) => false,
        };
        let held = if member {
            None
        } else if outside.get() < SESSIONS {
            Some(Outside::new(Rc::clone(&outside)))
        } else {
            session.close(Code(wire::session::REFUSED));
            continue;
        };
        let link = hub.link(session.clone());
        let serve = serve(session, mesh.clone(), link, clock.clone(), tasks.clone());
        sessions.spawn(Box::pin(async move {
            serve.await;
            drop(held);
        }));
    }
}

/// One open session of a peer outside the region, in the count it holds until it
/// drops.
struct Outside(Rc<Cell<usize>>);

impl Outside {
    fn new(count: Rc<Cell<usize>>) -> Self {
        count.set(count.get() + 1);
        Self(count)
    }
}

impl Drop for Outside {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

/// Routes each stream that the peer of `session` opens, each in its own future on
/// `tasks`, so a stream whose header is late delays no other. Ends when the session
/// ends.
async fn serve(
    session: Session,
    mesh: Option<Mesh>,
    link: Link,
    clock: env::clock::Clock,
    tasks: env::tasks::Tasks,
) {
    let mut streams = Scope::new(tasks);
    while let Ok(incoming) = session.accept().await {
        let peer = session.peer();
        let route = route(incoming, peer, mesh.clone(), link.clone(), clock.clone());
        streams.spawn(Box::pin(route));
    }
}

/// Reads the header of `incoming`, its first message, and routes the stream that
/// `peer` opened by its protocol. A `Mesh` stream of a node goes to `mesh`, and a
/// `Hub` stream of a member of the region to `link`; each other stream, and one
/// whose header does not arrive within [`HEADER`] on `clock`, is rejected.
async fn route(
    mut incoming: Incoming,
    peer: Peer,
    mesh: Option<Mesh>,
    link: Link,
    clock: env::clock::Clock,
) {
    let first = {
        let mut recv = pin!(incoming.receiver.recv());
        let mut late = pin!(clock.sleep(HEADER));
        poll_fn(|cx| match recv.as_mut().poll(cx) {
            Poll::Ready(first) => Poll::Ready(Some(first)),
            Poll::Pending => late.as_mut().poll(cx).map(|()| None),
        })
        .await
    };
    let first = match first {
        Some(Ok(first)) => first,
        Some(Err(_)) => return,
        None => return reject(incoming),
    };
    let Some(protocol) = first.as_deref().and_then(header) else {
        return reject(incoming);
    };
    match protocol {
        Protocol::Mesh => match (peer, mesh) {
            (Peer::Node(key), Some(mesh)) => {
                // `serve` stops the stream with the code of its error.
                drop(mesh.serve(key, incoming).await);
            }
            (Peer::Client, _) | (_, None) => reject(incoming),
        },
        Protocol::Hub => match (peer, mesh) {
            (Peer::Node(key), Some(mesh)) if mesh.holder(key).is_some() => {
                // `serve` stops the stream with the code of its error.
                drop(link.serve(incoming).await);
            }
            // A program waits until `node` handles a `Served::Request` (#1744).
            (Peer::Node(_) | Peer::Client, _) => reject(incoming),
        },
        Protocol::Clock | Protocol::Replica | Protocol::Blob => reject(incoming),
    }
}

/// The protocol that `message` names when it is a whole header, with no byte after it.
fn header(message: &[u8]) -> Option<Protocol> {
    match wire::header::decode(message) {
        Ok((protocol, [])) => Some(protocol),
        Ok(_) | Err(_) => None,
    }
}

/// Stops `incoming`, and resets its reply half, with the code of a rejected header.
fn reject(incoming: Incoming) {
    let code = Code(wire::header::REJECTED);
    incoming.receiver.stop(code);
    if let Some(sender) = incoming.sender {
        sender.reset(code);
    }
}

#[cfg(test)]
mod tests {
    use wire::Protocol;

    use super::header;

    /// A first message is a header only when it is the whole message.
    #[test]
    fn a_header_with_a_byte_after_it_names_no_protocol() {
        let mut message = wire::header::encode(Protocol::Mesh).to_vec();
        assert_eq!(header(&message), Some(Protocol::Mesh));
        message.push(0);
        assert_eq!(header(&message), None);
    }
}

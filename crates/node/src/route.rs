//! Shard 0's sessions: each stream goes to the server of the protocol its header
//! names.

use std::rc::Rc;

use hub::{Hub, Link};
use mesh::Mesh;
use transport::stream::Incoming;
use transport::{Code, Error, Peer, Session, Transport};
use wire::Protocol;

use crate::scope::Scope;

/// Serves each session of `transport` in its own future on `tasks`, until the
/// transport stops, and gives the error that stopped it. Admits every peer to a
/// session; `route` decides each stream.
pub(crate) async fn accept(
    transport: Rc<Transport>,
    mesh: Option<Mesh>,
    hub: Hub,
    tasks: env::tasks::Tasks,
) -> Error {
    let mut sessions = Scope::new(tasks.clone());
    loop {
        match transport.accept().await {
            Ok(session) => {
                let link = hub.link(session.clone());
                let serve = serve(session, mesh.clone(), link, tasks.clone());
                sessions.spawn(Box::pin(serve));
            }
            Err(error) => return error,
        }
    }
}

/// Routes each stream that the peer of `session` opens, each in its own future on
/// `tasks`, so a stream whose header is late delays no other. Ends when the session
/// ends.
async fn serve(
    session: Session,
    mesh: Option<Mesh>,
    link: Link,
    tasks: env::tasks::Tasks,
) {
    let mut streams = Scope::new(tasks);
    while let Ok(incoming) = session.accept().await {
        let route = route(incoming, session.peer(), mesh.clone(), link.clone());
        streams.spawn(Box::pin(route));
    }
}

/// Reads the header of `incoming`, its first message, and routes the stream that
/// `peer` opened by its protocol. A `Mesh` stream of a node goes to `mesh`, and a
/// `Hub` stream of a member of the region to `link`; each other stream is rejected.
async fn route(mut incoming: Incoming, peer: Peer, mesh: Option<Mesh>, link: Link) {
    let Ok(first) = incoming.receiver.recv().await else {
        return;
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

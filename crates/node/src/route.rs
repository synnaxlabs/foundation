//! Shard 0's sessions: each stream goes to the server of the protocol its header
//! names.

use transport::stream::Incoming;
use transport::{Code, Session, Transport};
use wire::Protocol;

use crate::scope::Scope;

/// Serves each session of `transport` in its own future on `tasks`, until the
/// transport stops. Admits every peer.
pub(crate) async fn accept(transport: Transport, tasks: env::tasks::Tasks) {
    let mut sessions = Scope::new(tasks.clone());
    while let Ok(session) = transport.accept().await {
        sessions.spawn(Box::pin(serve(session, tasks.clone())));
    }
}

/// Routes each stream that the peer of `session` opens, each in its own future on
/// `tasks`, so a stream whose header is late delays no other. Ends when the session
/// ends.
async fn serve(session: Session, tasks: env::tasks::Tasks) {
    let mut streams = Scope::new(tasks);
    while let Ok(incoming) = session.accept().await {
        streams.spawn(Box::pin(route(incoming)));
    }
}

/// Reads the header of `incoming`, its first message, and routes the stream by its
/// protocol. No protocol has a server yet, so each stream is rejected.
async fn route(mut incoming: Incoming) {
    let Ok(first) = incoming.receiver.recv().await else {
        return;
    };
    let Some(protocol) = first.as_deref().and_then(header) else {
        return reject(incoming);
    };
    match protocol {
        Protocol::Clock
        | Protocol::Mesh
        | Protocol::Replica
        | Protocol::Blob
        | Protocol::Hub => reject(incoming),
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

    /// A first message is a header only when it is the whole message. Each arm rejects
    /// today, so no peer sees the difference yet.
    #[test]
    fn a_header_with_a_byte_after_it_names_no_protocol() {
        let mut message = wire::header::encode(Protocol::Mesh).to_vec();
        assert_eq!(header(&message), Some(Protocol::Mesh));
        message.push(0);
        assert_eq!(header(&message), None);
    }
}

//! The hub's part of one transport session.

use std::cell::RefCell;
use std::rc::Rc;

use transport::Peer;
use transport::stream::Incoming;

use crate::State;
use crate::serve::{self, Request, client};

/// The hub's part of one transport session. For a client session, it holds the
/// admitted hello, so the hello is checked once for the connection, and it closes the
/// session when the hello expires.
#[derive(Debug)]
pub struct Link(Rc<Session>);

/// What [`Link::serve`] ended with.
#[derive(Debug)]
pub enum Served {
    /// The stream ended with no request: a reader session, the hello stream, or a
    /// request stream that the program finished before its request.
    Ended,
    /// A verified request, which waits for its reply.
    Request(Box<Request>),
}

/// The session that each stream of a [`Link`] serves.
#[derive(Debug)]
pub(crate) struct Session {
    pub(crate) state: Rc<RefCell<State>>,
    pub(crate) transport: transport::Session,
    /// Unused for a node peer.
    pub(crate) client: client::Gate,
}

/// What a stream of the link carries, taken when [`Link::serve`] is called.
enum Role {
    Reader,
    Hello,
    Request,
}

impl Link {
    pub(crate) fn new(
        state: Rc<RefCell<State>>,
        transport: transport::Session,
    ) -> Self {
        Self(Rc::new(Session {
            state,
            transport,
            client: client::Gate::default(),
        }))
    }

    /// Serves `incoming`, a hub stream of the link's session whose header the caller
    /// read. Call it in the order that `Session::accept` gives the streams: the first
    /// stream of a client session is its hello stream, and the role is taken at the
    /// call, not at the first poll. `accept` gives streams by class, not in open
    /// order, so this holds because a program opens no request stream before the
    /// challenge after its hello. From a node, it serves a reader session. From a
    /// client, the hello stream lives as long as the session, and closes it when it
    /// ends; a request stream gives [`Served::Request`] once its body is read and
    /// verified.
    ///
    /// # Errors
    ///
    /// The [`serve::Error`] that ended the stream. The stream stops with its code,
    /// except after [`serve::Error::Stream`]. A refusal on the hello stream also
    /// closes the session with that code.
    pub fn serve(
        &self,
        incoming: Incoming,
    ) -> impl Future<Output = Result<Served, serve::Error>> + use<> {
        let session = Rc::clone(&self.0);
        let role = match session.transport.peer() {
            Peer::Node(_) => Role::Reader,
            Peer::Client if session.client.first() => Role::Hello,
            Peer::Client => Role::Request,
        };
        async move {
            match role {
                Role::Reader => serve::run(&session.state, incoming)
                    .await
                    .map(|()| Served::Ended),
                Role::Hello => client::hello(&session, incoming).await,
                Role::Request => client::request(&session, incoming).await,
            }
        }
    }
}

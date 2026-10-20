//! The hub's part of one transport session.

use std::cell::RefCell;
use std::rc::Rc;

use transport::stream::Incoming;

use crate::State;
use crate::serve::{self, Request, client};

/// The hub's part of one transport session. For a client session, it holds the
/// admitted hello, so the hello is checked once for the connection, and it closes the
/// session when the hello ends ([`access::proof::Admitted::ends`]). A clone is the
/// same link.
#[derive(Clone, Debug)]
pub struct Link(Peer);

/// What [`Link::serve`] ended with.
#[derive(Debug)]
pub enum Served {
    /// The stream ended with no request: a reader session, the hello stream, a read
    /// stream, or a stream that the program finished before its first message.
    Ended,
    /// A verified request, which waits for its reply.
    Request(Box<Request>),
}

/// The peer of a link's session, decided once at [`Link::new`].
#[derive(Clone, Debug)]
enum Peer {
    Node(Rc<RefCell<State>>),
    Client(Rc<Session>),
}

/// A client session of a [`Link`].
#[derive(Debug)]
pub(crate) struct Session {
    pub(crate) state: Rc<RefCell<State>>,
    pub(crate) transport: transport::Session,
    pub(crate) client: client::Gate,
}

/// What a stream of the link carries, taken when [`Link::serve`] is called.
enum Role {
    Reader(Rc<RefCell<State>>),
    Hello(Rc<Session>),
    /// A client stream after the hello stream: a request or a read.
    Later(Rc<Session>),
}

impl Link {
    pub(crate) fn new(
        state: Rc<RefCell<State>>,
        transport: transport::Session,
    ) -> Self {
        Self(match transport.peer() {
            transport::Peer::Node(_) => Peer::Node(state),
            transport::Peer::Client => {
                let client = client::Gate::new(state.borrow().public_key);
                Peer::Client(Rc::new(Session {
                    state,
                    transport,
                    client,
                }))
            }
        })
    }

    /// Serves `incoming`, a hub stream of the link's session whose header the caller
    /// read, in the role that it takes at the call, not at the first poll. From a node,
    /// it waits until the mesh names a home for the index of the open, then serves a
    /// reader session. From a client, the first stream given to `serve` is its hello
    /// stream, which lives as long as the session and closes it when it ends. Each
    /// later stream is a request stream, which gives [`Served::Request`] once its body
    /// is read and verified, or a read stream, which sends the frames of its reader
    /// until the program finishes it. A complete read goes on past a frame that the
    /// buffer lost. A program sends the header
    /// of a later stream only after the challenge after its hello, so the hello stream
    /// comes first in any order of the headers.
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
        let role = match &self.0 {
            Peer::Node(state) => Role::Reader(Rc::clone(state)),
            Peer::Client(session) if session.client.first() => {
                Role::Hello(Rc::clone(session))
            }
            Peer::Client(session) => Role::Later(Rc::clone(session)),
        };
        async move {
            match role {
                Role::Reader(state) => {
                    serve::run(&state, incoming).await.map(|()| Served::Ended)
                }
                Role::Hello(session) => client::hello(&session, incoming).await,
                Role::Later(session) => client::later(&session, incoming).await,
            }
        }
    }
}

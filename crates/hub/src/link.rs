//! The hub's part of one transport session.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use access::proof::Admitted;
use transport::stream::Incoming;
use transport::{Peer, Session};

use crate::State;
use crate::serve::{self, Request, client};

/// The hub's part of one transport session. For a client session, it holds the
/// admitted hello, so the hello is checked once for the connection, and it closes the
/// session when the hello expires. Clones share it.
#[derive(Clone, Debug)]
pub struct Link(Rc<Shared>);

/// What [`Link::serve`] ended with.
#[derive(Debug)]
pub enum Served {
    /// The stream ended with no request: a reader session, the hello stream, or a
    /// request stream that the program finished before its request.
    Ended,
    /// A verified request, which waits for its reply.
    Request(Box<Request>),
}

#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) state: Rc<RefCell<State>>,
    pub(crate) session: Session,
    /// A client session gave its first stream, the hello stream.
    hello_taken: Cell<bool>,
    /// The hello that the link admitted last, and its signature.
    pub(crate) admitted: RefCell<Option<(Admitted, [u8; 64])>>,
    /// A request is open: read, and its reply not yet sent or dropped.
    pub(crate) open: Cell<bool>,
}

/// What a stream of the link carries, taken when [`Link::serve`] is called.
enum Role {
    Reader,
    Hello,
    Request,
}

impl Link {
    pub(crate) fn new(state: Rc<RefCell<State>>, session: Session) -> Self {
        Self(Rc::new(Shared {
            state,
            session,
            hello_taken: Cell::new(false),
            admitted: RefCell::new(None),
            open: Cell::new(false),
        }))
    }

    /// Serves `incoming`, a hub stream of the link's session whose header the caller
    /// read. Call it in the order that `Session::accept` gives the streams: the first
    /// stream of a client session is its hello stream, and the role is taken at the
    /// call, not at the first poll. From a node, it serves a reader session. From a
    /// client, the hello stream lives as long as the session, and closes it when it
    /// ends; a request stream gives [`Served::Request`] once its body is read and
    /// verified.
    ///
    /// # Errors
    ///
    /// The [`serve::Error`] that ended the stream. The stream stops with its code,
    /// except after [`serve::Error::Stream`]. A refusal on the hello stream also
    /// closes the session with that code.
    ///
    /// # Panics
    ///
    /// When a reader session sends a frame: `transport` cannot yet send a message in
    /// parts (#68).
    pub fn serve(
        &self,
        incoming: Incoming,
    ) -> impl Future<Output = Result<Served, serve::Error>> + use<> {
        let shared = Rc::clone(&self.0);
        let role = match shared.session.peer() {
            Peer::Node(_) => Role::Reader,
            Peer::Client if shared.hello_taken.replace(true) => Role::Request,
            Peer::Client => Role::Hello,
        };
        async move {
            match role {
                Role::Reader => serve::run(&shared.state, incoming)
                    .await
                    .map(|()| Served::Ended),
                Role::Hello => client::hello(&shared, incoming).await,
                Role::Request => client::request(&shared, incoming).await,
            }
        }
    }
}

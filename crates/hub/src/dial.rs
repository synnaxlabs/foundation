//! Sessions to the hubs of other nodes of the region, with a retry of a session that
//! closes with `Code(0)`.

use std::cell::RefCell;
use std::rc::Rc;

use transport::Code;
use types::node;

use crate::State;

/// The most tries of one [`retry`]. The home's node dials only when it has no open
/// session, and runs one dial at a time, so its session beats at most the session that
/// was open and one that this node dialed before that session arrived.
const TRIES: u32 = 3;

/// The session to `home`, another node of the region, that the shard's transport
/// holds or dials.
///
/// # Panics
///
/// When the node is in no region, or the mesh does not name `home` a member.
pub(crate) async fn dial(
    state: &Rc<RefCell<State>>,
    home: node::Key,
) -> Result<transport::Session, transport::Error> {
    let (transport, card) = {
        let state = state.borrow();
        let region = &state
            .region
            .as_ref()
            .expect("invariant: only a node in a region has a remote home")
            .region;
        let member = region
            .mesh
            .member(home)
            .expect("invariant: the mesh names only a member as a home");
        (Rc::clone(&region.transport), member.card.card().clone())
    };
    transport
        .dial(card.public_key, card.addresses.as_slice())
        .await
}

/// Runs `attempt`, which opens a stream on the session that [`dial`] gives, until it
/// gives a result other than a close of the session with `Code(0)`, at most
/// [`TRIES`] times. `closed` gives the transport error that an error of `attempt`
/// carries. A session that loses the tie-break of ONE SESSION PER PEER closes so.
pub(crate) async fn retry<T, E>(
    mut attempt: impl AsyncFnMut() -> Result<T, E>,
    closed: fn(&E) -> Option<&transport::Error>,
) -> Result<T, E> {
    let mut tries = 1;
    loop {
        match attempt().await {
            Err(error)
                if tries < TRIES
                    && matches!(
                        closed(&error),
                        Some(
                            transport::Error::Closed { code: Code(0) }
                                | transport::Error::PeerClosed { code: Code(0) }
                        )
                    ) =>
            {
                tries += 1;
            }
            attempted => return attempted,
        }
    }
}

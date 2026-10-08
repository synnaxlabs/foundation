//! The homes of a hub in a region: which node is the home of each index, and the
//! session to each home of another node that a reader reaches.

use std::cell::{Cell, RefCell};

use transport::stream::{Receiver, Sender};
use transport::{Class, Session};
use types::channel;
use types::hash;
use types::node;

use crate::Region;

/// A [`Region`] and the session to each home of another node that a reader dialed,
/// which each later reader there shares.
#[derive(Debug)]
pub(crate) struct Homes {
    region: Region,
    /// Each session with the number of its dial, so a failed open forgets only that
    /// session, not one that a later dial made.
    sessions: RefCell<hash::Map<node::Key, (u64, Session)>>,
    dials: Cell<u64>,
}

impl Homes {
    pub(crate) fn new(region: Region) -> Self {
        Self {
            region,
            sessions: RefCell::default(),
            dials: Cell::new(0),
        }
    }

    /// Each home that the mesh names for `index`, from the one it names now.
    pub(crate) fn watch(&self, index: channel::Key) -> ::mesh::Watch {
        self.region.mesh.watch(index)
    }

    /// Opens a stream of `class` to `home`, another node. It opens on the held session
    /// to `home`, and dials a new one when none is held or the open on it fails.
    ///
    /// # Errors
    ///
    /// The error of the dial, or of the open on the new session.
    pub(crate) async fn open(
        &self,
        home: node::Key,
        class: Class,
    ) -> Result<(Sender, Receiver), transport::Error> {
        let held = self.sessions.borrow().get(&home).cloned();
        if let Some((dial, session)) = held {
            if let Ok(stream) = session.open(class).await {
                return Ok(stream);
            }
            self.forget(home, dial);
        }
        let (dial, session) = self.dial(home).await?;
        let opened = session.open(class).await;
        if opened.is_err() {
            self.forget(home, dial);
        }
        opened
    }

    /// Dials `home`, and keeps the session unless a concurrent dial kept one first.
    async fn dial(&self, home: node::Key) -> Result<(u64, Session), transport::Error> {
        let member = self
            .region
            .mesh
            .member(home)
            .expect("invariant: the mesh names only a member as a home");
        let card = member.card.card();
        let session = self
            .region
            .transport
            .dial(card.public_key, card.addresses.as_slice())
            .await?;
        let dial = self.dials.get();
        self.dials.set(dial + 1);
        Ok(self
            .sessions
            .borrow_mut()
            .entry(home)
            .or_insert((dial, session))
            .clone())
    }

    fn forget(&self, home: node::Key, dial: u64) {
        let mut sessions = self.sessions.borrow_mut();
        if sessions.get(&home).is_some_and(|&(held, _)| held == dial) {
            sessions.remove(&home);
        }
    }
}

use std::sync::Arc;

use types::frame::key_set::KeySet;

use super::Mode;
use crate::hub::client::stream::{self, Opening};
use crate::hub::{self, client::Error, client::Place};

/// A decoder of the start of a read stream at the program: the places and their
/// run. With the last place it gives the [`Start`] of the read.
#[derive(Debug)]
pub struct Reader {
    latest: bool,
    opening: Opening,
}

/// The start of a read: its places, their key set, and the decoder of the replies.
/// Only [`Reader::decode`] makes one that holds together, and no call takes one.
#[derive(Debug)]
pub struct Start {
    /// The places, in place order.
    pub places: Vec<Place>,
    /// The key set of each frame of the read, from [`KeySet::ordered`]: entry `n` is
    /// place `n`.
    pub set: Arc<KeySet>,
    /// The decoder of the replies of the reader session, for the count of places and
    /// the mode of the read.
    pub home: hub::Reader,
}

impl Reader {
    /// A decoder for a read that `mode` opened.
    #[must_use]
    pub fn new(mode: Mode) -> Self {
        Self {
            latest: mode == Mode::Latest,
            opening: Opening::read(),
        }
    }

    /// Decodes the next message from the node, up to the last place, or `None` when
    /// the node finished the stream. Gives the start of the read with the last place,
    /// else `None`.
    ///
    /// # Errors
    ///
    /// Those of [`write::Opening::decode`](crate::hub::client::write::Opening::decode),
    /// for the places of the read, where a place also breaks a rule when its index is
    /// not that of place 0: [`Error::Indexes`].
    ///
    /// # Panics
    ///
    /// When it gave the start before.
    pub fn decode(&mut self, message: Option<&[u8]>) -> Result<Option<Start>, Error> {
        let Some(stream::Start { places, set }) = self.opening.decode(message)? else {
            return Ok(None);
        };
        Ok(Some(Start {
            home: hub::Reader::of(count(&places), self.latest),
            places,
            set,
        }))
    }
}

fn count(places: &[Place]) -> u32 {
    u32::try_from(places.len()).expect("invariant: the places are at most PLACES_MAX")
}

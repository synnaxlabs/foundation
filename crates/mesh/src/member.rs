//! The region's record of one node.

use std::collections::BTreeMap;

use types::channel;
use types::name::Name;
use types::time::Span;

use crate::card;

/// The region's record of one node, keyed by its `node::Key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// What the node states about itself.
    pub card: card::Signed,
    /// The join ticket's signature over the node's first card.
    pub admission: [u8; 64],
    /// For an ephemeral node, the time offline after which the region removes it.
    pub expiry: Option<Span>,
    /// The node's status channel keys, by name under the node's name: `clock.offset`
    /// is `<card.name>.clock.offset` (X27). A status name keeps its meaning and data
    /// type in every release; a change takes a new name.
    pub status: BTreeMap<Name, channel::Key>,
}

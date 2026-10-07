//! The region's record of one node.

use types::channel;
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
    /// The node's status channel keys, in `node::status::TABLE` order.
    pub status: Vec<channel::Key>,
}

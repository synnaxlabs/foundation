//! The connection IDs this node makes: [`LEN`] bytes, and each one this node issues
//! starts with the shard that owns the connection.

use std::time::Duration;

use env::entropy::Entropy;
use noq_proto::{ConnectionId, ConnectionIdGenerator};

/// The bytes in every connection ID this node makes.
pub(super) const LEN: usize = 8;

/// A connection ID of random bytes from `entropy`.
pub(super) fn random(entropy: &Entropy) -> ConnectionId {
    let mut id = [0; LEN];
    entropy.fill(&mut id);
    ConnectionId::new(&id)
}

/// Issues one shard's connection IDs: the shard, then random bytes.
#[derive(Clone)]
pub(super) struct Issuer {
    pub(super) shard: u8,
    pub(super) entropy: Entropy,
}

impl ConnectionIdGenerator for Issuer {
    fn generate_cid(&mut self) -> ConnectionId {
        let mut id = [self.shard; LEN];
        self.entropy.fill(&mut id[1..]);
        ConnectionId::new(&id)
    }

    fn cid_len(&self) -> usize {
        LEN
    }

    fn cid_lifetime(&self) -> Option<Duration> {
        None
    }
}

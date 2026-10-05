//! Node identity.

/// A node's stable identity, a UUIDv7. It stays the same when the node rotates its
/// public key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(u128);

impl Key {
    /// Wraps a key's 128 bits.
    #[must_use]
    pub const fn from_u128(bits: u128) -> Self {
        Self(bits)
    }

    /// The key's 128 bits.
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        self.0
    }
}

/// A node's Ed25519 public key. The transport authenticates peers with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PublicKey(pub [u8; 32]);

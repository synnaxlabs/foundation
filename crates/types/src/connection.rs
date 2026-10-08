//! Program connections.

/// The key of one program connection at every owner: 16 random bytes that the
/// program picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key(pub [u8; 16]);

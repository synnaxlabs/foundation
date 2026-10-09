//! The codes that close a whole session. They share one space with the codes that
//! stop or reset a stream.

/// The code that closes a session that the node does not admit: over its bound on
/// sessions, or of a node that the region removed.
pub const REFUSED: u32 = 3;

//! The codes that close a whole session. They share one space with the codes that stop
//! or reset a stream.

/// The code that closes a session that the node does not admit.
pub const REFUSED: u32 = 3;

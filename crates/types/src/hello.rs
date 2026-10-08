//! The hello of a subject.

use crate::connection;
use crate::ed25519::PublicKey;
use crate::name::Name;
use crate::node;
use crate::time::Stamp;

/// A subject's claim, sent first on each program connection and signed with one of
/// its keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// The subject that the program acts as.
    pub subject: Name,
    /// The key that signs the hello and each request of the connection.
    pub key: PublicKey,
    /// The node that the program connects to.
    pub via: node::Key,
    /// The connection that the hello and its requests name.
    pub connection: connection::Key,
    /// Bytes from the node that `via` names. Only that node checks them.
    pub nonce: [u8; 16],
    /// The mesh time at which the hello stops being valid.
    pub expires: Stamp,
}

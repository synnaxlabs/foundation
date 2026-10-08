use std::net::SocketAddr;

use types::ed25519::PublicKey;

/// Where a node accepts sessions. Addresses come from the mesh and from join tickets,
/// never from DNS. The transport chooses the carrier for each.
///
/// ```
/// use transport::Address;
///
/// fn direct(at: std::net::SocketAddr) -> [Address; 2] {
///     [Address::Udp(at), Address::Tcp(at)]
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Address {
    /// A UDP socket.
    Udp(SocketAddr),
    /// A TCP socket.
    Tcp(SocketAddr),
    /// A relay node that forwards sessions to the node.
    Relay {
        /// The relay node's key.
        node: PublicKey,
        /// The relay node's TCP socket.
        at: SocketAddr,
    },
}

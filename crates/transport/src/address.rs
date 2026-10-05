use std::net::SocketAddr;

use types::node::PublicKey;

/// Where a node accepts sessions. Addresses come from the mesh and from join tickets,
/// never from DNS.
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
    /// A UDP socket, for QUIC.
    Udp(SocketAddr),
    /// A TCP socket, for TLS over TCP.
    Tcp(SocketAddr),
    /// A relay node that forwards sessions to the node over TLS on TCP.
    Relay {
        /// The relay node's key.
        node: PublicKey,
        /// The relay node's TCP socket.
        at: SocketAddr,
    },
}

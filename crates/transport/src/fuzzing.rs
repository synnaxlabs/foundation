//! The hello decoder and encoder, and the server's reading of a dialer's
//! certificates, for the fuzz crate only. Not a stable surface.

use rustls::pki_types::CertificateDer;
use types::ed25519::PrivateKey;

pub use crate::quic::{Hello, connection::Fault};
use crate::{Peer, tls};

/// The peer that a node's server makes of a dialer's ALPN `protocol` and
/// certificate `chain`, or `None` when the server refuses them. An empty chain is a
/// dialer that sent no certificate.
#[must_use]
pub fn peer(protocol: Option<&[u8]>, chain: &[&[u8]]) -> Option<Peer> {
    let chain: Vec<_> = chain.iter().map(|&der| CertificateDer::from(der)).collect();
    tls::accept(protocol, &chain).ok()
}

/// The certificate that the node with `private_key` presents.
#[must_use]
pub fn certificate(private_key: &PrivateKey) -> Vec<u8> {
    tls::certificate(private_key)
}

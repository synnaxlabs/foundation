//! The hello decoder and encoder, and the server's reading of a dialer's
//! certificates, for the fuzz crate only. Not a stable surface.

use rustls::pki_types::CertificateDer;

pub use crate::quic::{Hello, connection::Fault};
pub use crate::tls::certificate;
use crate::{Peer, tls};

/// The peer that a node's server makes of a dialer's certificate `chain`, once they
/// agree on the protocol, or `None` when the server refuses the chain. An empty chain
/// is a dialer that sent no certificate.
#[must_use]
pub fn peer(chain: &[&[u8]]) -> Option<Peer> {
    let chain: Vec<_> = chain.iter().map(|&der| CertificateDer::from(der)).collect();
    tls::accept(&chain).ok()
}

#[cfg(test)]
mod tests {
    use types::ed25519::PrivateKey;

    use super::*;

    #[test]
    fn a_node_certificate_reads_back_to_its_node() {
        let private_key = PrivateKey([1; 32]);
        let der = certificate(&private_key);
        assert_eq!(peer(&[&der]), Some(Peer::Node(private_key.public())));
    }

    #[test]
    fn an_empty_chain_is_a_client() {
        assert_eq!(peer(&[]), Some(Peer::Client));
    }

    #[test]
    fn a_refused_chain_gives_no_peer() {
        assert_eq!(peer(&[&[1, 2, 3]]), None);
    }
}

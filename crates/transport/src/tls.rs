//! TLS 1.3 that trusts a node's key, not a certificate authority.

use std::sync::Arc;
use std::time::Duration;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use rustls::crypto::aws_lc_rs::sign::any_eddsa_type;
use rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::{CertifiedKey, SingleCertAndKey};
use rustls::time_provider::TimeProvider;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName,
    ServerConfig, SignatureScheme,
};
use types::node::{PrivateKey, PublicKey};

use crate::session::Peer;

/// The protocol both sides offer in ALPN. A new session protocol gets a new name.
pub(crate) const ALPN: &[u8] = b"foundation/1";

/// Ed25519 (1.3.101.112) as an `AlgorithmIdentifier`.
const ED25519: &[u8] = b"\x30\x05\x06\x03\x2b\x65\x70";
/// `CN=foundation`, both issuer and subject.
const NAME: &[u8] = b"\x30\x15\x31\x13\x30\x11\x06\x03\x55\x04\x03\x0c\x0afoundation";
/// From 1970 to `99991231235959Z`, which RFC 5280 reads as no expiry.
const VALIDITY: &[u8] = b"\x30\x20\x17\x0d700101000000Z\x18\x0f99991231235959Z";
/// A `SubjectPublicKeyInfo` up to its 32-byte Ed25519 key.
const SPKI: &[u8] = b"\x30\x2a\x30\x05\x06\x03\x2b\x65\x70\x03\x21\x00";
/// The `TBSCertificate` header, version 3, and serial 1.
const TBS: &[u8] = b"\x30\x81\x8b\xa0\x03\x02\x01\x02\x02\x01\x01";
/// The `Certificate` header.
const CERTIFICATE: &[u8] = b"\x30\x81\xd8";
/// The `BIT STRING` header of a 64-byte Ed25519 signature.
const SIGNATURE: &[u8] = b"\x03\x41\x00";
/// PKCS#8 v1 up to its 32-byte Ed25519 private key.
const PKCS8: &[u8] =
    b"\x30\x2e\x02\x01\x00\x30\x05\x06\x03\x2b\x65\x70\x04\x22\x04\x20";

const TBS_BYTES: usize =
    TBS.len() + ED25519.len() + 2 * NAME.len() + VALIDITY.len() + SPKI.len() + 32;
const CERTIFICATE_BYTES: usize =
    CERTIFICATE.len() + TBS_BYTES + ED25519.len() + SIGNATURE.len() + 64;
const _: () = assert!(TBS_BYTES == 3 + 0x8b, "the TBS header holds its length");
const _: () = assert!(
    CERTIFICATE_BYTES == 3 + 0xd8,
    "the certificate header holds its length"
);

/// The node's TLS: its certificate and key, and the configs that use them. Made once
/// per transport.
pub(crate) struct Tls {
    provider: Arc<CryptoProvider>,
    resolver: Arc<SingleCertAndKey>,
    time: Arc<dyn TimeProvider>,
    server: Arc<ServerConfig>,
}

impl Tls {
    /// Makes a self-signed certificate for the node key. The same key always gives
    /// the same bytes.
    pub(crate) fn new(private_key: &PrivateKey) -> Self {
        let pair = Ed25519KeyPair::from_seed_unchecked(&private_key.0)
            .expect("invariant: any 32 bytes are an Ed25519 private key");
        let key = pair.public_key().as_ref();
        let mut tbs = Vec::with_capacity(TBS_BYTES);
        for part in [TBS, ED25519, NAME, VALIDITY, NAME, SPKI, key] {
            tbs.extend_from_slice(part);
        }
        let signature = pair.sign(&tbs);
        let mut certificate = Vec::with_capacity(CERTIFICATE_BYTES);
        for part in [CERTIFICATE, &tbs, ED25519, SIGNATURE, signature.as_ref()] {
            certificate.extend_from_slice(part);
        }
        let pkcs8 = PrivatePkcs8KeyDer::from([PKCS8, &private_key.0].concat());
        let key = any_eddsa_type(&pkcs8)
            .expect("invariant: the PKCS#8 template holds an Ed25519 key");
        Self::with(CertifiedKey::new(vec![certificate.into()], key))
    }

    fn with(certified: CertifiedKey) -> Self {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let resolver = Arc::new(SingleCertAndKey::from(certified));
        let time: Arc<dyn TimeProvider> = Arc::new(Epoch);
        let algorithms = provider.signature_verification_algorithms;
        #[expect(clippy::disallowed_methods, reason = "it passes the fixed time")]
        let mut server = ServerConfig::builder_with_details(
            Arc::clone(&provider),
            Arc::clone(&time),
        )
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("invariant: aws-lc-rs has TLS 1.3 suites")
        .with_client_cert_verifier(Arc::new(AnyKey { algorithms }))
        .with_cert_resolver(Arc::<SingleCertAndKey>::clone(&resolver));
        server.alpn_protocols = vec![ALPN.to_vec()];
        server.send_tls13_tickets = 0;
        Self {
            provider,
            resolver,
            time,
            server: Arc::new(server),
        }
    }

    /// Dials a node: accepts the server only when it proves `expected`.
    pub(crate) fn client(&self, expected: PublicKey) -> Arc<ClientConfig> {
        let algorithms = self.provider.signature_verification_algorithms;
        #[expect(clippy::disallowed_methods, reason = "it passes the fixed time")]
        let mut config = ClientConfig::builder_with_details(
            Arc::clone(&self.provider),
            Arc::clone(&self.time),
        )
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("invariant: aws-lc-rs has TLS 1.3 suites")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            expected,
            algorithms,
        }))
        .with_client_cert_resolver(Arc::<SingleCertAndKey>::clone(&self.resolver));
        config.alpn_protocols = vec![ALPN.to_vec()];
        config.resumption = rustls::client::Resumption::disabled();
        Arc::new(config)
    }

    /// Accepts nodes and clients. A client may send no certificate.
    pub(crate) fn server(&self) -> Arc<ServerConfig> {
        Arc::clone(&self.server)
    }
}

/// The peer of a handshake whose certificates a verifier here accepted.
///
/// # Panics
///
/// When the certificate carries no Ed25519 key, which the verifiers refuse.
pub(crate) fn peer(certificates: Option<&[CertificateDer<'_>]>) -> Peer {
    match certificates.and_then(<[_]>::first) {
        None => Peer::Client,
        Some(certificate) => Peer::Node(
            key(certificate).expect("invariant: a verifier accepted this certificate"),
        ),
    }
}

/// The node key that `certificate` carries.
fn key(certificate: &CertificateDer<'_>) -> Result<PublicKey, rustls::Error> {
    let parsed = ParsedCertificate::try_from(certificate)?;
    parsed
        .subject_public_key_info()
        .strip_prefix(SPKI)
        .and_then(|key| <[u8; 32]>::try_from(key).ok())
        .map(PublicKey)
        .ok_or_else(|| CertificateError::ApplicationVerificationFailure.into())
}

/// Accepts a server only when it proves the key the caller dialed.
#[derive(Debug)]
struct Pinned {
    expected: PublicKey,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if key(end_entity)? == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(CertificateError::ApplicationVerificationFailure.into())
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        unreachable!("invariant: TLS 1.2 is not compiled in")
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// Accepts any Ed25519 node key, or no certificate. Admission is the caller's job.
#[derive(Debug)]
struct AnyKey {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AnyKey {
    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        key(end_entity).map(|_| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        unreachable!("invariant: TLS 1.2 is not compiled in")
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// rustls reads wall time on each handshake, and its default reads the OS clock.
/// Nothing here uses the time: the verifiers ignore dates and resumption is off.
#[derive(Debug)]
struct Epoch;

impl TimeProvider for Epoch {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(Duration::ZERO))
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv6Addr};

    use proptest::prelude::*;
    use rustls::client::ResolvesClientCert;
    use rustls::{CertificateError, Connection, HandshakeKind};

    use super::*;

    /// The public key, derived apart from the certificate template.
    fn public(private_key: &PrivateKey) -> PublicKey {
        let pair =
            Ed25519KeyPair::from_seed_unchecked(&private_key.0).expect("32 bytes");
        PublicKey(pair.public_key().as_ref().try_into().expect("32 bytes"))
    }

    /// Moves every pending TLS record from `from` to `to`.
    fn pass(from: &mut Connection, to: &mut Connection) {
        let mut wire = Vec::new();
        while from.wants_write() {
            from.write_tls(&mut wire).expect("writes to a Vec");
        }
        let mut rest = wire.as_slice();
        while !rest.is_empty() {
            to.read_tls(&mut rest).expect("reads from a slice");
        }
    }

    /// Joins `client` and `server` in memory with a full handshake. Returns the peer
    /// each side sees, client first, or the first error either side raised.
    fn handshake(
        client: Arc<ClientConfig>,
        server: Arc<ServerConfig>,
    ) -> Result<(Peer, Peer), rustls::Error> {
        let name = ServerName::from(IpAddr::from(Ipv6Addr::LOCALHOST));
        let mut client = Connection::from(rustls::ClientConnection::new(client, name)?);
        let mut server = Connection::from(rustls::ServerConnection::new(server)?);
        for _ in 0..8 {
            if !client.is_handshaking() && !server.is_handshaking() {
                assert_eq!(client.alpn_protocol(), Some(ALPN));
                assert_eq!(server.alpn_protocol(), Some(ALPN));
                assert_eq!(client.handshake_kind(), Some(HandshakeKind::Full));
                return Ok((
                    peer(client.peer_certificates()),
                    peer(server.peer_certificates()),
                ));
            }
            pass(&mut client, &mut server);
            server.process_new_packets()?;
            pass(&mut server, &mut client);
            client.process_new_packets()?;
        }
        panic!("the handshake did not finish in 8 rounds");
    }

    /// A client like an SDK: it pins the server's key and has no certificate.
    fn anonymous(expected: PublicKey) -> Arc<ClientConfig> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let algorithms = provider.signature_verification_algorithms;
        #[expect(clippy::disallowed_methods, reason = "it passes the fixed time")]
        let mut config = ClientConfig::builder_with_details(provider, Arc::new(Epoch))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3 is available")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned {
                expected,
                algorithms,
            }))
            .with_no_client_auth();
        config.alpn_protocols = vec![ALPN.to_vec()];
        Arc::new(config)
    }

    /// The node's certificate and key, as its resolver gives them.
    fn certified(tls: &Tls) -> Arc<CertifiedKey> {
        let schemes = [SignatureScheme::ED25519];
        ResolvesClientCert::resolve(&*tls.resolver, &[], &schemes).expect("a key")
    }

    fn certificate(tls: &Tls) -> CertificateDer<'static> {
        certified(tls).cert[0].clone()
    }

    /// TLS that presents `certificate`'s certificate but signs with `signer`'s key.
    fn borrowing(certificate: &Tls, signer: &Tls) -> Tls {
        Tls::with(CertifiedKey::new(
            certified(certificate).cert.clone(),
            Arc::clone(&certified(signer).key),
        ))
    }

    mod handshake {
        use super::*;

        #[test]
        fn when_key_matches_both_sides_see_the_other_node() {
            let (a, b) = (PrivateKey([1; 32]), PrivateKey([2; 32]));
            let peers =
                handshake(Tls::new(&a).client(public(&b)), Tls::new(&b).server());
            assert_eq!(peers, Ok((Peer::Node(public(&b)), Peer::Node(public(&a)))));
        }

        #[test]
        fn when_key_differs_the_client_refuses() {
            let (a, b, c) = (
                PrivateKey([1; 32]),
                PrivateKey([2; 32]),
                PrivateKey([3; 32]),
            );
            let peers =
                handshake(Tls::new(&a).client(public(&c)), Tls::new(&b).server());
            assert_eq!(
                peers,
                Err(CertificateError::ApplicationVerificationFailure.into())
            );
        }

        #[test]
        fn when_client_has_no_certificate_the_server_sees_a_client() {
            let b = PrivateKey([2; 32]);
            let peers = handshake(anonymous(public(&b)), Tls::new(&b).server());
            assert_eq!(peers, Ok((Peer::Node(public(&b)), Peer::Client)));
        }

        #[test]
        fn when_server_sends_tickets_the_node_does_not_resume() {
            let (a, b) = (PrivateKey([1; 32]), PrivateKey([2; 32]));
            let client = Tls::new(&a).client(public(&b));
            let mut server = (*Tls::new(&b).server()).clone();
            server.send_tls13_tickets = 2;
            let server = Arc::new(server);
            for _ in 0..2 {
                let peers = handshake(Arc::clone(&client), Arc::clone(&server));
                assert_eq!(peers, Ok((Peer::Node(public(&b)), Peer::Node(public(&a)))));
            }
        }

        #[test]
        fn when_client_is_an_sdk_it_does_not_resume() {
            let b = PrivateKey([2; 32]);
            let client = anonymous(public(&b));
            let server = Tls::new(&b).server();
            for _ in 0..2 {
                let peers = handshake(Arc::clone(&client), Arc::clone(&server));
                assert_eq!(peers, Ok((Peer::Node(public(&b)), Peer::Client)));
            }
        }

        #[test]
        fn when_client_certificate_is_borrowed_the_server_refuses() {
            let (thief, b, victim) = (
                PrivateKey([1; 32]),
                PrivateKey([2; 32]),
                PrivateKey([3; 32]),
            );
            let peers = handshake(
                borrowing(&Tls::new(&victim), &Tls::new(&thief)).client(public(&b)),
                Tls::new(&b).server(),
            );
            assert_eq!(peers, Err(CertificateError::BadSignature.into()));
        }

        #[test]
        fn when_server_certificate_is_borrowed_the_client_refuses() {
            let (a, thief, victim) = (
                PrivateKey([1; 32]),
                PrivateKey([2; 32]),
                PrivateKey([3; 32]),
            );
            let peers = handshake(
                Tls::new(&a).client(public(&victim)),
                borrowing(&Tls::new(&victim), &Tls::new(&thief)).server(),
            );
            assert_eq!(peers, Err(CertificateError::BadSignature.into()));
        }
    }

    mod key {
        use super::*;

        #[test]
        fn refuses_a_key_that_is_not_ed25519() {
            let mut der = certificate(&Tls::new(&PrivateKey([1; 32]))).to_vec();
            let at = der
                .windows(SPKI.len())
                .position(|window| window == SPKI)
                .expect("the certificate has an Ed25519 key");
            // 1.3.101.112 (Ed25519) becomes 1.3.101.110 (X25519).
            der[at + 8] = 0x6e;
            assert_eq!(
                key(&CertificateDer::from(der)),
                Err(CertificateError::ApplicationVerificationFailure.into())
            );
        }

        #[test]
        fn refuses_bytes_that_are_not_der() {
            assert_eq!(
                key(&CertificateDer::from(vec![1, 2, 3])),
                Err(CertificateError::BadEncoding.into())
            );
        }
    }

    mod certificate {
        use super::*;

        proptest! {
            #[test]
            fn carries_the_public_key(bytes: [u8; 32]) {
                let private_key = PrivateKey(bytes);
                let certificate = certificate(&Tls::new(&private_key));
                prop_assert_eq!(key(&certificate), Ok(public(&private_key)));
            }
        }

        #[test]
        fn is_the_same_for_the_same_key() {
            let first = certificate(&Tls::new(&PrivateKey([1; 32])));
            let second = certificate(&Tls::new(&PrivateKey([1; 32])));
            assert_eq!(first, second);
        }
    }
}

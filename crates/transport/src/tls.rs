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
const ALPN: &[u8] = b"foundation/1";

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
/// The largest certificate a peer may present. It is part of `foundation/1`, so a
/// larger one needs a new ALPN.
const CERTIFICATE_BYTES_MAX: usize = 1024;
const _: () = assert!(
    CERTIFICATE_BYTES <= CERTIFICATE_BYTES_MAX,
    "a node takes a peer's template"
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
        let certificate = issue(&public(private_key).to_bytes(), |tbs| {
            pair.sign(tbs).as_ref().to_vec()
        });
        let pkcs8 = PrivatePkcs8KeyDer::from([PKCS8, &private_key.0].concat());
        let key = any_eddsa_type(&pkcs8)
            .expect("invariant: the PKCS#8 template holds an Ed25519 key");
        Self::with(CertifiedKey::new(vec![certificate.into()], key))
    }

    fn with(certified: CertifiedKey) -> Self {
        let provider = Arc::new(provider());
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

/// aws-lc's TLS 1.3 suites and groups in a fixed order, so no rustls feature changes
/// them. The dialer's order decides, so nodes agree AES-128-GCM and X25519MLKEM768 on
/// every CPU. A FIPS build must drop `ChaCha20`.
fn provider() -> CryptoProvider {
    use rustls::crypto::aws_lc_rs::{cipher_suite, default_provider, kx_group};
    CryptoProvider {
        cipher_suites: vec![
            cipher_suite::TLS13_AES_128_GCM_SHA256,
            cipher_suite::TLS13_AES_256_GCM_SHA384,
            cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
        ],
        kx_groups: vec![
            kx_group::X25519MLKEM768,
            kx_group::X25519,
            kx_group::SECP256R1,
            kx_group::SECP384R1,
        ],
        ..default_provider()
    }
}

/// The template certificate for `key`, with the signature `sign` gives for its
/// to-be-signed part.
fn issue(key: &[u8], sign: impl FnOnce(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let mut tbs = Vec::with_capacity(TBS_BYTES);
    for part in [TBS, ED25519, NAME, VALIDITY, NAME, SPKI, key] {
        tbs.extend_from_slice(part);
    }
    let signature = sign(&tbs);
    let mut certificate = Vec::with_capacity(CERTIFICATE_BYTES);
    for part in [CERTIFICATE, &tbs, ED25519, SIGNATURE, &signature] {
        certificate.extend_from_slice(part);
    }
    certificate
}

/// The peer of a finished handshake, from the protocol it agreed and the
/// certificates a verifier here accepted.
///
/// # Errors
///
/// [`rustls::Error::NoApplicationProtocol`] when the protocol is not [`ALPN`]. rustls
/// requires a protocol only over QUIC.
///
/// # Panics
///
/// When the chain is not one certificate that carries a node key, which the
/// verifiers refuse.
#[expect(
    clippy::unwrap_in_result,
    reason = "another key here is a verifier defect, not a peer error"
)]
pub(crate) fn peer(
    protocol: Option<&[u8]>,
    certificates: Option<&[CertificateDer<'_>]>,
) -> Result<Peer, rustls::Error> {
    if protocol != Some(ALPN) {
        return Err(rustls::Error::NoApplicationProtocol);
    }
    Ok(match certificates.and_then(<[_]>::split_first) {
        None => Peer::Client,
        Some((end_entity, intermediates)) => Peer::Node(
            key(end_entity, intermediates)
                .expect("invariant: a verifier accepted this chain"),
        ),
    })
}

/// The node key that a chain carries. Refuses a chain that is not one certificate of
/// at most `CERTIFICATE_BYTES_MAX`, so a peer cannot make the node hold more.
fn key(
    end_entity: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
) -> Result<PublicKey, rustls::Error> {
    if !intermediates.is_empty() || end_entity.len() > CERTIFICATE_BYTES_MAX {
        return Err(CertificateError::ApplicationVerificationFailure.into());
    }
    let parsed = ParsedCertificate::try_from(end_entity)?;
    parsed
        .subject_public_key_info()
        .strip_prefix(SPKI)
        .and_then(|key| <[u8; 32]>::try_from(key).ok())
        .and_then(|key| PublicKey::new(key).ok())
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
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if key(end_entity, intermediates)? == self.expected {
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
        unreachable!("invariant: the config offers only TLS 1.3")
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
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        key(end_entity, intermediates).map(|_| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        unreachable!("invariant: the config offers only TLS 1.3")
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

/// The wall time this crate gives a library that asks for one: `UNIX_EPOCH`. The
/// defaults read the OS clock. Nothing here uses the time: the verifiers ignore
/// dates, and resumption, Retry, and `NEW_TOKEN` are off.
#[derive(Debug)]
pub(crate) struct Epoch;

impl TimeProvider for Epoch {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(Duration::ZERO))
    }
}

/// The public key of `private_key`: the key that [`Tls::new`] certifies.
pub(crate) fn public(private_key: &PrivateKey) -> PublicKey {
    let pair = Ed25519KeyPair::from_seed_unchecked(&private_key.0)
        .expect("invariant: any 32 bytes are an Ed25519 private key");
    let bytes = pair.public_key().as_ref().try_into();
    PublicKey::new(bytes.expect("invariant: an Ed25519 public key is 32 bytes"))
        .expect("invariant: aws-lc makes no key of small order")
}

/// A client like an SDK: it pins the server's key and has no certificate.
#[cfg(test)]
pub(crate) fn anonymous(
    provider: CryptoProvider,
    expected: PublicKey,
) -> Arc<ClientConfig> {
    let provider = Arc::new(provider);
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

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv6Addr};

    use aws_lc_rs::signature::{
        ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, UnparsedPublicKey,
    };
    use proptest::prelude::*;
    use rustls::client::ResolvesClientCert;
    use rustls::crypto::SupportedKxGroup;
    use rustls::crypto::aws_lc_rs::sign::any_ecdsa_type;
    use rustls::crypto::aws_lc_rs::{
        ALL_CIPHER_SUITES, ALL_KX_GROUPS, default_provider,
    };
    use rustls::pki_types::PrivateKeyDer;
    use rustls::server::Acceptor;
    use rustls::sign::{Signer, SigningKey};
    use rustls::{
        CertificateError, CipherSuite, ClientConnection, Connection, HandshakeKind,
        NamedGroup, ServerConnection, SignatureAlgorithm, SupportedCipherSuite,
    };

    use super::*;

    /// Moves every pending TLS record from `from` to `to`, and has `to` process
    /// each part as it reads it, because its read buffer is smaller than a flight.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a Vec and a slice give no I/O error; the Result is the peer's"
    )]
    fn pass(from: &mut Connection, to: &mut Connection) -> Result<(), rustls::Error> {
        let mut wire = Vec::new();
        while from.wants_write() {
            from.write_tls(&mut wire).expect("writes to a Vec");
        }
        let mut rest = wire.as_slice();
        while !rest.is_empty() {
            to.read_tls(&mut rest).expect("reads from a slice");
            to.process_new_packets()?;
        }
        Ok(())
    }

    /// Joins `client` and `server` in memory until both finish the handshake and the
    /// records sent with the last flight arrive. Returns both, or the first error
    /// either side raised.
    fn connect(
        client: Arc<ClientConfig>,
        server: Arc<ServerConfig>,
    ) -> Result<(ClientConnection, ServerConnection), rustls::Error> {
        let name = ServerName::from(IpAddr::from(Ipv6Addr::LOCALHOST));
        let mut client = Connection::from(ClientConnection::new(client, name)?);
        let mut server = Connection::from(ServerConnection::new(server)?);
        for _ in 0..8 {
            pass(&mut client, &mut server)?;
            pass(&mut server, &mut client)?;
            if !client.is_handshaking() && !server.is_handshaking() {
                let (Connection::Client(client), Connection::Server(server)) =
                    (client, server)
                else {
                    unreachable!("built as a client and a server");
                };
                return Ok((client, server));
            }
        }
        panic!("the handshake did not finish in 8 rounds");
    }

    /// The peer each side sees after a full handshake, client first.
    fn handshake(
        client: Arc<ClientConfig>,
        server: Arc<ServerConfig>,
    ) -> Result<(Peer, Peer), rustls::Error> {
        let (client, server) = connect(client, server)?;
        assert_eq!(client.handshake_kind(), Some(HandshakeKind::Full));
        Ok((
            peer(client.alpn_protocol(), client.peer_certificates())?,
            peer(server.alpn_protocol(), server.peer_certificates())?,
        ))
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

    /// TLS that presents `tls`'s certificate twice.
    fn chained(tls: &Tls) -> Tls {
        let chain = [certified(tls).cert.clone(), certified(tls).cert.clone()].concat();
        Tls::with(CertifiedKey::new(chain, Arc::clone(&certified(tls).key)))
    }

    /// TLS with [`padded_der`]'s certificate.
    fn padded(private_key: &PrivateKey, subject_bytes: usize) -> Tls {
        Tls::with(CertifiedKey::new(
            vec![padded_der(private_key, subject_bytes).into()],
            Arc::clone(&certified(&Tls::new(private_key)).key),
        ))
    }

    /// A self-signed certificate for `private_key`, like the template but with a
    /// subject name of `subject_bytes` junk bytes.
    fn padded_der(private_key: &PrivateKey, subject_bytes: usize) -> Vec<u8> {
        fn seq(content: &[u8]) -> Vec<u8> {
            let len = u16::try_from(content.len()).expect("under 64 KiB");
            [&[0x30, 0x82], len.to_be_bytes().as_slice(), content].concat()
        }
        let pair =
            Ed25519KeyPair::from_seed_unchecked(&private_key.0).expect("32 bytes");
        let subject = seq(&vec![0xa5; subject_bytes]);
        // `TBS` without its header.
        let tbs = seq(&[
            &TBS[3..],
            ED25519,
            NAME,
            VALIDITY,
            &subject,
            SPKI,
            pair.public_key().as_ref(),
        ]
        .concat());
        let signature = pair.sign(&tbs);
        seq(&[&tbs[..], ED25519, SIGNATURE, signature.as_ref()].concat())
    }

    /// The subject bytes that make [`padded_der`] `bytes` long.
    fn subject_bytes(private_key: &PrivateKey, bytes: usize) -> usize {
        bytes - padded_der(private_key, 0).len()
    }

    /// TLS with an ECDSA P-256 certificate, that signs with ECDSA whatever schemes
    /// the peer asks for.
    fn ecdsa() -> Tls {
        const P256: &[u8] = b"\x30\x59\x30\x13\x06\x07\x2a\x86\x48\xce\x3d\x02\x01\
            \x06\x08\x2a\x86\x48\xce\x3d\x03\x01\x07\x03\x42\x00";
        let sec1 = [
            b"\x30\x31\x02\x01\x01\x04\x20".as_slice(),
            &[1; 32],
            b"\xa0\x0a\x06\x08\x2a\x86\x48\xce\x3d\x03\x01\x07",
        ]
        .concat();
        let pair =
            EcdsaKeyPair::from_private_key_der(&ECDSA_P256_SHA256_ASN1_SIGNING, &sec1)
                .expect("a P-256 key");
        let mut tbs = b"\x30\x81\xba\xa0\x03\x02\x01\x02\x02\x01\x01".to_vec();
        for part in [
            ED25519,
            NAME,
            VALIDITY,
            NAME,
            P256,
            pair.public_key().as_ref(),
        ] {
            tbs.extend_from_slice(part);
        }
        assert_eq!(tbs.len(), 3 + 0xba);
        let mut der = b"\x30\x82\x01\x07".to_vec();
        for part in [&tbs[..], ED25519, SIGNATURE, &[0; 64]] {
            der.extend_from_slice(part);
        }
        assert_eq!(der.len(), 4 + 0x107);
        let key = any_ecdsa_type(&PrivateKeyDer::Sec1(sec1.into())).expect("ECDSA");
        Tls::with(CertifiedKey::new(vec![der.into()], Arc::new(Ecdsa(key))))
    }

    #[derive(Debug)]
    struct Ecdsa(Arc<dyn SigningKey>);

    impl SigningKey for Ecdsa {
        fn choose_scheme(&self, _: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
            self.0
                .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
        }

        fn algorithm(&self) -> SignatureAlgorithm {
            self.0.algorithm()
        }
    }

    /// The identity point of Ed25519: a public key that has no private key.
    const IDENTITY: [u8; 32] = {
        let mut point = [0; 32];
        point[0] = 1;
        point
    };

    /// Signs every message with `R = identity, S = 0`, with no private key.
    #[derive(Debug)]
    struct Forged;

    impl SigningKey for Forged {
        fn choose_scheme(&self, _: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
            Some(Box::new(Self))
        }

        fn algorithm(&self) -> SignatureAlgorithm {
            SignatureAlgorithm::ED25519
        }
    }

    impl Signer for Forged {
        fn sign(&self, _: &[u8]) -> Result<Vec<u8>, rustls::Error> {
            Ok([IDENTITY, [0; 32]].concat())
        }

        fn scheme(&self) -> SignatureScheme {
            SignatureScheme::ED25519
        }
    }

    /// TLS for `key`, a point of small order, made with no private key.
    fn keyless(key: [u8; 32]) -> Tls {
        let certificate = issue(&key, |_| vec![0; 64]);
        Tls::with(CertifiedKey::new(
            vec![certificate.into()],
            Arc::new(Forged),
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
            let peers = handshake(
                anonymous(default_provider(), public(&b)),
                Tls::new(&b).server(),
            );
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
            let client = anonymous(default_provider(), public(&b));
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
        fn when_client_chain_has_more_certificates_the_server_refuses() {
            let (a, b) = (PrivateKey([1; 32]), PrivateKey([2; 32]));
            let peers = handshake(
                chained(&Tls::new(&a)).client(public(&b)),
                Tls::new(&b).server(),
            );
            assert_eq!(
                peers,
                Err(CertificateError::ApplicationVerificationFailure.into())
            );
        }

        #[test]
        fn when_server_chain_has_more_certificates_the_client_refuses() {
            let (a, b) = (PrivateKey([1; 32]), PrivateKey([2; 32]));
            let peers = handshake(
                Tls::new(&a).client(public(&b)),
                chained(&Tls::new(&b)).server(),
            );
            assert_eq!(
                peers,
                Err(CertificateError::ApplicationVerificationFailure.into())
            );
        }

        #[test]
        fn when_client_certificate_is_padded_the_server_refuses() {
            let (a, b) = (PrivateKey([1; 32]), PrivateKey([2; 32]));
            let peers =
                handshake(padded(&a, 60_000).client(public(&b)), Tls::new(&b).server());
            assert_eq!(
                peers,
                Err(CertificateError::ApplicationVerificationFailure.into())
            );
        }

        #[test]
        fn when_server_certificate_is_padded_the_client_refuses() {
            let (a, b) = (PrivateKey([1; 32]), PrivateKey([2; 32]));
            let peers =
                handshake(Tls::new(&a).client(public(&b)), padded(&b, 60_000).server());
            assert_eq!(
                peers,
                Err(CertificateError::ApplicationVerificationFailure.into())
            );
        }

        #[test]
        fn when_both_certificates_are_at_the_limit_each_side_takes_the_other() {
            let (a, b) = (PrivateKey([1; 32]), PrivateKey([2; 32]));
            let full = |key| padded(key, subject_bytes(key, CERTIFICATE_BYTES_MAX));
            let peers = handshake(full(&a).client(public(&b)), full(&b).server());
            assert_eq!(peers, Ok((Peer::Node(public(&b)), Peer::Node(public(&a)))));
        }

        #[test]
        fn when_client_key_is_ecdsa_the_server_refuses() {
            let b = PrivateKey([2; 32]);
            let peers = handshake(ecdsa().client(public(&b)), Tls::new(&b).server());
            assert_eq!(
                peers,
                Err(CertificateError::ApplicationVerificationFailure.into())
            );
        }

        #[test]
        fn when_both_are_nodes_they_agree_aes_128_gcm_and_the_hybrid() {
            let (a, b) = (PrivateKey([1; 32]), PrivateKey([2; 32]));
            let (client, _) =
                connect(Tls::new(&a).client(public(&b)), Tls::new(&b).server())
                    .expect("a handshake");
            let agreed = (
                client.negotiated_cipher_suite().map(|suite| suite.suite()),
                client
                    .negotiated_key_exchange_group()
                    .map(rustls::crypto::SupportedKxGroup::name),
            );
            assert_eq!(
                agreed,
                (
                    Some(CipherSuite::TLS13_AES_128_GCM_SHA256),
                    Some(NamedGroup::X25519MLKEM768)
                )
            );
        }

        const ALPNS: &str =
            include_str!("../../../oracles/conformance/transport/alpn.txt");
        const SUITES: &str =
            include_str!("../../../oracles/conformance/transport/suites.txt");
        const GROUPS: &str =
            include_str!("../../../oracles/conformance/transport/groups.txt");

        /// The code points of an oracle list, which holds one in hex and its name on
        /// each line, in the order a node offers them.
        fn codes(list: &str) -> Vec<u16> {
            let code = |line: &str| {
                let code = line.split_whitespace().next().expect("a code point");
                u16::from_str_radix(code, 16).expect("hex")
            };
            list.lines().map(code).collect()
        }

        /// aws-lc's suites of [`SUITES`].
        fn suites() -> Vec<SupportedCipherSuite> {
            let find = |code| {
                let mut all = ALL_CIPHER_SUITES.iter();
                *all.find(|suite| u16::from(suite.suite()) == code)
                    .expect("aws-lc has the suite")
            };
            codes(SUITES).into_iter().map(find).collect()
        }

        /// aws-lc's groups of [`GROUPS`].
        fn groups() -> Vec<&'static dyn SupportedKxGroup> {
            let find = |code| {
                let mut all = ALL_KX_GROUPS.iter();
                *all.find(|group| u16::from(group.name()) == code)
                    .expect("aws-lc has the group")
            };
            codes(GROUPS).into_iter().map(find).collect()
        }

        #[test]
        fn when_a_node_dials_its_client_hello_offers_the_oracle_lists() {
            let (a, b) = (PrivateKey([1; 32]), PrivateKey([2; 32]));
            let name = ServerName::from(IpAddr::from(Ipv6Addr::LOCALHOST));
            let client = Tls::new(&a).client(public(&b));
            let mut client = ClientConnection::new(client, name).expect("a client");
            let mut wire = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut wire).expect("writes to a Vec");
            }
            let mut acceptor = Acceptor::default();
            let mut rest = wire.as_slice();
            acceptor.read_tls(&mut rest).expect("reads from a slice");
            let accepted = acceptor.accept().expect("a hello").expect("a whole hello");
            let hello = accepted.client_hello();
            let alpn = hello.alpn().expect("ALPN").collect::<Vec<_>>();
            assert_eq!(alpn, ALPNS.lines().map(str::as_bytes).collect::<Vec<_>>());
            let suites = hello.cipher_suites().iter().map(|&suite| u16::from(suite));
            assert_eq!(suites.collect::<Vec<_>>(), codes(SUITES));
            let groups = hello.named_groups().expect("groups").iter();
            let groups = groups.map(|&group| u16::from(group));
            assert_eq!(groups.collect::<Vec<_>>(), codes(GROUPS));
        }

        #[test]
        fn when_an_sdk_offers_one_suite_and_group_the_server_agrees() {
            let b = PrivateKey([2; 32]);
            let server = Tls::new(&b).server();
            for suite in suites() {
                for group in groups() {
                    let provider = CryptoProvider {
                        cipher_suites: vec![suite],
                        kx_groups: vec![group],
                        ..default_provider()
                    };
                    let client = anonymous(provider, public(&b));
                    let peers = handshake(client, Arc::clone(&server));
                    let expected = Ok((Peer::Node(public(&b)), Peer::Client));
                    assert_eq!(peers, expected, "{suite:?} {group:?}");
                }
            }
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

        #[test]
        fn when_client_key_is_the_identity_point_the_server_refuses() {
            let b = PrivateKey([2; 32]);
            let peers =
                handshake(keyless(IDENTITY).client(public(&b)), Tls::new(&b).server());
            assert_eq!(
                peers,
                Err(CertificateError::ApplicationVerificationFailure.into())
            );
        }
    }

    mod alpn {
        use super::*;

        #[test]
        fn when_client_offers_no_protocol_the_server_refuses() {
            let b = PrivateKey([2; 32]);
            let mut config = (*anonymous(default_provider(), public(&b))).clone();
            config.alpn_protocols.clear();
            let (_, server) = connect(Arc::new(config), Tls::new(&b).server())
                .expect("rustls finishes a handshake over TCP with no protocol");
            assert_eq!(
                peer(server.alpn_protocol(), server.peer_certificates()),
                Err(rustls::Error::NoApplicationProtocol)
            );
        }

        #[test]
        fn when_server_agrees_no_protocol_the_client_refuses() {
            let (a, b) = (PrivateKey([1; 32]), PrivateKey([2; 32]));
            let mut config = (*Tls::new(&b).server()).clone();
            config.alpn_protocols.clear();
            let (client, _) =
                connect(Tls::new(&a).client(public(&b)), Arc::new(config))
                    .expect("rustls finishes a handshake over TCP with no protocol");
            assert_eq!(
                peer(client.alpn_protocol(), client.peer_certificates()),
                Err(rustls::Error::NoApplicationProtocol)
            );
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
                key(&CertificateDer::from(der), &[]),
                Err(CertificateError::ApplicationVerificationFailure.into())
            );
        }

        #[test]
        fn takes_a_certificate_at_the_limit() {
            let private_key = PrivateKey([1; 32]);
            let subject = subject_bytes(&private_key, CERTIFICATE_BYTES_MAX);
            let der = padded_der(&private_key, subject);
            assert_eq!(der.len(), 1024);
            assert_eq!(
                key(&CertificateDer::from(der), &[]),
                Ok(public(&private_key))
            );
        }

        #[test]
        fn refuses_a_certificate_over_the_limit() {
            let private_key = PrivateKey([1; 32]);
            let subject = subject_bytes(&private_key, CERTIFICATE_BYTES_MAX + 1);
            let der = padded_der(&private_key, subject);
            assert_eq!(der.len(), 1025);
            assert_eq!(
                key(&CertificateDer::from(der), &[]),
                Err(CertificateError::ApplicationVerificationFailure.into())
            );
        }

        #[test]
        fn refuses_bytes_that_are_not_der() {
            assert_eq!(
                key(&CertificateDer::from(vec![1, 2, 3]), &[]),
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
                prop_assert_eq!(key(&certificate, &[]), Ok(public(&private_key)));
            }

            #[test]
            fn certifies_the_key_that_signs(bytes: [u8; 32]) {
                let certified = certified(&Tls::new(&PrivateKey(bytes)));
                let signer = certified
                    .key
                    .choose_scheme(&[SignatureScheme::ED25519])
                    .expect("an Ed25519 signer");
                let signature = signer.sign(b"message").expect("a signature");
                let key = key(&certified.cert[0], &[]).expect("a node key");
                let verifier = UnparsedPublicKey::new(
                    &aws_lc_rs::signature::ED25519,
                    key.to_bytes(),
                );
                prop_assert_eq!(verifier.verify(b"message", &signature), Ok(()));
            }
        }

        #[test]
        fn matches_the_oracle() {
            let golden =
                include_str!("../../../oracles/conformance/transport/certificate.txt");
            let golden = golden.split_whitespace().collect::<String>();
            let certificate = certificate(&Tls::new(&PrivateKey([1; 32])));
            let hex = certificate.iter().map(|byte| format!("{byte:02x}"));
            assert_eq!(hex.collect::<String>(), golden);
        }

        #[test]
        fn is_the_same_for_the_same_key() {
            let first = certificate(&Tls::new(&PrivateKey([1; 32])));
            let second = certificate(&Tls::new(&PrivateKey([1; 32])));
            assert_eq!(first, second);
        }

        #[test]
        fn is_signed_by_its_own_key() {
            let private_key = PrivateKey([1; 32]);
            let certificate = certificate(&Tls::new(&private_key));
            let der = certificate.as_ref();
            let tbs = &der[CERTIFICATE.len()..][..TBS_BYTES];
            let signature = &der[der.len() - 64..];
            let verifier = UnparsedPublicKey::new(
                &aws_lc_rs::signature::ED25519,
                public(&private_key).to_bytes(),
            );
            assert_eq!(verifier.verify(tbs, signature), Ok(()));
        }
    }
}

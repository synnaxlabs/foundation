//! What noq-proto gets from a [`Config`].

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aws_lc_rs::{hkdf, hmac};
use env::entropy::Entropy;
use noq_proto::congestion::CubicConfig;
use noq_proto::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use noq_proto::crypto::{CryptoError, HandshakeTokenKey};
use noq_proto::{
    ClientConfig, ConnectionId, ConnectionIdGenerator, Endpoint, EndpointConfig,
    IdleTimeout, MtuDiscoveryConfig, NoneTokenLog, NoneTokenStore, ServerConfig,
    TimeSource, TransportConfig, ValidationTokenConfig, VarInt,
};
use types::node::{PrivateKey, PublicKey};
use types::time::Span;

use crate::Config;
use crate::tls::Tls;

/// The bytes in every connection ID this node issues: the shard, then random bytes.
const ID_BYTES: usize = 8;

const QUIC_V1: u32 = 1;

/// One shard's noq-proto settings. Every option that changes behavior is set by
/// name, and every random value outside TLS comes from `Entropy`.
pub(super) struct Settings {
    endpoint: Arc<EndpointConfig>,
    server: Arc<ServerConfig>,
    transport: Arc<TransportConfig>,
    tls: Tls,
    entropy: Entropy,
}

impl Settings {
    /// The settings of `shard`, whose connection IDs all start with `shard`.
    ///
    /// # Panics
    ///
    /// When `config.idle` is not positive.
    pub(super) fn new(config: &Config, shard: u8) -> Self {
        let transport = Arc::new(transport(config));
        let tls = Tls::new(&config.private_key);
        Self {
            endpoint: Arc::new(endpoint(config, shard)),
            server: Arc::new(server(&tls, Arc::clone(&transport))),
            transport,
            tls,
            entropy: config.entropy.clone(),
        }
    }

    /// A new endpoint that accepts connections.
    pub(super) fn endpoint(&self) -> Endpoint {
        let server = Some(Arc::clone(&self.server));
        Endpoint::new(Arc::clone(&self.endpoint), server, true)
    }

    /// The settings for a dial that expects `expected`.
    pub(super) fn client(&self, expected: PublicKey) -> ClientConfig {
        let crypto = QuicClientConfig::try_from(self.tls.client(expected))
            .expect("invariant: the TLS suites include AES-128-GCM");
        let entropy = self.entropy.clone();
        let mut client = ClientConfig::new(Arc::new(crypto));
        client
            .transport_config(Arc::clone(&self.transport))
            .token_store(Arc::new(NoneTokenStore))
            .version(QUIC_V1)
            .initial_dst_cid_provider(Arc::new(move || {
                let mut id = [0; ID_BYTES];
                entropy.fill(&mut id);
                ConnectionId::new(&id)
            }));
        client
    }
}

fn endpoint(config: &Config, shard: u8) -> EndpointConfig {
    let mut rng = [0; 32];
    config.entropy.fill(&mut rng);
    let issuer = Issuer {
        shard,
        entropy: config.entropy.clone(),
    };
    let mut endpoint = EndpointConfig::new(Arc::new(reset_key(&config.private_key)));
    endpoint
        .max_udp_payload_size(1472)
        .expect("invariant: QUIC allows 1200 to 65527")
        .cid_generator(Arc::new(move || -> Box<dyn ConnectionIdGenerator> {
            Box::new(issuer.clone())
        }))
        .rng_seed(Some(rng))
        .supported_versions(vec![QUIC_V1])
        .grease_quic_bit(true)
        .min_reset_interval(Duration::from_millis(20));
    endpoint
}

fn server(tls: &Tls, transport: Arc<TransportConfig>) -> ServerConfig {
    let crypto = QuicServerConfig::try_from(tls.server())
        .expect("invariant: the TLS suites include AES-128-GCM");
    let mut tokens = ValidationTokenConfig::default();
    tokens.sent(0).log(Arc::new(NoneTokenLog));
    let mut server = ServerConfig::new(Arc::new(crypto), Arc::new(NoTokens));
    server
        .transport_config(transport)
        .validation_token_config(tokens)
        .migration(true)
        .time_source(Arc::new(Epoch));
    server
}

fn transport(config: &Config) -> TransportConfig {
    let idle_ms = idle_ms(config.idle);
    let window = VarInt::try_from(config.window_bytes).unwrap_or(VarInt::MAX);
    let streams = VarInt::from_u32(config.streams_max.get());
    let mut mtu = MtuDiscoveryConfig::default();
    mtu.upper_bound(1452)
        .interval(Duration::from_secs(600))
        .black_hole_cooldown(Duration::from_secs(60))
        .minimum_change(20);
    let mut transport = TransportConfig::default();
    transport
        .max_concurrent_bidi_streams(streams)
        .max_concurrent_uni_streams(streams)
        .stream_receive_window(window)
        .receive_window(window)
        .send_window(window.into_inner())
        .send_fairness(true)
        .max_idle_timeout(Some(IdleTimeout::from(
            VarInt::from_u64(idle_ms).expect("invariant: i64 nanoseconds fit"),
        )))
        .keep_alive_interval(Some(Duration::from_millis(idle_ms) / 3))
        .initial_rtt(Duration::from_millis(333))
        .packet_threshold(3)
        .time_threshold(9.0 / 8.0)
        .persistent_congestion_threshold(3)
        .initial_mtu(1200)
        .min_mtu(1200)
        .mtu_discovery_config(Some(mtu))
        .pad_to_mtu(false)
        .enable_segmentation_offload(true)
        .ack_frequency_config(None)
        .max_outgoing_bytes_per_second(None)
        .crypto_buffer_size(16 << 10)
        .allow_spin(false)
        .datagram_receive_buffer_size(None)
        .max_concurrent_multipath_paths(0)
        .max_remote_nat_traversal_addresses(0)
        .server_handshake_migration(false)
        .send_observed_address_reports(false)
        .receive_observed_address_reports(false)
        // BBR3 reads the thread's random generator.
        .congestion_controller_factory(Arc::new(CubicConfig::default()));
    transport
}

/// `idle` in whole milliseconds, rounded up, so at least 1: QUIC counts
/// milliseconds, and 0 turns the timeout off.
fn idle_ms(idle: Span) -> u64 {
    let nanos = u64::try_from(idle.nanos())
        .ok()
        .filter(|&nanos| nanos > 0)
        .unwrap_or_else(|| panic!("idle must be positive, not {idle:?}"));
    nanos.div_ceil(1_000_000)
}

/// The key that signs stateless resets. It comes from the node key, so every shard
/// and every restart of the node signs alike, and a restarted node resets a
/// peer's stale connection at once.
fn reset_key(private_key: &PrivateKey) -> hmac::Key {
    hkdf::Salt::new(hkdf::HKDF_SHA256, b"foundation/1 quic")
        .extract(&private_key.0)
        .expand(&[b"stateless reset".as_slice()], hmac::HMAC_SHA256)
        .expect("invariant: an HMAC key is shorter than HKDF's limit")
        .into()
}

/// Issues this shard's connection IDs.
#[derive(Clone)]
struct Issuer {
    shard: u8,
    entropy: Entropy,
}

impl ConnectionIdGenerator for Issuer {
    fn generate_cid(&mut self) -> ConnectionId {
        let mut id = [self.shard; ID_BYTES];
        self.entropy.fill(&mut id[1..]);
        ConnectionId::new(&id)
    }

    fn cid_len(&self) -> usize {
        ID_BYTES
    }

    fn cid_lifetime(&self) -> Option<Duration> {
        None
    }
}

/// A token key for an endpoint that sends no tokens: Retry and `NEW_TOKEN` are off.
struct NoTokens;

impl HandshakeTokenKey for NoTokens {
    fn seal(&self, _: u128, _: &mut Vec<u8>) -> Result<(), CryptoError> {
        panic!("a token was sealed, but Retry and NEW_TOKEN are off")
    }

    fn open<'a>(&self, _: u128, _: &'a mut [u8]) -> Result<&'a [u8], CryptoError> {
        Err(CryptoError)
    }
}

/// The wall time for tokens, which are off. Nothing else in noq-proto reads it.
struct Epoch;

impl TimeSource for Epoch {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
}

#[cfg(test)]
mod tests {
    use noq_proto::crypto::HmacKey;

    use super::*;

    mod reset_key {
        use super::*;

        fn sign(private_key: [u8; 32]) -> [u8; 32] {
            let key = reset_key(&PrivateKey(private_key));
            let mut signature = [0; 32];
            key.sign(b"a connection id", &mut signature);
            signature
        }

        #[test]
        fn signs_alike_for_one_node_key() {
            assert_eq!(sign([1; 32]), sign([1; 32]));
        }

        #[test]
        fn signs_otherwise_for_another_node_key() {
            assert_ne!(sign([1; 32]), sign([2; 32]));
        }
    }

    mod no_tokens {
        use super::*;

        #[test]
        fn refuses_every_token() {
            let mut token = vec![0; 32];
            let opened = NoTokens.open(0, &mut token);
            assert!(matches!(opened, Err(CryptoError)), "{opened:?}");
        }

        #[test]
        #[should_panic(
            expected = "a token was sealed, but Retry and NEW_TOKEN are off"
        )]
        fn panics_on_seal() {
            let mut token = Vec::new();
            let sealed = NoTokens.seal(0, &mut token);
            unreachable!("seal returned {sealed:?}");
        }
    }

    mod handshake {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use std::num::NonZeroUsize;

        use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
        use bytes::BytesMut;
        use noq_proto::{DatagramEvent, FourTuple};

        use super::*;
        use crate::quic::testing;

        const CLIENT: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
        const SERVER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2);

        /// The destination and source IDs of a QUIC v1 long-header packet.
        fn ids(datagram: &[u8]) -> (&[u8], &[u8]) {
            let [form, 0, 0, 0, 1, length, rest @ ..] = datagram else {
                panic!("not QUIC v1: {datagram:02x?}");
            };
            assert_eq!(form & 0x80, 0x80, "a long header");
            let (destination, rest) = rest.split_at(usize::from(*length));
            let [length, rest @ ..] = rest else {
                panic!("no source ID: {datagram:02x?}");
            };
            (destination, &rest[..usize::from(*length)])
        }

        /// Starts a dial from shard 3 to shard 5 in a run made from `value`, and gives
        /// each side's first datagram.
        fn first_datagrams(value: u64) -> (Vec<u8>, Vec<u8>) {
            testing::run(value, |shard| {
                let now = shard.clock.epoch();
                let one = NonZeroUsize::MIN;
                let server_key = PrivateKey([2; 32]);
                let pair = Ed25519KeyPair::from_seed_unchecked(&server_key.0)
                    .expect("32 bytes");
                let expected =
                    PublicKey(pair.public_key().as_ref().try_into().expect("32 bytes"));
                let near = Settings::new(&shard.config(PrivateKey([1; 32])), 3);
                let far = Settings::new(&shard.config(server_key), 5);

                let (_, mut outbound) = near
                    .endpoint()
                    .connect(now, near.client(expected), SERVER, "foundation")
                    .expect("the dial starts");
                let mut initial = Vec::new();
                let sent = outbound
                    .poll_transmit(now, one, &mut initial)
                    .expect("an Initial");
                assert_eq!((sent.destination, sent.size), (SERVER, initial.len()));

                let mut listener = far.endpoint();
                let mut response = Vec::new();
                let event = listener.handle(
                    now,
                    FourTuple::new(CLIENT, None),
                    None,
                    BytesMut::from(initial.as_slice()),
                    &mut response,
                );
                let Some(DatagramEvent::NewConnection(incoming)) = event else {
                    panic!("the Initial starts no connection");
                };
                let (_, mut inbound) = listener
                    .accept(incoming, now, &mut response, None)
                    .expect("the server accepts");
                let mut reply = Vec::new();
                let sent = inbound
                    .poll_transmit(now, one, &mut reply)
                    .expect("a reply");
                assert_eq!((sent.destination, sent.size), (CLIENT, reply.len()));
                (initial, reply)
            })
        }

        #[test]
        fn pads_the_initial_to_the_minimum_mtu() {
            let (initial, _) = first_datagrams(1);
            assert!(initial.len() >= 1200, "{} bytes", initial.len());
        }

        #[test]
        fn gives_ids_of_eight_bytes_that_start_with_the_shard() {
            let (initial, reply) = first_datagrams(1);
            let (destination, source) = ids(&initial);
            assert_eq!(destination.len(), ID_BYTES, "{destination:?}");
            assert_eq!(source.len(), ID_BYTES, "{source:?}");
            assert_eq!(source[0], 3, "{source:?}");
            let (back, server) = ids(&reply);
            assert_eq!(back, source);
            assert_eq!(server.len(), ID_BYTES, "{server:?}");
            assert_eq!(server[0], 5, "{server:?}");
        }

        #[test]
        fn draws_every_id_from_entropy() {
            let id_sets = |value| {
                let (initial, reply) = first_datagrams(value);
                let (destination, source) = ids(&initial);
                let (_, server) = ids(&reply);
                [destination, source, server].map(<[u8]>::to_vec)
            };
            assert_eq!(id_sets(1), id_sets(1));
            let (one, two) = (id_sets(1), id_sets(2));
            for (a, b) in one.iter().zip(&two) {
                assert_ne!(a, b);
            }
        }
    }

    mod idle {
        use super::*;

        #[test]
        fn rounds_up_to_a_whole_millisecond() {
            for (nanos, ms) in [(1, 1), (1_000_000, 1), (1_000_001, 2), (2_500_000, 3)]
            {
                assert_eq!(idle_ms(Span::from_nanos(nanos)), ms, "{nanos} ns");
            }
        }

        #[test]
        #[should_panic(expected = "idle must be positive, not")]
        fn panics_when_not_positive() {
            idle_ms(Span::from_nanos(0));
        }
    }
}

//! What noq-proto gets from a [`Config`], and the datagrams it never gets. Every
//! option that changes behavior is set by name, and every random value outside TLS
//! comes from `Entropy`.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aws_lc_rs::{hkdf, hmac};
use env::entropy::Entropy;
use noq_proto::congestion::CubicConfig;
use noq_proto::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use noq_proto::crypto::{CryptoError, HandshakeTokenKey};
use noq_proto::{
    ClientConfig, ConnectionIdGenerator, Endpoint, EndpointConfig, IdleTimeout,
    MtuDiscoveryConfig, NoneTokenLog, NoneTokenStore, ServerConfig, TimeSource,
    TransportConfig, ValidationTokenConfig, VarInt,
};
use types::node::{PrivateKey, PublicKey};
use types::time::Span;

use super::cid;
use crate::Config;
use crate::tls::{Epoch, Tls};

const QUIC_V1: u32 = 1;

/// The smallest datagram QUIC allows. Every datagram is this size until MTU
/// discovery finds a larger one.
pub(super) const MTU_MIN: u16 = 1200;

/// Ethernet's 1500 bytes less the IPv4 and UDP headers: the largest datagram this
/// node takes.
const PAYLOAD_IPV4: u16 = 1472;

/// Ethernet's 1500 bytes less the IPv6 and UDP headers: the largest datagram MTU
/// discovery tries, so it fits both IP versions.
const PAYLOAD_IPV6: u16 = 1452;

/// What each dial from one shard needs.
pub(super) struct Settings {
    transport: Arc<TransportConfig>,
    tls: Tls,
    entropy: Entropy,
}

impl Settings {
    /// The settings of `shard`, and its endpoint, which accepts connections. Every
    /// connection ID the endpoint issues starts with `shard`.
    ///
    /// # Panics
    ///
    /// When `config.idle` is not positive.
    pub(super) fn new(config: &Config, shard: u8) -> (Self, Endpoint) {
        let transport = Arc::new(transport(config));
        let tls = Tls::new(&config.private_key);
        let server = Arc::new(server(&tls, Arc::clone(&transport)));
        // `true`: `env::net` sets don't-fragment, so MTU discovery may run.
        #[expect(clippy::disallowed_methods, reason = "the config sets rng_seed")]
        let endpoint =
            Endpoint::new(Arc::new(endpoint(config, shard)), Some(server), true);
        let settings = Self {
            transport,
            tls,
            entropy: config.entropy.clone(),
        };
        (settings, endpoint)
    }

    /// The settings for a dial that expects `expected`.
    pub(super) fn client(&self, expected: PublicKey) -> ClientConfig {
        let crypto = QuicClientConfig::try_from(self.tls.client(expected))
            .expect("invariant: the TLS suites include AES-128-GCM");
        let entropy = self.entropy.clone();
        #[expect(
            clippy::disallowed_methods,
            reason = "the config sets the first destination ID and the token store"
        )]
        let mut client = ClientConfig::new(Arc::new(crypto));
        client
            .transport_config(Arc::clone(&self.transport))
            .token_store(Arc::new(NoneTokenStore))
            .version(QUIC_V1)
            .initial_dst_cid_provider(Arc::new(move || cid::random(&entropy)));
        client
    }
}

fn endpoint(config: &Config, shard: u8) -> EndpointConfig {
    let mut rng = [0; 32];
    config.entropy.fill(&mut rng);
    let issuer = cid::Issuer {
        shard,
        entropy: config.entropy.clone(),
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "the config sets the ID generator and rng_seed"
    )]
    let mut endpoint = EndpointConfig::new(Arc::new(reset_key(&config.private_key)));
    endpoint
        .max_udp_payload_size(PAYLOAD_IPV4)
        .expect("invariant: QUIC allows 1200 to 65527")
        .cid_generator(Arc::new(move || -> Box<dyn ConnectionIdGenerator> {
            Box::new(issuer.clone())
        }))
        .rng_seed(Some(rng))
        .supported_versions(vec![QUIC_V1])
        .grease_quic_bit(true)
        // A reset is smaller than the datagram that caused it, so it needs no rate
        // limit, and one shared limit lets junk from one address take every reset.
        .min_reset_interval(Duration::ZERO);
    endpoint
}

/// Whether the endpoint drops `datagram` unread: a long header of an unknown
/// version in fewer than [`MTU_MIN`] bytes. noq-proto answers such a header at any
/// size, so a spoofed source would get more bytes than it sent. Version 0 is a
/// version negotiation for a dial, so it passes.
pub(super) fn dropped(datagram: &[u8]) -> bool {
    match *datagram {
        [form, a, b, c, d, ..]
            if form & 0x80 != 0 && datagram.len() < usize::from(MTU_MIN) =>
        {
            !matches!(u32::from_be_bytes([a, b, c, d]), 0 | QUIC_V1)
        }
        _ => false,
    }
}

fn server(tls: &Tls, transport: Arc<TransportConfig>) -> ServerConfig {
    let crypto = QuicServerConfig::try_from(tls.server())
        .expect("invariant: the TLS suites include AES-128-GCM");
    let mut tokens = ValidationTokenConfig::default();
    tokens.sent(0).log(Arc::new(NoneTokenLog));
    #[expect(clippy::disallowed_methods, reason = "the config sets the time source")]
    let mut server = ServerConfig::new(Arc::new(crypto), Arc::new(NoTokens));
    // Each `Incoming` is accepted when it arrives, so none waits and none buffers a
    // datagram.
    server
        .transport_config(transport)
        .validation_token_config(tokens)
        .migration(true)
        .preferred_address_v4(None)
        .preferred_address_v6(None)
        .max_incoming(1)
        .incoming_buffer_size(0)
        .incoming_buffer_size_total(0)
        .time_source(Arc::new(Epoch));
    server
}

fn transport(config: &Config) -> TransportConfig {
    let idle_ms = idle_ms(config.idle);
    let window = VarInt::try_from(config.window_bytes).unwrap_or(VarInt::MAX);
    let streams = VarInt::from_u32(config.streams_max.get());
    let mut mtu = MtuDiscoveryConfig::default();
    mtu.upper_bound(PAYLOAD_IPV6)
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
        .initial_mtu(MTU_MIN)
        .min_mtu(MTU_MIN)
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
        .expect("invariant: Transport::new refuses a non-positive idle");
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

impl TimeSource for Epoch {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::net::SocketAddr;

    use std::num::NonZeroUsize;
    use std::task::Poll;

    use env::net::udp::Meta;
    use noq_proto::Dir;
    use noq_proto::crypto::HmacKey;
    use types::time::Monotonic;

    use super::*;
    use crate::quic::testing::{self, CLIENT_SHARD, Pair, SERVER_SHARD, Side};
    use crate::quic::{Endpoint, Event};
    use crate::{Class, Error, tls};

    /// The link delay each way in [`dial`].
    const DELAY: Duration = Duration::from_millis(10);

    /// A dial with an idle of 1 s, after `span`.
    fn dial(shard: &testing::Shard, span: Duration) -> Pair {
        let mut pair = Pair::new(shard, Span::SECOND, DELAY);
        pair.dial(tls::public(&testing::SERVER_KEY));
        pair.run(span);
        pair
    }

    fn connected(side: &Side) -> bool {
        let mut events = side.events.iter();
        events.any(|(_, event)| matches!(event, Event::Connected { .. }))
    }

    /// When and why `side`'s connection ended.
    fn lost(side: &Side) -> Option<(Duration, &Error)> {
        side.events.iter().find_map(|(at, event)| match event {
            Event::Closed { error, .. } => Some((*at, error)),
            _ => None,
        })
    }

    /// Sends "ping" from the client on a new stream, and gives what the server reads.
    fn ping(shard: &testing::Shard, pair: &mut Pair) -> Vec<u8> {
        let (now, key) = (pair.now(), pair.client.key.expect("a connection"));
        let client = &mut pair.client.endpoint;
        let opened = client.open_sender(now, key, Class::Command);
        let mut sender = opened.expect("a stream");
        let written = client.write(now, &mut sender, shard.block(b"ping"));
        assert_eq!(written, Ok(Poll::Ready(())));
        pair.run(Duration::from_millis(100));
        let (now, key) = (pair.now(), pair.server.key.expect("a connection"));
        let server = &mut pair.server.endpoint;
        let mut incoming = server.accept(key).expect("a stream");
        let read = server.read(now, &mut incoming.receiver).expect("read");
        let Poll::Ready(Some(message)) = read else {
            panic!("no message");
        };
        message.to_vec()
    }

    /// The meta of `datagram` alone, from `source`.
    fn meta(source: SocketAddr, datagram: &[u8]) -> Meta {
        let len = datagram.len();
        Meta {
            source,
            destination: None,
            ecn: None,
            len,
            stride: len,
        }
    }

    /// What `endpoint` sends back for `datagram` from `source` at `now`.
    fn reply(
        endpoint: &mut Endpoint,
        now: Monotonic,
        source: SocketAddr,
        datagram: &[u8],
    ) -> Option<Vec<u8>> {
        endpoint.receive(now, &meta(source, datagram), datagram);
        let mut buffer = Vec::new();
        let transmit = endpoint.transmit(now, &mut buffer)?;
        Some(transmit.contents.to_vec())
    }

    /// The destination ID of a datagram's first packet, and its source ID when the
    /// header is long.
    fn ids(datagram: &[u8]) -> (&[u8], Option<&[u8]>) {
        match datagram {
            [form, _, _, _, _, length, rest @ ..] if form & 0x80 != 0 => {
                let (destination, rest) = rest.split_at(usize::from(*length));
                let [length, rest @ ..] = rest else {
                    panic!("no source ID: {datagram:02x?}");
                };
                (destination, Some(&rest[..usize::from(*length)]))
            }
            [_, rest @ ..] => (&rest[..cid::LEN], None),
            [] => panic!("an empty datagram"),
        }
    }

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

    mod dial {
        use super::*;

        #[test]
        fn carries_stream_data() {
            testing::run(1, |shard| {
                let mut pair = dial(shard, Duration::from_millis(100));
                assert!(connected(&pair.client) && connected(&pair.server));
                assert_eq!(ping(shard, &mut pair), b"ping");
            });
        }

        #[test]
        fn lets_the_peer_open_streams_max_streams_of_each_kind() {
            testing::run(1, |shard| {
                let mut pair = dial(shard, Duration::from_millis(100));
                let mut streams = pair.client.connection().streams();
                for dir in [Dir::Bi, Dir::Uni] {
                    let opened = (0..=testing::STREAMS_MAX)
                        .map_while(|_| streams.open(dir))
                        .count();
                    let max = usize::try_from(testing::STREAMS_MAX).expect("fits");
                    assert_eq!(opened, max, "{dir:?}");
                }
            });
        }

        #[test]
        fn pads_the_first_initial_to_the_minimum_mtu() {
            testing::run(1, |shard| {
                let pair = dial(shard, Duration::ZERO);
                let (_, _, initial) = pair.client.sent.first().expect("an Initial");
                assert_eq!(initial.len(), usize::from(MTU_MIN));
            });
        }

        #[test]
        fn keeps_the_connection_when_the_client_address_changes() {
            testing::run(1, |shard| {
                let mut pair = dial(shard, Duration::from_millis(100));
                let moved = SocketAddr::new(testing::CLIENT.ip(), 3);
                pair.client.address = moved;
                assert_eq!(ping(shard, &mut pair), b"ping");
                let (_, to, _) = pair.server.sent.last().expect("a datagram");
                assert_eq!(*to, moved);
            });
        }
    }

    mod ids {
        use super::*;

        #[test]
        fn are_eight_bytes_and_start_with_the_issuing_shard() {
            testing::run(1, |shard| {
                let pair = dial(shard, Duration::from_millis(100));
                let mut longs = 0;
                for (_, _, datagram) in pair.client.sent.iter().chain(&pair.server.sent)
                {
                    if let (destination, Some(source)) = ids(datagram) {
                        assert_eq!([destination.len(), source.len()], [cid::LEN; 2]);
                        longs += 1;
                    }
                }
                assert!(longs > 0, "no long header");
                let random = ids(&pair.client.sent[0].2).0;
                let issued = |side: &Side| -> Vec<u8> {
                    let ids = side.sent.iter().map(|(_, _, datagram)| ids(datagram).0);
                    ids.filter(|&id| id != random).map(|id| id[0]).collect()
                };
                let (to_server, to_client) =
                    (issued(&pair.client), issued(&pair.server));
                assert!(!to_server.is_empty() && !to_client.is_empty());
                assert!(to_server.iter().all(|&shard| shard == SERVER_SHARD));
                assert!(to_client.iter().all(|&shard| shard == CLIENT_SHARD));
            });
        }

        #[test]
        fn stay_the_same_for_the_whole_connection() {
            testing::run(1, |shard| {
                let pair = dial(shard, Duration::from_secs(10));
                let short = |side: &Side| {
                    let sent = side.sent.iter().map(|(_, _, datagram)| ids(datagram));
                    let short = sent.filter_map(|(destination, source)| {
                        source.is_none().then_some(destination.to_vec())
                    });
                    short.collect::<BTreeSet<_>>()
                };
                let to_client = short(&pair.server);
                assert_eq!(to_client.len(), 1, "{to_client:02x?}");
                let mut sent = pair
                    .server
                    .sent
                    .iter()
                    .map(|(_, _, datagram)| ids(datagram));
                let issued =
                    sent.find_map(|(_, source)| source).expect("a long header");
                assert_eq!(short(&pair.client), BTreeSet::from([issued.to_vec()]));
            });
        }
    }

    mod replay {
        use super::*;

        /// What a run made from `value` shows outside encryption. For each datagram:
        /// the sender's shard, when, the size, the header bits that are not
        /// protected, and the destination ID.
        fn trace(value: u64) -> Vec<(u8, Duration, usize, u8, Vec<u8>)> {
            testing::run(value, |shard| {
                let pair = dial(shard, Duration::from_secs(2));
                let sides =
                    [(CLIENT_SHARD, &pair.client), (SERVER_SHARD, &pair.server)];
                let datagrams = sides.into_iter().flat_map(|(shard, side)| {
                    side.sent
                        .iter()
                        .map(move |(at, _, datagram)| (shard, *at, datagram))
                });
                datagrams
                    .map(|(shard, at, datagram)| {
                        let bits = datagram[0] & 0xe0;
                        (shard, at, datagram.len(), bits, ids(datagram).0.to_vec())
                    })
                    .collect()
            })
        }

        #[test]
        fn gives_one_trace_for_one_value() {
            assert_eq!(trace(1), trace(1));
        }

        #[test]
        fn draws_every_id_from_entropy() {
            let ids = |value| {
                let trace = trace(value).into_iter();
                trace.map(|(.., id)| id).collect::<BTreeSet<_>>()
            };
            let (one, two) = (ids(1), ids(2));
            assert!(one.is_disjoint(&two), "{one:?} {two:?}");
        }
    }

    mod versions {
        use super::*;

        #[test]
        fn offers_only_quic_v1_to_a_peer_of_another_version() {
            let versions = testing::run(1, |shard| {
                let config = shard.config(testing::SERVER_KEY, Span::SECOND);
                let mut endpoint =
                    Endpoint::new(&config, SERVER_SHARD, NonZeroUsize::MIN);
                let (meta, initial) = testing::draft_29();
                endpoint.receive(Monotonic(0), &meta, &initial);
                let mut buffer = Vec::new();
                let transmit = endpoint.transmit(Monotonic(0), &mut buffer);
                let reply = transmit.expect("a reply").contents;
                let (_, Some(_)) = ids(reply) else {
                    panic!("not a long header: {reply:02x?}");
                };
                let versions = &reply[1 + 4 + 1 + cid::LEN + 1 + cid::LEN..];
                let versions = versions.chunks(4).map(|version| {
                    u32::from_be_bytes(version.try_into().expect("4 bytes"))
                });
                versions
                    .filter(|version| version & 0x0f0f_0f0f != 0x0a0a_0a0a)
                    .collect::<Vec<_>>()
            });
            assert_eq!(versions, [QUIC_V1]);
        }

        #[test]
        fn ignores_another_version_in_fewer_than_1200_bytes() {
            let replies = testing::run(1, |shard| {
                let config = shard.config(testing::SERVER_KEY, Span::SECOND);
                let mut endpoint =
                    Endpoint::new(&config, SERVER_SHARD, NonZeroUsize::MIN);
                let (_, mut initial) = testing::draft_29();
                initial.pop();
                let bare = [0xc0, 0xff, 0, 0, 0x1d, 0, 0];
                [bare.as_slice(), &initial].map(|datagram| {
                    reply(&mut endpoint, Monotonic(0), testing::CLIENT, datagram)
                })
            });
            assert_eq!(replies, [None, None]);
        }

        #[test]
        fn ends_a_dial_at_a_version_negotiation() {
            let reason = testing::run(1, |shard| {
                let mut pair = Pair::new(shard, Span::SECOND, DELAY);
                pair.server.silent = true;
                pair.dial(tls::public(&testing::SERVER_KEY));
                pair.run(Duration::ZERO);
                let (.., initial) = &pair.client.sent[0];
                let (destination, Some(source)) = ids(initial) else {
                    panic!("not a long header: {initial:02x?}");
                };
                let len = [u8::try_from(cid::LEN).expect("fits")];
                let unknown = [0x0a, 0x1a, 0x2a, 0x3a];
                let negotiation = [
                    [0x80, 0, 0, 0, 0].as_slice(),
                    &len,
                    source,
                    &len,
                    destination,
                    &unknown,
                ]
                .concat();
                let meta = meta(testing::SERVER, &negotiation);
                pair.client
                    .endpoint
                    .receive(pair.now(), &meta, &negotiation);
                pair.run(Duration::ZERO);
                lost(&pair.client).map(|(_, reason)| reason.clone())
            });
            let broken = Error::Broken {
                reason: "peer doesn't implement any supported version".into(),
            };
            assert_eq!(reason, Some(broken));
        }
    }

    mod idle {
        use super::*;

        #[test]
        fn closes_the_connection_within_four_thirds_idle_after_the_peer_stops() {
            let (silence, reason) = testing::run(1, |shard| {
                let mut pair = dial(shard, Duration::from_millis(100));
                pair.server.silent = true;
                pair.run(Duration::from_secs(3));
                let &(last, ..) = pair.server.sent.last().expect("a datagram");
                let (at, reason) = lost(&pair.client).expect("the connection ends");
                let silence = at.checked_sub(last + DELAY).expect("after it arrives");
                (silence, reason.clone())
            });
            assert_eq!(reason, Error::TimedOut);
            let idle = Duration::from_secs(1);
            assert!(idle <= silence && silence <= idle * 4 / 3, "{silence:?}");
        }

        #[test]
        fn keeps_a_quiet_connection_open() {
            testing::run(1, |shard| {
                let pair = dial(shard, Duration::from_secs(10));
                assert_eq!(lost(&pair.client), None);
                assert_eq!(lost(&pair.server), None);
            });
        }

        #[test]
        fn rounds_up_to_a_whole_millisecond() {
            for (nanos, ms) in [(1, 1), (1_000_000, 1), (1_000_001, 2), (2_500_000, 3)]
            {
                assert_eq!(idle_ms(Span::from_nanos(nanos)), ms, "{nanos} ns");
            }
        }

        #[test]
        #[should_panic(
            expected = "invariant: Transport::new refuses a non-positive idle"
        )]
        fn panics_when_not_positive() {
            idle_ms(Span::from_nanos(0));
        }
    }

    mod loss {
        use super::*;

        #[test]
        fn sends_a_lost_packet_again_nine_eighths_of_a_round_trip_after_it_left() {
            let sent = testing::run(1, |shard| {
                let idle = Span::from_nanos(10 * Span::SECOND.nanos());
                let mut pair = Pair::new(shard, idle, ROUND_TRIP / 2);
                pair.dial(tls::public(&testing::SERVER_KEY));
                pair.run(Duration::from_secs(1));
                let before = pair.client.sent.len();
                pair.client.drops = 1;
                let mut streams = pair.client.connection().streams();
                let stream = streams.open(Dir::Uni).expect("a stream");
                for _ in 0..2 {
                    let mut send = pair.client.connection().send_stream(stream);
                    send.write(&[0; 100]).expect("written");
                    pair.run(Duration::ZERO);
                }
                pair.run(Duration::from_secs(1));
                let sent = pair.client.sent[before..].iter();
                sent.map(|&(at, ..)| at).take(3).collect::<Vec<_>>()
            });
            let [lost, next, again] = sent[..] else {
                panic!("{sent:?}");
            };
            assert_eq!(lost, next);
            // The peer reports its ACK delay in 8 µs units, rounded down, so the
            // smoothed round trip can be up to 8 µs too long.
            let resend = again.checked_sub(lost).expect("after the loss");
            let expected = ROUND_TRIP * 9 / 8;
            let late = Duration::from_micros(9);
            assert!(expected <= resend && resend < expected + late, "{resend:?}");
        }

        const ROUND_TRIP: Duration = Duration::from_millis(125);
    }

    mod restart {
        use super::*;

        #[test]
        fn resets_the_old_connection_at_its_next_datagram() {
            let reason = testing::run(1, |shard| {
                let mut pair = dial(shard, Duration::from_millis(100));
                pair.restart(shard);
                pair.run(Duration::from_secs(1));
                let (_, reason) = lost(&pair.client).expect("the connection ends");
                reason.clone()
            });
            let broken = Error::Broken {
                reason: "reset by peer".into(),
            };
            assert_eq!(reason, broken);
        }

        #[test]
        fn resets_each_stale_datagram_while_another_address_sends_junk() {
            let resets = testing::run(1, |shard| {
                let config = shard.config(testing::SERVER_KEY, Span::SECOND);
                let mut endpoint =
                    Endpoint::new(&config, SERVER_SHARD, NonZeroUsize::MIN);
                let attacker = SocketAddr::new(testing::CLIENT.ip(), 9);
                let short = |id, len| {
                    let mut datagram = vec![0x40, SERVER_SHARD];
                    datagram.resize(len, id);
                    datagram
                };
                let (junk, stale) = (short(9, 23), short(1, 40));
                let mut resets = 0;
                for ms in 0..1_000 {
                    let now = testing::at(Duration::from_millis(ms));
                    if ms % 10 == 0 {
                        reply(&mut endpoint, now, attacker, &junk);
                    }
                    if ms % 100 == 5 {
                        let reset = reply(&mut endpoint, now, testing::CLIENT, &stale);
                        resets += usize::from(reset.is_some());
                    }
                }
                resets
            });
            assert_eq!(resets, 10);
        }
    }
}

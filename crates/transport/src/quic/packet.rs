//! A reader of the QUIC v1 packets that a node sends, for tests that check the frames
//! on the wire. It panics on each packet or frame that it cannot read.

use std::sync::{Arc, Mutex};

use aws_lc_rs::aead::quic::{AES_128, Algorithm, CHACHA20, HeaderProtectionKey};
use aws_lc_rs::aead::{
    AES_128_GCM, Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey,
};
use aws_lc_rs::hkdf::{HKDF_SHA256, KeyType, Prk};
use rustls::crypto::CryptoProvider;
use rustls::crypto::aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256;
use rustls::crypto::aws_lc_rs::default_provider;
use types::node::PublicKey;

use super::cid;
use crate::{tls, varint};

/// The TLS secrets that a client logs, and so the keys of its server's packets.
#[derive(Debug, Default)]
pub(super) struct Log(Mutex<Vec<(String, Vec<u8>)>>);

impl Log {
    /// The config of a client that pins `peer`, has only the AES-128-GCM suite, and
    /// logs its secrets here.
    pub(super) fn client(
        self: &Arc<Self>,
        peer: PublicKey,
    ) -> Arc<rustls::ClientConfig> {
        let provider = CryptoProvider {
            cipher_suites: vec![TLS13_AES_128_GCM_SHA256],
            ..default_provider()
        };
        let mut config = (*tls::anonymous(provider, peer)).clone();
        config.key_log = Arc::<Self>::clone(self);
        Arc::new(config)
    }

    /// Each `RESET_STREAM` frame, in order, in the 1-RTT packets of `datagrams`, which
    /// the server of a [`Log::client`] sent to it.
    ///
    /// # Panics
    ///
    /// When the client has no 1-RTT secret, a packet is not QUIC v1 or does not
    /// decrypt, a long header packet is not Initial or Handshake, the keys update, or
    /// a frame type is unknown.
    pub(super) fn resets<'a>(
        &self,
        datagrams: impl IntoIterator<Item = &'a [u8]>,
    ) -> Vec<Reset> {
        let secret = self.get("SERVER_TRAFFIC_SECRET_0");
        let keys = Keys::new(Suite::Aes128Gcm, &secret);
        let (mut largest, mut resets) = (None, Vec::new());
        for datagram in datagrams {
            let mut at = 0;
            while at < datagram.len() && datagram[at] & LONG != 0 {
                at = past_long(datagram, at);
            }
            let Some(packet) = datagram.get(at..).filter(|packet| !packet.is_empty())
            else {
                continue;
            };
            let (number, frames) = keys.open(packet, 1 + cid::LEN, largest);
            largest = Some(largest.map_or(number, |largest: u64| largest.max(number)));
            resets.extend(resets_in(&frames));
        }
        resets
    }

    /// The secret with `label`, as `SSLKEYLOGFILE` names it.
    fn get(&self, label: &str) -> Vec<u8> {
        let secrets = self.0.lock().expect("not poisoned");
        let found = secrets.iter().find(|(other, _)| other == label);
        found
            .unwrap_or_else(|| panic!("no secret {label}"))
            .1
            .clone()
    }
}

impl rustls::KeyLog for Log {
    fn log(&self, label: &str, _: &[u8], secret: &[u8]) {
        let mut secrets = self.0.lock().expect("not poisoned");
        secrets.push((label.to_owned(), secret.to_vec()));
    }
}

/// A TLS 1.3 cipher suite with SHA-256.
#[derive(Clone, Copy, Debug)]
enum Suite {
    Aes128Gcm,
    ChaCha20Poly1305,
}

/// The keys that protect the packets of one side, at one level.
struct Keys {
    packet: LessSafeKey,
    iv: [u8; IV_LEN],
    header: HeaderProtectionKey,
}

const IV_LEN: usize = 12;
const SAMPLE_LEN: usize = 16;
/// The longest a packet number is on the wire.
const NUMBER_MAX: usize = 4;

impl Keys {
    /// The keys from the traffic `secret` of `suite`.
    fn new(suite: Suite, secret: &[u8]) -> Self {
        assert_eq!(secret.len(), 32, "a SHA-256 traffic secret");
        let (packet, header, len): (_, &Algorithm, _) = match suite {
            Suite::Aes128Gcm => (&AES_128_GCM, &AES_128, 16),
            Suite::ChaCha20Poly1305 => (&CHACHA20_POLY1305, &CHACHA20, 32),
        };
        let key = expand(secret, b"quic key", len);
        let packet = LessSafeKey::new(UnboundKey::new(packet, &key).expect("a key"));
        let iv = expand(secret, b"quic iv", IV_LEN)
            .try_into()
            .expect("12 bytes");
        let hp = expand(secret, b"quic hp", len);
        let header = HeaderProtectionKey::new(header, &hp).expect("a key");
        Self { packet, iv, header }
    }

    /// Removes the protection of `packet`, whose packet number starts at `at`, and
    /// gives its packet number and its frames. `largest` is the largest packet
    /// number opened before in its space.
    ///
    /// # Panics
    ///
    /// When the packet does not decrypt, or it has a short header and the keys
    /// update.
    fn open(&self, packet: &[u8], at: usize, largest: Option<u64>) -> (u64, Vec<u8>) {
        let mut packet = packet.to_vec();
        let sample = &packet[at + NUMBER_MAX..at + NUMBER_MAX + SAMPLE_LEN];
        let mask = self.header.new_mask(sample).expect("a mask");
        if packet[0] & LONG == 0 {
            packet[0] ^= mask[0] & 0x1f;
            assert_eq!(packet[0] & PHASE, 0, "the keys update");
        } else {
            packet[0] ^= mask[0] & 0x0f;
        }
        let len = usize::from(packet[0] & 3) + 1;
        let mut truncated = 0;
        for (byte, mask) in packet[at..at + len].iter_mut().zip(&mask[1..]) {
            *byte ^= mask;
            truncated = (truncated << 8) | u64::from(*byte);
        }
        let number = decode(largest, truncated, len);
        let mut nonce = self.iv;
        for (byte, number) in nonce[IV_LEN - 8..].iter_mut().zip(number.to_be_bytes()) {
            *byte ^= number;
        }
        let (header, payload) = packet.split_at_mut(at + len);
        let nonce = Nonce::assume_unique_for_key(nonce);
        let opened = self
            .packet
            .open_in_place(nonce, Aad::from(&*header), payload);
        let frames =
            opened.unwrap_or_else(|_| panic!("packet {number} does not decrypt"));
        (number, frames.to_vec())
    }
}

/// The bit of a long header.
const LONG: u8 = 0x80;
/// The key phase bit of a short header.
const PHASE: u8 = 0x04;

/// A `RESET_STREAM` frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Reset {
    pub(super) stream: u64,
    pub(super) code: u64,
}

/// The end of the Initial or Handshake packet at `at` in `datagram`.
fn past_long(datagram: &[u8], at: usize) -> usize {
    let version = &datagram[at + 1..at + 5];
    assert_eq!(version, [0, 0, 0, 1], "QUIC v1");
    let kind = (datagram[at] >> 4) & 3;
    let mut p = at + 5;
    p += 1 + usize::from(datagram[p]);
    p += 1 + usize::from(datagram[p]);
    match kind {
        0 => {
            let token = next(datagram, &mut p);
            p += usize::try_from(token).expect("fits");
        }
        2 => {}
        _ => panic!("a long header packet of type {kind}"),
    }
    let len = next(datagram, &mut p);
    p + usize::try_from(len).expect("fits")
}

const PADDING: u64 = 0x00;
const PING: u64 = 0x01;
const ACK: u64 = 0x02;
const ACK_ECN: u64 = 0x03;
const RESET_STREAM: u64 = 0x04;
const STOP_SENDING: u64 = 0x05;
const CRYPTO: u64 = 0x06;
const NEW_TOKEN: u64 = 0x07;
/// The first of the eight STREAM types. Bit 0x04 adds an offset and bit 0x02 a
/// length.
const STREAM: u64 = 0x08;
const MAX_DATA: u64 = 0x10;
const MAX_STREAM_DATA: u64 = 0x11;
const MAX_STREAMS_BIDI: u64 = 0x12;
const MAX_STREAMS_UNI: u64 = 0x13;
const DATA_BLOCKED: u64 = 0x14;
const STREAM_DATA_BLOCKED: u64 = 0x15;
const STREAMS_BLOCKED_BIDI: u64 = 0x16;
const STREAMS_BLOCKED_UNI: u64 = 0x17;
const NEW_CONNECTION_ID: u64 = 0x18;
const RETIRE_CONNECTION_ID: u64 = 0x19;
const PATH_CHALLENGE: u64 = 0x1a;
const PATH_RESPONSE: u64 = 0x1b;
const CONNECTION_CLOSE: u64 = 0x1c;
const CONNECTION_CLOSE_APPLICATION: u64 = 0x1d;
const HANDSHAKE_DONE: u64 = 0x1e;
/// Of the ACK frequency extension.
const IMMEDIATE_ACK: u64 = 0x1f;

/// The bytes of a stateless reset token.
const TOKEN_LEN: usize = 16;
/// The bytes of a path challenge or response.
const PATH_LEN: usize = 8;

/// Each `RESET_STREAM` frame in `frames`, in order.
fn resets_in(frames: &[u8]) -> Vec<Reset> {
    let (mut p, mut resets) = (0, Vec::new());
    let skip = |n, p: &mut usize| (0..n).for_each(|_| _ = next(frames, p));
    let bytes = |p: &mut usize| {
        let len = next(frames, p);
        *p += usize::try_from(len).expect("fits");
    };
    while p < frames.len() {
        match next(frames, &mut p) {
            PADDING | PING | HANDSHAKE_DONE | IMMEDIATE_ACK => {}
            kind @ (ACK | ACK_ECN) => {
                skip(2, &mut p);
                let ranges = next(frames, &mut p);
                skip(1 + 2 * ranges, &mut p);
                if kind == ACK_ECN {
                    skip(3, &mut p);
                }
            }
            RESET_STREAM => {
                let stream = next(frames, &mut p);
                let code = next(frames, &mut p);
                skip(1, &mut p);
                resets.push(Reset { stream, code });
            }
            STOP_SENDING | MAX_STREAM_DATA | STREAM_DATA_BLOCKED => skip(2, &mut p),
            CRYPTO => {
                skip(1, &mut p);
                bytes(&mut p);
            }
            NEW_TOKEN => bytes(&mut p),
            kind @ STREAM..=0x0f => {
                skip(1 + u64::from(kind & 0x04 != 0), &mut p);
                if kind & 0x02 == 0 {
                    p = frames.len();
                } else {
                    bytes(&mut p);
                }
            }
            MAX_DATA | MAX_STREAMS_BIDI | MAX_STREAMS_UNI | DATA_BLOCKED
            | STREAMS_BLOCKED_BIDI | STREAMS_BLOCKED_UNI | RETIRE_CONNECTION_ID => {
                skip(1, &mut p);
            }
            NEW_CONNECTION_ID => {
                skip(2, &mut p);
                p += 1 + usize::from(frames[p]) + TOKEN_LEN;
            }
            PATH_CHALLENGE | PATH_RESPONSE => p += PATH_LEN,
            kind @ (CONNECTION_CLOSE | CONNECTION_CLOSE_APPLICATION) => {
                skip(1 + u64::from(kind == CONNECTION_CLOSE), &mut p);
                bytes(&mut p);
            }
            kind => panic!("a frame of type {kind:#x}"),
        }
    }
    assert_eq!(p, frames.len(), "the frames end at the end of the packet");
    resets
}

/// The packet number of a `len` byte `truncated` number, in a space whose largest
/// number opened before is `largest`.
fn decode(largest: Option<u64>, truncated: u64, len: usize) -> u64 {
    let expected = largest.map_or(0, |largest| largest + 1);
    let window = 1 << (8 * len);
    let half = window / 2;
    let candidate = (expected & !(window - 1)) | truncated;
    if candidate + half <= expected && candidate < (1 << 62) - window {
        candidate + window
    } else if candidate > expected + half && candidate >= window {
        candidate - window
    } else {
        candidate
    }
}

/// HKDF-Expand-Label of TLS 1.3 with SHA-256 and no context.
fn expand(secret: &[u8], label: &[u8], len: usize) -> Vec<u8> {
    struct Len(usize);
    impl KeyType for Len {
        fn len(&self) -> usize {
            self.0
        }
    }
    let label = [b"tls13 ".as_slice(), label].concat();
    let info = [
        &u16::try_from(len).expect("fits").to_be_bytes()[..],
        &[u8::try_from(label.len()).expect("fits")],
        &label,
        &[0],
    ]
    .concat();
    let prk = Prk::new_less_safe(HKDF_SHA256, secret);
    let infos = [info.as_slice()];
    let okm = prk.expand(&infos, Len(len)).expect("expanded");
    let mut out = vec![0; len];
    okm.fill(&mut out).expect("filled");
    out
}

/// The QUIC varint at `at` in `bytes`. Moves `at` past it.
fn next(bytes: &[u8], at: &mut usize) -> u64 {
    let len = varint::len(bytes[*at]);
    let value = varint::value(&bytes[*at..*at + len]);
    *at += len;
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> =
            text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        let digits = digits
            .chunks(2)
            .map(|pair| std::str::from_utf8(pair).expect("ascii"));
        digits
            .map(|pair| u8::from_str_radix(pair, 16).expect("hex"))
            .collect()
    }

    // The vectors of RFC 9001, Appendix A.

    const INITIAL_SECRET: &str =
        "7db5df06e7a69e432496adedb00851923595221596ae2ae9fb8115c1e9ed0a44";
    const SERVER_INITIAL_SECRET: &str =
        "3c199828fd139efd216c155ad844cc81fb82fa8d7446fa7d78be803acdda951b";

    #[test]
    fn expand_gives_the_initial_secrets_and_keys() {
        let secret = hex(SERVER_INITIAL_SECRET);
        assert_eq!(expand(&hex(INITIAL_SECRET), b"server in", 32), secret);
        let key = hex("cf3a5331653c364c88f0f379b6067e37");
        assert_eq!(expand(&secret, b"quic key", 16), key);
        assert_eq!(
            expand(&secret, b"quic iv", 12),
            hex("0ac1493ca1905853b0bba03e")
        );
        let hp = hex("c206b8d9b9f0f37644430b490eeaa314");
        assert_eq!(expand(&secret, b"quic hp", 16), hp);
    }

    #[test]
    fn open_gives_the_server_initial() {
        let packet = hex(
            "cf000000010008f067a5502a4262b5004075c0d95a482cd0991cd25b0aac406a
             5816b6394100f37a1c69797554780bb38cc5a99f5ede4cf73c3ec2493a1839b3
             dbcba3f6ea46c5b7684df3548e7ddeb9c3bf9c73cc3f3bded74b562bfb19fb84
             022f8ef4cdd93795d77d06edbb7aaf2f58891850abbdca3d20398c276456cbc4
             2158407dd074ee",
        );
        let keys = Keys::new(Suite::Aes128Gcm, &hex(SERVER_INITIAL_SECRET));
        let frames = hex(
            "02000000000600405a020000560303eefce7f7b37ba1d1632e96677825ddf739
             88cfc79825df566dc5430b9a045a1200130100002e00330024001d00209d3c94
             0d89690b84d08a60993c144eca684d1081287c834d5311bcf32bb9da1a002b00
             020304",
        );
        assert_eq!(keys.open(&packet, 18, None), (1, frames.clone()));
        assert_eq!(past_long(&packet, 0), packet.len());
        // An ACK and a CRYPTO frame, and no reset.
        assert_eq!(resets_in(&frames), []);
    }

    #[test]
    fn open_gives_the_chacha20_short_header_packet() {
        let secret = "9ac312a7f877468ebe69422748ad00a15443f18203a07d6060f688f30f21632b";
        let keys = Keys::new(Suite::ChaCha20Poly1305, &hex(secret));
        let packet = hex("4cfe4189655e5cd55c41f69080575d7999c25a5bfb");
        let opened = keys.open(&packet, 1, Some(654_360_563));
        assert_eq!(opened, (654_360_564, vec![0x01]));
    }

    #[test]
    #[should_panic = "the keys update"]
    fn open_panics_on_a_key_update() {
        let secret = "9ac312a7f877468ebe69422748ad00a15443f18203a07d6060f688f30f21632b";
        let keys = Keys::new(Suite::ChaCha20Poly1305, &hex(secret));
        // The key phase bit is under header protection.
        let packet = hex("48fe4189655e5cd55c41f69080575d7999c25a5bfb");
        keys.open(&packet, 1, Some(654_360_563));
    }

    #[test]
    #[should_panic = "a SHA-256 traffic secret"]
    fn keys_panic_on_a_secret_of_another_hash() {
        Keys::new(Suite::Aes128Gcm, &[0; 48]);
    }

    #[test]
    #[should_panic = "QUIC v1"]
    fn past_long_panics_on_another_version() {
        past_long(&[0xc0, 0, 0, 0, 2, 0, 0, 0, 0], 0);
    }

    #[test]
    fn past_long_panics_on_0_rtt_and_retry() {
        for (first, kind) in [(0xd0, 1), (0xf0, 3)] {
            let packet = [first, 0, 0, 0, 1, 0, 0, 0];
            let panicked = std::panic::catch_unwind(|| past_long(&packet, 0));
            let message = panicked.expect_err("a panic");
            let message = message.downcast_ref::<String>().expect("a message");
            assert_eq!(*message, format!("a long header packet of type {kind}"));
        }
    }

    #[test]
    #[should_panic = "does not decrypt"]
    fn open_panics_on_a_packet_it_cannot_decrypt() {
        let mut packet = hex("4cfe4189655e5cd55c41f69080575d7999c25a5bfb");
        packet[20] ^= 1;
        let keys = Keys::new(Suite::ChaCha20Poly1305, &[0; 32]);
        keys.open(&packet, 1, None);
    }

    #[test]
    fn decode_gives_the_full_packet_number() {
        // RFC 9000, Appendix A.3.
        assert_eq!(decode(Some(0xa82f_30ea), 0x9b32, 2), 0xa82f_9b32);
        assert_eq!(decode(None, 0, 1), 0);
        assert_eq!(decode(Some(0xff), 0x01, 1), 0x101);
        assert_eq!(decode(Some(0x101), 0xff, 1), 0xff);
        assert_eq!(decode(Some(0xfe), 0x00, 1), 0x100);
        assert_eq!(decode(Some(0x17f), 0x00, 1), 0x200);
        assert_eq!(decode(Some(0x17e), 0x00, 1), 0x100);
        assert_eq!(decode(Some(0xff), 0x80, 1), 0x180);
        assert_eq!(decode(Some((1 << 62) - 0x81), 0x00, 1), (1 << 62) - 0x100);
    }

    #[test]
    fn resets_in_reads_past_each_frame_type() {
        let frames: [&[u8]; 29] = [
            &[0x00],
            &[0x01],
            &[0x02, 0x0a, 0x00, 0x00, 0x01],
            &[0x02, 0x0a, 0x00, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00],
            &[0x03, 0x0a, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x02, 0x03],
            &[0x05, 0x04, 0x09],
            &[0x06, 0x40, 0x40, 0x02, 0xaa, 0xbb],
            &[0x07, 0x02, 0xaa, 0xbb],
            &[0x0a, 0x01, 0x02, 0xaa, 0xbb],
            &[0x0b, 0x01, 0x01, 0xaa],
            &[0x0e, 0x01, 0x05, 0x01, 0xab],
            &[0x0f, 0x01, 0x40, 0x05, 0x01, 0xab],
            &[0x10, 0x41, 0x00],
            &[0x11, 0x01, 0x41, 0x00],
            &[0x12, 0x05],
            &[0x13, 0x05],
            &[0x14, 0x41, 0x00],
            &[0x15, 0x01, 0x05],
            &[0x16, 0x05],
            &[0x17, 0x05],
            &[
                0x18, 0x01, 0x00, 0x08, 1, 2, 3, 4, 5, 6, 7, 8, 1, 2, 3, 4, 5, 6, 7, 8,
                9, 10, 11, 12, 13, 14, 15, 16,
            ],
            &[0x19, 0x01],
            &[0x1a, 1, 2, 3, 4, 5, 6, 7, 8],
            &[0x1b, 1, 2, 3, 4, 5, 6, 7, 8],
            &[0x1c, 0x01, 0x08, 0x02, 0xaa, 0xbb],
            &[0x1d, 0x01, 0x02, 0xaa, 0xbb],
            &[0x1e],
            &[0x1f],
            &[0x04, 0x01, 0x05, 0x00],
        ];
        let mut packet = Vec::new();
        for (index, frame) in (0_u8..).zip(frames) {
            packet.extend_from_slice(frame);
            packet.extend_from_slice(&[0x04, index, 0x40, index, 0x41, 0x00]);
        }
        let mut expected: Vec<Reset> = (0..29)
            .map(|index| Reset {
                stream: index,
                code: index,
            })
            .collect();
        expected.insert(28, Reset { stream: 1, code: 5 });
        assert_eq!(resets_in(&packet), expected);
    }

    #[test]
    fn resets_in_takes_a_stream_frame_with_no_length_to_the_end() {
        let frames = [0x04, 0x09, 0x1e, 0x00, 0x09, 0x01, 0x04, 0x01, 0x00, 0x00];
        let reset = Reset {
            stream: 9,
            code: 30,
        };
        assert_eq!(resets_in(&frames), [reset]);
    }

    #[test]
    #[should_panic = "the frames end at the end of the packet"]
    fn resets_in_panics_on_a_frame_past_the_end() {
        resets_in(&[0x07, 0x05, 0xaa]);
    }

    #[test]
    #[should_panic = "a frame of type 0x40"]
    fn resets_in_panics_on_an_unknown_frame() {
        resets_in(&[0x40, 0x40]);
    }
}

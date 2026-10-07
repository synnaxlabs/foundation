//! A reader of the QUIC v1 packets that a node sends, for tests that check the frames
//! on the wire. It panics on each packet or frame that it cannot read.

use std::sync::Mutex;

use aws_lc_rs::aead::quic::{AES_128, Algorithm, CHACHA20, HeaderProtectionKey};
use aws_lc_rs::aead::{
    AES_128_GCM, Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey,
};
use aws_lc_rs::hkdf::{HKDF_SHA256, KeyType, Prk};

use super::cid;

/// The TLS secrets of a connection, from its key log.
#[derive(Debug, Default)]
pub(super) struct Secrets(Mutex<Vec<(String, Vec<u8>)>>);

impl Secrets {
    /// The secret with `label`, as `SSLKEYLOGFILE` names it.
    ///
    /// # Panics
    ///
    /// When the log has no secret with `label`.
    pub(super) fn get(&self, label: &str) -> Vec<u8> {
        let secrets = self.0.lock().expect("not poisoned");
        let found = secrets.iter().find(|(other, _)| other == label);
        found
            .unwrap_or_else(|| panic!("no secret {label}"))
            .1
            .clone()
    }
}

impl rustls::KeyLog for Secrets {
    fn log(&self, label: &str, _: &[u8], secret: &[u8]) {
        let mut secrets = self.0.lock().expect("not poisoned");
        secrets.push((label.to_owned(), secret.to_vec()));
    }
}

/// A TLS 1.3 cipher suite with SHA-256.
#[derive(Clone, Copy, Debug)]
pub(super) enum Suite {
    Aes128Gcm,
    ChaCha20Poly1305,
}

/// The keys that protect the packets of one side, at one level.
pub(super) struct Keys {
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
    pub(super) fn new(suite: Suite, secret: &[u8]) -> Self {
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
    pub(super) fn open(
        &self,
        packet: &[u8],
        at: usize,
        largest: Option<u64>,
    ) -> (u64, Vec<u8>) {
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

/// Each `RESET_STREAM` frame in the 1-RTT packets of `datagrams`, in order. Each
/// datagram is one that a node sent under `keys`.
///
/// # Panics
///
/// When a packet is not QUIC v1 or does not decrypt, a long header packet is not
/// Initial or Handshake, the keys update, or a frame type is unknown.
pub(super) fn resets(datagrams: &[Vec<u8>], keys: &Keys) -> Vec<Reset> {
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
        walk(&frames, &mut resets);
    }
    resets
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
            let token = varint(datagram, &mut p);
            p += usize::try_from(token).expect("fits");
        }
        2 => {}
        _ => panic!("a long header packet of type {kind}"),
    }
    let len = varint(datagram, &mut p);
    p + usize::try_from(len).expect("fits")
}

/// Adds each `RESET_STREAM` in `frames` to `resets`.
fn walk(frames: &[u8], resets: &mut Vec<Reset>) {
    let mut p = 0;
    let skip = |n, p: &mut usize| (0..n).for_each(|_| _ = varint(frames, p));
    let bytes = |p: &mut usize| {
        let len = varint(frames, p);
        *p += usize::try_from(len).expect("fits");
    };
    while p < frames.len() {
        match varint(frames, &mut p) {
            // PADDING, PING, HANDSHAKE_DONE, and IMMEDIATE_ACK of the ACK frequency
            // extension.
            0x00 | 0x01 | 0x1e | 0x1f => {}
            kind @ (0x02 | 0x03) => {
                skip(2, &mut p);
                let ranges = varint(frames, &mut p);
                skip(1 + 2 * ranges, &mut p);
                if kind == 0x03 {
                    skip(3, &mut p);
                }
            }
            0x04 => {
                let stream = varint(frames, &mut p);
                let code = varint(frames, &mut p);
                skip(1, &mut p);
                resets.push(Reset { stream, code });
            }
            // STOP_SENDING, MAX_STREAM_DATA, STREAM_DATA_BLOCKED.
            0x05 | 0x11 | 0x15 => skip(2, &mut p),
            0x06 => {
                skip(1, &mut p);
                bytes(&mut p);
            }
            0x07 => bytes(&mut p),
            kind @ 0x08..=0x0f => {
                skip(1 + u64::from(kind & 0x04 != 0), &mut p);
                if kind & 0x02 == 0 {
                    p = frames.len();
                } else {
                    bytes(&mut p);
                }
            }
            // MAX_DATA, MAX_STREAMS, DATA_BLOCKED, STREAMS_BLOCKED,
            // RETIRE_CONNECTION_ID.
            0x10 | 0x12 | 0x13 | 0x14 | 0x16 | 0x17 | 0x19 => skip(1, &mut p),
            0x18 => {
                skip(2, &mut p);
                p += 1 + usize::from(frames[p]) + 16;
            }
            0x1a | 0x1b => p += 8,
            kind @ (0x1c | 0x1d) => {
                skip(1 + u64::from(kind == 0x1c), &mut p);
                bytes(&mut p);
            }
            kind => panic!("a frame of type {kind:#x}"),
        }
    }
    assert_eq!(p, frames.len(), "the frames end at the end of the packet");
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
fn varint(bytes: &[u8], at: &mut usize) -> u64 {
    let len = 1 << (bytes[*at] >> 6);
    let mut value = u64::from(bytes[*at] & 0x3f);
    for byte in &bytes[*at + 1..*at + len] {
        value = (value << 8) | u64::from(*byte);
    }
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
        assert_eq!(keys.open(&packet, 18, None), (1, frames));
        assert_eq!(past_long(&packet, 0), packet.len());
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
    }

    #[test]
    fn walk_gives_each_reset() {
        // PING, ACK, RESET_STREAM, two STREAM frames with a length, RESET_STREAM,
        // HANDSHAKE_DONE, PADDING, and a STREAM frame to the end.
        let frames = hex(
            "01 0201000000 0409401e05 0a0102abcd 0e010501ab 0401414400 1e 0000
             0901abab",
        );
        let mut resets = Vec::new();
        walk(&frames, &mut resets);
        let codes = [
            Reset {
                stream: 9,
                code: 30,
            },
            Reset {
                stream: 1,
                code: 0x144,
            },
        ];
        assert_eq!(resets, codes);
    }

    #[test]
    #[should_panic = "a frame of type 0x30"]
    fn walk_panics_on_an_unknown_frame() {
        walk(&[0x30, 0x00], &mut Vec::new());
    }
}

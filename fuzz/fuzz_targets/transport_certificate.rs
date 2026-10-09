//! `transport::fuzzing::peer` never panics, and gives the peer that the TLS rules of
//! a node's server give for a dialer's certificate chain. A certificate that a node
//! issues reads back to its key.
//!
//! The input is a head byte, then the chain. The low two bits of the head are the
//! number of certificates. Each certificate is a 2-byte big-endian length and that
//! many bytes, cut at the end of the input. The last 32 bytes, when there are as
//! many, are a private key.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;
use transport::Peer;
use transport::fuzzing::{certificate, peer};
use types::ed25519::PrivateKey;

/// A `SubjectPublicKeyInfo` up to its 32-byte Ed25519 key.
const SPKI: &[u8] = b"\x30\x2a\x30\x05\x06\x03\x2b\x65\x70\x03\x21\x00";

/// The first `len` bytes of `bytes`, or all of them, and the rest.
fn take(bytes: &[u8], len: usize) -> (&[u8], &[u8]) {
    bytes.split_at(len.min(bytes.len()))
}

/// The chain in `bytes`.
fn read(bytes: &[u8]) -> Vec<&[u8]> {
    let Some((&head, mut rest)) = bytes.split_first() else {
        return Vec::new();
    };
    let mut chain = Vec::new();
    for _ in 0..head & 3 {
        let (len, after) = take(rest, 2);
        let len = len
            .iter()
            .fold(0, |len, &byte| len << 8 | usize::from(byte));
        let (der, after) = take(after, len);
        chain.push(der);
        rest = after;
    }
    chain
}

/// Whether `der` holds `SPKI` followed by `key`.
fn carries(der: &[u8], key: &[u8; 32]) -> bool {
    let spki = [SPKI, key].concat();
    der.windows(spki.len()).any(|window| window == spki)
}

fuzz_target!(|bytes: &[u8]| {
    let chain = read(bytes);
    let got = peer(&chain);
    match chain.as_slice() {
        [] => assert_eq!(got, Some(Peer::Client), "no certificate, no client"),
        [der] if der.len() <= 1024 => match got {
            None => {}
            Some(Peer::Node(key)) => {
                assert!(carries(der, &key.to_bytes()), "a key the certificate lacks");
            }
            Some(Peer::Client) => panic!("a certificate gave a client"),
        },
        _ => assert_eq!(got, None, "a peer from a long chain or certificate"),
    }
    if let Some(key) = bytes.last_chunk::<32>() {
        let private_key = PrivateKey(*key);
        let der = certificate(&private_key);
        let node = Some(Peer::Node(private_key.public()));
        assert_eq!(peer(&[&der]), node, "the issued key changed");
        assert_eq!(peer(&[&der, &der]), None, "a chain of two passed");
    }
});

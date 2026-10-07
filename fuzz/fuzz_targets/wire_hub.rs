//! `wire::hub` never panics on a decode, and what it reads encodes to the same bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use wire::hub::{Credit, Open, Reply, ends, keys};

fuzz_target!(|bytes: &[u8]| {
    if let Ok(open) = Open::decode(bytes) {
        let mut out = vec![0; open.encoded_len()];
        open.encode(&mut out);
        assert_eq!(out, bytes, "the open changed");
    }
    if let Ok(credit) = Credit::decode(bytes) {
        let mut out = [0; Credit::LEN];
        credit.encode(&mut out);
        assert_eq!(out, bytes, "the credit changed");
    }
    if let Ok(reply) = Reply::decode(bytes) {
        let mut out = vec![0; reply.encoded_len()];
        reply.encode(&mut out);
        assert_eq!(out, bytes, "the reply changed");
    }
    if let Ok(decoded) = keys::decode(bytes) {
        let decoded: Vec<_> = decoded.collect();
        let mut out = vec![0; bytes.len()];
        keys::encode(&decoded, &mut out);
        assert_eq!(out, bytes, "the keys changed");
    }
    if let Ok(decoded) = ends::decode(bytes) {
        let mut out = vec![0; bytes.len()];
        ends::encode(decoded, &mut out);
        assert_eq!(out, bytes, "the ends changed");
    }
});

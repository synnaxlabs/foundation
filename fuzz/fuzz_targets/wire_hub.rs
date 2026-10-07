//! `wire::hub` never panics on a decode, and what it reads encodes to the same bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use wire::hub::{FromHome, FromReader, ends, keys};

fuzz_target!(|bytes: &[u8]| {
    if let Ok(message) = FromReader::decode(bytes) {
        let mut out = vec![0; message.encoded_len()];
        message.encode(&mut out);
        assert_eq!(out, bytes, "the message from the reader changed");
    }
    if let Ok(message) = FromHome::decode(bytes) {
        let mut out = vec![0; message.encoded_len()];
        message.encode(&mut out);
        assert_eq!(out, bytes, "the message from the home changed");
    }
    let (keys_run, _) = bytes.as_chunks::<16>();
    let keys_run = keys_run.as_flattened();
    let keys: Vec<_> = keys::decode(keys_run).collect();
    let mut out = vec![0; keys_run.len()];
    keys::encode(&keys, &mut out);
    assert_eq!(out, keys_run, "the keys changed");
    let (ends_run, _) = bytes.as_chunks::<8>();
    let ends_run = ends_run.as_flattened();
    let mut out = vec![0; ends_run.len()];
    ends::encode(ends::decode(ends_run), &mut out);
    assert_eq!(out, ends_run, "the ends changed");
});

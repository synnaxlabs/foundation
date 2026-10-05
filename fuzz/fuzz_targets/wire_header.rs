//! `wire::header::decode` never panics, and a header it reads encodes to the same
//! bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use wire::header;

fuzz_target!(|bytes: &[u8]| {
    let Ok((protocol, rest)) = header::decode(bytes) else {
        return;
    };
    let (head, tail) = bytes.split_at(header::LEN);
    assert_eq!(header::encode(protocol), head, "the header changed");
    assert_eq!(rest, tail, "the bytes after the header changed");
});

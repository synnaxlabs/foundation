//! `transport::fuzzing::Hello::decode` never panics, gives the hello that a second
//! reader gives, and a hello it reads decodes from its own encoding.

#![no_main]

use libfuzzer_sys::fuzz_target;
use transport::fuzzing::Hello;

/// The QUIC varint at the start of `bytes`, and the bytes after it.
fn varint(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let len = 1 << (bytes.first()? >> 6);
    let (head, rest) = bytes.split_at_checked(len)?;
    let value = head[1..]
        .iter()
        .fold(u64::from(head[0] & 0x3f), |value, &byte| value << 8 | u64::from(byte));
    Some((value, rest))
}

/// The hello in `bytes` by the STREAM WIRE rules, written apart from `decode`.
fn read(bytes: &[u8]) -> Option<Hello> {
    if bytes.len() > 256 {
        return None;
    }
    let (mut rest, mut last, mut window, mut message) = (bytes, None, None, None);
    while !rest.is_empty() {
        let (id, after) = varint(rest)?;
        let (value, after) = varint(after)?;
        rest = after;
        if last >= Some(id) {
            return None;
        }
        last = Some(id);
        let value = usize::try_from(value).unwrap_or(usize::MAX);
        match id {
            0 => window = Some(value),
            1 => message = Some(value),
            _ => {}
        }
    }
    let hello = Hello { window_bytes: window?, message_bytes_max: message? };
    let limits = hello.window_bytes >= hello.message_bytes_max
        && hello.message_bytes_max >= 1472;
    limits.then_some(hello)
}

fuzz_target!(|bytes: &[u8]| {
    let hello = Hello::decode(bytes).ok();
    assert_eq!(hello, read(bytes), "the readers disagree");
    if let Some(hello) = hello {
        assert_eq!(Hello::decode(&hello.encode()), Ok(hello), "the hello changed");
    }
});

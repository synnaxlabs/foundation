//! `transport::fuzzing::Hello::decode` never panics. A hello it reads agrees with a
//! second reader, keeps the limit order, and decodes from its own encoding.

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

fuzz_target!(|bytes: &[u8]| {
    let Ok(hello) = Hello::decode(bytes) else {
        return;
    };
    assert!(bytes.len() <= 256, "a hello over 256 bytes");
    let (mut rest, mut last, mut window, mut message) = (bytes, None, None, None);
    while !rest.is_empty() {
        let (id, after) = varint(rest).expect("a whole id");
        let (value, after) = varint(after).expect("a whole value");
        rest = after;
        assert!(last < Some(id), "id {id} after id {last:?}");
        last = Some(id);
        let value = usize::try_from(value).unwrap_or(usize::MAX);
        match id {
            0 => window = Some(value),
            1 => message = Some(value),
            _ => {}
        }
    }
    assert_eq!(window, Some(hello.window_bytes), "another window_bytes");
    assert_eq!(message, Some(hello.message_bytes_max), "another message_bytes_max");
    assert!(hello.message_bytes_max >= 1472, "a message_bytes_max below 1472");
    assert!(
        hello.window_bytes >= hello.message_bytes_max,
        "a window_bytes below the message_bytes_max"
    );
    assert_eq!(Hello::decode(&hello.encode()), Ok(hello), "the hello changed");
});

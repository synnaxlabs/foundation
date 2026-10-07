//! `wire::hub` never panics on a decode, and an open or message it reads encodes to
//! the same bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use wire::hub::{Message, Open};

fuzz_target!(|bytes: &[u8]| {
    if let Ok(open) = Open::decode(bytes) {
        let mut out = vec![0; open.encoded_len()];
        open.encode(&mut out);
        assert_eq!(out, bytes, "the open changed");
    }
    if let Ok(message) = Message::decode(bytes) {
        let mut out = vec![0; message.encoded_len()];
        message.encode(&mut out);
        assert_eq!(out, bytes, "the message changed");
        if let Message::Frame(head) = message {
            assert_eq!(head.body_len(), head.ends().last().map_or(0, |(_, end)| end));
        }
    }
});

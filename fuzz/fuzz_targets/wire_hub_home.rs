//! `wire::hub::Home` never panics, and each event encodes to its message.
//!
//! Input: the messages from the reader's node (`fuzz::messages`).

#![no_main]

use libfuzzer_sys::fuzz_target;
use wire::hub::{Credit, FromReader, Home, keys};

fuzz_target!(|bytes: &[u8]| {
    let mut home = Home::default();
    for message in fuzz::messages(bytes) {
        match home.decode(message) {
            Ok(FromReader::Open(open)) => {
                let mut out = vec![0; open.encoded_len()];
                open.encode(&mut out);
                assert_eq!(out, message, "the open changed");
            }
            Ok(FromReader::Keys { keys, .. }) => {
                let keys: Vec<_> = keys.collect();
                let mut out = vec![0; message.len()];
                keys::encode(&keys, &mut out);
                assert_eq!(out, message, "the keys changed");
            }
            Ok(FromReader::Credit(credit)) => {
                let mut out = [0; Credit::LEN];
                credit.encode(&mut out);
                assert_eq!(out, message, "the credit changed");
            }
            Err(_) => {}
        }
    }
});

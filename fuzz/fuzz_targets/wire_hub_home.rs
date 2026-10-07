//! `wire::hub::Home` never panics, each event encodes to its message, and each valid
//! message that the reader's node writes reads back.
//!
//! Input: the messages from the reader's node (`fuzz::messages`).

#![no_main]

use libfuzzer_sys::{
    arbitrary::{self, Unstructured},
    fuzz_target,
};
use types::channel;
use wire::hub::{Credit, FromReader, Home, Mode, Open, keys};

/// The most keys of a written run.
const RUN_MAX: u32 = 4;

/// Each event of the session in `bytes` must encode to its message.
fn read(bytes: &[u8]) {
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
}

/// Each valid message made from `input` must decode to itself. A count of 0 is not
/// valid, so it becomes 1: an input that ends early writes the smallest messages.
fn write(input: &mut Unstructured) -> arbitrary::Result<()> {
    let limit_bytes = input.arbitrary()?;
    let channels = input.arbitrary::<u32>()?.max(1);
    for mode in [Mode::Latest, Mode::Complete { limit_bytes }] {
        let open = Open { mode, channels };
        let mut out = vec![0; open.encoded_len()];
        open.encode(&mut out);
        match Home::default().decode(&out) {
            Ok(FromReader::Open(decoded)) => {
                assert_eq!(decoded, open, "the open changed")
            }
            other => panic!("an open did not read back: {other:?}"),
        }
    }

    let channels = input.int_in_range(1..=RUN_MAX)?;
    let open = Open {
        mode: Mode::Latest,
        channels,
    };
    let mut out = vec![0; open.encoded_len()];
    open.encode(&mut out);
    let mut home = Home::default();
    home.decode(&out).expect("the open decodes");
    let written = (0..channels)
        .map(|_| input.arbitrary().map(channel::Key::from_u128))
        .collect::<arbitrary::Result<Vec<_>>>()?;
    let mut out = vec![0; written.len() * keys::LEN];
    keys::encode(&written, &mut out);
    match home.decode(&out) {
        Ok(FromReader::Keys { keys, last: true }) => {
            assert_eq!(keys.collect::<Vec<_>>(), written, "the keys changed");
        }
        other => panic!("the keys did not read back: {other:?}"),
    }

    let credit = Credit { limit_bytes };
    let mut out = [0; Credit::LEN];
    credit.encode(&mut out);
    match home.decode(&out) {
        Ok(FromReader::Credit(decoded)) => {
            assert_eq!(decoded, credit, "the credit changed")
        }
        other => panic!("a credit did not read back: {other:?}"),
    }
    Ok(())
}

fuzz_target!(|bytes: &[u8]| {
    read(bytes);
    write(&mut Unstructured::new(bytes)).expect("an input that ends gives zeros");
});

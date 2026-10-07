//! `wire::hub::Home` never panics, each event encodes to its message and comes in the
//! order of a session, each refusal is one that the order gives, and each valid
//! message that the reader's node writes reads back.
//!
//! Input: the messages from the reader's node (`fuzz::messages`).

#![no_main]

use libfuzzer_sys::{
    arbitrary::{self, Unstructured},
    fuzz_target,
};
use types::channel;
use wire::hub::{Credit, Error, FromReader, Home, Mode, Open, keys};

/// The most keys of a written run.
const RUN_MAX: u32 = 4;

/// The kind bytes of an open: one for each mode.
const OPENS: [u8; 2] = [1, 2];

/// The kind byte of a credit.
const CREDIT: u8 = 3;

/// What a home must take next, kept apart from the home.
#[derive(Clone, Copy, Debug)]
enum Next {
    Open,
    /// `remain` keys of the run are still to come.
    Keys {
        remain: u32,
    },
    Credit,
}

/// Whether `error` says only that the bytes of a message are no open and no credit.
fn malformed(error: Error) -> bool {
    matches!(
        error,
        Error::Empty | Error::Kind { .. } | Error::Length { .. } | Error::Channels
    )
}

/// Whether a home that must take `next` refuses `message` with `error`. For a message
/// of a run, only one error is correct.
fn refused(next: Next, message: &[u8], error: Error) -> bool {
    match next {
        Next::Open => match error {
            Error::Unopened { kind } => {
                kind == CREDIT && message.first() == Some(&CREDIT)
            }
            error => malformed(error),
        },
        Next::Keys { remain } => fuzz::run_refused(message, keys::LEN, remain, error),
        Next::Credit => match error {
            Error::Reopen { kind } => {
                OPENS.contains(&kind) && message.first() == Some(&kind)
            }
            error => malformed(error),
        },
    }
}

/// Each event of the session in `bytes` must encode to its message and come in the
/// order of a session, and each refusal must be the one that the order gives.
fn read(bytes: &[u8]) {
    let mut home = Home::default();
    let mut next = Next::Open;
    for message in fuzz::messages(bytes) {
        next = match (next, home.decode(message)) {
            (Next::Open, Ok(FromReader::Open(open))) => {
                let mut out = vec![0; open.encoded_len()];
                open.encode(&mut out);
                assert_eq!(out, message, "the open changed");
                Next::Keys {
                    remain: open.channels,
                }
            }
            (Next::Keys { remain }, Ok(FromReader::Keys { keys, last })) => {
                let keys: Vec<_> = keys.collect();
                let mut out = vec![0; message.len()];
                keys::encode(&keys, &mut out);
                assert_eq!(out, message, "the keys changed");
                assert!(!keys.is_empty(), "a run message has no key");
                let remain = u32::try_from(keys.len())
                    .ok()
                    .and_then(|count| remain.checked_sub(count))
                    .expect("a run message has more keys than remain");
                assert_eq!(last, remain == 0, "the run ends at another message");
                if last {
                    Next::Credit
                } else {
                    Next::Keys { remain }
                }
            }
            (Next::Credit, Ok(FromReader::Credit(credit))) => {
                let mut out = [0; Credit::LEN];
                credit.encode(&mut out);
                assert_eq!(out, message, "the credit changed");
                Next::Credit
            }
            (next, Err(error)) => {
                assert!(
                    refused(next, message, error),
                    "{error:?} is not the refusal of {message:?} for {next:?}"
                );
                next
            }
            (next, Ok(event)) => panic!("{event:?} came, not {next:?}"),
        };
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

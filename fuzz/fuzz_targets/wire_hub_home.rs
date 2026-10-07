//! `wire::hub::Home` never panics, each event encodes to its message and comes in the
//! order of a session, each refusal is one that the order and the mode give, and each
//! valid message that the reader's node writes reads back.
//!
//! Input: the messages from the reader's node (`fuzz::hub::messages`).

#![no_main]

use fuzz::hub::Run;
use libfuzzer_sys::{
    arbitrary::{self, Unstructured},
    fuzz_target,
};
use types::channel;
use wire::hub::{Credit, Error, FromReader, Home, Mode, Open, keys};

/// The most keys of a written run.
const RUN_MAX: u32 = 4;

/// What a home must take next, kept apart from the home. `latest` holds for a
/// latest session, which takes no credit.
#[derive(Clone, Copy, Debug)]
enum Next {
    Open,
    Keys { run: Run, latest: bool },
    Credit { latest: bool },
}

/// A home that decoded a complete open of one channel and its key, so a credit is in
/// order.
fn keyed() -> Home {
    let open = Open {
        mode: Mode::Complete { limit_bytes: 0 },
        channels: 1,
    };
    let mut out = vec![0; open.encoded_len()];
    open.encode(&mut out);
    let mut home = Home::default();
    home.decode(&out).expect("the open decodes");
    let mut out = [0; keys::LEN];
    keys::encode(&[channel::Key::from_u128(0)], &mut out);
    match home.decode(&out) {
        Ok(FromReader::Keys { last: true, .. }) => home,
        other => panic!("a key did not read back: {other:?}"),
    }
}

/// The open or the credit in `message`, read where its kind is in order: an open by a
/// home that decoded nothing, and a credit by a home that has its keys.
fn alone(message: &[u8]) -> Result<FromReader<'_>, Error> {
    match Home::default().decode(message) {
        Ok(open) => Ok(open),
        Err(_) => keyed().decode(message),
    }
}

/// The kind byte that the encoder writes for `open`.
fn open_kind(open: Open) -> u8 {
    let mut out = vec![0; open.encoded_len()];
    open.encode(&mut out);
    out[0]
}

/// The kind byte that the encoder writes for `credit`.
fn credit_kind(credit: Credit) -> u8 {
    let mut out = [0; Credit::LEN];
    credit.encode(&mut out);
    out[0]
}

/// Whether `error` says only that the bytes of a message are no open and no credit.
fn malformed(error: Error) -> bool {
    matches!(
        error,
        Error::Empty | Error::Kind { .. } | Error::Length { .. } | Error::Channels
    )
}

/// Whether a home that must take `next` refuses `message` with `error`. Only one
/// error is correct.
fn refused(next: Next, message: &[u8], error: Error) -> bool {
    match next {
        Next::Open => match alone(message) {
            Ok(FromReader::Credit(credit)) => {
                let kind = credit_kind(credit);
                error == Error::Unopened { kind }
            }
            Ok(_) => false,
            Err(other) => malformed(other) && error == other,
        },
        Next::Keys { run, .. } => run.refused(message, error),
        Next::Credit { latest } => match alone(message) {
            Ok(FromReader::Open(open)) => {
                let kind = open_kind(open);
                error == Error::Reopen { kind }
            }
            Ok(FromReader::Credit(credit)) => {
                let kind = credit_kind(credit);
                latest && error == Error::Latest { kind }
            }
            Ok(_) => false,
            Err(other) => malformed(other) && error == other,
        },
    }
}

/// Each event of the session in `bytes` must encode to its message and come in the
/// order of a session, and each refusal must be the one that the order gives.
fn read(bytes: &[u8]) {
    let mut home = Home::default();
    let mut next = Next::Open;
    for message in fuzz::hub::messages(bytes) {
        next = match (next, home.decode(message)) {
            (Next::Open, Ok(FromReader::Open(open))) => {
                let mut out = vec![0; open.encoded_len()];
                open.encode(&mut out);
                assert_eq!(out, message, "the open changed");
                Next::Keys {
                    run: Run::new(keys::LEN, open.channels),
                    latest: open.mode == Mode::Latest,
                }
            }
            (Next::Keys { run, latest }, Ok(FromReader::Keys { keys, last })) => {
                let keys: Vec<_> = keys.collect();
                let mut out = vec![0; message.len()];
                keys::encode(&keys, &mut out);
                assert_eq!(out, message, "the keys changed");
                run.take(keys.len(), last)
                    .map_or(Next::Credit { latest }, |run| Next::Keys { run, latest })
            }
            (Next::Credit { latest: false }, Ok(FromReader::Credit(credit))) => {
                let mut out = [0; Credit::LEN];
                credit.encode(&mut out);
                assert_eq!(out, message, "the credit changed");
                Next::Credit { latest: false }
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
        mode: Mode::Complete { limit_bytes },
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

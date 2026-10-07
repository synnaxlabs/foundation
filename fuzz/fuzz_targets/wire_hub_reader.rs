//! `wire::hub::Reader` never panics, each event encodes to its message and comes in
//! the order of a session, each refusal is one that the order gives, the body starts
//! after the ends run and ends at its last end, and each valid message that the home
//! writes reads back.
//!
//! Input: one byte, the places of the session less 1, then the messages from the home
//! (`fuzz::messages`).

#![no_main]

use libfuzzer_sys::{
    arbitrary::{self, Unstructured},
    fuzz_target,
};
use types::frame::{Path, Range};
use wire::hub::{Error, FromHome, Head, Mode, Open, Reader, Reply, ends};

/// The most ends of a written run.
const RUN_MAX: u32 = 4;

/// The kind byte of an opened.
const OPENED: u8 = 1;

/// The kind byte of a head.
const HEAD: u8 = 2;

/// Where a head holds the count of its series.
const SERIES_AT: usize = 14;

/// What a reader must take next, kept apart from the reader.
#[derive(Clone, Copy, Debug)]
enum Next {
    Opened,
    Head,
    /// `remain` ends of the run are still to come.
    Ends {
        remain: u32,
    },
    /// `remain` bytes of a body of `end` bytes are still to come.
    Body {
        end: usize,
        remain: usize,
    },
}

/// Whether `error` says only that the bytes of a message are no reply.
fn malformed(error: Error) -> bool {
    matches!(
        error,
        Error::Empty
            | Error::Kind { .. }
            | Error::Length { .. }
            | Error::Series
            | Error::Path { .. }
    )
}

/// Whether a reader of `places` places that must take `next` refuses `message` with
/// `error`. For a message of a run or of a body, only one error is correct.
fn refused(next: Next, places: u32, message: &[u8], error: Error) -> bool {
    let len = message.len();
    match next {
        Next::Opened => match error {
            Error::Unopened { kind } => kind == HEAD && message.first() == Some(&HEAD),
            error => message != [OPENED] && malformed(error),
        },
        Next::Head => match error {
            Error::Reopen { kind } => kind == OPENED && message == [OPENED],
            Error::Places {
                series,
                places: limit,
            } => {
                let named = message.get(SERIES_AT..) == Some(&series.to_le_bytes()[..]);
                named && limit == places && series > places
            }
            error => message != [OPENED] && malformed(error),
        },
        Next::Ends { remain } => fuzz::run_refused(message, ends::LEN, remain, error),
        Next::Body { .. } if len == 0 => error == Error::Empty,
        Next::Body { remain, .. } => {
            len > remain && error == Error::Body { len, remain }
        }
    }
}

/// Each event of the session in `bytes` must encode to its message and come in the
/// order of a session, each refusal must be the one that the order gives, and the body
/// must be where [`Reader::body`] says.
fn read(bytes: &[u8]) {
    let [places, rest @ ..] = bytes else {
        return;
    };
    let places = u32::from(*places) + 1;
    let mut reader = Reader::new(&Open {
        mode: Mode::Latest,
        channels: places,
    });
    let mut next = Next::Opened;
    for message in fuzz::messages(rest) {
        next = match (next, reader.decode(message)) {
            (Next::Opened, Ok(FromHome::Opened)) => {
                assert_eq!(message, [OPENED], "the opened changed");
                Next::Head
            }
            (Next::Head, Ok(FromHome::Head(head))) => {
                let mut out = [0; 18];
                Reply::Head(head).encode(&mut out);
                assert_eq!(out, message, "the head changed");
                assert!(head.series <= places, "a head has more series than places");
                Next::Ends {
                    remain: head.series,
                }
            }
            (Next::Ends { remain }, Ok(FromHome::Ends { ends, last })) => {
                let ends: Vec<_> = ends.collect();
                let mut out = vec![0; message.len()];
                ends::encode(ends.iter().copied(), &mut out);
                assert_eq!(out, message, "the ends changed");
                let remain = u32::try_from(ends.len())
                    .ok()
                    .and_then(|count| remain.checked_sub(count))
                    .expect("a run message has more ends than remain");
                assert_eq!(last, remain == 0, "the run ends at another message");
                match ends.last() {
                    None => panic!("a run message has no end"),
                    Some(_) if !last => Next::Ends { remain },
                    Some(&(_, 0)) => Next::Head,
                    Some(&(_, end)) => {
                        let end = usize::try_from(end).expect("a usize holds a u32");
                        Next::Body { end, remain: end }
                    }
                }
            }
            (Next::Body { end, remain }, Ok(FromHome::Body { bytes, last })) => {
                assert_eq!(bytes, message, "the body changed");
                assert!(!bytes.is_empty(), "a body message has no byte");
                let remain = remain
                    .checked_sub(bytes.len())
                    .expect("a body message has more bytes than remain");
                assert_eq!(last, remain == 0, "the body ends at another message");
                if last {
                    Next::Head
                } else {
                    Next::Body { end, remain }
                }
            }
            (next, Err(error)) => {
                assert!(
                    refused(next, places, message, error),
                    "{error:?} is not the refusal of {message:?} for {next:?}"
                );
                next
            }
            (next, Ok(event)) => panic!("{event:?} came, not {next:?}"),
        };
        let body = match next {
            Next::Body { end, remain } => Some(end - remain),
            _ => None,
        };
        assert_eq!(reader.body(), body, "the body is not where it should be");
    }
}

/// A reader of `places` places that decoded the home's `Opened`.
fn opened(places: u32) -> Reader {
    let mut reader = Reader::new(&Open {
        mode: Mode::Latest,
        channels: places,
    });
    let mut out = vec![0; Reply::Opened.encoded_len()];
    Reply::Opened.encode(&mut out);
    match reader.decode(&out) {
        Ok(FromHome::Opened) => reader,
        other => panic!("an opened did not read back: {other:?}"),
    }
}

/// Each valid message made from `input` must decode to itself. A count of 0 is not
/// valid, so it becomes 1: an input that ends early writes the smallest messages.
fn write(input: &mut Unstructured) -> arbitrary::Result<()> {
    let range = Range {
        seq: input.arbitrary()?,
        count: input.arbitrary()?,
    };
    let series = input.arbitrary::<u32>()?.max(1);
    for path in [Path::Live, Path::Backfill] {
        let head = Head {
            path,
            range,
            series,
        };
        let mut out = vec![0; Reply::Head(head).encoded_len()];
        Reply::Head(head).encode(&mut out);
        match opened(series).decode(&out) {
            Ok(FromHome::Head(decoded)) => {
                assert_eq!(decoded, head, "the head changed")
            }
            other => panic!("a head did not read back: {other:?}"),
        }
    }

    let series = input.int_in_range(1..=RUN_MAX)?;
    let head = Reply::Head(Head {
        path: Path::Live,
        range,
        series,
    });
    let mut out = vec![0; head.encoded_len()];
    head.encode(&mut out);
    let mut reader = opened(series);
    reader.decode(&out).expect("the head decodes");
    let written = (0..series)
        .map(|_| input.arbitrary::<(u32, u32)>())
        .collect::<arbitrary::Result<Vec<_>>>()?;
    let mut out = vec![0; written.len() * ends::LEN];
    ends::encode(written.iter().copied(), &mut out);
    match reader.decode(&out) {
        Ok(FromHome::Ends { ends, last: true }) => {
            assert_eq!(ends.collect::<Vec<_>>(), written, "the ends changed");
        }
        other => panic!("the ends did not read back: {other:?}"),
    }
    Ok(())
}

fuzz_target!(|bytes: &[u8]| {
    read(bytes);
    write(&mut Unstructured::new(bytes)).expect("an input that ends gives zeros");
});

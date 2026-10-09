//! `wire::hub::Reader` never panics, each event encodes to its message and comes in the
//! order of a session, each refusal is one that the order or the mode gives, the body
//! starts after the ends run and ends at its last end, and each valid message that the
//! home writes reads back. A latest and a complete session each read the input.
//!
//! Input: one byte, the places of the session less 1, then the messages from the home
//! (`fuzz::messages`).

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use fuzz::hub::Run;
use libfuzzer_sys::{
    arbitrary::{self, Unstructured},
    fuzz_target,
};
use types::frame::{Path, Range};
use wire::hub::{Error, FromHome, Head, Mode, Open, Reader, Reply, ends};

/// The most ends of a written run.
const RUN_MAX: u32 = 4;

/// What a reader must take next, kept apart from the reader.
#[derive(Clone, Copy, Debug)]
enum Next {
    Opened,
    Head,
    Ends(Run),
    /// `remain` bytes of a body of `end` bytes are still to come.
    Body {
        end: usize,
        remain: usize,
    },
    /// The home sent `Behind`, and no message may follow.
    Ended,
}

/// A complete session's mode.
const COMPLETE: Mode = Mode::Complete { limit_bytes: 0 };

/// A reader of `places` places in `mode` that decoded nothing.
fn reader(places: u32, mode: Mode) -> Reader {
    Reader::new(&Open {
        mode,
        channels: places,
    })
}

/// The reply in `message`, read where its kind is in order: an opened by a reader that
/// decoded nothing, and a head or a behind by a complete reader that has a place for
/// each series.
fn reply(message: &[u8]) -> Result<Reply, Error> {
    if let Ok(FromHome::Opened) = reader(u32::MAX, COMPLETE).decode(message) {
        return Ok(Reply::Opened);
    }
    match opened(u32::MAX).decode(message)? {
        FromHome::Head(head) => Ok(Reply::Head(head)),
        FromHome::Behind => Ok(Reply::Behind),
        event => panic!("{event:?} came where only a head or a behind is in order"),
    }
}

/// The kind byte that the encoder writes for `reply`.
fn kind(reply: Reply) -> u8 {
    let mut out = vec![0; reply.encoded_len()];
    reply.encode(&mut out);
    out[0]
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

/// Whether a reader of `places` places in `mode` that must take `next` refuses
/// `message` with `error`. Only one error is correct.
fn refused(next: Next, places: u32, mode: Mode, message: &[u8], error: Error) -> bool {
    let len = message.len();
    match next {
        Next::Opened => match reply(message) {
            Ok(Reply::Opened) => false,
            Ok(reply) => error == Error::Unopened { kind: kind(reply) },
            Err(other) => malformed(other) && error == other,
        },
        Next::Head => match reply(message) {
            Ok(Reply::Opened) => {
                let kind = kind(Reply::Opened);
                error == Error::Reopen { kind }
            }
            Ok(Reply::Head(Head { series, .. })) => {
                series > places && error == Error::Places { series, places }
            }
            Ok(Reply::Behind) => {
                let kind = kind(Reply::Behind);
                mode == Mode::Latest && error == Error::Latest { kind }
            }
            Err(other) => malformed(other) && error == other,
        },
        Next::Ends(run) => run.refused(message, error),
        Next::Body { .. } if len == 0 => error == Error::Empty,
        Next::Body { remain, .. } => {
            len > remain && error == Error::Body { len, remain }
        }
        Next::Ended => error == Error::Ended,
    }
}

/// Each event of the session in `bytes` must encode to its message and come in the
/// order of a session, each refusal must be the one that the order or the mode gives,
/// and the body must be where [`Reader::body`] says.
fn read(bytes: &[u8]) {
    let [places, rest @ ..] = bytes else {
        return;
    };
    let places = u32::from(*places) + 1;
    for mode in [Mode::Latest, COMPLETE] {
        read_session(places, mode, rest);
    }
}

/// [`read`] for a session of `places` places in `mode`.
fn read_session(places: u32, mode: Mode, rest: &[u8]) {
    let mut reader = reader(places, mode);
    let mut next = Next::Opened;
    for message in fuzz::messages(rest) {
        next = match (next, reader.decode(message)) {
            (Next::Opened, Ok(FromHome::Opened)) => {
                assert_eq!(message, [kind(Reply::Opened)], "the opened changed");
                Next::Head
            }
            (Next::Head, Ok(FromHome::Head(head))) => {
                let mut out = vec![0; Reply::Head(head).encoded_len()];
                Reply::Head(head).encode(&mut out);
                assert_eq!(out, message, "the head changed");
                assert!(head.series <= places, "a head has more series than places");
                Next::Ends(Run::new(ends::LEN, head.series))
            }
            (Next::Head, Ok(FromHome::Behind)) => {
                assert_eq!(message, [kind(Reply::Behind)], "the behind changed");
                assert_eq!(mode, COMPLETE, "a latest session took a behind");
                Next::Ended
            }
            (Next::Ends(run), Ok(FromHome::Ends { ends, last })) => {
                let ends: Vec<_> = ends.collect();
                let mut out = vec![0; message.len()];
                ends::encode(ends.iter().copied(), &mut out);
                assert_eq!(out, message, "the ends changed");
                let run = run.take(ends.len(), last);
                let &(_, end) = ends.last().expect("the run took an end");
                match (run, end) {
                    (Some(run), _) => Next::Ends(run),
                    (None, 0) => Next::Head,
                    (None, end) => {
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
                    refused(next, places, mode, message, error),
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

/// A complete reader of `places` places that decoded the home's `Opened`.
fn opened(places: u32) -> Reader {
    let mut reader = reader(places, COMPLETE);
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
    let mut out = vec![0; Reply::Behind.encoded_len()];
    Reply::Behind.encode(&mut out);
    match opened(1).decode(&out) {
        Ok(FromHome::Behind) => {}
        other => panic!("a behind did not read back: {other:?}"),
    }

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

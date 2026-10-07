//! `wire::hub::Reader` never panics, each event encodes to its message, the body
//! starts after the ends run and ends at its last end, and each valid message that the
//! home writes reads back.
//!
//! Input: one byte, the places of the session less 1, then the messages from the home
//! (`fuzz::messages`).

#![no_main]

use libfuzzer_sys::{
    arbitrary::{self, Unstructured},
    fuzz_target,
};
use types::frame::{Path, Range};
use wire::hub::{FromHome, Head, Mode, Open, Reader, Reply, ends};

/// The most ends of a written run.
const RUN_MAX: u32 = 4;

/// Each event of the session in `bytes` must encode to its message, and the body must
/// be where [`Reader::body`] says.
fn read(bytes: &[u8]) {
    let [places, rest @ ..] = bytes else {
        return;
    };
    let mut reader = Reader::new(&Open {
        mode: Mode::Latest,
        channels: u32::from(*places) + 1,
    });
    for message in fuzz::messages(rest) {
        let at = reader.body();
        let Ok(event) = reader.decode(message) else {
            assert_eq!(reader.body(), at, "a refused message moved the body");
            continue;
        };
        let body = match event {
            FromHome::Opened => {
                assert_eq!(message, [1], "the opened changed");
                None
            }
            FromHome::Head(head) => {
                let mut out = [0; 18];
                Reply::Head(head).encode(&mut out);
                assert_eq!(out, message, "the head changed");
                None
            }
            FromHome::Ends { ends, last } => {
                let ends: Vec<_> = ends.collect();
                let mut out = vec![0; message.len()];
                ends::encode(ends.iter().copied(), &mut out);
                assert_eq!(out, message, "the ends changed");
                match ends.last() {
                    Some(&(_, end)) if last && end > 0 => Some(0),
                    _ => None,
                }
            }
            FromHome::Body { bytes, last } => {
                assert_eq!(bytes, message, "the body changed");
                let at = at.expect("a body message comes where the body is");
                (!last).then_some(at + bytes.len())
            }
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

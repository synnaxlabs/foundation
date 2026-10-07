//! `wire::hub::Reader` never panics, each event encodes to its message, and the body
//! starts after the ends run and ends at its last end.
//!
//! Input: one byte, the places of the session less 1, then the messages from the home
//! (`fuzz::messages`).

#![no_main]

use libfuzzer_sys::fuzz_target;
use wire::hub::{FromHome, Mode, Open, Reader, Reply, ends};

fuzz_target!(|bytes: &[u8]| {
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
});

//! `document::read::size` never panics, reads only the digits of a count, one space,
//! and a unit, as that count of the unit, and gives no fix of the form `Write "<text>"`
//! for a text that it refuses.

#![no_main]

use document::read::size;
use document::value::{Kind, Value};
use libfuzzer_sys::fuzz_target;

const UNITS: [(&str, u64); 5] = [
    ("B", 1),
    ("KiB", 1 << 10),
    ("MiB", 1 << 20),
    ("GiB", 1 << 30),
    ("TiB", 1 << 40),
];

fn string(text: &str) -> Value {
    Value {
        kind: Kind::String(text.into()),
        span: None,
    }
}

fuzz_target!(|text: &str| {
    match size(&string(text)) {
        Ok(bytes) => {
            let (count, unit) = text.split_once(' ').expect("a size has a space");
            assert!(count.bytes().all(|b| b.is_ascii_digit()), "{text:?}");
            let (_, each) = UNITS
                .iter()
                .find(|(name, _)| *name == unit)
                .expect("a size has a unit");
            let expected = count.parse::<u64>().ok().and_then(|n| n.checked_mul(*each));
            assert_eq!(expected, Some(bytes), "{text:?}");
        }
        Err(diagnostic) => {
            let written = diagnostic
                .fix
                .strip_prefix("Write \"")
                .and_then(|rest| rest.strip_suffix('"'));
            if let Some(written) = written {
                assert!(size(&string(written)).is_ok(), "{text:?} gives {written:?}");
            }
        }
    }
});

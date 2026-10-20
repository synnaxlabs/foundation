//! The read of a join ticket from the text that an operator carries never panics, and
//! a ticket it reads gives the same text, with no whitespace at its ends.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &[u8]| {
    if let Some(out) = mesh::testing::round_trip_ticket(text) {
        assert_eq!(out, text.trim_ascii(), "the text changed");
    }
});

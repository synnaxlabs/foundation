//! The decode of one mesh log record, as a log file holds it, never panics, and a
//! record it reads encodes to the same bytes.
//!
//! The target seals each input before the decode: it writes the length and both
//! checks, so a change to a field, or a byte put in or taken out of the body, still
//! reaches the decode of the hard state and the entries.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let mut record = bytes.to_vec();
    mesh::testing::seal_log_record(&mut record);
    if let Some(out) = mesh::testing::round_trip_log_record(&record) {
        assert_eq!(out, record, "the record changed");
    }
});

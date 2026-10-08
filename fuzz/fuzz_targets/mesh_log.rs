//! The decode of one mesh log record, as a log file holds it, never panics, and a
//! record it reads encodes to the same bytes.
//!
//! The target seals each input with the log's own checks before the decode, so a
//! change to a field still reaches the decode of the fields after the checks.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let mut record = bytes.to_vec();
    mesh::testing::seal_log_record(&mut record);
    if let Some(out) = mesh::testing::round_trip_log_record(&record) {
        assert_eq!(out, record, "the record changed");
    }
});

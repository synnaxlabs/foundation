//! The decode of `raft` entries, as the mesh log and an append hold them, never panics,
//! and entries it reads encode to the same bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Some(out) = mesh::testing::round_trip_entries(bytes) {
        assert_eq!(out, bytes, "the entries changed");
    }
});

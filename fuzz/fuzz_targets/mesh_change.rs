//! The decode of a `mesh` change record never panics, and a record it reads encodes to
//! the same bytes.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Some(out) = mesh::testing::round_trip_change(bytes) {
        assert_eq!(out, bytes, "the change record changed");
    }
});

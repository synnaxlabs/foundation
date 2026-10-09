//! The decode of a mesh message never panics, and a message it reads encodes to the
//! same bytes.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Some(out) = mesh::testing::round_trip_message(bytes) {
        assert_eq!(out, bytes, "the message changed");
    }
});

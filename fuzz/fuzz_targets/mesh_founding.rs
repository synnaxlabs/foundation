//! The decode of a region founding, which a join answer and the file `founding` carry,
//! never panics, and a founding it reads encodes to the same bytes.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Some(out) = mesh::testing::round_trip_founding(bytes) {
        assert_eq!(out, bytes, "the founding changed");
    }
});

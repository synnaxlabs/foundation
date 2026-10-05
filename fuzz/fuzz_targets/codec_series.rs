//! `codec::validate` and `codec::decode` never panic and give the same result.
//!
//! Input: one byte picks the scalar, then a little-endian `u16` sample count, then
//! the encoded series.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let [scalar, low, high, series @ ..] = bytes else {
        return;
    };
    let scalar = fuzz::scalar(*scalar);
    let count = usize::from(u16::from_le_bytes([*low, *high]));
    let mut out = vec![0; count * scalar.width()];
    let validated = codec::validate(scalar, count, series);
    let decoded = codec::decode(scalar, count, series, &mut out);
    assert_eq!(validated, decoded, "validate and decode disagree");
});

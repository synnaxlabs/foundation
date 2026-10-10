//! `codec::validate`, `codec::decode`, and `codec::Decoder` never panic and give the
//! same result. `decode` leaves `out` at the length that `validate` gives.
//!
//! Input: one byte picks the scalar, then a little-endian `u16` sample count, then
//! the encoded series.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;
use types::sample::Type;

fuzz_target!(|bytes: &[u8]| {
    let [scalar, low, high, series @ ..] = bytes else {
        return;
    };
    let scalar = fuzz::codec::scalar(*scalar);
    let count = usize::from(u16::from_le_bytes([*low, *high]));
    let data_type = Type::Scalar(scalar);
    let mut out = Vec::new();
    let validated = codec::validate(data_type, count, series);
    if let Ok(len) = validated {
        assert_eq!(len, count * scalar.width(), "validate gives another length");
    }
    let decoded = codec::decode(data_type, count, series, &mut out);
    assert_eq!(
        validated.clone().map(|_| ()),
        decoded,
        "validate and decode disagree"
    );
    if let Ok(len) = validated {
        assert_eq!(out.len(), len, "decode gives another length");
    }
    let mut decoder = codec::Decoder::new(scalar, count, series);
    let mut vector = vec![0; codec::VECTOR_LEN * scalar.width()];
    let mut joined = Vec::new();
    let streamed = loop {
        match decoder.next(&mut vector) {
            Some(Ok(samples)) => joined.extend_from_slice(samples),
            Some(Err(error)) => break Err(error),
            None => break Ok(()),
        }
    };
    assert_eq!(streamed, decoded, "Decoder and decode disagree");
    if streamed.is_ok() {
        assert_eq!(joined, out, "Decoder gives other samples");
    }
});

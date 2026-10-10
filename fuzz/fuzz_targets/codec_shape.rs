//! `codec::validate` and `codec::decode` of an array, matrix, list, `String`, or
//! `Bytes` series never panic and give the same result. An array or a matrix series
//! gives the result of the series of its elements. A `String` series gives the result
//! of a `Bytes` series, or `Error::Utf8` at the first sample that `str::from_utf8`
//! refuses. A valid series decodes with zeros for the padding, to samples that encode
//! and decode unchanged.
//!
//! Input: the type that `fuzz::codec::shape` reads, then a little-endian `u32` sample
//! count, then a little-endian `u16` length of `out` for a series that is not valid,
//! then the encoded series.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use codec::{Encoder, Error};
use fuzz::codec::{PAD, Shape};
use libfuzzer_sys::fuzz_target;
use types::sample::Type;

fuzz_target!(|bytes: &[u8]| {
    let Some((data_type, shape, rest)) = fuzz::codec::shape(bytes) else {
        return;
    };
    let [a, b, c, d, e, f, series @ ..] = rest else {
        return;
    };
    let count = usize::try_from(u32::from_le_bytes([*a, *b, *c, *d])).unwrap();
    let held = usize::from(u16::from_le_bytes([*e, *f]));
    let validated = codec::validate(data_type, count, series);
    let mut out = vec![PAD; *validated.as_ref().unwrap_or(&held)];
    let decoded = codec::decode(data_type, count, series, &mut out);
    assert_eq!(
        decoded,
        validated.clone().map(|_| ()),
        "validate and decode disagree"
    );
    match shape {
        Shape::Fixed { element, len } => assert_eq!(
            validated,
            codec::validate(Type::Scalar(element), count * len, series),
            "the series of the elements gives another result"
        ),
        Shape::Variable { element, .. } if validated.is_ok() => assert!(
            out[4 * count..fuzz::codec::start(element, count)]
                .iter()
                .all(|&byte| byte == 0),
            "the padding is not zeros"
        ),
        Shape::Variable { .. } => {}
    }
    if data_type == Type::String {
        assert_eq!(validated, text(count, series), "a String series differs");
    }
    if validated.is_ok() {
        let mut again = vec![0; codec::max_len(data_type, out.len())];
        let len = Encoder::new(data_type)
            .encode(count, &out, &mut again)
            .expect("decoded samples encode");
        let mut twice = vec![PAD; out.len()];
        let decoded = codec::decode(data_type, count, &again[..len], &mut twice);
        assert_eq!(decoded, Ok(()), "encoded samples do not decode");
        assert_eq!(twice, out, "the samples changed");
    }
});

/// What `validate` gives for `series` as `count` `String` samples, from what it gives
/// for them as `Bytes` samples.
fn text(count: usize, series: &[u8]) -> Result<usize, Error> {
    let as_bytes = codec::validate(Type::Bytes, count, series);
    let valid = match as_bytes {
        Ok(_) => series,
        Err(Error::Trailing { extra }) => &series[..series.len() - extra],
        Err(_) => return as_bytes,
    };
    let len = codec::validate(Type::Bytes, count, valid).expect("the front is valid");
    let mut raw = vec![0; len];
    codec::decode(Type::Bytes, count, valid, &mut raw).expect("the front decodes");
    let (ends, elements) = raw.split_at(4 * count);
    let ends = ends
        .as_chunks::<4>()
        .0
        .iter()
        .map(|end| u32::from_le_bytes(*end));
    match fuzz::codec::not_utf8(ends, elements) {
        Some(sample) => Err(Error::Utf8 { sample }),
        None => as_bytes,
    }
}

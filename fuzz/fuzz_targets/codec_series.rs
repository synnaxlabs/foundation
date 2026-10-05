//! `codec::validate` and `codec::decode` never panic and give the same result, and
//! an encoded series decodes to its samples.
//!
//! Input: one byte picks the scalar, one byte picks the mode. Mode "decode" reads a
//! little-endian `u16` sample count and takes the rest as an encoded series. Mode
//! "encode" takes the rest, cut to whole samples, as the samples.

#![no_main]

use codec::Encoder;
use libfuzzer_sys::fuzz_target;
use types::sample::Scalar;

const SCALARS: [Scalar; 14] = [
    Scalar::Bool,
    Scalar::I8,
    Scalar::I16,
    Scalar::I32,
    Scalar::I64,
    Scalar::U8,
    Scalar::U16,
    Scalar::U32,
    Scalar::U64,
    Scalar::F32,
    Scalar::F64,
    Scalar::Stamp,
    Scalar::Span,
    Scalar::Uuid,
];

fuzz_target!(|bytes: &[u8]| {
    let [scalar, mode, rest @ ..] = bytes else {
        return;
    };
    let scalar = SCALARS[usize::from(*scalar) % SCALARS.len()];
    if mode % 2 == 0 {
        let [low, high, series @ ..] = rest else {
            return;
        };
        decode(
            scalar,
            usize::from(u16::from_le_bytes([*low, *high])),
            series,
        );
    } else {
        let whole = rest.len() - rest.len() % scalar.width();
        round_trip(scalar, &rest[..whole]);
    }
});

fn decode(scalar: Scalar, count: usize, series: &[u8]) {
    let mut out = vec![0; count * scalar.width()];
    let validated = codec::validate(scalar, count, series);
    let decoded = codec::decode(scalar, count, series, &mut out);
    assert_eq!(validated, decoded, "validate and decode disagree");
}

fn round_trip(scalar: Scalar, samples: &[u8]) {
    let count = samples.len() / scalar.width();
    let mut series = vec![0; codec::max_len(scalar, count)];
    let len = Encoder::new(scalar).encode(samples, &mut series);
    let series = &series[..len];
    assert_eq!(
        codec::validate(scalar, count, series),
        Ok(()),
        "an encoded series is not valid"
    );
    let mut out = vec![0; samples.len()];
    assert_eq!(
        codec::decode(scalar, count, series, &mut out),
        Ok(()),
        "an encoded series does not decode"
    );
    assert_eq!(out, samples, "the samples changed");
}

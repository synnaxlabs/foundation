//! An encoded series is valid and decodes to its samples.
//!
//! Input: one byte picks the scalar. The rest, cut to whole samples, is the samples.

#![no_main]

use codec::Encoder;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let [scalar, rest @ ..] = bytes else {
        return;
    };
    let scalar = fuzz::scalar(*scalar);
    let samples = &rest[..rest.len() - rest.len() % scalar.width()];
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
});

//! An encoded series is valid and decodes to its samples, and a count that does not
//! fit the samples is refused.
//!
//! Input: one byte picks the scalar. The rest, cut to whole samples, is the samples.

#![no_main]

use codec::{Encoder, Error};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let [scalar, rest @ ..] = bytes else {
        return;
    };
    let scalar = fuzz::scalar(*scalar);
    let samples = &rest[..rest.len() - rest.len() % scalar.width()];
    let count = samples.len() / scalar.width();
    let mut series = vec![0; codec::max_len(scalar, samples.len())];
    let mut encoder = Encoder::new(scalar);
    for wrong in [count.checked_sub(1), Some(count + 1)].into_iter().flatten() {
        assert_eq!(
            encoder.encode(wrong, samples, &mut series),
            Err(Error::Length {
                expected: wrong * scalar.width(),
                actual: samples.len(),
            }),
            "a wrong count is not refused"
        );
    }
    let len = encoder
        .encode(count, samples, &mut series)
        .expect("the samples fit the count");
    let series = &series[..len];
    assert_eq!(
        codec::validate(scalar, count, series),
        Ok(samples.len()),
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

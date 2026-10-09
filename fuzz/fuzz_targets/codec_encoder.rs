//! An encoded series is valid and decodes to its samples, and a count that does not
//! fit the samples is refused.
//!
//! Input: one byte picks the scalar. The rest, cut to whole samples, is the samples.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use codec::Encoder;
use libfuzzer_sys::fuzz_target;
use types::sample::Type;

fuzz_target!(|bytes: &[u8]| {
    let [scalar, rest @ ..] = bytes else {
        return;
    };
    let scalar = fuzz::codec::scalar(*scalar);
    let samples = &rest[..rest.len() - rest.len() % scalar.width()];
    let count = samples.len() / scalar.width();
    let data_type = Type::Scalar(scalar);
    let mut encoder = Encoder::new(data_type);
    fuzz::codec::check_wrong_counts(&mut encoder, data_type, samples, scalar.width());
    let mut series = vec![0; codec::max_len(data_type, samples.len())];
    let len = encoder
        .encode(count, samples, &mut series)
        .expect("the samples fit the count");
    fuzz::codec::check_decodes(data_type, count, &series[..len], samples);
});

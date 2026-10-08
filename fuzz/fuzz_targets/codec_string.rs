//! A `String` series is refused at the first sample that is not UTF-8, by
//! `Encoder::encode`, `codec::validate`, and `codec::decode`, and at no other.
//!
//! Input: a little-endian `u16` count of ASCII bytes before the first sample, modulo
//! 4,096, then a byte that gives 1 to 4 copies of the samples, then the samples as
//! length-prefixed messages.

#![no_main]

use codec::{Encoder, Error};
use libfuzzer_sys::fuzz_target;
use types::sample::Type;

fuzz_target!(|bytes: &[u8]| {
    let [low, high, copies, rest @ ..] = bytes else {
        return;
    };
    let pad = usize::from(u16::from_le_bytes([*low, *high])) % 4_096;
    let copies = usize::from(copies % 4) + 1;
    let mut samples: Vec<Vec<u8>> = Vec::new();
    for _ in 0..copies {
        samples.extend(fuzz::messages(rest).map(<[u8]>::to_vec));
    }
    if samples.is_empty() {
        samples.push(Vec::new());
    }
    samples[0].splice(0..0, std::iter::repeat_n(b'a', pad));
    let count = samples.len();
    let mut values = Vec::new();
    let mut end = 0_u32;
    for sample in &samples {
        end += u32::try_from(sample.len()).expect("a sample is at most 4,350 bytes");
        values.extend(end.to_le_bytes());
    }
    for sample in &samples {
        values.extend(sample);
    }
    let expected = match samples.iter().position(|s| str::from_utf8(s).is_err()) {
        Some(sample) => Err(Error::Utf8 { sample }),
        None => Ok(()),
    };

    let mut series = vec![0; codec::max_len(Type::String, values.len())];
    let encoded = Encoder::new(Type::String).encode(count, &values, &mut series);
    assert_eq!(encoded.map(|_| ()), expected, "encode gives another result");

    let mut series = vec![0; codec::max_len(Type::Bytes, values.len())];
    let len = Encoder::new(Type::Bytes)
        .encode(count, &values, &mut series)
        .expect("any bytes are valid");
    let series = &series[..len];
    assert_eq!(
        codec::validate(Type::String, count, series),
        expected.clone().map(|()| values.len()),
        "validate gives another result"
    );
    let mut out = vec![0; values.len()];
    let decoded = codec::decode(Type::String, count, series, &mut out);
    assert_eq!(decoded, expected, "decode gives another result");
    if decoded.is_ok() {
        assert_eq!(out, values, "decode gives other samples");
    }
});

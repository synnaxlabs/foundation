//! `codec::Encoder` on an array, matrix, list, `String`, or `Bytes` series gives the
//! refusal that a second reader of the raw form gives, or a valid series that decodes
//! unchanged, with zeros for the padding. An array or a matrix series encodes as the
//! series of its elements.
//!
//! Input: the type that `fuzz::codec::data_type` reads, then a little-endian `u16` count.
//! For an array or a matrix, the rest, cut to whole samples, is the samples, and the
//! count is used only when a sample has no bytes. For a list, `String`, or `Bytes`,
//! one signed byte for each sample changes its end from the last, and the rest gives
//! the elements, cut to what the last end needs.

#![no_main]

use codec::{Encoder, Error};
use libfuzzer_sys::fuzz_target;
use types::sample::{Scalar, Sides, Type};

/// The byte that fills the padding. `Encoder` does not read it.
const PAD: u8 = 0xa5;

fuzz_target!(|bytes: &[u8]| {
    let Some((data_type, rest)) = fuzz::codec::data_type(bytes) else {
        return;
    };
    let [low, high, rest @ ..] = rest else {
        return;
    };
    let count = usize::from(u16::from_le_bytes([*low, *high]));
    match data_type {
        Type::Array { element, len } => {
            fixed(
                data_type,
                element,
                usize::try_from(len).unwrap(),
                count,
                rest,
            );
        }
        Type::Matrix {
            element,
            sides: Sides { rows, columns },
        } => {
            let len = usize::from(rows) * usize::from(columns);
            fixed(data_type, element, len, count, rest);
        }
        Type::List { element, max } => variable(data_type, element, max, count, rest),
        Type::String | Type::Bytes => {
            variable(data_type, Scalar::U8, u32::MAX, count, rest);
        }
        Type::Scalar(_) => unreachable!("fuzz::codec::data_type gives no scalar"),
    }
});

/// Encodes the samples in `rest` of an array or matrix `data_type` of `len` elements.
fn fixed(data_type: Type, element: Scalar, len: usize, count: usize, rest: &[u8]) {
    let width = element.width() * len;
    let (count, samples) = if width == 0 {
        (count, &rest[..0])
    } else {
        let samples = &rest[..rest.len() - rest.len() % width];
        (samples.len() / width, samples)
    };
    let mut encoder = Encoder::new(data_type);
    let mut series = vec![0; codec::max_len(data_type, samples.len())];
    if width > 0 {
        for wrong in [count.checked_sub(1), Some(count + 1)]
            .into_iter()
            .flatten()
        {
            assert_eq!(
                encoder.encode(wrong, samples, &mut series),
                Err(Error::Length {
                    expected: wrong * width,
                    actual: samples.len(),
                }),
                "a wrong count is not refused"
            );
        }
    }
    let encoded = encoder
        .encode(count, samples, &mut series)
        .expect("the samples fit the count");
    let series = &series[..encoded];
    let scalar = Type::Scalar(element);
    let mut flat = vec![0; codec::max_len(scalar, samples.len())];
    let flat_len = Encoder::new(scalar)
        .encode(count * len, samples, &mut flat)
        .expect("the elements fit their count");
    assert_eq!(
        series,
        &flat[..flat_len],
        "the series differs from the series of its elements"
    );
    check(data_type, count, series, samples);
}

/// Encodes `count` samples of a list, `String`, or `Bytes` `data_type`, whose ends
/// change by the signed bytes at the front of `rest`.
fn variable(data_type: Type, element: Scalar, max: u32, count: usize, rest: &[u8]) {
    let (changes, rest) = rest.split_at(count.min(rest.len()));
    let mut ends = Vec::with_capacity(count);
    let mut last = 0_u32;
    for sample in 0..count {
        let change = changes.get(sample).map_or(0, |&byte| i32::from(byte as i8));
        last = last.wrapping_add_signed(change);
        ends.push(last);
    }
    let width = element.width();
    let start = (4 * count).next_multiple_of(width.min(8));
    let needed = usize::try_from(last).unwrap() * width;
    let elements = &rest[..needed.min(rest.len())];
    let mut values: Vec<u8> = ends.iter().flat_map(|end| end.to_le_bytes()).collect();
    values.resize(start, PAD);
    values.extend_from_slice(elements);
    let expected = refusal(data_type, max, &ends, start + needed, &values);

    let mut series = vec![0; codec::max_len(data_type, values.len())];
    let encoded = Encoder::new(data_type).encode(count, &values, &mut series);
    assert_eq!(
        encoded.clone().map(|_| ()),
        expected.map_or(Ok(()), Err),
        "encode gives another result"
    );
    if let Ok(len) = encoded {
        values[4 * count..start].fill(0);
        check(data_type, count, &series[..len], &values);
    }
}

/// The first refusal of `values`, whose ends are `ends`, read apart from `codec`: an
/// end below the one before it, a sample of more than `max` elements, a length that is
/// not `len`, then a `String` sample that is not UTF-8.
fn refusal(
    data_type: Type,
    max: u32,
    ends: &[u32],
    len: usize,
    values: &[u8],
) -> Option<Error> {
    let mut previous = 0;
    for (sample, &end) in ends.iter().enumerate() {
        let Some(elements) = end.checked_sub(previous) else {
            return Some(Error::Ends {
                sample,
                end,
                previous,
            });
        };
        if elements > max {
            return Some(Error::Long {
                sample,
                len: elements,
                max,
            });
        }
        previous = end;
    }
    if values.len() != len {
        return Some(Error::Length {
            expected: len,
            actual: values.len(),
        });
    }
    if data_type != Type::String {
        return None;
    }
    let text = &values[4 * ends.len()..];
    let mut start = 0;
    ends.iter().enumerate().find_map(|(sample, &end)| {
        let end = usize::try_from(end).unwrap();
        let refused = str::from_utf8(&text[start..end]).is_err();
        start = end;
        refused.then_some(Error::Utf8 { sample })
    })
}

/// Checks that `series` is valid and decodes to `values`.
fn check(data_type: Type, count: usize, series: &[u8], values: &[u8]) {
    assert_eq!(
        codec::validate(data_type, count, series),
        Ok(values.len()),
        "an encoded series is not valid"
    );
    let mut out = vec![0; values.len()];
    assert_eq!(
        codec::decode(data_type, count, series, &mut out),
        Ok(()),
        "an encoded series does not decode"
    );
    assert_eq!(out, values, "the samples changed");
}

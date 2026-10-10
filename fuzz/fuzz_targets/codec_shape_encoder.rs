//! `codec::Encoder` on an array, matrix, list, `String`, or `Bytes` series gives the
//! refusal that a second reader of the raw form gives, or a valid series that decodes
//! unchanged, with zeros for the padding. An array or a matrix series encodes as the
//! series of its elements.
//!
//! Input: the type that `fuzz::codec::shape` reads, then a little-endian `u16` count.
//! For an array or a matrix, the rest, cut to whole samples, is the samples, and the
//! count is used only when a sample has no bytes. For a list, `String`, or `Bytes`,
//! a byte picks the length of the values, then one signed byte for each sample changes
//! its end from the last, and the rest gives the elements.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use codec::{Encoder, Error};
use fuzz::codec::{PAD, Shape};
use libfuzzer_sys::fuzz_target;
use types::sample::{Scalar, Type};

fuzz_target!(|bytes: &[u8]| {
    let Some((data_type, shape, rest)) = fuzz::codec::shape(bytes) else {
        return;
    };
    let [low, high, rest @ ..] = rest else {
        return;
    };
    let count = usize::from(u16::from_le_bytes([*low, *high]));
    match shape {
        Shape::Fixed { element, len } => fixed(data_type, element, len, count, rest),
        Shape::Variable { element, max } => {
            variable(data_type, element, max, count, rest);
        }
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
    if width > 0 {
        fuzz::codec::check_wrong_counts(&mut encoder, data_type, samples, width);
    }
    let mut series = vec![0; codec::max_len(data_type, samples.len())];
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
    fuzz::codec::check_decodes(data_type, count, series, samples);
}

/// Encodes `count` samples of a list, `String`, or `Bytes` `data_type`. The remainder
/// by 3 of the first byte of `rest` keeps the values the length that their ends need,
/// adds a third of that byte in bytes, or cuts the values to that byte's share of
/// 256. The signed bytes after it change the ends.
fn variable(data_type: Type, element: Scalar, max: u32, count: usize, rest: &[u8]) {
    let [fit, rest @ ..] = rest else {
        return;
    };
    let (changes, rest) = rest.split_at(count.min(rest.len()));
    let mut ends = Vec::with_capacity(count);
    let mut last = 0_u32;
    for sample in 0..count {
        let change = changes.get(sample).map_or(0, |&byte| i32::from(byte as i8));
        last = last.wrapping_add_signed(change);
        ends.push(last);
    }
    let start = fuzz::codec::start(element, count);
    let needed = usize::try_from(last).unwrap() * element.width();
    let extra = if fit % 3 == 1 {
        usize::from(fit / 3)
    } else {
        0
    };
    let mut values: Vec<u8> = ends.iter().flat_map(|end| end.to_le_bytes()).collect();
    values.resize(start, PAD);
    values.extend_from_slice(&rest[..(needed + extra).min(rest.len())]);
    if fit % 3 == 2 {
        values.truncate(values.len() * usize::from(*fit) / 256);
    }
    let expected = refusal(data_type, max, &ends, start, start + needed, &values);

    let mut series = vec![0; codec::max_len(data_type, values.len())];
    let encoded = Encoder::new(data_type).encode(count, &values, &mut series);
    assert_eq!(
        encoded.clone().map(|_| ()),
        expected.map_or(Ok(()), Err),
        "encode gives another result"
    );
    if let Ok(len) = encoded {
        values[4 * count..start].fill(0);
        fuzz::codec::check_decodes(data_type, count, &series[..len], &values);
    }
}

/// The first refusal of `values`, whose ends are `ends` and whose elements are at
/// `start`, read apart from `codec`: values that cut the ends, an end below the one
/// before it, a sample of more than `max` elements, a length that is not `len`, then a
/// `String` sample that is not UTF-8.
fn refusal(
    data_type: Type,
    max: u32,
    ends: &[u32],
    start: usize,
    len: usize,
    values: &[u8],
) -> Option<Error> {
    let length = |expected| Error::Length {
        expected,
        actual: values.len(),
    };
    if values.len() < 4 * ends.len() {
        return Some(length(4 * ends.len()));
    }
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
        return Some(length(len));
    }
    if data_type != Type::String {
        return None;
    }
    fuzz::codec::not_utf8(ends.iter().copied(), &values[start..])
        .map(|sample| Error::Utf8 { sample })
}

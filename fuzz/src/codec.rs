//! What the codec targets have in common.

use codec::{Encoder, Error};
use types::sample::{Scalar, Sides, Type};

// Append only: a byte of each input in `oracles/fuzz/codec_series`,
// `oracles/fuzz/codec_encoder`, `oracles/fuzz/codec_shape`, and
// `oracles/fuzz/codec_shape_encoder` is an index into this table.
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

/// The scalar that a byte of a codec input picks.
///
/// # Panics
///
/// When the table does not hold the scalar at its index.
#[must_use]
pub fn scalar(byte: u8) -> Scalar {
    let at = usize::from(byte) % SCALARS.len();
    let scalar = SCALARS[at];
    assert_eq!(index(scalar), at, "the table of scalars is out of order");
    scalar
}

/// Stops the build when `Scalar` gets a variant that the table does not hold.
fn index(scalar: Scalar) -> usize {
    match scalar {
        Scalar::Bool => 0,
        Scalar::I8 => 1,
        Scalar::I16 => 2,
        Scalar::I32 => 3,
        Scalar::I64 => 4,
        Scalar::U8 => 5,
        Scalar::U16 => 6,
        Scalar::U32 => 7,
        Scalar::U64 => 8,
        Scalar::F32 => 9,
        Scalar::F64 => 10,
        Scalar::Stamp => 11,
        Scalar::Span => 12,
        Scalar::Uuid => 13,
    }
}

/// A byte other than zero, for the padding that `Encoder` does not read and for the
/// bytes that `decode` must write over.
pub const PAD: u8 = 0xa5;

/// A sample type other than a scalar, as a second reader of its raw form sees it.
#[derive(Clone, Copy, Debug)]
pub enum Shape {
    /// An array or a matrix: each sample is `len` elements.
    Fixed { element: Scalar, len: usize },
    /// A list, `String`, or `Bytes`: the `u32` end of each sample, then padding to
    /// [`start`], then the elements, at most `max` in each sample.
    Variable { element: Scalar, max: u32 },
}

// Append only: the first byte of each input in `oracles/fuzz/codec_shape` and
// `oracles/fuzz/codec_shape_encoder` picks a kind by its remainder by `KINDS`.
const KINDS: u8 = 5;

/// The sample type other than a scalar that the front of a codec input picks, its
/// shape, and the bytes after it. A byte picks the kind: an array, a matrix, a list,
/// `String`, or `Bytes`. An array then takes a scalar byte and a little-endian `u32`
/// length, a matrix a scalar byte and two `u16` sides, and a list a scalar byte and a
/// byte whose remainder by 5 is its `max`.
///
/// # Panics
///
/// When the kinds are out of order.
#[must_use]
pub fn shape(bytes: &[u8]) -> Option<(Type, Shape, &[u8])> {
    let (at, rest) = bytes.split_first()?;
    let at = at % KINDS;
    let (data_type, rest) = match at {
        0 => {
            let [element, a, b, c, d, rest @ ..] = rest else {
                return None;
            };
            let len = u32::from_le_bytes([*a, *b, *c, *d]);
            let element = scalar(*element);
            (Type::Array { element, len }, rest)
        }
        1 => {
            let [element, a, b, c, d, rest @ ..] = rest else {
                return None;
            };
            let sides = Sides {
                rows: u16::from_le_bytes([*a, *b]),
                columns: u16::from_le_bytes([*c, *d]),
            };
            let element = scalar(*element);
            (Type::Matrix { element, sides }, rest)
        }
        2 => {
            let [element, max, rest @ ..] = rest else {
                return None;
            };
            let max = u32::from(max % 5);
            let element = scalar(*element);
            (Type::List { element, max }, rest)
        }
        3 => (Type::String, rest),
        _ => (Type::Bytes, rest),
    };
    let (kind, shape) = kind(data_type);
    assert_eq!(kind, at, "the kinds are out of order");
    Some((data_type, shape, rest))
}

/// The kind of `data_type` and its shape. Stops the build when `Type` gets a variant
/// that [`shape`] does not pick.
fn kind(data_type: Type) -> (u8, Shape) {
    match data_type {
        Type::Array { element, len } => {
            let len = usize::try_from(len).expect("a u32 fits a usize");
            (0, Shape::Fixed { element, len })
        }
        Type::Matrix {
            element,
            sides: Sides { rows, columns },
        } => {
            let len = usize::from(rows) * usize::from(columns);
            (1, Shape::Fixed { element, len })
        }
        Type::List { element, max } => (2, Shape::Variable { element, max }),
        Type::String => (
            3,
            Shape::Variable {
                element: Scalar::U8,
                max: u32::MAX,
            },
        ),
        Type::Bytes => (
            4,
            Shape::Variable {
                element: Scalar::U8,
                max: u32::MAX,
            },
        ),
        Type::Scalar(_) => unreachable!("shape picks no scalar"),
    }
}

/// The offset of the first element in the raw form of `count` variable samples of
/// `element`.
#[must_use]
pub fn start(element: Scalar, count: usize) -> usize {
    (4 * count).next_multiple_of(element.width().min(8))
}

/// The first sample that is not UTF-8, of the samples whose `ends` cut `elements`.
///
/// # Panics
///
/// When an end is below the one before it or past `elements`.
#[must_use]
pub fn not_utf8(ends: impl IntoIterator<Item = u32>, elements: &[u8]) -> Option<usize> {
    let mut start = 0;
    ends.into_iter().enumerate().find_map(|(sample, end)| {
        let end = usize::try_from(end).expect("a u32 fits a usize");
        let refused = str::from_utf8(&elements[start..end]).is_err();
        start = end;
        refused.then_some(sample)
    })
}

/// Checks that `encoder` refuses `samples`, of `width` bytes each, at one sample
/// fewer and one more than they hold.
///
/// # Panics
///
/// When `encoder` does not refuse a wrong count with [`Error::Length`].
pub fn check_wrong_counts(
    encoder: &mut Encoder,
    data_type: Type,
    samples: &[u8],
    width: usize,
) {
    let count = samples.len() / width;
    let mut series = vec![0; codec::max_len(data_type, samples.len())];
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

/// Checks that `series`, an encoding of `count` samples of `data_type`, is valid and
/// decodes to `values` over a buffer of [`PAD`] bytes.
///
/// # Panics
///
/// When `series` is not valid or does not decode to `values`.
pub fn check_decodes(data_type: Type, count: usize, series: &[u8], values: &[u8]) {
    assert_eq!(
        codec::validate(data_type, count, series),
        Ok(values.len()),
        "an encoded series is not valid"
    );
    let mut out = vec![PAD; values.len()];
    assert_eq!(
        codec::decode(data_type, count, series, &mut out),
        Ok(()),
        "an encoded series does not decode"
    );
    assert_eq!(out, values, "the samples changed");
}

//! What the fuzz targets have in common.

use types::sample::{Scalar, Sides, Type};

pub mod hub;

/// The stream messages in an input: each is a length byte and then that many bytes.
/// The last message ends with the input.
pub fn messages(mut bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        let (&len, rest) = bytes.split_first()?;
        let (message, rest) = rest.split_at(usize::from(len).min(rest.len()));
        bytes = rest;
        Some(message)
    })
}

// Append only: the first byte of each input in `oracles/fuzz/codec_series` and
// `oracles/fuzz/codec_encoder` is an index into this table.
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

/// The scalar that the first byte of a codec input picks.
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

/// The sample type other than a scalar that the front of a codec input picks, and the
/// bytes after it. A byte picks the kind: an array, a matrix, a list, `String`, or
/// `Bytes`. An array then takes a scalar byte and a little-endian `u32` length, a
/// matrix a scalar byte and two `u16` sides, and a list a scalar byte and a byte whose
/// remainder by 5 is its `max`.
#[must_use]
pub fn data_type(bytes: &[u8]) -> Option<(Type, &[u8])> {
    let (kind, rest) = bytes.split_first()?;
    Some(match kind % 5 {
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
    })
}

/// Checks that a value read from `text` prints as text that reads back to it.
///
/// # Panics
///
/// When the printed text does not read back to the same value.
pub fn check_round_trip<T>(text: &str)
where
    T: std::str::FromStr + std::fmt::Display + PartialEq + std::fmt::Debug,
    T::Err: std::fmt::Debug + PartialEq,
{
    let Ok(value) = text.parse::<T>() else {
        return;
    };
    let printed = value.to_string();
    assert_eq!(
        printed.parse::<T>().as_ref(),
        Ok(&value),
        "{printed:?} does not read back"
    );
}

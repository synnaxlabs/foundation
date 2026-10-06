//! Encoding, checking, and decoding a series, whole or one vector at a time, make no
//! heap allocation. This binary has no test harness: the count covers each thread, and
//! a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use codec::{Decoder, Encoder, Error, VECTOR_LEN, max_len};
use types::sample::{Scalar, Type};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// The scalars that use every codec.
const INTS: [Scalar; 10] = [
    Scalar::I8,
    Scalar::I16,
    Scalar::I32,
    Scalar::I64,
    Scalar::U8,
    Scalar::U16,
    Scalar::U32,
    Scalar::U64,
    Scalar::Stamp,
    Scalar::Span,
];

/// The scalars that stay raw.
const OTHERS: [Scalar; 4] = [Scalar::Bool, Scalar::F32, Scalar::F64, Scalar::Uuid];

const COUNTS: [usize; 6] = [
    0,
    1,
    VECTOR_LEN - 1,
    VECTOR_LEN,
    VECTOR_LEN + 1,
    2 * VECTOR_LEN + 1,
];

/// The tags of format version 1 that pack values to a bit width.
const FFOR: u8 = 1;
const DELTA: u8 = 2;

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );
    for scalar in INTS {
        assert_eq!(round_trip(scalar), [true; 4], "{scalar:?} uses each codec");
        decode_every_width(scalar);
    }
    for scalar in OTHERS {
        assert_eq!(
            round_trip(scalar),
            [true, false, false, false],
            "{scalar:?}"
        );
    }
    round_trip_types();
    refuse();
}

/// A series that one codec packs best.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// Random values of this many bits: FFOR, or raw at full width.
    Noise(u32),
    /// A fixed step: delta with 0 bits.
    Step,
    /// A step with 1 bit of jitter: delta with 2 bits.
    Jitter,
    /// Random values in runs of this length: RLE.
    Runs(u64),
}

impl Shape {
    fn sample(self, i: u64) -> u64 {
        match self {
            Self::Noise(bits) => mix(i) & u64::MAX.checked_shr(64 - bits).unwrap_or(0),
            Self::Step => i * 1000,
            Self::Jitter => i * 3 + (mix(i) & 1),
            Self::Runs(len) => mix(i / len),
        }
    }
}

/// Encodes, checks, and decodes each shape and count of `scalar` with one encoder.
/// Returns which tags the first vectors used.
fn round_trip(scalar: Scalar) -> [bool; 4] {
    let width = scalar.width();
    let data_type = Type::Scalar(scalar);
    let mut encoder = Encoder::new(data_type);
    let mut tags = [false; 4];
    let bits = u32::try_from(8 * width).expect("widths are small").min(64);
    let shapes = (0..=bits).map(Shape::Noise).chain([
        Shape::Step,
        Shape::Jitter,
        Shape::Runs(256),
        Shape::Runs(8),
    ]);
    for shape in shapes {
        for count in COUNTS {
            let values: Vec<u8> = (0_u64..)
                .take(count)
                .flat_map(|i| u128::from(shape.sample(i)).to_le_bytes())
                .enumerate()
                .filter_map(|(n, byte)| (n % 16 < width).then_some(byte))
                .collect();
            let case = format!("{scalar:?}, {shape:?}, {count} samples");
            let bytes = check(&mut encoder, data_type, count, &values, &case);
            let (read, allocations) =
                ALLOCATOR.count(|| by_vector(scalar, count, &bytes, &values));
            assert_eq!(allocations, 0, "decoding {case} by vector allocated");
            assert_eq!(read, Ok(values.len()), "{case} decodes by vector");
            if let Some(&tag) = bytes.first() {
                tags[usize::from(tag)] = true;
            }
        }
    }
    tags
}

/// Encodes, checks, and decodes each count of an array type and of each variable
/// type with one encoder. At the largest count, the ends fill three vectors.
fn round_trip_types() {
    let types = [
        Type::Array {
            element: Scalar::I16,
            len: 3,
        },
        Type::String,
        Type::Bytes,
        Type::List {
            element: Scalar::U64,
            max: 4,
        },
    ];
    for data_type in types {
        let mut encoder = Encoder::new(data_type);
        for count in COUNTS {
            let case = format!("{data_type:?}, {count} samples");
            check(
                &mut encoder,
                data_type,
                count,
                &values(data_type, count),
                &case,
            );
        }
    }
}

/// The raw bytes of `count` samples of `data_type`, an array or a variable type. A
/// variable sample holds 0 to 4 elements, after the ends and their padding.
fn values(data_type: Type, count: usize) -> Vec<u8> {
    let bytes = |len| (0..len).map(|i| mix(i).to_le_bytes()[0]);
    let width = match data_type {
        Type::List { element, .. } => element.width(),
        Type::String | Type::Bytes => 1,
        fixed => {
            let len = count * fixed.width().expect("an array has a width");
            return bytes(u64::try_from(len).expect("lengths are small")).collect();
        }
    };
    let ends: Vec<u32> = (0_u64..)
        .take(count)
        .scan(0, |end, i| {
            *end += u32::try_from(mix(i) % 5).expect("lengths are small");
            Some(*end)
        })
        .collect();
    let elements = u64::from(ends.last().copied().unwrap_or(0));
    let elements = elements * u64::try_from(width).expect("widths are small");
    let mut values: Vec<u8> = ends.iter().flat_map(|end| end.to_le_bytes()).collect();
    values.resize(values.len().next_multiple_of(width.min(8)), 0);
    values.extend(bytes(elements));
    values
}

/// Encodes, checks, and decodes `values`, the raw bytes of `count` samples of
/// `data_type`, and asserts that no step allocates. Returns the encoded bytes.
fn check(
    encoder: &mut Encoder,
    data_type: Type,
    count: usize,
    values: &[u8],
    case: &str,
) -> Vec<u8> {
    let mut series = vec![0; max_len(data_type, values.len())];
    let mut out = vec![0; values.len()];
    let (written, allocations) =
        ALLOCATOR.count(|| encoder.encode(count, values, &mut series));
    assert_eq!(allocations, 0, "encoding {case} allocated");
    let len = written.unwrap_or_else(|error| panic!("encoding {case}: {error}"));
    series.truncate(len);
    let (valid, allocations) =
        ALLOCATOR.count(|| codec::validate(data_type, count, &series));
    assert_eq!(allocations, 0, "checking {case} allocated");
    assert_eq!(valid, Ok(values.len()), "{case} is valid");
    let (result, allocations) =
        ALLOCATOR.count(|| codec::decode(data_type, count, &series, &mut out));
    assert_eq!(allocations, 0, "decoding {case} allocated");
    assert_eq!(result, Ok(()), "{case} decodes");
    assert_eq!(out, values, "{case} reads back");
    series
}

/// Decodes one full FFOR and one full delta vector of `scalar` at each bit width,
/// which a peer may send although this encoder picks a width only when it pays.
fn decode_every_width(scalar: Scalar) {
    let width = scalar.width();
    let data_type = Type::Scalar(scalar);
    let mut out = vec![0; VECTOR_LEN * width];
    for (tag, fields, packed) in [(FFOR, 1, VECTOR_LEN), (DELTA, 2, VECTOR_LEN - 1)] {
        for bits in 0..=8 * width {
            let header = (2 + fields * width).next_multiple_of(width);
            let body = (packed * bits).div_ceil(8).next_multiple_of(width);
            let mut bytes: Vec<u8> = (0_u64..)
                .take(header + body)
                .map(|i| mix(i).to_le_bytes()[0])
                .collect();
            bytes[0] = tag;
            bytes[1] = u8::try_from(bits).expect("widths are small");
            let (result, allocations) = ALLOCATOR
                .count(|| codec::decode(data_type, VECTOR_LEN, &bytes, &mut out));
            let case = format!("{scalar:?}, tag {tag}, {bits} bits");
            assert_eq!(allocations, 0, "decoding {case} allocated");
            assert_eq!(result, Ok(()), "{case} decodes");
        }
    }
}

/// Checks and decodes one bad series for each error.
fn refuse() {
    let mut out = [0; 2];
    let cases = [
        (
            &[][..],
            Error::Truncated {
                vector: 0,
                needed: 2,
                available: 0,
            },
        ),
        (&[9, 0], Error::Tag { vector: 0, tag: 9 }),
        (
            &[FFOR, 17, 0, 0],
            Error::Width {
                vector: 0,
                bits: 17,
                max: 16,
            },
        ),
        (
            &[3, 0, 1, 0, 7, 0, 2, 0],
            Error::Runs {
                vector: 0,
                total: 2,
                count: 1,
            },
        ),
        (&[0, 0, 7, 0, 9], Error::Trailing { extra: 1 }),
    ];
    for (bytes, error) in cases {
        let (results, allocations) = ALLOCATOR.count(|| {
            (
                codec::validate(Type::Scalar(Scalar::U16), 1, bytes),
                codec::decode(Type::Scalar(Scalar::U16), 1, bytes, &mut out),
                by_vector(Scalar::U16, 1, bytes, &[7, 0]),
            )
        });
        assert_eq!(allocations, 0, "refusing {bytes:?} allocated");
        assert_eq!(
            results,
            (Err(error.clone()), Err(error.clone()), Err(error)),
            "{bytes:?}"
        );
    }
    let (result, allocations) = ALLOCATOR.count(|| {
        Encoder::new(Type::Scalar(Scalar::U16)).encode(2, &[1, 2, 3], &mut out)
    });
    assert_eq!(allocations, 0, "refusing values allocated");
    assert_eq!(
        result,
        Err(Error::Length {
            expected: 4,
            actual: 3,
        }),
        "values that do not fit the count"
    );
    refuse_ends();
}

/// Refuses decreasing ends and a long list sample, raw and encoded.
fn refuse_ends() {
    let list = Type::List {
        element: Scalar::U8,
        max: 1,
    };
    let cases = [
        (
            Type::String,
            [2, 1],
            &b"ab"[..],
            Error::Ends {
                sample: 1,
                end: 1,
                previous: 2,
            },
        ),
        (
            list,
            [1, 3],
            &[0; 3][..],
            Error::Long {
                sample: 1,
                len: 2,
                max: 1,
            },
        ),
    ];
    for (data_type, ends, elements, error) in cases {
        let mut values: Vec<u8> = ends
            .iter()
            .flat_map(|end: &u32| end.to_le_bytes())
            .collect();
        let mut series = vec![0; 64];
        let len = Encoder::new(Type::Scalar(Scalar::U32))
            .encode(2, &values, &mut series)
            .expect("two ends");
        let rest = &mut series[len..];
        let len = len
            + Encoder::new(Type::Scalar(Scalar::U8))
                .encode(elements.len(), elements, rest)
                .expect("the elements");
        series.truncate(len);
        values.extend(elements);
        let mut encoded = vec![0; max_len(data_type, values.len())];
        let mut out = [0; 16];
        let (results, allocations) = ALLOCATOR.count(|| {
            (
                Encoder::new(data_type).encode(2, &values, &mut encoded),
                codec::validate(data_type, 2, &series),
                codec::decode(data_type, 2, &series, &mut out),
            )
        });
        assert_eq!(allocations, 0, "refusing {data_type:?} allocated");
        let expected = (Err(error.clone()), Err(error.clone()), Err(error));
        assert_eq!(results, expected, "{data_type:?}");
    }
}

/// Decodes `bytes` with a [`Decoder`] and checks each vector against the next samples
/// of `values`. Returns the bytes read, or the first error.
fn by_vector(
    scalar: Scalar,
    count: usize,
    bytes: &[u8],
    values: &[u8],
) -> Result<usize, Error> {
    let mut out = [0; VECTOR_LEN * 16];
    let mut decoder = Decoder::new(scalar, count, bytes);
    let mut read = 0;
    while let Some(vector) = decoder.next(&mut out) {
        let vector = vector?;
        assert_eq!(
            vector,
            &values[read..read + vector.len()],
            "the vector at byte {read}"
        );
        read += vector.len();
    }
    Ok(read)
}

/// A fixed pseudo-random value for `i` (the splitmix64 finalizer).
fn mix(i: u64) -> u64 {
    let mut z = i.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

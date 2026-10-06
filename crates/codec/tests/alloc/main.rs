//! Encoding, checking, and decoding a series, whole or one vector at a time, make no
//! heap allocation. This binary has no test harness: the count covers each thread, and
//! a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use codec::{Decoder, Encoder, Error, VECTOR_LEN, max_len};
use types::sample::Scalar;

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
    let most = COUNTS[COUNTS.len() - 1];
    let mut encoder = Encoder::new(scalar);
    let mut series = vec![0; max_len(scalar, most * width)];
    let mut decoded = vec![0; most * width];
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
            let (written, allocations) = ALLOCATOR.count(|| {
                let bound = max_len(scalar, values.len());
                encoder.encode(count, &values, &mut series[..bound])
            });
            assert_eq!(allocations, 0, "encoding {case} allocated");
            let len =
                written.unwrap_or_else(|error| panic!("encoding {case}: {error}"));
            let bytes = &series[..len];
            let (valid, allocations) =
                ALLOCATOR.count(|| codec::validate(scalar, count, bytes));
            assert_eq!(allocations, 0, "checking {case} allocated");
            assert_eq!(valid, Ok(values.len()), "{case} is valid");
            let out = &mut decoded[..values.len()];
            let (result, allocations) =
                ALLOCATOR.count(|| codec::decode(scalar, count, bytes, out));
            assert_eq!(allocations, 0, "decoding {case} allocated");
            assert_eq!(result, Ok(()), "{case} decodes");
            assert_eq!(*out, *values, "{case} reads back");
            let (read, allocations) =
                ALLOCATOR.count(|| by_vector(scalar, count, bytes, &values));
            assert_eq!(allocations, 0, "decoding {case} by vector allocated");
            assert_eq!(read, Ok(values.len()), "{case} decodes by vector");
            if let Some(&tag) = bytes.first() {
                tags[usize::from(tag)] = true;
            }
        }
    }
    tags
}

/// Decodes one full FFOR and one full delta vector of `scalar` at each bit width,
/// which a peer may send although this encoder picks a width only when it pays.
fn decode_every_width(scalar: Scalar) {
    let width = scalar.width();
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
            let (result, allocations) =
                ALLOCATOR.count(|| codec::decode(scalar, VECTOR_LEN, &bytes, &mut out));
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
                codec::validate(Scalar::U16, 1, bytes),
                codec::decode(Scalar::U16, 1, bytes, &mut out),
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
    let (result, allocations) =
        ALLOCATOR.count(|| Encoder::new(Scalar::U16).encode(2, &[1, 2, 3], &mut out));
    assert_eq!(allocations, 0, "refusing values allocated");
    assert_eq!(
        result,
        Err(Error::Length {
            expected: 4,
            actual: 3,
        }),
        "values that do not fit the count"
    );
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

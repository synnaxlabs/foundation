//! Encoding, checking, and decoding a series make no heap allocation. This binary has no
//! test harness: the count covers each thread, and a harness allocates on its own thread
//! at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use codec::{Encoder, VECTOR_LEN, max_len};
use types::sample::Scalar;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

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

const COUNTS: [usize; 5] = [0, 1, VECTOR_LEN - 1, VECTOR_LEN, VECTOR_LEN + 1];

/// Sample `i` of a series that raw, FFOR, delta, or RLE encodes best, by tag.
const SHAPES: [fn(u64) -> u64; 4] = [
    mix,
    |i| 0x50 + (mix(i) & 7),
    |i| i * 3 + (mix(i) & 1),
    |i| i / 256,
];

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );
    let mut tags = [false; 4];
    for scalar in SCALARS {
        for (shape, sample) in SHAPES.iter().enumerate() {
            for count in COUNTS {
                let values: Vec<u8> = (0_u64..)
                    .take(count)
                    .flat_map(|i| {
                        let bytes = u128::from(sample(i)).to_le_bytes();
                        bytes.into_iter().take(scalar.width())
                    })
                    .collect();
                let mut encoded = vec![0; max_len(scalar, count)];
                let mut decoded = vec![0; values.len()];
                let (len, allocations) = ALLOCATOR.count(|| {
                    let len = Encoder::new(scalar).encode(&values, &mut encoded);
                    codec::validate(scalar, count, &encoded[..len])
                        .expect("the encoder writes a valid series");
                    codec::decode(scalar, count, &encoded[..len], &mut decoded)
                        .expect("the encoder writes a valid series");
                    len
                });
                let case = format!("{scalar:?}, shape {shape}, {count} samples");
                assert_eq!(allocations, 0, "{case} allocated");
                assert_eq!(decoded, values, "{case} reads back");
                if let Some(&tag) = encoded[..len].first() {
                    tags[usize::from(tag)] = true;
                }
            }
        }
    }
    assert_eq!(tags, [true; 4], "the shapes select each codec");
}

/// A fixed pseudo-random value for `i` (the splitmix64 finalizer).
fn mix(i: u64) -> u64 {
    let mut z = i.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

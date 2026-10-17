//! Encodes one vector of integers with the codec that one pass of statistics picks.

use crate::vector::{self, Plan};
use crate::word;

/// The order in which the codecs compare the integers of a vector.
// `Signed` first moves each call by 1 to 5 instructions: decode less, encode more
// (box2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Order {
    Unsigned,
    Signed,
}

/// Writes one vector of `W`-byte samples into the front of `out` with the smallest
/// plan for the integers that `sortable` gives, and returns its length. The plan is raw
/// when no codec saves at least 1/8 of the raw size.
///
/// # Panics
///
/// Panics when `chunk` holds no sample.
pub(crate) fn encode<const W: usize>(
    chunk: &[u8],
    order: Order,
    sortable: impl Fn(u64) -> u64 + Copy,
    out: &mut [u8],
) -> usize {
    vector::write::<W>(chunk, plan::<W>(chunk, order, sortable), sortable, out)
}

/// The plan of [`encode`].
pub(crate) fn plan<const W: usize>(
    chunk: &[u8],
    order: Order,
    sortable: impl Fn(u64) -> u64,
) -> Plan {
    // No codec saves 1/8 of one sample. Skips the statistics of a poll.
    if chunk.len() == W {
        return Plan::Raw;
    }
    let mask = word::mask(W);
    let high = mask ^ mask.wrapping_shr(1);
    // Flipping the sign bit makes unsigned order match signed order.
    let sign = match order {
        Order::Signed => high,
        Order::Unsigned => 0,
    };
    let mut samples = word::samples::<W>(chunk).map(sortable);
    let first = samples.next().expect("invariant: a vector holds a sample");
    let (mut min, mut max) = (first ^ sign, first ^ sign);
    let (mut delta_min, mut delta_max) = (u64::MAX, 0);
    let (mut count, mut runs, mut previous) = (1_usize, 1_usize, first);
    for sample in samples {
        let ordered = sample ^ sign;
        min = min.min(ordered);
        max = max.max(ordered);
        let delta = (sample.wrapping_sub(previous) & mask) ^ high;
        delta_min = delta_min.min(delta);
        delta_max = delta_max.max(delta);
        count = count.strict_add(1);
        runs = runs.strict_add(usize::from(sample != previous));
        previous = sample;
    }
    let ffor = Plan::Ffor {
        reference: min ^ sign,
        bits: width(max.strict_sub(min)),
    };
    let delta = (count > 1).then(|| Plan::Delta {
        first,
        base: delta_min ^ high,
        bits: width(delta_max.strict_sub(delta_min)),
    });
    let len = |plan: Plan| plan.len(count, W);
    let best = [delta, Some(Plan::Rle { runs })]
        .into_iter()
        .flatten()
        .fold(
            ffor,
            |best, plan| if len(plan) < len(best) { plan } else { best },
        );
    if len(best).strict_mul(8) <= len(Plan::Raw).strict_mul(7) {
        best
    } else {
        Plan::Raw
    }
}

/// The bits that values up to `range` need.
pub(crate) fn width(range: u64) -> u8 {
    u8::try_from(u64::BITS.strict_sub(range.leading_zeros()))
        .expect("invariant: a u64 needs at most 64 bits")
}

#[cfg(test)]
#[expect(
    clippy::arithmetic_side_effects,
    reason = "tests compute expected values from small inputs"
)]
mod tests {
    use std::convert::identity;

    use proptest::prelude::*;
    use proptest::sample::select;

    use super::*;
    use crate::VECTOR_LEN;

    /// The tag, the bit width, and the length that [`encode`] gives for `chunk`.
    fn encoded(width: usize, chunk: &[u8], order: Order) -> (u8, u8, usize) {
        let mut out = vec![0; chunk.len() + 16];
        let len = match width {
            1 => encode::<1>(chunk, order, identity, &mut out),
            2 => encode::<2>(chunk, order, identity, &mut out),
            4 => encode::<4>(chunk, order, identity, &mut out),
            _ => encode::<8>(chunk, order, identity, &mut out),
        };
        (out[0], out[1], len)
    }

    /// The size of the smallest encoding, computed from the samples as `i128`.
    fn smallest(values: &[i128], width: usize) -> usize {
        let n = values.len();
        let pad = |len: usize| len.next_multiple_of(width);
        let packed = |count: usize, range: i128| {
            let bits = 128 - usize::try_from(range.leading_zeros()).unwrap();
            pad((count * bits).div_ceil(8))
        };
        let (min, max) = (values.iter().min().unwrap(), values.iter().max().unwrap());
        let modulus = 1_i128 << (8 * width);
        let deltas: Vec<i128> = values
            .windows(2)
            .map(|pair| {
                (pair[1] - pair[0] + modulus / 2).rem_euclid(modulus) - modulus / 2
            })
            .collect();
        let runs = 1 + deltas.iter().filter(|delta| **delta != 0).count();
        let raw = pad(2) + n * width;
        let ffor = pad(2 + width) + packed(n, max - min);
        let delta = deltas
            .iter()
            .min()
            .zip(deltas.iter().max())
            .map(|(min, max)| pad(2 + 2 * width) + packed(n - 1, max - min));
        let rle = pad(4) + runs * width + pad(2 * runs);
        let best = [Some(ffor), delta, Some(rle)]
            .into_iter()
            .flatten()
            .min()
            .unwrap();
        if best * 8 <= raw * 7 { best } else { raw }
    }

    fn check(width: usize, order: Order, samples: &[u64]) {
        let chunk: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes().into_iter().take(width))
            .collect();
        let shift = 128 - 8 * u32::try_from(width).unwrap();
        let values: Vec<i128> = samples
            .iter()
            .map(|sample| {
                let value = i128::from(sample & word::mask(width));
                if order == Order::Signed {
                    (value << shift) >> shift
                } else {
                    value
                }
            })
            .collect();
        let (tag, bits, len) = encoded(width, &chunk, order);
        assert_eq!(
            len,
            smallest(&values, width),
            "tag {tag} at {bits} bits for {width}-byte samples {values:?}"
        );
    }

    #[test]
    fn breaks_a_tie_toward_the_earlier_codec() {
        let chunk = [0xff, 0xff, 0, 0, 1, 0, 2, 0];
        let ffor = Plan::Ffor {
            reference: 0xffff,
            bits: 2,
        };
        let delta = Plan::Delta {
            first: 0xffff,
            base: 1,
            bits: 0,
        };
        assert_eq!(ffor.len(4, 2), delta.len(4, 2), "the sizes must tie");
        let (tag, bits, _) = encoded(2, &chunk, Order::Signed);
        assert_eq!((tag, bits), (vector::FFOR, 2));
        let chunk = [[0; 24], [1; 24], [2; 24], [3; 24]].concat();
        let delta = Plan::Delta {
            first: 0,
            base: 0,
            bits: 1,
        };
        let rle = Plan::Rle { runs: 4 };
        assert_eq!(delta.len(96, 1), rle.len(96, 1), "the sizes must tie");
        let (tag, bits, _) = encoded(1, &chunk, Order::Unsigned);
        assert_eq!((tag, bits), (vector::DELTA, 1));
    }

    #[test]
    fn plans_a_vector_of_one_sample_raw() {
        for width in [1, 2, 4, 8] {
            for sample in [0, 1, 0x80, u64::MAX] {
                let chunk = &sample.to_le_bytes()[..width];
                for order in [Order::Signed, Order::Unsigned] {
                    assert_eq!(
                        encoded(width, chunk, order),
                        (vector::RAW, 0, vector::header_len(0, width) + width),
                        "{width}-byte sample {sample:#x}"
                    );
                }
                check(width, Order::Signed, &[sample]);
            }
        }
    }

    proptest! {
        #[test]
        fn picks_the_smallest_exact_size(
            width in select(&[1_usize, 2, 4, 8]),
            order in select(&[Order::Signed, Order::Unsigned]),
            shift in 0..64_u32,
            repeat in 1..300_usize,
            cumulative in any::<bool>(),
            words in proptest::collection::vec(any::<u64>(), 1..1_024),
        ) {
            let samples: Vec<u64> = words
                .iter()
                .flat_map(|word| std::iter::repeat_n(word >> shift, repeat))
                .take(VECTOR_LEN)
                .scan(0_u64, |sum, step| {
                    *sum = if cumulative { sum.wrapping_add(step) } else { step };
                    Some(*sum)
                })
                .collect();
            check(width, order, &samples);
        }
    }
}

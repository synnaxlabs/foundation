//! Picks the codec of one integer vector from one pass of statistics.

use crate::vector::Plan;
use crate::word;

/// The smallest plan for a vector of `W`-byte samples, or raw when no codec saves at
/// least 1/8 of the raw size.
///
/// # Panics
///
/// Panics when `chunk` holds no sample.
pub(crate) fn plan<const W: usize>(chunk: &[u8], signed: bool) -> Plan {
    let mask = word::mask(W);
    let high = mask ^ mask.wrapping_shr(1);
    // Flipping the sign bit makes unsigned order match signed order.
    let sign = if signed { high } else { 0 };
    let mut samples = word::samples::<W>(chunk);
    let first = samples.next().expect("invariant: a vector holds a sample");
    let (mut min, mut max) = (first ^ sign, first ^ sign);
    let (mut delta_min, mut delta_max) = (u64::MAX, 0);
    let (mut count, mut runs, mut previous) = (1_usize, 1_usize, first);
    for sample in samples {
        let key = sample ^ sign;
        min = min.min(key);
        max = max.max(key);
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
fn width(range: u64) -> u8 {
    u8::try_from(u64::BITS.strict_sub(range.leading_zeros()))
        .expect("invariant: a u64 needs at most 64 bits")
}

#[cfg(test)]
#[expect(
    clippy::arithmetic_side_effects,
    reason = "tests compute expected values from small inputs"
)]
mod tests {
    use proptest::prelude::*;
    use proptest::sample::select;

    use super::*;
    use crate::VECTOR_LEN;

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

    fn check(width: usize, signed: bool, samples: &[u64]) {
        let chunk: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes().into_iter().take(width))
            .collect();
        let shift = 128 - 8 * u32::try_from(width).unwrap();
        let values: Vec<i128> = samples
            .iter()
            .map(|sample| {
                let value = i128::from(sample & word::mask(width));
                if signed {
                    (value << shift) >> shift
                } else {
                    value
                }
            })
            .collect();
        let plan = match width {
            1 => plan::<1>(&chunk, signed),
            2 => plan::<2>(&chunk, signed),
            4 => plan::<4>(&chunk, signed),
            _ => plan::<8>(&chunk, signed),
        };
        assert_eq!(
            plan.len(samples.len(), width),
            smallest(&values, width),
            "{plan:?} for {width}-byte samples {values:?}"
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
        assert_eq!(plan::<2>(&chunk, true), ffor);
        let chunk = [[0; 24], [1; 24], [2; 24], [3; 24]].concat();
        let delta = Plan::Delta {
            first: 0,
            base: 0,
            bits: 1,
        };
        let rle = Plan::Rle { runs: 4 };
        assert_eq!(delta.len(96, 1), rle.len(96, 1), "the sizes must tie");
        assert_eq!(plan::<1>(&chunk, false), delta);
    }

    proptest! {
        #[test]
        fn picks_the_smallest_exact_size(
            width in select(&[1_usize, 2, 4, 8]),
            signed in any::<bool>(),
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
            check(width, signed, &samples);
        }
    }
}

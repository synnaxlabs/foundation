//! ALP: floats held as the integers `round(v * 10^e / 10^f)` of an exponent `e` and a
//! factor `f`, with the floats that do not round-trip held raw as exceptions.

use std::cmp::Reverse;

use crate::vector::{self, Plan};
use crate::{bits, int, word};

/// `10^i` for floats of 8 bytes. Each is exact.
const POWERS_64: [f64; 19] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14,
    1e15, 1e16, 1e17, 1e18,
];
/// The floats of 8 bytes nearest to `10^-i`.
const INVERSES_64: [f64; 19] = [
    1e0, 1e-1, 1e-2, 1e-3, 1e-4, 1e-5, 1e-6, 1e-7, 1e-8, 1e-9, 1e-10, 1e-11, 1e-12,
    1e-13, 1e-14, 1e-15, 1e-16, 1e-17, 1e-18,
];
/// `10^i` for floats of 4 bytes. Each is exact.
const POWERS_32: [f32; 11] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10];
/// The floats of 4 bytes nearest to `10^-i`.
const INVERSES_32: [f32; 11] = [
    1e0, 1e-1, 1e-2, 1e-3, 1e-4, 1e-5, 1e-6, 1e-7, 1e-8, 1e-9, 1e-10,
];

/// Adding then subtracting it rounds a float under half of it to an integer, ties to
/// even: `1.5 * 2^52` and `1.5 * 2^23`.
const MAGIC_64: f64 = 6_755_399_441_055_744.0;
const MAGIC_32: f32 = 12_582_912.0;

/// Pairs that an encoder keeps.
const TOP: usize = 5;
/// Samples in one run of a search.
const RUN: usize = 32;
/// Runs of a search of all pairs.
const RUNS: usize = 8;
/// Values after which an encoder searches all pairs again.
const SEARCH: usize = 102_400;

/// The largest exponent for floats of `width` bytes: `10^e` fits their integer.
pub(crate) fn max_exponent(width: usize) -> u8 {
    if width == 4 { 10 } else { 18 }
}

/// An exponent `e` and a factor `f`, with `f <= e`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pair {
    pub(crate) exponent: u8,
    pub(crate) factor: u8,
}

impl Pair {
    /// Each pair for floats of `W` bytes, in search order.
    fn all<const W: usize>() -> impl Iterator<Item = Self> {
        (0..=max_exponent(W)).flat_map(|exponent| {
            (0..=exponent).map(move |factor| Self { exponent, factor })
        })
    }

    /// The place of the pair in [`Pair::all`].
    fn index(self) -> usize {
        let exponent = usize::from(self.exponent);
        exponent
            .strict_mul(exponent.strict_add(1))
            .strict_div(2)
            .strict_add(usize::from(self.factor))
    }
}

/// Converts between the floats of `W` bytes and the integers of one pair.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Coder<const W: usize> {
    /// `10^e` and `10^-f`, widened to `f64`.
    encode: (f64, f64),
    /// `10^f` and `10^-e`, widened to `f64`.
    decode: (f64, f64),
}

impl<const W: usize> Coder<W> {
    /// The coder of `pair`.
    ///
    /// # Panics
    ///
    /// Panics when the exponent of `pair` is over [`max_exponent`].
    pub(crate) fn new(pair: Pair) -> Self {
        let power = |i: u8| {
            let i = usize::from(i);
            let entry = if W == 4 {
                POWERS_32
                    .get(i)
                    .zip(INVERSES_32.get(i))
                    .map(|(power, inverse)| (f64::from(*power), f64::from(*inverse)))
            } else {
                POWERS_64.get(i).copied().zip(INVERSES_64.get(i).copied())
            };
            entry.expect("invariant: the exponent is at most its maximum")
        };
        let (exponent, factor) = (power(pair.exponent), power(pair.factor));
        Self {
            encode: (exponent.0, factor.1),
            decode: (factor.0, exponent.1),
        }
    }

    /// The integer of the float with bits `bits` when it decodes to the same bits.
    #[inline]
    pub(crate) fn exact(self, bits: u64) -> Option<i64> {
        let n = self.integer(bits);
        (self.float(n) == bits).then_some(n)
    }

    /// The integer nearest to the float with bits `bits` times `10^e / 10^f`,
    /// saturated to a signed integer of `W` bytes. Far from an integer, it is any
    /// integer: [`Coder::exact`] then refuses it.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "saturating float to integer casts; the f32 values were f32"
    )]
    #[inline]
    fn integer(self, bits: u64) -> i64 {
        if W == 4 {
            let value = f32::from_bits(bits as u32);
            let scaled = value * self.encode.0 as f32 * self.encode.1 as f32;
            i64::from((scaled + MAGIC_32 - MAGIC_32) as i32)
        } else {
            let scaled = f64::from_bits(bits) * self.encode.0 * self.encode.1;
            (scaled + MAGIC_64 - MAGIC_64) as i64
        }
    }

    /// The bits of the float `n * 10^f * 10^-e`, two multiplications in the float type
    /// of `W` bytes. `n` is a signed integer of `W` bytes.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "n holds W bytes; the f32 values were f32; integers round to floats"
    )]
    #[inline]
    pub(crate) fn float(self, n: i64) -> u64 {
        if W == 4 {
            let scaled = n as i32 as f32 * self.decode.0 as f32 * self.decode.1 as f32;
            scaled.to_bits().into()
        } else {
            (n as f64 * self.decode.0 * self.decode.1).to_bits()
        }
    }
}

/// The signed integer of the low `W` bytes of `value`.
pub(crate) fn signed<const W: usize>(value: u64) -> i64 {
    let shift = 64_u32.strict_sub(u32::from(word::bits(W)));
    value.wrapping_shl(shift).cast_signed().wrapping_shr(shift)
}

/// The integers and exceptions of some samples under one pair. `min` and `max` are 0
/// when each sample is an exception.
struct Stats {
    min: i64,
    max: i64,
    exceptions: usize,
}

impl Stats {
    /// The stats of `samples`, floats of `W` bytes, under `pair`.
    fn of<const W: usize>(samples: &[u8], pair: Pair) -> Self {
        let coder = Coder::<W>::new(pair);
        let mut stats = if W == 4 {
            Self::of_f32(samples, coder)
        } else {
            Self::of_f64(samples, coder).unwrap_or_else(|| Self::of_any(samples, coder))
        };
        if stats.min > stats.max {
            (stats.min, stats.max) = (0, 0);
        }
        stats
    }

    /// [`Stats::of`] for floats of 4 bytes, with no branch, so that it vectorizes.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "the steps of `Coder::integer` and `Coder::float` for 4 bytes"
    )]
    fn of_f32<const W: usize>(samples: &[u8], coder: Coder<W>) -> Self {
        let encode = (coder.encode.0 as f32, coder.encode.1 as f32);
        let decode = (coder.decode.0 as f32, coder.decode.1 as f32);
        let (mut min, mut max, mut exceptions) = (i32::MAX, i32::MIN, 0_u32);
        for sample in samples.as_chunks::<4>().0 {
            let bits = u32::from_le_bytes(*sample);
            let scaled = f32::from_bits(bits) * encode.0 * encode.1;
            let n = (scaled + MAGIC_32 - MAGIC_32) as i32;
            let exact = (n as f32 * decode.0 * decode.1).to_bits() == bits;
            min = if exact { min.min(n) } else { min };
            max = if exact { max.max(n) } else { max };
            // At most `VECTOR_LEN` samples: it never wraps.
            exceptions = exceptions.wrapping_add(u32::from(!exact));
        }
        Self {
            min: min.into(),
            max: max.into(),
            exceptions: exceptions
                .try_into()
                .expect("invariant: a u32 fits a usize"),
        }
    }

    /// [`Stats::of`] for floats of 8 bytes, with no branch and no conversion between
    /// floats and integers, so that it vectorizes. `None` when an integer is outside
    /// `-2^51..2^51`, which only [`Stats::of_any`] gives.
    fn of_f64<const W: usize>(samples: &[u8], coder: Coder<W>) -> Option<Self> {
        const SATURATED: f64 = 9_223_372_036_854_775_808.0;
        const LIMIT: i64 = 1 << 51;
        let magic = MAGIC_64.to_bits().cast_signed();
        // Ends with `min > max` when each sample is an exception.
        let (mut min, mut max, mut exceptions) = (LIMIT - 1, -LIMIT, 0_usize);
        for sample in samples.as_chunks::<8>().0 {
            let bits = u64::from_le_bytes(*sample);
            let scaled = f64::from_bits(bits) * coder.encode.0 * coder.encode.1;
            let rounded = scaled + MAGIC_64;
            // The integer when it is in `-2^51..2^51`, else a value outside.
            let n = rounded.to_bits().cast_signed().wrapping_sub(magic);
            // The integer as a float, truncated and saturated as `Coder::integer`
            // casts it. NaN bits give a finite float and are never exact.
            let float = (rounded - MAGIC_64).trunc();
            let float = if float < SATURATED { float } else { SATURATED };
            let float = if float > -SATURATED {
                float
            } else {
                -SATURATED
            };
            let exact = (float * coder.decode.0 * coder.decode.1).to_bits() == bits;
            min = if exact { min.min(n) } else { min };
            max = if exact { max.max(n) } else { max };
            // At most `VECTOR_LEN` samples: it never wraps.
            exceptions = exceptions.wrapping_add(usize::from(!exact));
        }
        let range = -LIMIT..LIMIT;
        (range.contains(&min) && range.contains(&max)).then_some(Self {
            min,
            max,
            exceptions,
        })
    }

    /// [`Stats::of`] for floats of any width, one sample at a time.
    fn of_any<const W: usize>(samples: &[u8], coder: Coder<W>) -> Self {
        let mut stats = Self {
            min: i64::MAX,
            max: i64::MIN,
            exceptions: 0,
        };
        for bits in word::samples::<W>(samples) {
            match coder.exact(bits) {
                Some(n) => {
                    stats.min = stats.min.min(n);
                    stats.max = stats.max.max(n);
                }
                None => stats.exceptions = stats.exceptions.strict_add(1),
            }
        }
        stats
    }

    /// The smallest integer, as `W` bytes.
    fn reference<const W: usize>(&self) -> u64 {
        self.min.cast_unsigned() & word::mask(W)
    }

    /// The bits of the integers less the reference.
    fn bits(&self) -> u8 {
        int::width(
            self.max
                .cast_unsigned()
                .wrapping_sub(self.min.cast_unsigned()),
        )
    }

    /// The bits of the samples in an ALP body, without padding.
    fn size<const W: usize>(&self, count: usize) -> usize {
        let exception = usize::from(word::bits(W)).strict_add(16);
        count
            .strict_mul(usize::from(self.bits()))
            .strict_add(self.exceptions.strict_mul(exception))
    }
}

/// The pairs that the float series of one encoder pick, best first.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Pairs {
    top: [Pair; TOP],
    len: usize,
    /// Values since the last search of all pairs.
    values: usize,
}

impl Pairs {
    /// The plan of a vector of floats of `W` bytes: ALP when it is smaller than `best`,
    /// the plan of the integer codecs, and saves 1/8 of raw, else `best`.
    pub(crate) fn plan<const W: usize>(&mut self, chunk: &[u8], best: Plan) -> Plan {
        let count = chunk.len().div_euclid(W);
        if count < 2 {
            return best;
        }
        if self.len == 0 || self.values >= SEARCH {
            self.search::<W>(chunk);
            self.values = 0;
        }
        self.values = self.values.strict_add(count);
        let limit = best.len(count, W);
        let mut picked = [0; RUN * 8];
        let picked = pick::<W>(chunk, picked.split_at_mut(RUN.strict_mul(W)).0);
        let (pair, size) = self.best::<W>(picked);
        let estimate = vector::header_len(W.strict_add(4), W).strict_add(
            size.strict_mul(count)
                .div_ceil(picked.len().div_euclid(W).strict_mul(8)),
        );
        if estimate >= limit {
            return best;
        }
        let stats = Stats::of::<W>(chunk, pair);
        let alp = Plan::Alp {
            reference: stats.reference::<W>(),
            bits: stats.bits(),
            pair,
            exceptions: stats.exceptions,
        };
        let len = alp.len(count, W);
        if len < limit && len.strict_mul(8) <= chunk.len().strict_mul(7) {
            alp
        } else {
            best
        }
    }

    /// The pair of the top pairs with the smallest size on `picked`, and that size. It
    /// stops after two pairs in a row that are not smaller.
    fn best<const W: usize>(&self, picked: &[u8]) -> (Pair, usize) {
        let (first, rest) = self
            .top
            .split_at(self.len)
            .0
            .split_first()
            .expect("invariant: a search keeps a pair");
        let size =
            |pair| Stats::of::<W>(picked, pair).size::<W>(picked.len().div_euclid(W));
        let mut best = (*first, size(*first));
        let mut worse = 0_u8;
        for pair in rest {
            let size = size(*pair);
            if size < best.1 {
                best = (*pair, size);
                worse = 0;
            } else {
                worse = worse.strict_add(1);
                if worse == 2 {
                    break;
                }
            }
        }
        best
    }

    /// Keeps the pairs that win most runs of `chunk`, each run the smallest of all
    /// pairs on [`RUN`] samples. Ties go in search order.
    fn search<const W: usize>(&mut self, chunk: &[u8]) {
        let mut picked = [0; RUNS * RUN * 8];
        let picked =
            pick::<W>(chunk, picked.split_at_mut((RUNS * RUN).strict_mul(W)).0);
        let mut wins = [(Pair::default(), 0_usize); RUNS];
        let mut len = 0;
        for run in picked.chunks(RUN.strict_mul(W)) {
            let winner = Pair::all::<W>()
                .min_by_key(|pair| {
                    Stats::of::<W>(run, *pair).size::<W>(run.len().div_euclid(W))
                })
                .expect("invariant: there is a pair");
            let known = wins.split_at_mut(len).0.iter_mut();
            if let Some((_, count)) =
                known.into_iter().find(|(pair, _)| *pair == winner)
            {
                *count = count.strict_add(1);
                continue;
            }
            *wins.get_mut(len).expect("invariant: one winner a run") = (winner, 1);
            len = len.strict_add(1);
        }
        let wins = wins.split_at_mut(len).0;
        wins.sort_unstable_by_key(|(pair, count)| (Reverse(*count), pair.index()));
        self.len = len.min(TOP);
        for (slot, (pair, _)) in self.top.iter_mut().zip(wins.iter()) {
            *slot = *pair;
        }
    }
}

/// Fills the front of `out` with samples of `W` bytes of `chunk` at equal steps, as
/// many as fit, and returns them.
fn pick<'a, const W: usize>(chunk: &[u8], out: &'a mut [u8]) -> &'a [u8] {
    let samples = chunk.as_chunks::<W>().0;
    let len = samples.len().min(out.len().div_euclid(W));
    let step = samples.len().div_euclid(len);
    let out = out.split_at_mut(len.strict_mul(W)).0;
    let slots = out.as_chunks_mut::<W>().0;
    for (slot, sample) in slots.iter_mut().zip(samples.iter().step_by(step)) {
        *slot = *sample;
    }
    out
}

/// Writes the floats of `W` bytes of an ALP vector into `out`, which holds exactly
/// them. `body` is the vector's body, which [`vector::read`] checked.
pub(crate) fn decode<const W: usize>(
    reference: u64,
    bits: u8,
    pair: Pair,
    exceptions: usize,
    body: &[u8],
    out: &mut [u8],
) {
    let coder = Coder::<W>::new(pair);
    let samples = out.as_chunks_mut::<W>().0;
    let packed = bits::len(samples.len(), bits).next_multiple_of(W);
    let (packed, rest) = body.split_at(packed);
    if bits == 0 {
        samples.fill(word::store(coder.float(signed::<W>(reference))));
    } else {
        for (sample, residual) in samples.iter_mut().zip(bits::unpack(packed, bits)) {
            let n = signed::<W>(reference.wrapping_add(residual));
            *sample = word::store(coder.float(n));
        }
    }
    let (values, positions) = rest.split_at(exceptions.strict_mul(W));
    let positions = positions.as_chunks::<2>().0;
    for (value, position) in values.as_chunks::<W>().0.iter().zip(positions) {
        let position = usize::from(u16::from_le_bytes(*position));
        *samples
            .get_mut(position)
            .expect("invariant: read checks each position") = *value;
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// The bytes of `values` as floats of 8 bytes.
    fn bytes(values: &[f64]) -> Vec<u8> {
        values.iter().copied().flat_map(f64::to_le_bytes).collect()
    }

    fn fields(stats: &Stats) -> (i64, i64, usize) {
        (stats.min, stats.max, stats.exceptions)
    }

    fn pair(width: usize) -> impl Strategy<Value = Pair> {
        (0..=max_exponent(width))
            .prop_flat_map(|exponent| (Just(exponent), 0..=exponent))
            .prop_map(|(exponent, factor)| Pair { exponent, factor })
    }

    /// A pair, and floats of `W` bytes: any bits, and the floats of integers near the
    /// bounds of the fast loops under the pair.
    fn samples<const W: usize>() -> impl Strategy<Value = (Pair, Vec<u8>)> {
        pair(W).prop_flat_map(|pair| {
            let bound = prop::sample::select(vec![
                0,
                1 << 23,
                1 << 31,
                1 << 51,
                1 << 52,
                1 << 63,
            ]);
            let float = (bound, -8_i64..=8, any::<bool>()).prop_map(
                move |(bound, offset, negative): (i64, i64, bool)| {
                    let n = bound.wrapping_add(offset);
                    let n = if negative { n.wrapping_neg() } else { n };
                    Coder::<W>::new(pair).float(n)
                },
            );
            let sample = prop_oneof![any::<u64>(), float];
            let samples = prop::collection::vec(sample, 0..64).prop_map(|samples| {
                samples
                    .iter()
                    .flat_map(|sample| sample.to_le_bytes().into_iter().take(W))
                    .collect()
            });
            (Just(pair), samples)
        })
    }

    /// [`Stats::of`] with one sample at a time.
    fn of_any<const W: usize>(samples: &[u8], pair: Pair) -> (i64, i64, usize) {
        let stats = Stats::of_any(samples, Coder::<W>::new(pair));
        if stats.min > stats.max {
            (0, 0, stats.exceptions)
        } else {
            fields(&stats)
        }
    }

    proptest! {
        #[test]
        fn gives_the_stats_of_one_sample_at_a_time((pair, samples) in samples::<8>()) {
            let stats = Stats::of::<8>(&samples, pair);
            prop_assert_eq!(fields(&stats), of_any::<8>(&samples, pair));
        }

        #[test]
        fn gives_the_stats_of_one_sample_at_a_time_for_4_bytes(
            (pair, samples) in samples::<4>(),
        ) {
            let stats = Stats::of::<4>(&samples, pair);
            prop_assert_eq!(fields(&stats), of_any::<4>(&samples, pair));
        }
    }

    /// The integer of a sample under a pair, where the fast loop needs a truncated and
    /// a saturated float, as `Coder::integer` casts.
    #[test]
    fn gives_the_integer_that_the_cast_gives() {
        for (exponent, factor, n) in [
            (1, 0, -4_503_599_627_370_494),
            (8, 3, i64::MAX),
            (8, 3, i64::MIN),
        ] {
            let pair = Pair { exponent, factor };
            let sample = Coder::<8>::new(pair).float(n);
            let stats = Stats::of::<8>(&sample.to_le_bytes(), pair);
            assert_eq!(fields(&stats), (n, n, 0));
        }
    }

    /// The fast loop gives no integer of 2^51 or more, so the one-sample loop does.
    #[test]
    fn gives_an_integer_of_2_51_or_more() {
        let large = 4_503_599_627_370_496.0;
        for (values, stats) in [
            ([large, 1.0], (1, 1 << 52, 0)),
            ([-large, 1.0], (-1 << 52, 1, 0)),
        ] {
            let samples = bytes(&values);
            assert!(
                Stats::of_f64(&samples, Coder::<8>::new(Pair::default())).is_none()
            );
            assert_eq!(fields(&Stats::of::<8>(&samples, Pair::default())), stats);
        }
    }

    #[test]
    fn the_fast_loop_gives_the_stats_of_exceptions_only() {
        let samples = bytes(&[0.5, f64::NAN]);
        let stats = Stats::of_f64(&samples, Coder::<8>::new(Pair::default()));
        assert_eq!(stats.map(|stats| stats.exceptions), Some(2));
    }

    /// Four runs of quarters win `e` = 2, `f` = 0 first, then four runs of integers win
    /// `e` = 0, `f` = 0. The tie goes to the pair that comes first in search order.
    #[test]
    fn search_breaks_ties_in_search_order() {
        let chunk: Vec<u8> = (0..RUNS * RUN)
            .map(|k| {
                let k = f64::from(u32::try_from(k).expect("a small index"));
                if k < 128.0 { k / 4.0 + 0.25 } else { k }
            })
            .flat_map(f64::to_le_bytes)
            .collect();
        let mut pairs = Pairs::default();
        pairs.search::<8>(&chunk);
        let pair = |exponent, factor| Pair { exponent, factor };
        assert_eq!(pairs.top[..pairs.len], [pair(0, 0), pair(2, 0)]);
    }

    /// The factor divides on encode and multiplies on decode: 0.5 is 5 under `e` = 2
    /// and `f` = 1.
    #[test]
    fn codes_with_a_factor() {
        let pair = Pair {
            exponent: 2,
            factor: 1,
        };
        assert_eq!(
            Coder::<4>::new(pair).exact(0.5_f32.to_bits().into()),
            Some(5)
        );
        assert_eq!(Coder::<8>::new(pair).exact(0.5_f64.to_bits()), Some(5));
    }

    #[test]
    fn gives_a_reference_and_bits_of_0_when_each_sample_is_an_exception() {
        let stats = Stats::of::<8>(&bytes(&[f64::NAN; 4]), Pair::default());
        assert_eq!((stats.reference::<8>(), stats.bits()), (0, 0));
    }

    #[test]
    fn gives_the_integer_as_reference_when_each_sample_has_it() {
        let stats = Stats::of::<8>(&bytes(&[7.0; 4]), Pair::default());
        assert_eq!((stats.reference::<8>(), stats.bits()), (7, 0));
    }

    /// The pair that `best` picks from `top`, pairs `(e, f)` in that order, over odd
    /// quarters: `(0, 0)`, `(1, 0)`, and `(1, 1)` take no sample, `(2, 0)` and `(3, 1)`
    /// take each.
    fn best(top: &[(u8, u8)]) -> Pair {
        let picked: Vec<f64> = (0..RUN)
            .map(|k| {
                let k = f64::from(u32::try_from(k).expect("a small k"));
                (2.0 * k + 1.0) / 4.0
            })
            .collect();
        let picked = bytes(&picked);
        let mut pairs = Pairs::default();
        for (slot, (exponent, factor)) in pairs.top.iter_mut().zip(top) {
            *slot = Pair {
                exponent: *exponent,
                factor: *factor,
            };
        }
        pairs.len = top.len();
        pairs.best::<8>(&picked).0
    }

    #[test]
    fn picks_the_smaller_pair_after_one_that_is_not_smaller() {
        let pair = Pair {
            exponent: 2,
            factor: 0,
        };
        assert_eq!(best(&[(0, 0), (2, 0)]), pair);
        assert_eq!(best(&[(1, 0), (0, 0), (2, 0)]), pair);
    }

    #[test]
    fn keeps_the_first_of_two_pairs_of_one_size() {
        let pair = Pair {
            exponent: 2,
            factor: 0,
        };
        assert_eq!(best(&[(2, 0), (3, 1)]), pair);
    }

    #[test]
    fn stops_after_two_pairs_in_a_row_that_are_not_smaller() {
        assert_eq!(best(&[(0, 0), (1, 0), (1, 1), (2, 0)]), Pair::default());
    }
}

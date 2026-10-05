//! A fast, seeded random generator.

use std::fmt;

/// A random generator: xoshiro256++, seeded with `SplitMix64`. The same seed gives the
/// same values on every platform. It is not cryptographic; keys and nonces come from
/// [`Entropy::fill`](crate::entropy::Entropy::fill).
///
/// Take one from [`Entropy::rng`](crate::entropy::Entropy::rng), so that simulation
/// replays it.
///
/// ```
/// let mut rng = env::rng::Rng::from_seed(7);
/// let jitter = rng.below(1_000);
/// assert!(jitter < 1_000);
/// ```
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    /// Makes the generator for `seed`.
    ///
    /// ```
    /// let mut a = env::rng::Rng::from_seed(1);
    /// let mut b = env::rng::Rng::from_seed(1);
    /// assert_eq!(a.next_u64(), b.next_u64());
    /// ```
    #[must_use]
    pub fn from_seed(seed: u64) -> Self {
        let mut x = seed;
        Self {
            s: [
                splitmix(&mut x),
                splitmix(&mut x),
                splitmix(&mut x),
                splitmix(&mut x),
            ],
        }
    }

    /// Returns the next value, uniform over all of `u64`.
    ///
    /// ```
    /// let mut rng = env::rng::Rng::from_seed(0);
    /// let value: u64 = rng.next_u64();
    /// ```
    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[0].wrapping_add(s[3]).rotate_left(23).wrapping_add(s[0]);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// Returns a value uniform in `0..n`, without bias.
    ///
    /// # Panics
    ///
    /// When `n` is zero.
    ///
    /// ```
    /// let mut rng = env::rng::Rng::from_seed(0);
    /// let shard = rng.below(8);
    /// assert!(shard < 8);
    /// ```
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below(0) has no values");
        // Lemire's method: reject the products whose low half would bias the result.
        // A low half of at least `n` is never biased, so most calls skip the division.
        let (mut high, mut low) = widening_mul(self.next_u64(), n);
        if low < n {
            let threshold = n.wrapping_neg() % n;
            while low < threshold {
                (high, low) = widening_mul(self.next_u64(), n);
            }
        }
        high
    }

    /// Fills `bytes` with random values.
    ///
    /// ```
    /// let mut rng = env::rng::Rng::from_seed(0);
    /// let mut payload = [0u8; 64];
    /// rng.fill(&mut payload);
    /// ```
    pub fn fill(&mut self, bytes: &mut [u8]) {
        for chunk in bytes.chunks_mut(8) {
            let value = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&value[..chunk.len()]);
        }
    }
}

impl fmt::Debug for Rng {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rng").finish_non_exhaustive()
    }
}

/// The high and low halves of `a * b`.
fn widening_mul(a: u64, b: u64) -> (u64, u64) {
    let m = u128::from(a) * u128::from(b);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "splits the product into halves"
    )]
    ((m >> 64) as u64, m as u64)
}

/// One step of `SplitMix64`.
fn splitmix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first(seed: u64) -> [u64; 4] {
        let mut rng = Rng::from_seed(seed);
        [
            rng.next_u64(),
            rng.next_u64(),
            rng.next_u64(),
            rng.next_u64(),
        ]
    }

    mod next_u64 {
        use super::*;

        // From the reference C code at prng.di.unimi.it, seeded the same way.
        #[test]
        fn matches_the_reference_for_seed_0() {
            assert_eq!(
                first(0),
                [
                    0x5317_5d61_490b_23df,
                    0x61da_6f3d_c380_d507,
                    0x5c0f_df91_ec9a_7bfc,
                    0x02ee_bf8c_3bbe_5e1a,
                ]
            );
        }

        #[test]
        fn matches_the_reference_for_seed_1() {
            assert_eq!(
                first(1),
                [
                    0xcfc5_d07f_6f03_c29b,
                    0xbf42_4132_963f_e08d,
                    0x19a3_7d57_57aa_f520,
                    0xbf08_119f_05cd_56d6,
                ]
            );
        }

        #[test]
        fn matches_the_reference_for_seed_deadbeef() {
            assert_eq!(
                first(0xdead_beef),
                [
                    0x0c52_0eb8_fea9_8ede,
                    0x2b74_a633_8b80_e0e2,
                    0xbe23_8770_c379_5322,
                    0x5f23_5f98_a244_ea97,
                ]
            );
        }
    }

    mod below {
        use super::*;

        #[test]
        fn stays_in_range_for_many_seeds_and_bounds() {
            for seed in 0..64 {
                let mut rng = Rng::from_seed(seed);
                for n in [1, 2, 3, 7, 1_000, u64::MAX / 3, u64::MAX] {
                    assert!(rng.below(n) < n, "seed {seed}, n {n}");
                }
            }
        }

        // About half of all draws are rejected for this bound. The third value is
        // the first that differs from a generator that rejects none.
        #[test]
        fn rejects_the_draws_that_would_bias_a_large_bound() {
            let mut rng = Rng::from_seed(0);
            let n = (1 << 63) + 1;
            let values = [rng.below(n), rng.below(n), rng.below(n), rng.below(n)];
            assert_eq!(
                values,
                [
                    0x298b_aeb0_a485_91ef,
                    0x30ed_379e_e1c0_6a83,
                    0x6dba_4863_ad5a_8137,
                    0x25be_d050_11c4_f87f,
                ]
            );
        }

        #[test]
        fn reaches_every_value_of_a_small_range() {
            let mut rng = Rng::from_seed(3);
            let mut seen = [false; 6];
            for _ in 0..1_000 {
                seen[usize::try_from(rng.below(6)).unwrap()] = true;
            }
            assert_eq!(seen, [true; 6]);
        }

        #[test]
        #[should_panic(expected = "below(0) has no values")]
        fn panics_on_zero() {
            Rng::from_seed(0).below(0);
        }
    }

    mod fill {
        use super::*;

        #[test]
        fn writes_values_in_little_endian_order() {
            let mut bytes = [0u8; 12];
            Rng::from_seed(0).fill(&mut bytes);
            let [a, b, ..] = first(0);
            assert_eq!(bytes[..8], a.to_le_bytes());
            assert_eq!(bytes[8..], b.to_le_bytes()[..4]);
        }
    }
}

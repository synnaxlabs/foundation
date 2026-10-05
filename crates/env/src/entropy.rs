//! The source of random bytes.

use std::fmt;
use std::sync::Arc;

use crate::Rng;

/// The source of random bytes: the OS in production, the run's seed in simulation.
/// Clones read the same source.
///
/// ```
/// fn jitter(entropy: &env::Entropy) -> u64 {
///     entropy.rng().below(100)
/// }
/// ```
#[derive(Clone)]
pub struct Entropy(Arc<dyn Driver>);

impl Entropy {
    /// Wraps a driver.
    ///
    /// ```
    /// # struct Zeros;
    /// # impl env::entropy::Driver for Zeros {
    /// #     fn fill(&self, bytes: &mut [u8]) { bytes.fill(0) }
    /// # }
    /// let entropy = env::Entropy::new(Zeros);
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Arc::new(driver))
    }

    /// Fills `bytes` from the source. In production the bytes are fit for keys and
    /// nonces.
    ///
    /// ```
    /// fn key(entropy: &env::Entropy) -> [u8; 32] {
    ///     let mut key = [0u8; 32];
    ///     entropy.fill(&mut key);
    ///     key
    /// }
    /// ```
    pub fn fill(&self, bytes: &mut [u8]) {
        self.0.fill(bytes);
    }

    /// Makes a generator seeded from the source.
    ///
    /// ```
    /// fn shard(entropy: &env::Entropy, shards: u64) -> u64 {
    ///     entropy.rng().below(shards)
    /// }
    /// ```
    #[must_use]
    pub fn rng(&self) -> Rng {
        let mut seed = [0u8; 8];
        self.fill(&mut seed);
        Rng::from_seed(u64::from_le_bytes(seed))
    }
}

impl fmt::Debug for Entropy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entropy").finish_non_exhaustive()
    }
}

/// What `os` and `sim` implement to run an [`Entropy`].
///
/// ```
/// use std::sync::Mutex;
///
/// /// Bytes from a seeded generator, so a run replays.
/// struct Seeded(Mutex<env::Rng>);
///
/// impl env::entropy::Driver for Seeded {
///     fn fill(&self, bytes: &mut [u8]) {
///         self.0.lock().expect("invariant: no panic while locked").fill(bytes);
///     }
/// }
/// ```
pub trait Driver: Send + Sync {
    /// Fills `bytes` with random values.
    fn fill(&self, bytes: &mut [u8]);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counting;

    impl Driver for Counting {
        fn fill(&self, bytes: &mut [u8]) {
            for (i, b) in bytes.iter_mut().enumerate() {
                *b = u8::try_from(i).unwrap();
            }
        }
    }

    mod rng {
        use super::*;

        #[test]
        fn seeds_from_eight_little_endian_bytes() {
            let mut from_entropy = Entropy::new(Counting).rng();
            let mut expected = Rng::from_seed(0x0706_0504_0302_0100);
            assert_eq!(from_entropy.next_u64(), expected.next_u64());
        }
    }
}

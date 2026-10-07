//! Channel identity.

use std::fmt;
use std::str::FromStr;

use crate::hash;

/// A channel's identity: a UUIDv7 made with the channel. It never changes and is never
/// reused. Files never hold it; the stored spec maps each name to its key.
///
/// [`Key::v7`] makes new keys. Text, the wire, and disk read back any 128 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(u128);

impl Key {
    /// Makes a UUIDv7 key from a time and random bits. It keeps the time in whole
    /// milliseconds and uses the low 74 bits of `random`.
    ///
    /// # Panics
    ///
    /// When `time` is before the Unix epoch.
    #[must_use]
    pub fn v7(time: crate::time::Stamp, random: u128) -> Self {
        let nanos = time.nanos();
        let millis =
            u128::try_from(nanos.div_euclid(1_000_000)).unwrap_or_else(|_before| {
                panic!(
                    "invariant: a key is made after the Unix epoch, not at {nanos} ns"
                )
            });
        let rand_a = (random >> 62) & 0xfff;
        let rand_b = random & ((1 << 62) - 1);
        Self(millis << 80 | 0x7 << 76 | rand_a << 64 | 0b10 << 62 | rand_b)
    }

    /// Wraps a key's 128 bits.
    #[must_use]
    pub const fn from_u128(bits: u128) -> Self {
        Self(bits)
    }

    /// The key's 128 bits.
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        self.0
    }
}

impl fmt::Display for Key {
    /// Writes the key as a lowercase hyphenated UUID string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        crate::uuid::write(self.0, f)
    }
}

impl FromStr for Key {
    type Err = crate::uuid::Error;

    /// Reads a hyphenated UUID string, in either case.
    ///
    /// # Errors
    ///
    /// [`crate::uuid::Error`] when `s` is not 8-4-4-4-12 hex digits with hyphens.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        crate::uuid::read(s).map(Self)
    }
}

/// A node-local number for a channel, used on the hot path in place of its key. A
/// slot is never sent on the wire or stored on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(u32);

impl Slot {
    /// Wraps a slot number.
    #[must_use]
    pub const fn new(n: u32) -> Self {
        Self(n)
    }

    /// The slot number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// The node's table of channel slots. The node's
/// [`Interner`](crate::frame::key_set::Interner) owns it.
#[derive(Debug, Default)]
pub struct Slots(hash::Map<Key, Slot>);

impl Slots {
    /// A table with no slots.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The slot of `key`. The first call for a key assigns the next slot, from 0. A
    /// slot is never reused. Keys come from the spec or the node's disk, never from a
    /// peer: the table hashes with no key.
    ///
    /// # Panics
    ///
    /// If the table already holds 2^32 channels.
    pub fn assign(&mut self, key: Key) -> Slot {
        let next = self.0.len();
        *self.0.entry(key).or_insert_with(|| {
            Slot(u32::try_from(next).expect("a node holds at most 2^32 channels"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{Span, Stamp};
    use proptest::prelude::*;

    /// The UUIDv7 example in RFC 9562, appendix A.6.
    const EXAMPLE_MILLIS: i64 = 0x017f_22e2_79b0;
    const EXAMPLE_RANDOM: u128 = 0xcc3 << 62 | 0x18c4_dc0c_0c07_398f;
    const EXAMPLE: &str = "017f22e2-79b0-7cc3-98c4-dc0c0c07398f";

    /// One less than the last millisecond a stamp holds.
    const MAX_MILLIS: i64 = i64::MAX / 1_000_000 - 1;

    fn millis(ms: i64) -> Stamp {
        Stamp::from_nanos(ms * 1_000_000)
    }

    mod v7 {
        use super::*;

        fn check(time: Stamp, random: u128, expected: &str) {
            assert_eq!(Key::v7(time, random).to_string(), expected);
        }

        #[test]
        fn matches_the_rfc_example() {
            check(millis(EXAMPLE_MILLIS), EXAMPLE_RANDOM, EXAMPLE);
        }

        #[test]
        fn keeps_whole_milliseconds() {
            let later = millis(EXAMPLE_MILLIS) + Span::from_nanos(999_999);
            check(later, EXAMPLE_RANDOM, EXAMPLE);
        }

        #[test]
        fn keeps_all_74_random_bits() {
            check(
                millis(EXAMPLE_MILLIS),
                u128::MAX,
                "017f22e2-79b0-7fff-bfff-ffffffffffff",
            );
        }

        #[test]
        fn ignores_random_bits_past_the_low_74() {
            check(
                millis(EXAMPLE_MILLIS),
                EXAMPLE_RANDOM | u128::MAX << 74,
                EXAMPLE,
            );
        }

        #[test]
        fn accepts_the_epoch() {
            check(Stamp::EPOCH, 0, "00000000-0000-7000-8000-000000000000");
        }

        #[test]
        #[should_panic(
            expected = "invariant: a key is made after the Unix epoch, not at -1 ns"
        )]
        fn panics_before_the_epoch() {
            let _key = Key::v7(Stamp::from_nanos(-1), 0);
        }

        proptest! {
            #[test]
            fn has_the_version_and_variant(ms in 0..MAX_MILLIS, random in any::<u128>()) {
                let bits = Key::v7(millis(ms), random).as_u128();
                prop_assert_eq!(bits >> 76 & 0xf, 7);
                prop_assert_eq!(bits >> 62 & 0b11, 0b10);
                prop_assert_eq!(bits >> 80, u128::try_from(ms).unwrap());
            }

            #[test]
            fn later_milliseconds_sort_later(
                ms in 0..MAX_MILLIS,
                a in any::<u128>(),
                b in any::<u128>(),
            ) {
                prop_assert!(Key::v7(millis(ms), a) < Key::v7(millis(ms + 1), b));
            }
        }
    }

    mod parse {
        use super::*;

        #[test]
        fn reads_either_case() {
            let key = Key::v7(millis(EXAMPLE_MILLIS), EXAMPLE_RANDOM);
            assert_eq!(EXAMPLE.parse(), Ok(key));
            assert_eq!(EXAMPLE.to_uppercase().parse(), Ok(key));
        }

        #[test]
        fn rejects_other_forms() {
            for text in [
                "",
                "017f22e279b07cc398c4dc0c0c07398f",
                "017f22e2-79b0-7cc3-98c4-dc0c0c07398",
                "017f22e2-79b0-7cc3-98c4-dc0c0c07398f0",
                "017f22e2_79b0-7cc3-98c4-dc0c0c07398f",
                "017f22e-279b0-7cc3-98c4-dc0c0c07398f",
                "017f22e2-79b0-7cc3-98c4-dc0c0c07398g",
                "+17f22e2-79b0-7cc3-98c4-dc0c0c07398f",
                "{17f22e2-79b0-7cc3-98c4-dc0c0c0739}",
                "017f22e2-79b0-7cc3-98c4-dc0c0c0739\u{e9}",
            ] {
                assert_eq!(text.parse::<Key>(), Err(crate::uuid::Error), "{text}");
            }
        }

        proptest! {
            #[test]
            fn keys_round_trip(bits in any::<u128>()) {
                let key = Key::from_u128(bits);
                prop_assert_eq!(key.to_string().parse(), Ok(key));
            }
        }
    }

    #[test]
    fn assigns_dense_slots_once_per_key() {
        let mut slots = Slots::new();
        let a = Key::from_u128(7);
        let b = Key::from_u128(3);
        assert_eq!(slots.assign(a), Slot::new(0));
        assert_eq!(slots.assign(b), Slot::new(1));
        assert_eq!(slots.assign(a), Slot::new(0));
        assert_eq!(slots.assign(Key::from_u128(9)), Slot::new(2));
    }
}

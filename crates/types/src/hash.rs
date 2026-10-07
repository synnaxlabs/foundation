//! Hash maps and sets with a fixed, fast hasher.
//!
//! Iteration order and hashes are the same in every run, so a simulated run replays.
//! The hasher has no key, so a party outside the node that chooses keys freely can
//! make them collide. Never key these maps by such a value: that map needs a keyed
//! hasher with its key from `env` randomness, which comes with its first caller. A
//! QUIC stream ID is not chosen freely: a peer must use its stream IDs in order, and
//! the node limits how many are open.
//!
//! The hasher spreads a key by its low bits, so keys that differ only in their top
//! bits fall in few buckets. A key that the node makes keeps its varying bits low, as
//! the random bits of `channel::Key::v7` are.

use rustc_hash::FxBuildHasher;

/// A hash map with deterministic order and a fast, unkeyed hasher. Build it with
/// `Map::default()`.
#[expect(
    clippy::disallowed_types,
    reason = "the one deterministic alias that replaces HashMap"
)]
pub type Map<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;

/// A hash set with deterministic order and a fast, unkeyed hasher. Build it with
/// `Set::default()`.
#[expect(
    clippy::disallowed_types,
    reason = "the one deterministic alias that replaces HashSet"
)]
pub type Set<T> = std::collections::HashSet<T, FxBuildHasher>;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::hash::BuildHasher;

    use rustc_hash::FxBuildHasher;

    use super::{Map, Set};
    use crate::channel;
    use crate::time::Stamp;

    const RANDOM: u128 = 0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c835;

    #[test]
    fn maps_and_sets_hash_with_the_fx_hasher() {
        let key = 0x0123_4567_89ab_cdef_u64;
        let map = Map::<u64, u64>::default();
        let set = Set::<u64>::default();
        assert_eq!(map.hasher().hash_one(key), FxBuildHasher.hash_one(key));
        assert_eq!(set.hasher().hash_one(key), FxBuildHasher.hash_one(key));
    }

    #[test]
    fn channel_keys_made_by_the_node_spread_over_a_table() {
        let buckets = (0..1_i64 << 14)
            .map(|n| {
                let time = Stamp::from_nanos(1_700_000_000_000_000_000 + n * 250_000);
                let random = u128::try_from(n).unwrap().wrapping_mul(RANDOM);
                FxBuildHasher.hash_one(channel::Key::v7(time, random)) & 0xfff
            })
            .collect::<BTreeSet<_>>()
            .len();
        assert!(
            buckets >= 3_900,
            "16384 keys filled {buckets} of 4096 buckets"
        );
    }

    #[test]
    fn maps_with_the_same_inserts_iterate_in_the_same_order() {
        let build = || {
            let mut map = Map::default();
            for key in 0..1_000_u32 {
                map.insert(key.wrapping_mul(2_654_435_761), key);
            }
            map
        };
        let first: Vec<_> = build().into_iter().collect();
        let second: Vec<_> = build().into_iter().collect();
        assert_eq!(first, second, "same inserts gave a different order");
    }

    #[test]
    fn sets_with_the_same_inserts_iterate_in_the_same_order() {
        let build = || (0..1_000_u64).map(|v| v << 7).collect::<Set<_>>();
        let first: Vec<_> = build().into_iter().collect();
        let second: Vec<_> = build().into_iter().collect();
        assert_eq!(first, second, "same inserts gave a different order");
    }
}

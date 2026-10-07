//! Hash maps and sets with a fixed, fast hasher.
//!
//! Iteration order and hashes are the same in every run, so a simulated run replays.
//! The hasher has no key: never use these maps for keys that a party outside the node
//! chooses. Such a map needs a keyed hasher with its key from `env` randomness, which
//! comes with its first caller.

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
    use std::hash::BuildHasher;

    use rustc_hash::FxBuildHasher;

    use super::{Map, Set};

    #[test]
    fn maps_and_sets_hash_with_the_fx_hasher() {
        let key = 0x0123_4567_89ab_cdef_u64;
        let map = Map::<u64, u64>::default();
        let set = Set::<u64>::default();
        assert_eq!(map.hasher().hash_one(key), FxBuildHasher.hash_one(key));
        assert_eq!(set.hasher().hash_one(key), FxBuildHasher.hash_one(key));
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

//! Hash maps and sets with a fixed hasher.
//!
//! Iteration order and hashes are the same in every run, so a simulated run replays.
//! The hasher has fixed keys: a map keyed by outside input needs a keyed hasher
//! instead, which is not here yet.

use std::hash::{BuildHasherDefault, DefaultHasher};

/// A hash map with deterministic order and hashing. Build it with `Map::default()`.
#[expect(
    clippy::disallowed_types,
    reason = "the one deterministic alias that replaces HashMap"
)]
pub type Map<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<DefaultHasher>>;

/// A hash set with deterministic order and hashing. Build it with `Set::default()`.
#[expect(
    clippy::disallowed_types,
    reason = "the one deterministic alias that replaces HashSet"
)]
pub type Set<T> = std::collections::HashSet<T, BuildHasherDefault<DefaultHasher>>;

#[cfg(test)]
mod tests {
    use super::{Map, Set};

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

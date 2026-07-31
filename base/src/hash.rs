//! The ledger's keys are ids that are already well distributed, so the default SipHash would only
//! add latency to every lookup. `rustc-hash` is the same multiply-rotate hasher the Rust compiler
//! uses on its own maps. Being unseeded is a property this code needs rather than tolerates: the
//! eviction sweep walks a map in its own order, so a run that hashed differently would evict
//! differently.

pub use rustc_hash::{FxBuildHasher, FxHashMap};

#[cfg(test)]
mod tests {
    use std::hash::Hasher;

    use rustc_hash::FxHasher;

    use super::*;

    /// Two properties, and the map depends on both. It has to behave like a map for keys that are
    /// dense and for keys that are far apart, since account ids and transaction ids are both. And it
    /// has to be seeded the same way every time: a run that evicts a different entry than the last
    /// run cannot be compared with it, and eviction walks the map in its own order.
    #[test]
    fn the_map_keeps_every_key_and_orders_itself_the_same_way_every_time() {
        // Dense keys and far-apart keys, which is what account ids and transaction ids look like.
        let keys: Vec<u64> = (0..2_000)
            .chain((1..=2_000).map(|index: u64| index * 2_654_435_761))
            .collect();

        let mut map: FxHashMap<u64, u64> = FxHashMap::default();
        for (position, key) in keys.iter().enumerate() {
            map.insert(*key, position as u64);
        }
        for (position, key) in keys.iter().enumerate() {
            assert_eq!(map.get(key), Some(&(position as u64)), "lost key {key}");
        }

        let mut again: FxHashMap<u64, u64> = FxHashMap::default();
        for (position, key) in keys.iter().enumerate() {
            again.insert(*key, position as u64);
        }
        let first: Vec<u64> = map.keys().copied().collect();
        let second: Vec<u64> = again.keys().copied().collect();
        assert_eq!(
            first, second,
            "the same insertions must walk in the same order"
        );
    }

    /// A wide key is mixed whole: one that differs only in its high half must not land where one
    /// that differs only in its low half lands.
    #[test]
    fn a_wide_key_is_mixed_in_full() {
        let hash = |value: u128| {
            let mut hasher = FxHasher::default();
            hasher.write_u128(value);
            hasher.finish()
        };
        assert_ne!(hash(1), hash(1 << 64));
        assert_ne!(hash(u128::from(u64::MAX)), hash(u128::MAX));
    }
}

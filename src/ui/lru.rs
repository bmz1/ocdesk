//! A tiny least-recently-used map for per-frame render caches, keyed by
//! content hash. Evicting one entry at a time (instead of clearing when
//! full) keeps a large diff that's still on screen from being re-parsed on
//! every redraw.

use std::collections::HashMap;

pub struct Lru<V> {
    map: HashMap<u64, (V, u64)>,
    clock: u64,
    cap: usize,
}

impl<V: Clone> Lru<V> {
    pub fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            clock: 0,
            cap,
        }
    }

    pub fn get(&mut self, key: u64) -> Option<V> {
        self.clock += 1;
        let (v, used) = self.map.get_mut(&key)?;
        *used = self.clock;
        Some(v.clone())
    }

    pub fn insert(&mut self, key: u64, value: V) {
        self.clock += 1;
        if self.map.len() >= self.cap && !self.map.contains_key(&key) {
            // O(cap), only on a miss when full; caps are a few hundred.
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| *k)
            {
                self.map.remove(&oldest);
            }
        }
        self.map.insert(key, (value, self.clock));
    }
}

#[cfg(test)]
mod tests {
    use super::Lru;

    #[test]
    fn evicts_the_least_recently_used_entry_only() {
        let mut c = Lru::new(2);
        c.insert(1, "a");
        c.insert(2, "b");
        assert_eq!(c.get(1), Some("a"));
        c.insert(3, "c");
        assert_eq!(c.get(2), None, "2 was least recently used");
        assert_eq!(c.get(1), Some("a"));
        assert_eq!(c.get(3), Some("c"));
    }
}

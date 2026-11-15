//! A small map that drops its least recently used entries.

use std::collections::HashMap;
use std::hash::Hash;

/// Holds at most `capacity` entries whose weights sum to at most
/// `max_weight`, dropping the least recently used first. An entry heavier
/// than `max_weight` on its own is not stored. Eviction scans every entry,
/// which is fine at the few hundred entries it is used for.
#[derive(Debug)]
pub struct Lru<K, V> {
    capacity: usize,
    max_weight: usize,
    weight: usize,
    clock: u64,
    entries: HashMap<K, Entry<V>>,
}

#[derive(Debug)]
struct Entry<V> {
    value: V,
    weight: usize,
    used: u64,
}

impl<K: Eq + Hash + Clone, V> Lru<K, V> {
    pub fn new(capacity: usize, max_weight: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            max_weight,
            weight: 0,
            clock: 0,
            entries: HashMap::new(),
        }
    }

    /// The value for `key`, marking it as used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        self.get_mut(key).map(|value| &*value)
    }

    /// The value for `key`, marking it as used.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.clock += 1;
        let entry = self.entries.get_mut(key)?;
        entry.used = self.clock;
        Some(&mut entry.value)
    }

    pub fn insert(&mut self, key: K, value: V, weight: usize) {
        self.remove(&key);
        if weight > self.max_weight {
            return;
        }
        self.clock += 1;
        self.weight += weight;
        let entry = Entry {
            value,
            weight,
            used: self.clock,
        };
        self.entries.insert(key, entry);
        while self.entries.len() > self.capacity || self.weight > self.max_weight {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else { break };
            self.remove(&oldest);
        }
    }

    pub fn remove(&mut self, key: &K) {
        if let Some(old) = self.entries.remove(key) {
            self.weight -= old.weight;
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(key, entry)| (key, &entry.value))
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn weight(&self) -> usize {
        self.weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used() {
        let mut lru = Lru::new(2, usize::MAX);
        lru.insert(1, "a", 1);
        lru.insert(2, "b", 1);
        assert_eq!(lru.get(&1), Some(&"a"));
        lru.insert(3, "c", 1);
        assert_eq!(lru.len(), 2);
        assert_eq!(lru.get(&2), None);
        assert_eq!(lru.get(&1), Some(&"a"));
        assert_eq!(lru.get(&3), Some(&"c"));
    }

    #[test]
    fn bounds_total_weight() {
        let mut lru = Lru::new(10, 100);
        lru.insert(1, (), 40);
        lru.insert(2, (), 40);
        lru.insert(3, (), 40);
        assert_eq!((lru.len(), lru.weight()), (2, 80));
        assert!(lru.get(&1).is_none());
        // Replacing an entry replaces its weight.
        lru.insert(2, (), 10);
        assert_eq!((lru.len(), lru.weight()), (2, 50));
        // Too heavy to keep at all, and it displaces nothing.
        lru.insert(4, (), 101);
        assert_eq!((lru.len(), lru.weight()), (2, 50));
        assert!(lru.get(&4).is_none());
    }
}

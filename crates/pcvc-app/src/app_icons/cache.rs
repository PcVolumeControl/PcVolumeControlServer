//! A bounded map from icon key to lookup state.

use std::collections::HashMap;
use std::hash::Hash;

/// The state of one key.
#[derive(Debug, Clone, PartialEq)]
pub enum Slot<T> {
    /// A lookup is in flight.
    Pending,
    /// The lookup finished; `None` means nothing was found.
    Ready(Option<T>),
}

/// Remembers lookups, misses included, up to `capacity` keys. Beyond that
/// the least recently requested finished keys are dropped; pending ones are
/// kept so their results still land.
#[derive(Debug)]
pub struct IconCache<K, T> {
    capacity: usize,
    clock: u64,
    entries: HashMap<K, (Slot<T>, u64)>,
}

#[cfg_attr(not(test), expect(dead_code, reason = "used by the Mixer view next"))]
impl<K: Eq + Hash + Clone, T> IconCache<K, T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            clock: 0,
            entries: HashMap::new(),
        }
    }

    pub fn get(&self, key: &K) -> Option<&Slot<T>> {
        self.entries.get(key).map(|(slot, _)| slot)
    }

    /// Marks `key` as in use. Returns true if it is new, in which case it
    /// is now pending and the caller should look it up.
    pub fn request(&mut self, key: &K) -> bool {
        self.clock += 1;
        if let Some((_, used)) = self.entries.get_mut(key) {
            *used = self.clock;
            return false;
        }
        self.entries
            .insert(key.clone(), (Slot::Pending, self.clock));
        self.evict();
        true
    }

    /// Records a finished lookup. Results for keys no longer pending (or
    /// never requested) are stored too.
    pub fn finish(&mut self, key: K, value: Option<T>) {
        self.clock += 1;
        self.entries.insert(key, (Slot::Ready(value), self.clock));
        self.evict();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn evict(&mut self) {
        while self.entries.len() > self.capacity {
            let oldest = self
                .entries
                .iter()
                .filter(|(_, (slot, _))| !matches!(slot, Slot::Pending))
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => {
                    self.entries.remove(&key);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_hits_and_misses() {
        let mut cache = IconCache::<&str, u8>::new(8);
        assert!(cache.get(&"a").is_none());
        assert!(cache.request(&"a"));
        assert_eq!(cache.get(&"a"), Some(&Slot::Pending));
        assert!(!cache.request(&"a"), "already pending");
        cache.finish("a", None);
        assert_eq!(cache.get(&"a"), Some(&Slot::Ready(None)));
        assert!(!cache.request(&"a"), "a miss is not retried");
        assert!(cache.request(&"b"));
        cache.finish("b", Some(7));
        assert_eq!(cache.get(&"b"), Some(&Slot::Ready(Some(7))));
    }

    #[test]
    fn cap_evicts_least_recently_used_finished_entries() {
        let mut cache = IconCache::<u32, u32>::new(3);
        for k in 0..3 {
            cache.request(&k);
            cache.finish(k, Some(k));
        }
        // Touch 0 so 1 is the oldest.
        cache.request(&0);
        cache.request(&3);
        assert_eq!(cache.len(), 3);
        assert!(cache.get(&1).is_none());
        assert!(cache.get(&0).is_some());
        assert_eq!(cache.get(&3), Some(&Slot::Pending));
    }

    #[test]
    fn pending_entries_survive_eviction() {
        let mut cache = IconCache::<u32, u32>::new(2);
        for k in 0..4 {
            cache.request(&k);
        }
        assert_eq!(cache.len(), 4, "all pending");
        cache.finish(0, None);
        assert_eq!(cache.len(), 3);
        assert!(cache.get(&0).is_none(), "the finished entry went first");
        for k in 1..4 {
            cache.finish(k, Some(k));
        }
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&3), Some(&Slot::Ready(Some(3))));
    }
}

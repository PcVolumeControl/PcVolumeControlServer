//! A bounded map from icon key to lookup state.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// How long a miss is trusted. Requesting the key after that looks it up
/// again, so an application installed later gets its icon.
const MISS_TTL: Duration = Duration::from_secs(120);

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
    /// Each slot with its last use (a counter) and when it was filled.
    entries: HashMap<K, (Slot<T>, u64, Instant)>,
}

impl<K: Eq + Hash + Clone, T> IconCache<K, T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            clock: 0,
            entries: HashMap::new(),
        }
    }

    pub fn get(&self, key: &K) -> Option<&Slot<T>> {
        self.entries.get(key).map(|(slot, _, _)| slot)
    }

    /// Marks `key` as in use. Returns true if it is new or a miss older
    /// than [`MISS_TTL`], in which case it is now pending and the caller
    /// should look it up.
    pub fn request(&mut self, key: &K, now: Instant) -> bool {
        self.clock += 1;
        if let Some((slot, used, filled)) = self.entries.get_mut(key) {
            *used = self.clock;
            let stale_miss = matches!(slot, Slot::Ready(None))
                && now.saturating_duration_since(*filled) >= MISS_TTL;
            if !stale_miss {
                return false;
            }
            *slot = Slot::Pending;
            *filled = now;
            return true;
        }
        self.entries
            .insert(key.clone(), (Slot::Pending, self.clock, now));
        self.evict();
        true
    }

    /// Records a finished lookup. Results for keys no longer pending (or
    /// never requested) are stored too.
    pub fn finish(&mut self, key: K, value: Option<T>, now: Instant) {
        self.clock += 1;
        self.entries
            .insert(key, (Slot::Ready(value), self.clock, now));
        self.evict();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn evict(&mut self) {
        while self.entries.len() > self.capacity {
            let oldest = self
                .entries
                .iter()
                .filter(|(_, (slot, _, _))| !matches!(slot, Slot::Pending))
                .min_by_key(|(_, (_, used, _))| *used)
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
        let t0 = Instant::now();
        let mut cache = IconCache::<&str, u8>::new(8);
        assert!(cache.get(&"a").is_none());
        assert!(cache.request(&"a", t0));
        assert_eq!(cache.get(&"a"), Some(&Slot::Pending));
        assert!(!cache.request(&"a", t0), "already pending");
        cache.finish("a", None, t0);
        assert_eq!(cache.get(&"a"), Some(&Slot::Ready(None)));
        assert!(
            !cache.request(&"a", t0 + MISS_TTL / 2),
            "a fresh miss is kept"
        );
        assert!(cache.request(&"b", t0));
        cache.finish("b", Some(7), t0);
        assert_eq!(cache.get(&"b"), Some(&Slot::Ready(Some(7))));
        // Hits never expire; an old miss is looked up again once.
        let later = t0 + MISS_TTL * 2;
        assert!(!cache.request(&"b", later));
        assert!(cache.request(&"a", later));
        assert_eq!(cache.get(&"a"), Some(&Slot::Pending));
        assert!(!cache.request(&"a", later));
    }

    #[test]
    fn cap_evicts_least_recently_used_finished_entries() {
        let t0 = Instant::now();
        let mut cache = IconCache::<u32, u32>::new(3);
        for k in 0..3 {
            cache.request(&k, t0);
            cache.finish(k, Some(k), t0);
        }
        // Touch 0 so 1 is the oldest.
        cache.request(&0, t0);
        cache.request(&3, t0);
        assert_eq!(cache.len(), 3);
        assert!(cache.get(&1).is_none());
        assert!(cache.get(&0).is_some());
        assert_eq!(cache.get(&3), Some(&Slot::Pending));
    }

    #[test]
    fn pending_entries_survive_eviction() {
        let t0 = Instant::now();
        let mut cache = IconCache::<u32, u32>::new(2);
        for k in 0..4 {
            cache.request(&k, t0);
        }
        assert_eq!(cache.len(), 4, "all pending");
        cache.finish(0, None, t0);
        assert_eq!(cache.len(), 3);
        assert!(cache.get(&0).is_none(), "the finished entry went first");
        for k in 1..4 {
            cache.finish(k, Some(k), t0);
        }
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&3), Some(&Slot::Ready(Some(3))));
    }
}

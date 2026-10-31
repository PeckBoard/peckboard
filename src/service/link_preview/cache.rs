//! A small in-memory LRU with per-entry TTL and an optional total weight
//! (bytes) cap. Sized in the low thousands, so eviction scans linearly.

use std::collections::HashMap;
use std::time::{Duration, Instant};

struct Entry<V> {
    value: V,
    expires: Instant,
    used: u64,
    weight: usize,
}

pub struct TtlLru<V> {
    map: HashMap<String, Entry<V>>,
    max_entries: usize,
    max_weight: usize,
    weight: usize,
    tick: u64,
}

impl<V: Clone> TtlLru<V> {
    pub fn new(max_entries: usize, max_weight: usize) -> Self {
        Self {
            map: HashMap::new(),
            max_entries,
            max_weight,
            weight: 0,
            tick: 0,
        }
    }

    pub fn get(&mut self, key: &str) -> Option<V> {
        self.get_at(key, Instant::now())
    }

    fn get_at(&mut self, key: &str, now: Instant) -> Option<V> {
        self.tick += 1;
        let expired = match self.map.get_mut(key) {
            None => return None,
            Some(e) if e.expires <= now => true,
            Some(e) => {
                e.used = self.tick;
                return Some(e.value.clone());
            }
        };
        if expired {
            self.remove(key);
        }
        None
    }

    pub fn insert(&mut self, key: String, value: V, ttl: Duration, weight: usize) {
        self.insert_at(key, value, ttl, weight, Instant::now());
    }

    fn insert_at(&mut self, key: String, value: V, ttl: Duration, weight: usize, now: Instant) {
        if weight > self.max_weight {
            return;
        }
        self.remove(&key);
        self.tick += 1;
        while !self.map.is_empty()
            && (self.map.len() >= self.max_entries || self.weight + weight > self.max_weight)
        {
            // Expired entries first, else the least recently used.
            let victim = self
                .map
                .iter()
                .min_by_key(|(_, e)| (e.expires > now, e.used))
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => self.remove(&k),
                None => break,
            }
        }
        self.weight += weight;
        self.map.insert(
            key,
            Entry {
                value,
                expires: now + ttl,
                used: self.tick,
                weight,
            },
        );
    }

    fn remove(&mut self, key: &str) {
        if let Some(e) = self.map.remove(key) {
            self.weight -= e.weight;
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.weight = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expires_and_evicts_least_recently_used() {
        let t0 = Instant::now();
        let mut c: TtlLru<u32> = TtlLru::new(2, 100);
        c.insert_at("a".into(), 1, Duration::from_secs(10), 1, t0);
        c.insert_at("b".into(), 2, Duration::from_secs(10), 1, t0);
        assert_eq!(c.get_at("a", t0), Some(1)); // a is now fresher than b
        c.insert_at("c".into(), 3, Duration::from_secs(10), 1, t0);
        assert_eq!(c.get_at("b", t0), None);
        assert_eq!(c.get_at("a", t0), Some(1));
        assert_eq!(c.get_at("a", t0 + Duration::from_secs(11)), None);
        // Weight cap evicts until the newcomer fits; oversize never enters.
        let mut w: TtlLru<u32> = TtlLru::new(10, 10);
        w.insert_at("x".into(), 1, Duration::from_secs(10), 6, t0);
        w.insert_at("y".into(), 2, Duration::from_secs(10), 6, t0);
        assert_eq!(w.get_at("x", t0), None);
        assert_eq!(w.get_at("y", t0), Some(2));
        w.insert_at("z".into(), 3, Duration::from_secs(10), 11, t0);
        assert_eq!(w.get_at("z", t0), None);
    }
}

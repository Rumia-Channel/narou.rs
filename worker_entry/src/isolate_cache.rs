//! Byte- and entry-bounded isolate caches. Reads/writes prune every expired
//! entry, not just the requested key. Values should be immutable `Arc`s so a
//! cache hit does not duplicate a snapshot. No pending future is cached.

use std::collections::HashMap;
use std::sync::Mutex;

struct Entry<V> {
    stored_at: f64,
    bytes: usize,
    value: V,
}

pub struct TtlMap<V> {
    ttl_ms: f64,
    max_bytes: usize,
    weight: fn(&V) -> usize,
    entries: Mutex<HashMap<String, Entry<V>>>,
}

impl<V: Clone> TtlMap<V> {
    pub fn new(ttl_ms: f64, max_bytes: usize, weight: fn(&V) -> usize) -> Self {
        Self { ttl_ms, max_bytes, weight, entries: Mutex::new(HashMap::new()) }
    }

    fn prune(&self, entries: &mut HashMap<String, Entry<V>>, now: f64) {
        entries.retain(|_, entry| now >= entry.stored_at && now - entry.stored_at < self.ttl_ms);
    }

    pub fn get(&self, key: &str) -> Option<V> {
        self.get_at(key, now_ms())
    }

    fn get_at(&self, key: &str, now: f64) -> Option<V> {
        let mut entries = self.entries.lock().ok()?;
        self.prune(&mut entries, now);
        entries.get(key).map(|entry| entry.value.clone())
    }

    pub fn put(&self, key: impl Into<String>, value: V) {
        self.put_at(key.into(), value, now_ms());
    }

    fn put_at(&self, key: String, value: V, now: f64) {
        let Ok(mut entries) = self.entries.lock() else { return };
        self.prune(&mut entries, now);
        entries.remove(&key);
        let bytes = (self.weight)(&value).saturating_add(key.capacity())
            .saturating_add(std::mem::size_of::<Entry<V>>() + 64);
        if bytes > self.max_bytes { return; }
        let mut used: usize = entries.values().map(|entry| entry.bytes).sum();
        // The entry cap also bounds HashMap bookkeeping for empty values.
        while used.saturating_add(bytes) > self.max_bytes || entries.len() >= 128 {
            let oldest = entries.iter().min_by(|(_, a), (_, b)| a.stored_at.total_cmp(&b.stored_at))
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else { break };
            if let Some(entry) = entries.remove(&oldest) { used -= entry.bytes; }
        }
        entries.insert(key, Entry { stored_at: now, bytes, value });
    }

    pub fn invalidate(&self, key: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(key);
            self.prune(&mut entries, now_ms());
        }
    }
}

fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    { js_sys::Date::now() }
    #[cfg(not(target_arch = "wasm32"))]
    { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as f64 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn expiration_prunes_unrelated_keys() {
        let cache = TtlMap::new(30.0, 1024, |value: &Arc<String>| value.capacity());
        cache.put_at("old".into(), Arc::new("snapshot".into()), 0.0);
        assert!(cache.get_at("absent", 31.0).is_none());
        assert!(cache.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn resident_bytes_are_bounded_and_hits_share_snapshots() {
        let cache = TtlMap::new(30.0, 512, |value: &Arc<String>| value.capacity());
        let value = Arc::new("x".repeat(128));
        cache.put_at("first".into(), value.clone(), 0.0);
        assert!(Arc::ptr_eq(&value, &cache.get_at("first", 1.0).unwrap()));
        for n in 0..100 { cache.put_at(n.to_string(), value.clone(), 2.0); }
        assert!(cache.entries.lock().unwrap().values().map(|entry| entry.bytes).sum::<usize>() <= 512);
        cache.put_at("oversized".into(), Arc::new("x".repeat(1024)), 3.0);
        assert!(cache.get_at("oversized", 3.0).is_none());
    }
}

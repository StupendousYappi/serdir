use crate::{compression::CompressionSupport, Resource};
use sieve_cache::ShardedSieveCache;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
// use std::sync::Arc;

const DEFAULT_EXPIRATION_TIME: Duration = Duration::from_mins(1);
const DEFAULT_CAPACITY: usize = 64;

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    path: String,
    encodings: CompressionSupport,
}

impl CacheKey {
    pub(crate) fn new(path: String, encodings: CompressionSupport) -> Self {
        Self { path, encodings }
    }
}

#[derive(Clone)]
struct CacheValue {
    resource: Resource,
    insert_time: Instant,
}

impl CacheValue {
    fn len(&self) -> u64 {
        self.resource.len()
    }
}

/// Settings for caching resource contents.
pub struct CacheSettings {
    max_total_weight: u64,
    max_item_weight: u64,
    expire_after: Duration,
    capacity: usize,
}

impl CacheSettings {
    ///  Creates a new [`CacheSettings`] with the given maximum total weight and capacity.
    ///
    /// The default maximum item weight is 25% of the maximum total weight.
    pub fn new(max_total_weight: u64) -> Self {
        Self {
            max_total_weight,
            max_item_weight: max_total_weight / 4,
            expire_after: DEFAULT_EXPIRATION_TIME,
            capacity: DEFAULT_CAPACITY,
        }
    }

    /// Sets the maximum weight of a single item that can be stored in the cache.
    ///
    /// Items heavier than this will be silently skipped by caching logic.
    ///
    /// The default is 25% of `max_total_weight`.
    pub fn max_item_weight(mut self, weight: u64) -> Self {
        self.max_item_weight = weight;
        self
    }

    /// Sets the expiration time for cached resources.
    ///
    /// The default is 1 minute.
    pub fn expiration_time(mut self, duration: Duration) -> Self {
        self.expire_after = duration;
        self
    }

    /// Sets the maximum number of items the underlying cache is optimized for.
    pub fn capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }
}

impl From<CacheSettings> for ResourceCache {
    fn from(settings: CacheSettings) -> Self {
        Self {
            cache: ShardedSieveCache::new(settings.capacity).expect("capacity cannot be zero"),
            max_total_weight: settings.max_total_weight,
            max_item_weight: settings.max_item_weight,
            current_weight: AtomicU64::new(0),
            expire_after: settings.expire_after,
        }
    }
}

/// A cache of `Resource`s mapped by path.
///
/// It tracks the total size (weight) of the resources it contains and evicts
/// items using a sieve algorithm when the total weight exceeds a maximum threshold.
pub(crate) struct ResourceCache {
    cache: ShardedSieveCache<CacheKey, CacheValue>,
    max_total_weight: u64,
    max_item_weight: u64,
    current_weight: AtomicU64,
    expire_after: Duration,
}

impl ResourceCache {
    /// Gets a cloned `Resource` from the cache if it exists and is not expired.
    pub(crate) fn get(&self, path: &str, encodings: CompressionSupport) -> Option<Resource> {
        let key = CacheKey::new(path.to_string(), encodings);
        if let Some(value) = self.cache.get(&key) {
            if value.insert_time.elapsed() > self.expire_after {
                self.cache.with_key_lock(&key, |shard| {
                    // we fetch again to ensure that the entry wasn't removed by another thread
                    // between the first get and acquiring the lock.
                    if let Some(v) = shard.get(&key) {
                        if v.insert_time.elapsed() > self.expire_after {
                            if let Some(removed) = shard.remove(&key) {
                                self.current_weight
                                    .fetch_sub(removed.len(), Ordering::Relaxed);
                            }
                        }
                    }
                });
                return None;
            }
            return Some(value.resource);
        }
        None
    }

    /// Inserts a `Resource` into the cache.
    ///
    /// The resource will be internally converted to bytes storage using
    /// `Resource::with_bytes_storage()`. If the resource is heavier than
    /// `max_item_weight`, it is silently skipped. If the cache exceeds
    /// `max_total_weight` after insertion, it will evict entries until it drops
    /// below 75% of the max total weight.
    pub(crate) fn insert(&self, path: String, encodings: CompressionSupport, resource: Resource) {
        if resource.len() > self.max_item_weight {
            return;
        }

        let resource = match resource.with_bytes_storage() {
            Ok(r) => r,
            Err(e) => {
                log::warn!("Failed to convert resource to bytes storage: {}", e);
                return;
            }
        };

        let key = CacheKey::new(path, encodings);

        let mut shrink = false;

        let weight = resource.len();
        let value = CacheValue {
            resource,
            insert_time: Instant::now(),
        };

        self.cache.with_key_lock(&key, |cache| {
            let old_weight = cache.get(&key).map(|v| v.len()).unwrap_or(0);

            let is_new = cache.insert(key.clone(), value);

            self.current_weight.fetch_add(weight, Ordering::Relaxed);
            if !is_new {
                self.current_weight.fetch_sub(old_weight, Ordering::Relaxed);
            }

            if self.current_weight.load(Ordering::Relaxed) > self.max_total_weight {
                shrink = true;
            }
        });

        if shrink {
            self.shrink();
        }
    }

    fn shrink(&self) {
        let target_weight = (self.max_total_weight as f64 * 0.75) as u64;
        while self.current_weight.load(Ordering::Relaxed) > target_weight {
            if let Some(evicted) = self.cache.evict() {
                self.current_weight
                    .fetch_sub(evicted.len(), Ordering::Relaxed);
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResourceBuilder;
    use std::time::SystemTime;

    #[test]
    fn test_resource_cache() {
        let cache = ResourceCache::from(CacheSettings::new(100).capacity(10).max_item_weight(50));
        let mtime = SystemTime::now();
        let support = CompressionSupport::default();

        // Should be skipped (too large)
        let large_res = ResourceBuilder::for_bytes(vec![0; 60], mtime).build();
        cache.insert("large".to_string(), support, large_res);
        assert!(cache.get("large", support).is_none());
        assert_eq!(cache.current_weight.load(Ordering::Relaxed), 0);

        // Insert first item
        let res1 = ResourceBuilder::for_bytes(vec![0; 40], mtime).build();
        cache.insert("res1".to_string(), support, res1);
        assert!(cache.get("res1", support).is_some());
        assert_eq!(cache.current_weight.load(Ordering::Relaxed), 40);

        // Insert second item
        let res2 = ResourceBuilder::for_bytes(vec![0; 40], mtime).build();
        cache.insert("res2".to_string(), support, res2);
        assert!(cache.get("res2", support).is_some());
        assert_eq!(cache.current_weight.load(Ordering::Relaxed), 80);

        // Replace second item
        let res2_new = ResourceBuilder::for_bytes(vec![0; 30], mtime).build();
        cache.insert("res2".to_string(), support, res2_new);
        assert_eq!(cache.current_weight.load(Ordering::Relaxed), 70);

        // Insert third item, triggering eviction
        // Target weight becomes 75. Cache weight before insert: 70.
        // Insert 40 -> 110. Will evict res1 (40), leaving 70, which is <= 75.
        // Note: sieve cache eviction order might be non-deterministic if not all items were accessed.
        let res3 = ResourceBuilder::for_bytes(vec![0; 40], mtime).build();
        cache.insert("res3".to_string(), support, res3);

        let w = cache.current_weight.load(Ordering::Relaxed);
        assert!(w <= 75, "Weight {} should be <= 75 after eviction", w);

        // Test with different compression support
        let br_support = CompressionSupport::new(true, false, false);
        let res_br = ResourceBuilder::for_bytes(vec![0; 10], mtime).build();
        cache.insert("res1".to_string(), br_support, res_br.clone());

        // Check that different keys are distinct
        assert!(cache.get("res1", br_support).is_some());
        // We don't strictly assert if the original "res1" with default support is here or not,
        // as eviction order is non-deterministic for small caches.
    }

    #[test]
    fn test_resource_cache_expiration() {
        let cache = ResourceCache::from(
            CacheSettings::new(100)
                .capacity(10)
                .expiration_time(Duration::from_millis(10))
                .max_item_weight(50),
        );
        let mtime = SystemTime::now();
        let support = CompressionSupport::default();

        let res = ResourceBuilder::for_bytes(vec![0; 10], mtime).build();
        cache.insert("res".to_string(), support, res);

        assert!(cache.get("res", support).is_some());
        assert_eq!(cache.current_weight.load(Ordering::Relaxed), 10);

        std::thread::sleep(Duration::from_millis(20));

        assert!(cache.get("res", support).is_none());
        assert_eq!(cache.current_weight.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_resource_cache_default_max_item_weight() {
        // max_total_weight = 100, default max_item_weight = 25
        let cache = ResourceCache::from(CacheSettings::new(100).capacity(10));
        let mtime = SystemTime::now();
        let support = CompressionSupport::default();

        // 26 should be rejected
        let res_too_heavy = ResourceBuilder::for_bytes(vec![0; 26], mtime).build();
        cache.insert("heavy".to_string(), support, res_too_heavy);
        assert!(cache.get("heavy", support).is_none());

        // 25 should be accepted
        let res_ok = ResourceBuilder::for_bytes(vec![0; 25], mtime).build();
        cache.insert("ok".to_string(), support, res_ok);
        assert!(cache.get("ok", support).is_some());
    }
}

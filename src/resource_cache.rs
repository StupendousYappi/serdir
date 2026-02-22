use crate::{compression::CompressionSupport, Resource};
use sieve_cache::ShardedSieveCache;
use std::sync::atomic::{AtomicU64, Ordering};
// use std::sync::Arc;

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct CacheKey {
    path: String,
    encodings: CompressionSupport,
}

impl CacheKey {
    pub(crate) fn new(path: String, encodings: CompressionSupport) -> Self {
        Self { path, encodings }
    }
}

/// A cache of `Resource`s mapped by path.
///
/// It tracks the total size (weight) of the resources it contains and evicts
/// items using a sieve algorithm when the total weight exceeds a maximum threshold.
pub(crate) struct ResourceCache {
    cache: ShardedSieveCache<CacheKey, Resource>,
    max_total_weight: u64,
    max_item_weight: u64,
    current_weight: AtomicU64,
}

impl ResourceCache {
    /// Creates a new `ResourceCache` with the specified maximum total weight and maximum item weight.
    /// Capacity determines the initial/maximum number of items the underlying sieve cache is optimized for.
    pub(crate) fn new(max_total_weight: u64, max_item_weight: u64, capacity: usize) -> Self {
        Self {
            cache: ShardedSieveCache::new(capacity).expect("capacity cannot be zero"),
            max_total_weight,
            max_item_weight,
            current_weight: AtomicU64::new(0),
        }
    }

    /// Gets a cloned `Resource` from the cache if it exists.
    pub(crate) fn get(&self, path: &str, encodings: CompressionSupport) -> Option<Resource> {
        let key = CacheKey::new(path.to_string(), encodings);
        self.cache.get(&key)
    }

    /// Inserts a `Resource` into the cache.
    ///
    /// The resource will be internally converted to bytes storage using
    /// `Resource::with_bytes_storage()`. If the resource is heavier than
    /// `max_item_weight`, it is silently skipped. If the cache exceeds
    /// `max_total_weight` after insertion, it will evict entries until
    /// it drops below 75% of the max total weight.
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

        self.cache.with_key_lock(&key, |cache| {
            let old_weight = cache.get(&key).map(|r| r.len()).unwrap_or(0);
            let weight = resource.len();

            let is_new = cache.insert(key.clone(), resource);

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
        let cache = ResourceCache::new(100, 50, 10);
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
}

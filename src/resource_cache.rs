use crate::Resource;
use sieve_cache::ShardedSieveCache;
use std::sync::atomic::{AtomicU64, Ordering};
// use std::sync::Arc;

/// A cache of `Resource`s mapped by path.
///
/// It tracks the total size (weight) of the resources it contains and evicts
/// items using a sieve algorithm when the total weight exceeds a maximum threshold.
pub struct ResourceCache {
    cache: ShardedSieveCache<String, Resource>,
    max_total_weight: u64,
    max_item_weight: u64,
    current_weight: AtomicU64,
}

impl ResourceCache {
    /// Creates a new `ResourceCache` with the specified maximum total weight and maximum item weight.
    /// Capacity determines the initial/maximum number of items the underlying sieve cache is optimized for.
    pub fn new(max_total_weight: u64, max_item_weight: u64, capacity: usize) -> Self {
        Self {
            cache: ShardedSieveCache::new(capacity).expect("capacity cannot be zero"),
            max_total_weight,
            max_item_weight,
            current_weight: AtomicU64::new(0),
        }
    }

    /// Gets a cloned `Resource` from the cache if it exists.
    pub fn get(&self, path: &str) -> Option<Resource> {
        self.cache.get(path)
    }

    /// Inserts a `Resource` into the cache.
    ///
    /// The resource will be internally converted to bytes storage using
    /// `Resource::with_bytes_storage()`. If the resource is heavier than
    /// `max_item_weight`, it is silently skipped. If the cache exceeds
    /// `max_total_weight` after insertion, it will evict entries until
    /// it drops below 75% of the max total weight.
    pub fn insert(&self, path: String, resource: Resource) {
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

        let mut to_evict = false;

        self.cache.with_key_lock(&path, |cache| {
            let old_weight = cache.get(&path).map(|r| r.len()).unwrap_or(0);
            let weight = resource.len();

            let is_new = cache.insert(path.clone(), resource);

            self.current_weight.fetch_add(weight, Ordering::Relaxed);
            if !is_new {
                self.current_weight.fetch_sub(old_weight, Ordering::Relaxed);
            }

            if self.current_weight.load(Ordering::Relaxed) > self.max_total_weight {
                to_evict = true;
            }
        });

        if to_evict {
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

        // Should be skipped (too large)
        let large_res = ResourceBuilder::for_bytes(vec![0; 60], mtime).build();
        cache.insert("large".to_string(), large_res);
        assert!(cache.get("large").is_none());
        assert_eq!(cache.current_weight.load(Ordering::Relaxed), 0);

        // Insert first item
        let res1 = ResourceBuilder::for_bytes(vec![0; 40], mtime).build();
        cache.insert("res1".to_string(), res1);
        assert!(cache.get("res1").is_some());
        assert_eq!(cache.current_weight.load(Ordering::Relaxed), 40);

        // Insert second item
        let res2 = ResourceBuilder::for_bytes(vec![0; 40], mtime).build();
        cache.insert("res2".to_string(), res2);
        assert!(cache.get("res2").is_some());
        assert_eq!(cache.current_weight.load(Ordering::Relaxed), 80);

        // Replace second item
        let res2_new = ResourceBuilder::for_bytes(vec![0; 30], mtime).build();
        cache.insert("res2".to_string(), res2_new);
        assert_eq!(cache.current_weight.load(Ordering::Relaxed), 70);

        // Insert third item, triggering eviction
        // Target weight becomes 75. Cache weight before insert: 70.
        // Insert 40 -> 110. Will evict res1 (40), leaving 70, which is <= 75.
        // Note: sieve cache eviction order might be non-deterministic if not all items were accessed.
        let res3 = ResourceBuilder::for_bytes(vec![0; 40], mtime).build();
        cache.insert("res3".to_string(), res3);

        let w = cache.current_weight.load(Ordering::Relaxed);
        assert!(w <= 75, "Weight {} should be <= 75 after eviction", w);
    }
}

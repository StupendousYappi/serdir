use crate::{compression::CompressionSupport, Resource};
use sieve_cache::{Weigh, WeightedShardedSieveCache};
use std::time::{Duration, Instant};

const DEFAULT_MAX_TOTAL_WEIGHT: u64 = 8_000_000;
const DEFAULT_EXPIRATION_TIME: Duration = Duration::from_secs(120);
const FIXED_CAPACITY: usize = 128;

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

impl Weigh for CacheValue {
    fn weigh(&self) -> usize {
        let stack_size: usize = size_of::<CacheValue>();
        let heap_size = self.resource.len() as usize;
        stack_size + heap_size
    }
}

impl Weigh for CacheKey {
    fn weigh(&self) -> usize {
        let stack_size: usize = size_of::<CacheKey>();
        let heap_size = self.path.len();
        stack_size + heap_size
    }
}

/// Settings for caching resource contents.
pub struct CacheSettings {
    max_total_weight: u64,
    max_item_weight: Option<u64>,
    expire_after: Duration,
}

impl CacheSettings {
    /// Creates a new [`CacheSettings`] with default values.
    ///
    /// The default maximum item weight is 25% of the maximum total weight.
    pub fn new() -> Self {
        Self {
            max_total_weight: DEFAULT_MAX_TOTAL_WEIGHT,
            max_item_weight: None,
            expire_after: DEFAULT_EXPIRATION_TIME,
        }
    }

    /// Sets the maximum total weight of all items that can be stored in the cache.
    ///
    /// If the value is less than 16 bytes, it will be set to 16 bytes.
    ///
    /// The default is 8MB.
    pub fn max_total_weight(mut self, weight: u64) -> Self {
        self.max_total_weight = weight.max(16);
        self
    }

    /// Sets the maximum weight of a single item that can be stored in the cache.
    ///
    /// Items heavier than this will be silently skipped by caching logic.
    ///
    /// The default is 25% of `max_total_weight`.
    pub fn max_item_weight(mut self, weight: u64) -> Self {
        self.max_item_weight = Some(weight);
        self
    }

    /// Sets the expiration time for cached resources.
    ///
    /// The default is 2 minutes.
    pub fn expiration_time(mut self, duration: Duration) -> Self {
        self.expire_after = duration;
        self
    }

    /// Converts these settings into a [`ResourceCache`].
    pub(crate) fn into_resource_cache(self) -> ResourceCache {
        let cache = WeightedShardedSieveCache::new(FIXED_CAPACITY, self.max_total_weight as usize)
            .expect("max_total_weight validation in CacheSettings::new makes this safe");
        ResourceCache {
            cache,
            max_item_weight: self.max_item_weight.unwrap_or(self.max_total_weight / 4),
            expire_after: self.expire_after,
        }
    }
}

impl Default for CacheSettings {
    fn default() -> Self {
        Self::new()
    }
}

/// A cache of `Resource`s mapped by path.
///
/// It tracks the total size (weight) of the resources it contains and evicts
/// items using a sieve algorithm when the total weight exceeds a maximum threshold.
pub(crate) struct ResourceCache {
    cache: WeightedShardedSieveCache<CacheKey, CacheValue>,
    max_item_weight: u64,
    expire_after: Duration,
}

impl ResourceCache {
    pub(crate) fn get(&self, path: &str, encodings: CompressionSupport) -> Option<Resource> {
        let key = CacheKey::new(path.to_string(), encodings);
        if let Some(value) = self.cache.get(&key) {
            if value.insert_time.elapsed() > self.expire_after {
                // Take the opportunity to cleanup all expired entries, not just this one.
                let now = Instant::now();
                self.cache
                    .retain(|_, v| now.duration_since(v.insert_time) < self.expire_after);
                return None;
            }
            return Some(value.resource);
        }
        None
    }

    pub(crate) fn insert(&self, path: String, encodings: CompressionSupport, resource: Resource) {
        if resource.len() > self.max_item_weight {
            return;
        }

        let resource = match resource.with_bytes_storage() {
            Ok(r) => r,
            Err(e) => {
                log::warn!("Failed to convert resource to bytes storage: {e}");
                return;
            }
        };

        let key = CacheKey::new(path, encodings);

        let value = CacheValue {
            resource,
            insert_time: Instant::now(),
        };

        self.cache.insert(key, value);
    }

    #[cfg(test)]
    pub(crate) fn current_weight(&self) -> usize {
        self.cache.current_weight()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResourceBuilder;
    use std::time::SystemTime;

    #[test]
    fn test_resource_cache() {
        let cache = CacheSettings::new()
            .max_total_weight(1000)
            .max_item_weight(500)
            .into_resource_cache();
        let mtime = SystemTime::now();
        let support = CompressionSupport::default();

        // Should be skipped (too large)
        let large_res = ResourceBuilder::for_bytes(vec![0; 600], mtime).build();
        cache.insert("large".to_string(), support, large_res);
        assert!(cache.get("large", support).is_none());
        assert_eq!(cache.current_weight(), 0);

        // Insert first item
        let res1 = ResourceBuilder::for_bytes(vec![0; 40], mtime).build();
        cache.insert("res1".to_string(), support, res1);
        assert!(cache.get("res1", support).is_some());
        assert!(cache.current_weight() > 40);
        let w1 = cache.current_weight();

        // Insert second item
        let res2 = ResourceBuilder::for_bytes(vec![0; 40], mtime).build();
        cache.insert("res2".to_string(), support, res2);
        assert!(cache.get("res2", support).is_some());
        assert!(cache.current_weight() > w1);
        let w2 = cache.current_weight();

        // Replace second item
        let res2_new = ResourceBuilder::for_bytes(vec![0; 30], mtime).build();
        cache.insert("res2".to_string(), support, res2_new);
        assert!(cache.current_weight() < w2);

        // Insert third item, triggering eviction
        let res3 = ResourceBuilder::for_bytes(vec![0; 400], mtime).build();
        cache.insert("res3".to_string(), support, res3);

        let w = cache.current_weight();
        assert!(w <= 1000, "Weight {} should be <= 1000 after eviction", w);

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
        let cache = CacheSettings::new()
            .max_total_weight(1000)
            .expiration_time(Duration::from_millis(10))
            .max_item_weight(500)
            .into_resource_cache();
        let mtime = SystemTime::now();
        let support = CompressionSupport::default();

        let res = ResourceBuilder::for_bytes(vec![0; 10], mtime).build();
        cache.insert("res".to_string(), support, res);

        assert!(cache.get("res", support).is_some());
        assert!(cache.current_weight() > 0);

        std::thread::sleep(Duration::from_millis(20));

        assert!(cache.get("res", support).is_none());
        assert_eq!(cache.current_weight(), 0);
    }

    #[test]
    fn test_resource_cache_default_max_item_current_weight() {
        // max_total_weight = 100, default max_item_weight = 25
        let cache = CacheSettings::new()
            .max_total_weight(100)
            .into_resource_cache();
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

    #[test]
    fn test_cache_settings_max_total_weight() {
        // Test that setting a valid weight works
        let settings = CacheSettings::new().max_total_weight(100);
        assert_eq!(settings.max_total_weight, 100);

        let settings = settings.max_total_weight(200);
        assert_eq!(settings.max_total_weight, 200);

        // Test that the minimum weight floor of 16 is enforced
        let settings = CacheSettings::new().max_total_weight(15);
        assert_eq!(settings.max_total_weight, 16);

        let settings = CacheSettings::new().max_total_weight(0);
        assert_eq!(settings.max_total_weight, 16);
    }
}

//! The default memory backend: foyer's in-memory cache, which implements every
//! [`EvictionPolicy`].

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use foyer_common::event::{Event, EventListener};
use foyer_memory::{
    Cache, CacheBuilder, EvictionConfig, FifoConfig, LfuConfig, LruConfig, S3FifoConfig,
    SieveConfig,
};

use super::{
    BackendUsage, EvictionListener, EvictionPolicy, MemEntry, MemoryBackend, MemoryConfig,
    UnsupportedPolicy,
};
use crate::key::CacheKey;

/// A [`MemoryBackend`] on `foyer-memory`.
pub struct FoyerBackend {
    cache: Cache<CacheKey, MemEntry>,
    entries: Arc<AtomicU64>,
}

impl FoyerBackend {
    /// Builds the cache. foyer offers every policy, so this cannot fail.
    pub(super) fn build(config: &MemoryConfig, on_evict: EvictionListener) -> Self {
        let entries = Arc::new(AtomicU64::new(0));
        let listener = Listener {
            entries: entries.clone(),
            on_evict,
        };
        let cache = CacheBuilder::new(usize::try_from(config.capacity_bytes).unwrap_or(usize::MAX))
            .with_name("mammoth-memory")
            .with_shards(config.shards())
            .with_eviction_config(eviction_config(config.eviction))
            .with_weighter(|key: &CacheKey, entry: &MemEntry| entry.weight(key))
            .with_event_listener(Arc::new(listener))
            .build();
        Self { cache, entries }
    }
}

/// foyer's configuration for each policy: its defaults, except plain LRU without foyer's
/// high-priority pool.
fn eviction_config(policy: EvictionPolicy) -> EvictionConfig {
    match policy {
        EvictionPolicy::S3Fifo => S3FifoConfig::default().into(),
        EvictionPolicy::Lru => LruConfig {
            high_priority_pool_ratio: 0.0,
        }
        .into(),
        EvictionPolicy::WTinyLfu => LfuConfig::default().into(),
        EvictionPolicy::Sieve => SieveConfig.into(),
        EvictionPolicy::Fifo => FifoConfig::default().into(),
    }
}

impl MemoryBackend for FoyerBackend {
    fn new(config: &MemoryConfig, on_evict: EvictionListener) -> Result<Self, UnsupportedPolicy> {
        Ok(Self::build(config, on_evict))
    }

    fn get(&self, key: &CacheKey) -> Option<MemEntry> {
        self.cache.get(key).map(|entry| entry.value().clone())
    }

    fn contains(&self, key: &CacheKey) -> bool {
        self.cache.contains(key)
    }

    fn insert(&self, key: CacheKey, entry: MemEntry) {
        // Count first: the listener may see this entry leave before `insert` returns.
        self.entries.fetch_add(1, Ordering::Relaxed);
        self.cache.insert(key, entry);
    }

    fn remove(&self, key: &CacheKey) -> Option<MemEntry> {
        self.cache.remove(key).map(|entry| entry.value().clone())
    }

    fn usage(&self) -> BackendUsage {
        BackendUsage {
            entries: self.entries.load(Ordering::Relaxed),
            weight: self.cache.usage() as u64,
            capacity: self.cache.capacity() as u64,
        }
    }
}

impl fmt::Debug for FoyerBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FoyerBackend")
            .field("usage", &self.usage())
            .finish_non_exhaustive()
    }
}

/// Keeps the entry count and reports evictions. foyer calls it under a shard lock, so it only
/// touches atomics.
struct Listener {
    entries: Arc<AtomicU64>,
    on_evict: EvictionListener,
}

impl EventListener for Listener {
    type Key = CacheKey;
    type Value = MemEntry;

    fn on_leave(&self, reason: Event, _key: &CacheKey, _value: &MemEntry) {
        self.entries.fetch_sub(1, Ordering::Relaxed);
        if matches!(reason, Event::Evict) {
            (self.on_evict)();
        }
    }
}

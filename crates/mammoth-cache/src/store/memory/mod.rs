//! L1: the memory tier (design doc 0010).
//!
//! [`MemoryStore`] implements the blob store contract once (streaming, put-if-absent for immutable
//! keys, usage and eviction metrics) and delegates storage and eviction to a [`MemoryBackend`]. The
//! backend is a type parameter, so the hot path is statically dispatched; [`FoyerBackend`] is the
//! default. Entries are shared `Bytes`, so a hit is a reference-count bump.

mod foyer;

use std::{fmt, io, str::FromStr, sync::Arc};

use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};

pub use self::foyer::FoyerBackend;
use super::{
    Blob, BlobStore, BoundedStore, BoxFuture, ByteStream, PutOutcome, Tier, TierUsage,
    meta::{Checksum, EntryMeta, unix_now},
};
use crate::key::CacheKey;

/// Bytes an entry costs beyond its key and body: the cache record, the hash-table slot, the
/// `Bytes` handle and [`EntryMeta`]. Counted in every entry's weight, so a tier full of tiny
/// entries still stays within its capacity.
pub const ENTRY_OVERHEAD: usize = 160;

/// Each shard holds at least this many maximum-size entries.
const MIN_ENTRIES_PER_SHARD: u64 = 64;

/// How the memory tier chooses what to evict: `memory.eviction` in `mammoth.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EvictionPolicy {
    /// Small probationary FIFO, main FIFO and a ghost queue. Robust to one-hit wonders and scans.
    #[default]
    S3Fifo,
    /// Least recently used.
    Lru,
    /// Window LRU with frequency-sketch admission into a segmented LRU.
    WTinyLfu,
    /// FIFO with a visited bit and a moving hand.
    Sieve,
    /// First in, first out.
    Fifo,
}

impl EvictionPolicy {
    /// Every policy, in documentation order.
    pub const ALL: [Self; 5] = [
        Self::S3Fifo,
        Self::Lru,
        Self::WTinyLfu,
        Self::Sieve,
        Self::Fifo,
    ];

    /// The policy's name in configuration and logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::S3Fifo => "s3-fifo",
            Self::Lru => "lru",
            Self::WTinyLfu => "w-tinylfu",
            Self::Sieve => "sieve",
            Self::Fifo => "fifo",
        }
    }
}

impl fmt::Display for EvictionPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EvictionPolicy {
    type Err = UnknownPolicy;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|policy| policy.as_str() == s)
            .ok_or_else(|| UnknownPolicy(s.to_owned()))
    }
}

/// A `memory.eviction` value that names no policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownPolicy(pub String);

impl fmt::Display for UnknownPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown eviction policy {:?}; expected one of ", self.0)?;
        for (i, policy) in EvictionPolicy::ALL.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(policy.as_str())?;
        }
        Ok(())
    }
}

impl std::error::Error for UnknownPolicy {}

/// A policy the chosen backend does not implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedPolicy(pub EvictionPolicy);

impl fmt::Display for UnsupportedPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "this build's memory backend does not offer the {} policy",
            self.0
        )
    }
}

impl std::error::Error for UnsupportedPolicy {}

/// Settings for the memory tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryConfig {
    /// The most bytes the tier holds, counting keys and [`ENTRY_OVERHEAD`].
    pub capacity_bytes: u64,
    /// The largest body the tier keeps. Larger entries are read and discarded.
    pub max_entry_bytes: u64,
    /// What to evict when full.
    pub eviction: EvictionPolicy,
}

impl MemoryConfig {
    /// A config with the default policy, keeping entries up to a 64th of the capacity.
    #[must_use]
    pub fn new(capacity_bytes: u64) -> Self {
        Self {
            capacity_bytes,
            max_entry_bytes: capacity_bytes / MIN_ENTRIES_PER_SHARD,
            eviction: EvictionPolicy::default(),
        }
    }

    /// How many shards to split the capacity into: the smallest power of two at least the number
    /// of cores, halved until each shard holds 64 maximum-size entries.
    #[must_use]
    pub fn shards(&self) -> usize {
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        let floor = self.max_entry_bytes.saturating_mul(MIN_ENTRIES_PER_SHARD);
        let mut shards = cores.next_power_of_two();
        while shards > 1 && self.capacity_bytes / (shards as u64) < floor {
            shards /= 2;
        }
        shards
    }
}

/// An entry held in memory: its metadata and its bytes.
#[derive(Debug, Clone)]
pub struct MemEntry {
    /// The entry's metadata.
    pub meta: EntryMeta,
    /// The entry's bytes, in one exact-size allocation.
    pub body: Bytes,
}

impl MemEntry {
    /// What the entry costs against the tier's capacity.
    #[must_use]
    pub fn weight(&self, key: &CacheKey) -> usize {
        self.body.len() + key.as_str().len() + ENTRY_OVERHEAD
    }
}

/// Called once for each entry a backend evicts to make room.
pub type EvictionListener = Arc<dyn Fn() + Send + Sync>;

/// A backend's population.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackendUsage {
    /// Entries held.
    pub entries: u64,
    /// Sum of the entries' weights.
    pub weight: u64,
    /// The configured capacity.
    pub capacity: u64,
}

/// A concurrent, byte-weighted cache with a configurable eviction policy.
///
/// Implementations weigh entries with [`MemEntry::weight`] and call the listener for each entry
/// evicted to make room, but not for replacements or removals.
pub trait MemoryBackend: Send + Sync + fmt::Debug + 'static {
    /// Builds the cache. Fails if this backend does not offer `config.eviction`.
    fn new(config: &MemoryConfig, on_evict: EvictionListener) -> Result<Self, UnsupportedPolicy>
    where
        Self: Sized;

    /// Looks up an entry, recording the access with the eviction policy.
    fn get(&self, key: &CacheKey) -> Option<MemEntry>;

    /// Whether an entry is present, without recording an access.
    fn contains(&self, key: &CacheKey) -> bool;

    /// Inserts an entry, replacing any existing one, and evicts per the policy to make room.
    fn insert(&self, key: CacheKey, entry: MemEntry);

    /// Removes an entry.
    fn remove(&self, key: &CacheKey) -> Option<MemEntry>;

    /// Current population.
    fn usage(&self) -> BackendUsage;
}

/// The memory tier: a [`BlobStore`] over a [`MemoryBackend`].
#[derive(Debug)]
pub struct MemoryStore<B: MemoryBackend = FoyerBackend> {
    backend: B,
    config: MemoryConfig,
}

impl MemoryStore {
    /// Creates an empty store on the default backend with the default policy.
    #[must_use]
    pub fn new(capacity_bytes: u64) -> Self {
        let config = MemoryConfig::new(capacity_bytes);
        let on_evict = eviction_counter();
        Self {
            backend: FoyerBackend::build(&config, on_evict),
            config,
        }
    }
}

impl<B: MemoryBackend> MemoryStore<B> {
    /// Creates an empty store. Fails if the backend lacks the configured policy.
    pub fn with_config(config: MemoryConfig) -> Result<Self, UnsupportedPolicy> {
        let backend = B::new(&config, eviction_counter())?;
        tracing::info!(
            capacity_bytes = config.capacity_bytes,
            max_entry_bytes = config.max_entry_bytes,
            eviction = %config.eviction,
            shards = config.shards(),
            "memory tier ready",
        );
        Ok(Self { backend, config })
    }

    /// The tier's settings.
    #[must_use]
    pub fn config(&self) -> &MemoryConfig {
        &self.config
    }
}

impl<B: MemoryBackend> BlobStore for MemoryStore<B> {
    fn tier(&self) -> Tier {
        Tier::Memory
    }

    fn get<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<Blob>>> {
        Box::pin(async move {
            Ok(self.backend.get(key).map(|entry| {
                let body = entry.body;
                Blob {
                    size: entry.meta.size,
                    body: Box::pin(stream::once(async move { Ok(body) })),
                }
            }))
        })
    }

    fn head<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<u64>>> {
        Box::pin(async move { Ok(self.backend.get(key).map(|entry| entry.meta.size)) })
    }

    fn put<'a>(
        &'a self,
        key: &'a CacheKey,
        size_hint: Option<u64>,
        mut body: ByteStream,
    ) -> BoxFuture<'a, io::Result<PutOutcome>> {
        Box::pin(async move {
            let immutable = key.namespace().is_immutable();
            if immutable && self.backend.contains(key) {
                return Ok(PutOutcome::Exists);
            }
            let limit = self.config.max_entry_bytes;
            let mut keep = size_hint.is_none_or(|n| n <= limit);
            let capacity = size_hint
                .filter(|_| keep)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(0);
            let mut buf = BytesMut::with_capacity(capacity);
            let mut checksum = Checksum::EMPTY;
            // An error mid-stream returns before the insert, so nothing partial is kept.
            while let Some(chunk) = body.next().await {
                let chunk = chunk?;
                if keep && (buf.len() + chunk.len()) as u64 > limit {
                    keep = false;
                    buf = BytesMut::new();
                }
                if keep {
                    checksum = checksum.update(&chunk);
                    buf.extend_from_slice(&chunk);
                }
            }
            if !keep {
                // Too big for memory. For a cache, an entry not kept is a later miss, not an error.
                tracing::debug!(%key, limit, "entry too large for the memory tier; not kept");
                return Ok(PutOutcome::Stored);
            }
            if immutable && self.backend.contains(key) {
                return Ok(PutOutcome::Exists);
            }
            let body = exact_size(buf);
            let meta = EntryMeta {
                size: body.len() as u64,
                checksum,
                created: unix_now(),
            };
            self.backend.insert(key.clone(), MemEntry { meta, body });
            Ok(PutOutcome::Stored)
        })
    }
}

impl<B: MemoryBackend> BoundedStore for MemoryStore<B> {
    fn usage(&self) -> TierUsage {
        let usage = self.backend.usage();
        TierUsage {
            entries: usage.entries,
            size_bytes: usage.weight,
            capacity_bytes: usage.capacity,
        }
    }
}

/// Counts evictions in `mammoth_tier_evictions_total` (design doc 0004).
fn eviction_counter() -> EvictionListener {
    Arc::new(|| {
        metrics::counter!(
            "mammoth_tier_evictions_total",
            "tier" => Tier::Memory.as_str(),
            "reason" => "size",
        )
        .increment(1);
    })
}

/// Freezes a buffer, copying it into an exact-size allocation when growth left more than an eighth
/// spare, so that an entry's weight matches the memory it holds.
fn exact_size(buf: BytesMut) -> Bytes {
    if buf.capacity() - buf.len() > buf.len() / 8 {
        Bytes::copy_from_slice(&buf)
    } else {
        buf.freeze()
    }
}

#[cfg(test)]
mod tests {
    use futures_util::TryStreamExt;

    use super::*;

    fn gradle(key: &str) -> CacheKey {
        CacheKey::gradle(key).unwrap()
    }

    fn body(bytes: &'static [u8]) -> ByteStream {
        Box::pin(stream::once(async move { Ok(Bytes::from_static(bytes)) }))
    }

    fn chunks(n: usize, size: usize) -> ByteStream {
        Box::pin(stream::iter(
            (0..n).map(move |_| Ok(Bytes::from(vec![7u8; size]))),
        ))
    }

    async fn read(store: &impl BlobStore, key: &CacheKey) -> Option<Bytes> {
        let blob = store.get(key).await.unwrap()?;
        let parts: Vec<Bytes> = blob.body.try_collect().await.unwrap();
        Some(parts.concat().into())
    }

    fn store(capacity: u64, eviction: EvictionPolicy) -> MemoryStore {
        MemoryStore::with_config(MemoryConfig {
            eviction,
            ..MemoryConfig::new(capacity)
        })
        .unwrap()
    }

    #[test]
    fn policy_names_round_trip() {
        for policy in EvictionPolicy::ALL {
            assert_eq!(policy.as_str().parse::<EvictionPolicy>(), Ok(policy));
        }
        assert_eq!(EvictionPolicy::default(), EvictionPolicy::S3Fifo);
        let err = "arc".parse::<EvictionPolicy>().unwrap_err();
        assert!(
            err.to_string()
                .contains("s3-fifo, lru, w-tinylfu, sieve, fifo")
        );
    }

    #[test]
    fn shards_keep_room_for_large_entries() {
        let config = MemoryConfig {
            capacity_bytes: 1 << 30,
            max_entry_bytes: 1 << 20,
            eviction: EvictionPolicy::S3Fifo,
        };
        let shards = config.shards();
        assert!(shards.is_power_of_two());
        assert!(config.capacity_bytes / shards as u64 >= 64 * config.max_entry_bytes);
        assert_eq!(MemoryConfig::new(4096).shards(), 1);
    }

    #[tokio::test]
    async fn round_trips_with_every_policy() {
        for policy in EvictionPolicy::ALL {
            let store = store(1 << 20, policy);
            let key = gradle("abc123");
            assert_eq!(
                store.put(&key, Some(5), body(b"hello")).await.unwrap(),
                PutOutcome::Stored
            );
            assert_eq!(read(&store, &key).await.as_deref(), Some(&b"hello"[..]));
            assert_eq!(store.head(&key).await.unwrap(), Some(5));
            let meta = store.backend.get(&key).unwrap().meta;
            assert_eq!(meta.size, 5);
            assert_eq!(meta.checksum, Checksum::of(b"hello"));
            assert!(meta.created > 0);
        }
    }

    #[tokio::test]
    async fn gradle_entries_are_immutable_and_maven_entries_replace() {
        let store = store(1 << 20, EvictionPolicy::S3Fifo);
        let key = gradle("abc");
        store.put(&key, None, body(b"one")).await.unwrap();
        assert_eq!(
            store.put(&key, None, body(b"two")).await.unwrap(),
            PutOutcome::Exists
        );
        assert_eq!(read(&store, &key).await.as_deref(), Some(&b"one"[..]));

        let path = CacheKey::maven("v1.2/g/a/c/buildinfo.xml").unwrap();
        store.put(&path, None, body(b"one")).await.unwrap();
        assert_eq!(
            store.put(&path, None, body(b"two")).await.unwrap(),
            PutOutcome::Stored
        );
        assert_eq!(read(&store, &path).await.as_deref(), Some(&b"two"[..]));
        assert_eq!(store.usage().entries, 2);
    }

    #[tokio::test]
    async fn failed_stream_keeps_nothing() {
        let store = store(1 << 20, EvictionPolicy::S3Fifo);
        let key = gradle("bad");
        let failing: ByteStream = Box::pin(stream::iter([
            Ok(Bytes::from_static(b"part")),
            Err(io::Error::other("client went away")),
        ]));
        assert!(store.put(&key, None, failing).await.is_err());
        assert!(read(&store, &key).await.is_none());
        assert_eq!(store.usage().entries, 0);
    }

    #[tokio::test]
    async fn entries_over_the_limit_are_not_kept() {
        let store = store(64 * 1024, EvictionPolicy::S3Fifo);
        assert_eq!(store.config().max_entry_bytes, 1024);
        // Declared too large, and too large only once streamed.
        let declared = gradle("aa");
        store
            .put(&declared, Some(2048), chunks(2, 1024))
            .await
            .unwrap();
        let streamed = gradle("bb");
        store.put(&streamed, None, chunks(4, 512)).await.unwrap();
        assert!(read(&store, &declared).await.is_none());
        assert!(read(&store, &streamed).await.is_none());
        assert_eq!(store.usage().entries, 0);
    }

    #[tokio::test]
    async fn capacity_is_enforced_with_overhead_counted() {
        for policy in EvictionPolicy::ALL {
            let capacity = 64 * 1024;
            let store = store(capacity, policy);
            for i in 0..2000 {
                let key = gradle(&format!("{i:x}"));
                store.put(&key, None, chunks(1, 200)).await.unwrap();
            }
            let usage = store.usage();
            assert!(usage.size_bytes <= capacity, "{policy}: {usage:?}");
            let per_entry = (200 + ENTRY_OVERHEAD) as u64;
            assert!(usage.entries <= capacity / per_entry, "{policy}: {usage:?}");
            assert!(usage.entries > 0, "{policy}");
        }
    }

    #[test]
    fn exact_size_trims_spare_capacity() {
        let mut grown = BytesMut::with_capacity(4096);
        grown.extend_from_slice(&[1; 100]);
        assert_eq!(exact_size(grown).len(), 100);

        let mut snug = BytesMut::with_capacity(100);
        snug.extend_from_slice(&[1; 100]);
        assert_eq!(exact_size(snug).len(), 100);
    }
}

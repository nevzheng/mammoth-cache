//! The core: tiering rules over the blob store port.
//!
//! The core knows nothing of HTTP, file systems or cloud SDKs (design doc 0003). It owns the lookup
//! order, the entry size limit and the hit and miss counts. Promotion between tiers, single-flight
//! bucket reads, checksums and the background bucket upload are not implemented yet.

use std::{
    fmt, io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::{StreamExt, stream};

use crate::{
    key::{CacheKey, Namespace},
    store::{Blob, BlobStore, BoundedStore, ByteStream, PutOutcome, Tier, TierUsage},
};

/// The default largest entry, matching the Build Cache Node's `cache.maxArtifactSize`.
pub const DEFAULT_MAX_ENTRY_SIZE: u64 = 100 * 1024 * 1024;

/// What to do with a Gradle entry over the size limit. Maven entries are always refused, because
/// Maven only logs a failed store, and dropping one file could leave a build's `buildinfo.xml`
/// without the files it lists (design doc 0001).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OversizePolicy {
    /// Answer as if stored, discard the bytes and count the entry. The default: Gradle turns its
    /// remote cache off for the rest of a build after any non-2xx store response, `413` included,
    /// so refusing one large entry would cost the build every later hit.
    #[default]
    Drop,
    /// Refuse with [`CacheError::TooLarge`] (HTTP `413`), as the Build Cache Node does.
    Reject,
}

/// Whether to take an upload, decided from its declared size before reading it.
#[derive(Debug)]
pub enum Admission {
    /// Read the body and store it.
    Accept,
    /// Answer success without storing.
    Drop,
    /// Refuse.
    Reject(CacheError),
}

/// What [`Cache::put`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOutcome {
    /// The entry was written.
    Stored,
    /// The key was already present; nothing was written.
    Exists,
    /// The entry was over the size limit and discarded under [`OversizePolicy::Drop`].
    Dropped,
}

impl From<PutOutcome> for StoreOutcome {
    fn from(outcome: PutOutcome) -> Self {
        match outcome {
            PutOutcome::Stored => Self::Stored,
            PutOutcome::Exists => Self::Exists,
        }
    }
}

/// A cache hit: the entry and the tier that served it.
#[derive(Debug)]
pub struct Hit {
    /// The tier that held the entry.
    pub tier: Tier,
    /// The entry.
    pub blob: Blob,
}

/// Why a cache operation failed.
#[derive(Debug)]
pub enum CacheError {
    /// The entry is over the size limit. `size` is the declared size, when known before reading.
    TooLarge { size: Option<u64>, limit: u64 },
    /// A tier failed in a way that may pass, such as a timeout.
    Unavailable(io::Error),
    /// Anything else.
    Internal(io::Error),
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge {
                size: Some(size),
                limit,
            } => {
                write!(f, "cache entry is {size} bytes; the limit is {limit}")
            }
            Self::TooLarge { size: None, limit } => {
                write!(f, "cache entry is over the limit of {limit} bytes")
            }
            Self::Unavailable(e) => write!(f, "storage unavailable: {e}"),
            Self::Internal(e) => write!(f, "storage error: {e}"),
        }
    }
}

impl std::error::Error for CacheError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TooLarge { .. } => None,
            Self::Unavailable(e) | Self::Internal(e) => Some(e),
        }
    }
}

impl From<io::Error> for CacheError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Self::Unavailable(e),
            _ => Self::Internal(e),
        }
    }
}

/// Marks the error a size-limited body stream fails with, so it maps back to `TooLarge`.
#[derive(Debug)]
struct EntryTooLarge;

impl fmt::Display for EntryTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("entry exceeds the size limit")
    }
}

impl std::error::Error for EntryTooLarge {}

/// Point-in-time counters, for `/v1/serverStatus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheStatus {
    /// Time since the cache was built.
    pub uptime: Duration,
    /// Lookups that found an entry, across all tiers.
    pub hits: u64,
    /// Lookups that found nothing.
    pub misses: u64,
    /// Stores over the size limit, dropped or refused.
    pub oversized: u64,
    /// Population of each bounded tier, in lookup order.
    pub tiers: Vec<(Tier, TierUsage)>,
}

#[derive(Debug)]
struct TierHandle {
    store: Arc<dyn BlobStore>,
    bounded: Option<Arc<dyn BoundedStore>>,
}

/// Builds a [`Cache`] from its tiers, in lookup order.
#[derive(Debug)]
pub struct CacheBuilder {
    tiers: Vec<TierHandle>,
    max_entry_size: u64,
    oversize: OversizePolicy,
}

impl Default for CacheBuilder {
    fn default() -> Self {
        Self {
            tiers: Vec::new(),
            max_entry_size: DEFAULT_MAX_ENTRY_SIZE,
            oversize: OversizePolicy::default(),
        }
    }
}

impl CacheBuilder {
    /// Adds a local tier with a capacity (memory or disk).
    #[must_use]
    pub fn bounded_tier<S: BoundedStore + 'static>(mut self, store: Arc<S>) -> Self {
        self.tiers.push(TierHandle {
            store: store.clone(),
            bounded: Some(store),
        });
        self
    }

    /// Adds a tier without a capacity (the bucket).
    #[must_use]
    pub fn tier<S: BlobStore + 'static>(mut self, store: Arc<S>) -> Self {
        self.tiers.push(TierHandle {
            store,
            bounded: None,
        });
        self
    }

    /// Sets the largest entry accepted, in bytes.
    #[must_use]
    pub fn max_entry_size(mut self, bytes: u64) -> Self {
        self.max_entry_size = bytes;
        self
    }

    /// Sets what happens to a Gradle entry over the size limit.
    #[must_use]
    pub fn oversize(mut self, policy: OversizePolicy) -> Self {
        self.oversize = policy;
        self
    }

    /// Builds the cache.
    #[must_use]
    pub fn build(self) -> Cache {
        Cache {
            tiers: self.tiers,
            max_entry_size: self.max_entry_size,
            oversize: self.oversize,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            oversized: AtomicU64::new(0),
            started: Instant::now(),
        }
    }
}

/// The cache core. Cheap to share behind an `Arc`; all methods take `&self`.
#[derive(Debug)]
pub struct Cache {
    tiers: Vec<TierHandle>,
    max_entry_size: u64,
    oversize: OversizePolicy,
    hits: AtomicU64,
    misses: AtomicU64,
    oversized: AtomicU64,
    started: Instant,
}

impl Cache {
    /// Starts building a cache.
    #[must_use]
    pub fn builder() -> CacheBuilder {
        CacheBuilder::default()
    }

    /// The largest entry accepted, in bytes.
    pub fn max_entry_size(&self) -> u64 {
        self.max_entry_size
    }

    /// Loads an entry, looking in each tier in order. `Ok(None)` is a miss.
    ///
    /// A tier that fails is skipped and the next one tried: for a cache, a failed read is a miss.
    pub async fn get(&self, key: &CacheKey) -> Result<Option<Hit>, CacheError> {
        for handle in &self.tiers {
            let tier = handle.store.tier();
            match handle.store.get(key).await {
                Ok(Some(blob)) => {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(Some(Hit { tier, blob }));
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(%key, %tier, %error, "tier read failed; skipping"),
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        Ok(None)
    }

    /// Returns an entry's size without reading it. `Ok(None)` is a miss.
    pub async fn head(&self, key: &CacheKey) -> Result<Option<u64>, CacheError> {
        for handle in &self.tiers {
            match handle.store.head(key).await {
                Ok(Some(size)) => return Ok(Some(size)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%key, tier = %handle.store.tier(), %error, "tier head failed");
                }
            }
        }
        Ok(None)
    }

    /// Decides from an upload's declared size whether to take it, and counts it if oversized.
    ///
    /// Callers should check this before reading a body: under [`OversizePolicy::Drop`] the
    /// transport may need to discard the body itself (see the Gradle adapter).
    pub fn admit(&self, key: &CacheKey, declared_size: Option<u64>) -> Admission {
        let limit = self.max_entry_size;
        let Some(size) = declared_size.filter(|&size| size > limit) else {
            return Admission::Accept;
        };
        self.oversized.fetch_add(1, Ordering::Relaxed);
        match self.oversize_policy(key.namespace()) {
            OversizePolicy::Drop => {
                tracing::info!(%key, size, limit, "dropping oversized entry");
                Admission::Drop
            }
            OversizePolicy::Reject => Admission::Reject(CacheError::TooLarge {
                size: Some(size),
                limit,
            }),
        }
    }

    /// What happens to an oversized entry in a namespace.
    #[must_use]
    pub fn oversize_policy(&self, namespace: Namespace) -> OversizePolicy {
        match namespace {
            Namespace::Gradle => self.oversize,
            Namespace::Maven => OversizePolicy::Reject,
        }
    }

    /// Stores an entry from a stream. `declared_size` is the request's `Content-Length`.
    ///
    /// A store to an existing immutable (Gradle) key writes nothing; one to a mutable (Maven) key
    /// replaces the entry.
    ///
    /// An entry over the size limit is dropped or refused according to the [`OversizePolicy`],
    /// whether its declared size says so or the body passes the limit while streaming. A body
    /// dropped mid-stream is read to the end and discarded, so the client sees a clean response.
    /// Writes to the first tier only for now; the full write path (disk, then a background bucket
    /// upload) arrives with the disk and bucket tiers.
    pub async fn put(
        &self,
        key: &CacheKey,
        declared_size: Option<u64>,
        body: ByteStream,
    ) -> Result<StoreOutcome, CacheError> {
        match self.admit(key, declared_size) {
            Admission::Accept => {}
            Admission::Drop => return Ok(StoreOutcome::Dropped),
            Admission::Reject(e) => return Err(e),
        }
        if key.namespace().is_immutable() && self.head(key).await?.is_some() {
            return Ok(StoreOutcome::Exists);
        }
        let Some(first) = self.tiers.first() else {
            return Err(CacheError::Internal(io::Error::other(
                "no storage tiers configured",
            )));
        };

        let limit = self.max_entry_size;
        let drain = self.oversize_policy(key.namespace()) == OversizePolicy::Drop;
        match first
            .store
            .put(key, declared_size, limit_stream(body, limit, drain))
            .await
        {
            Ok(outcome) => Ok(outcome.into()),
            Err(e) if is_too_large(&e) => {
                self.oversized.fetch_add(1, Ordering::Relaxed);
                if drain {
                    tracing::info!(%key, limit, "dropping oversized entry");
                    Ok(StoreOutcome::Dropped)
                } else {
                    Err(CacheError::TooLarge { size: None, limit })
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Current counters and tier populations.
    pub fn status(&self) -> CacheStatus {
        CacheStatus {
            uptime: self.started.elapsed(),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            oversized: self.oversized.load(Ordering::Relaxed),
            tiers: self
                .tiers
                .iter()
                .filter_map(|h| h.bounded.as_ref().map(|b| (b.tier(), b.usage())))
                .collect(),
        }
    }
}

/// Fails the stream once more than `limit` bytes have passed, so the store discards the entry.
/// With `drain`, reads the rest of the body first, so the client finishes sending it.
fn limit_stream(body: ByteStream, limit: u64, drain: bool) -> ByteStream {
    Box::pin(stream::unfold(
        Some((body, 0_u64)),
        move |state| async move {
            let (mut body, seen) = state?;
            let chunk = match body.next().await? {
                Ok(chunk) => chunk,
                Err(e) => return Some((Err(e), None)),
            };
            let seen = seen + chunk.len() as u64;
            if seen <= limit {
                return Some((Ok(chunk), Some((body, seen))));
            }
            if drain {
                while let Some(Ok(_)) = body.next().await {}
            }
            Some((Err(io::Error::other(EntryTooLarge)), None))
        },
    ))
}

fn is_too_large(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<EntryTooLarge>)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures_util::{StreamExt, stream};

    use super::*;
    use crate::store::memory::MemoryStore;

    fn key(s: &str) -> CacheKey {
        CacheKey::gradle(s).unwrap()
    }

    fn body(chunks: &[&'static [u8]]) -> ByteStream {
        let chunks: Vec<io::Result<Bytes>> =
            chunks.iter().map(|c| Ok(Bytes::from_static(c))).collect();
        Box::pin(stream::iter(chunks))
    }

    async fn read_all(mut blob: Blob) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(chunk) = blob.body.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    fn cache_with(limit: u64, policy: OversizePolicy) -> Cache {
        Cache::builder()
            .bounded_tier(Arc::new(MemoryStore::new(1024)))
            .max_entry_size(limit)
            .oversize(policy)
            .build()
    }

    fn cache(limit: u64) -> Cache {
        cache_with(limit, OversizePolicy::Drop)
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let cache = cache(1024);
        let k = key("abc123");
        let outcome = cache
            .put(&k, Some(11), body(&[b"hello ", b"world"]))
            .await
            .unwrap();
        assert_eq!(outcome, StoreOutcome::Stored);

        let hit = cache.get(&k).await.unwrap().unwrap();
        assert_eq!(hit.tier, Tier::Memory);
        assert_eq!(hit.blob.size, 11);
        assert_eq!(read_all(hit.blob).await, b"hello world");
        assert_eq!(cache.head(&k).await.unwrap(), Some(11));
    }

    #[tokio::test]
    async fn second_put_writes_nothing() {
        let cache = cache(1024);
        let k = key("abc123");
        cache.put(&k, None, body(&[b"first"])).await.unwrap();
        let outcome = cache.put(&k, None, body(&[b"second"])).await.unwrap();
        assert_eq!(outcome, StoreOutcome::Exists);
        let hit = cache.get(&k).await.unwrap().unwrap();
        assert_eq!(read_all(hit.blob).await, b"first");
    }

    #[tokio::test]
    async fn declared_size_over_limit_is_rejected_before_reading() {
        let cache = cache_with(4, OversizePolicy::Reject);
        let err = cache
            .put(&key("ab"), Some(5), body(&[b"12345"]))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CacheError::TooLarge {
                size: Some(5),
                limit: 4
            }
        ));
    }

    #[tokio::test]
    async fn streamed_body_over_limit_is_rejected_and_not_stored() {
        let cache = cache_with(4, OversizePolicy::Reject);
        let k = key("ab");
        let err = cache
            .put(&k, None, body(&[b"123", b"45"]))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CacheError::TooLarge {
                size: None,
                limit: 4
            }
        ));
        assert!(cache.get(&k).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn declared_size_over_limit_is_dropped_by_default() {
        let cache = cache(4);
        let k = key("ab");
        let outcome = cache.put(&k, Some(5), body(&[b"12345"])).await.unwrap();
        assert_eq!(outcome, StoreOutcome::Dropped);
        assert!(cache.get(&k).await.unwrap().is_none());
        assert_eq!(cache.status().oversized, 1);
    }

    #[tokio::test]
    async fn streamed_body_over_limit_is_drained_and_dropped() {
        let cache = cache(4);
        let k = key("ab");
        let read = Arc::new(AtomicU64::new(0));
        let counted = read.clone();
        let chunks = body(&[b"123", b"45", b"678", b"9"]).inspect(move |_| {
            counted.fetch_add(1, Ordering::Relaxed);
        });
        let outcome = cache.put(&k, None, Box::pin(chunks)).await.unwrap();
        assert_eq!(outcome, StoreOutcome::Dropped);
        assert_eq!(read.load(Ordering::Relaxed), 4, "every chunk is read");
        assert!(cache.get(&k).await.unwrap().is_none());
        assert_eq!(cache.status().oversized, 1);
    }

    #[tokio::test]
    async fn maven_store_replaces_existing_entry() {
        let cache = cache(1024);
        let k = CacheKey::maven("v1.2/g/a/abc/buildinfo.xml").unwrap();
        cache.put(&k, None, body(&[b"first"])).await.unwrap();
        let outcome = cache.put(&k, None, body(&[b"second"])).await.unwrap();
        assert_eq!(outcome, StoreOutcome::Stored);
        let hit = cache.get(&k).await.unwrap().unwrap();
        assert_eq!(read_all(hit.blob).await, b"second");
    }

    #[tokio::test]
    async fn maven_oversize_is_rejected_even_when_gradle_drops() {
        let cache = cache(4);
        let k = CacheKey::maven("v1/g/a/abc/a.jar").unwrap();
        let err = cache.put(&k, Some(5), body(&[b"12345"])).await.unwrap_err();
        assert!(matches!(err, CacheError::TooLarge { .. }));
        let err = cache
            .put(&k, None, body(&[b"123", b"45"]))
            .await
            .unwrap_err();
        assert!(matches!(err, CacheError::TooLarge { size: None, .. }));
    }

    #[tokio::test]
    async fn counts_hits_and_misses() {
        let cache = cache(1024);
        let k = key("ab");
        assert!(cache.get(&k).await.unwrap().is_none());
        cache.put(&k, None, body(&[b"x"])).await.unwrap();
        assert!(cache.get(&k).await.unwrap().is_some());

        let status = cache.status();
        assert_eq!((status.hits, status.misses), (1, 1));
        assert_eq!(status.tiers.len(), 1);
        assert_eq!(status.tiers[0].0, Tier::Memory);
        assert_eq!(status.tiers[0].1.entries, 1);
    }
}

//! The core: tiering rules over the blob store port.
//!
//! The core knows nothing of HTTP, file systems or cloud SDKs (design doc 0003). It owns the lookup
//! order, the write path, promotion between tiers, the background bucket upload, the entry size
//! limit and the hit and miss counts. Single-flight bucket reads are not implemented yet.
//!
//! - **Reads** look in each local tier, then the bucket. A hit below the first tier is copied up
//!   into the tiers above it while it streams to the client. Copies never slow the client: one
//!   that falls behind is abandoned.
//! - **Writes** stream into every local tier at once; the deepest (disk, when configured) decides
//!   the outcome. The bucket gets the entry in the background through a bounded queue, or inline
//!   with [`BucketWrite::Sync`]. A node with no local tier writes straight to the bucket.

use std::{
    fmt, io,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::{StreamExt, future, stream};
use tokio::{
    sync::{Notify, mpsc},
    task::JoinHandle,
};

use crate::{
    key::{CacheKey, Namespace},
    store::{Blob, BlobStore, BoundedStore, ByteStream, HealthCheck, PutOutcome, Tier, TierUsage},
    tee::{self, tee},
};

/// Bucket uploads in flight at once.
const UPLOAD_CONCURRENCY: usize = 8;

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

/// When a store reaches the bucket (`bucket.write` in `mammoth.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketWrite {
    /// Answer once the local tiers have the entry, then upload in the background. Up to `queue`
    /// uploads wait; when the queue is full, the upload is dropped and counted, which costs other
    /// nodes a miss and nothing more.
    Async {
        /// Uploads that may wait.
        queue: usize,
    },
    /// Answer only after the upload, trading `PUT` latency for other nodes seeing the entry sooner.
    Sync,
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

#[derive(Debug)]
struct BucketHandle {
    store: Arc<dyn BlobStore>,
    /// The background upload queue; `None` when stores reach the bucket inline.
    uploads: Option<Uploads>,
}

/// The background upload queue and its worker.
#[derive(Debug)]
struct Uploads {
    queue: mpsc::Sender<CacheKey>,
    /// Uploads queued or in progress.
    pending: Arc<AtomicU64>,
    /// Tells the worker to take no more keys and finish the queued ones.
    stop: Arc<Notify>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// Builds a [`Cache`] from its tiers, in lookup order.
#[derive(Debug)]
pub struct CacheBuilder {
    tiers: Vec<TierHandle>,
    bucket: Option<(Arc<dyn BlobStore>, BucketWrite)>,
    max_entry_size: u64,
    oversize: OversizePolicy,
}

impl Default for CacheBuilder {
    fn default() -> Self {
        Self {
            tiers: Vec::new(),
            bucket: None,
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

    /// Sets the shared bucket, looked up after every local tier.
    #[must_use]
    pub fn bucket_tier<S: BlobStore + 'static>(
        mut self,
        store: Arc<S>,
        write: BucketWrite,
    ) -> Self {
        self.bucket = Some((store, write));
        self
    }

    /// Adds a local tier without a capacity, such as a custom store.
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

    /// Builds the cache. With a bucket and [`BucketWrite::Async`], starts the upload workers, so
    /// it must be called inside a Tokio runtime.
    #[must_use]
    pub fn build(self) -> Cache {
        // The deepest local tier holds every stored entry, unless it caps entry size (memory).
        let source = self.tiers.last().map(|handle| handle.store.clone());
        let source_cap = source.as_ref().and_then(|store| store.max_entry_size());
        let mut max_entry_size = self.max_entry_size;
        if self.bucket.is_none()
            && let Some(cap) = source_cap
        {
            // Nothing below would keep a larger entry, so it is oversized.
            max_entry_size = max_entry_size.min(cap);
        }
        let bucket = self.bucket.map(|(store, write)| {
            let uploads = match (write, source) {
                // Background uploads read the entry back, so its local copy must be whole.
                (BucketWrite::Async { queue }, Some(source)) if source_cap.is_none() => {
                    let (tx, rx) = mpsc::channel(queue.max(1));
                    let pending = Arc::new(AtomicU64::new(0));
                    let stop = Arc::new(Notify::new());
                    let worker = tokio::spawn(upload_worker(
                        rx,
                        source,
                        store.clone(),
                        pending.clone(),
                        stop.clone(),
                    ));
                    Some(Uploads {
                        queue: tx,
                        pending,
                        stop,
                        worker: Mutex::new(Some(worker)),
                    })
                }
                // Inline: asked for, or there is no whole local copy to upload from.
                _ => None,
            };
            BucketHandle { store, uploads }
        });
        Cache {
            tiers: self.tiers,
            bucket,
            max_entry_size,
            oversize: self.oversize,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            oversized: AtomicU64::new(0),
            started: Instant::now(),
            shutting_down: AtomicBool::new(false),
        }
    }
}

/// The cache core. Cheap to share behind an `Arc`; all methods take `&self`.
#[derive(Debug)]
pub struct Cache {
    tiers: Vec<TierHandle>,
    bucket: Option<BucketHandle>,
    max_entry_size: u64,
    oversize: OversizePolicy,
    hits: AtomicU64,
    misses: AtomicU64,
    oversized: AtomicU64,
    started: Instant,
    shutting_down: AtomicBool,
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

    /// Every tier in lookup order: the local tiers, then the bucket.
    fn lookup_order(&self) -> impl Iterator<Item = &Arc<dyn BlobStore>> {
        self.tiers
            .iter()
            .map(|handle| &handle.store)
            .chain(self.bucket.as_ref().map(|bucket| &bucket.store))
    }

    /// Loads an entry, looking in each tier in order. `Ok(None)` is a miss.
    ///
    /// A hit below the first tier is copied into the local tiers above it as it streams. A tier
    /// that fails is skipped and the next one tried: for a cache, a failed read is a miss.
    pub async fn get(&self, key: &CacheKey) -> Result<Option<Hit>, CacheError> {
        for (depth, store) in self.lookup_order().enumerate() {
            let tier = store.tier();
            match store.get(key).await {
                Ok(Some(blob)) => {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    let above = &self.tiers[..depth.min(self.tiers.len())];
                    let blob = promote(key, tier, blob, above);
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
        for store in self.lookup_order() {
            match store.head(key).await {
                Ok(Some(size)) => return Ok(Some(size)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%key, tier = %store.tier(), %error, "tier head failed");
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
            // Maven and Bazel only log a failed store and keep using the cache.
            Namespace::Maven | Namespace::BazelAc | Namespace::BazelCas => OversizePolicy::Reject,
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
    /// The entry is written to every local tier, and to the bucket inline or in the background
    /// (see [`BucketWrite`]).
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
        // The deepest local tier decides the outcome; the others, and the bucket when it is
        // written inline, get copies of the same stream.
        let mut writers: Vec<&Arc<dyn BlobStore>> = self
            .tiers
            .iter()
            .rev()
            .map(|handle| &handle.store)
            .collect();
        if let Some(bucket) = &self.bucket
            && bucket.uploads.is_none()
        {
            writers.push(&bucket.store);
        }
        let Some((primary, others)) = writers.split_first() else {
            return Err(CacheError::Internal(io::Error::other(
                "no storage tiers configured",
            )));
        };

        let limit = self.max_entry_size;
        let drain = self.oversize_policy(key.namespace()) == OversizePolicy::Drop;
        let (body, copies) = tee(
            limit_stream(body, limit, drain),
            others.len(),
            tee::Mode::Lockstep,
            None,
        );
        let secondary = others.iter().zip(copies).map(|(store, copy)| async move {
            (store.tier(), store.put(key, declared_size, copy).await)
        });
        let (result, secondary) = future::join(
            primary.put(key, declared_size, body),
            future::join_all(secondary),
        )
        .await;
        if result.is_ok() {
            for (tier, outcome) in secondary {
                if let Err(error) = outcome {
                    tracing::warn!(%key, %tier, %error, "write to a secondary tier failed");
                }
            }
        }

        match result {
            Ok(outcome) => {
                if outcome == PutOutcome::Stored {
                    self.enqueue_upload(key);
                }
                Ok(outcome.into())
            }
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

    /// Queues a background upload to the bucket, when there is one. A full queue drops the upload.
    fn enqueue_upload(&self, key: &CacheKey) {
        let Some(uploads) = self.uploads() else {
            return;
        };
        match uploads.queue.try_send(key.clone()) {
            Ok(()) => {
                uploads.pending.fetch_add(1, Ordering::Relaxed);
                metrics::gauge!("mammoth_bucket_upload_queue_entries").increment(1.0);
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                metrics::counter!("mammoth_bucket_uploads_dropped_total", "reason" => "queue-full")
                    .increment(1);
                tracing::warn!(%key, "bucket upload queue full; upload dropped");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                metrics::counter!("mammoth_bucket_uploads_dropped_total", "reason" => "shutdown")
                    .increment(1);
                tracing::warn!(%key, "shutting down; bucket upload dropped");
            }
        }
    }

    fn uploads(&self) -> Option<&Uploads> {
        self.bucket.as_ref()?.uploads.as_ref()
    }

    /// Marks the node as shutting down, so readiness fails. Requests are still served.
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        metrics::gauge!("mammoth_shutting_down").set(1.0);
    }

    /// Whether [`begin_shutdown`](Self::begin_shutdown) was called.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    /// The local tiers' readiness checks, from their last background probes. Never touches the
    /// bucket: a bucket outage should degrade nodes to their local tiers, not remove them all.
    pub fn health(&self) -> Vec<HealthCheck> {
        self.tiers
            .iter()
            .filter_map(|handle| handle.bounded.as_ref())
            .flat_map(|store| store.health())
            .collect()
    }

    /// Uploads queued or in progress to the bucket.
    pub fn pending_uploads(&self) -> u64 {
        self.uploads()
            .map_or(0, |uploads| uploads.pending.load(Ordering::Relaxed))
    }

    /// Stops taking background uploads and waits for the queued ones to finish. Stores after
    /// this upload nothing in the background. Bound it with a timeout: a slow bucket can make it
    /// take as long as the queue does.
    pub async fn flush_uploads(&self) {
        let Some(uploads) = self.uploads() else {
            return;
        };
        uploads.stop.notify_one();
        let worker = uploads
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(worker) = worker
            && let Err(error) = worker.await
        {
            tracing::error!(%error, "bucket upload worker failed");
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

/// Copies a hit from a lower tier into the tiers `above` it while it streams to the client. Each
/// copy is written in the background and abandoned if it falls behind the client.
fn promote(key: &CacheKey, from: Tier, blob: Blob, above: &[TierHandle]) -> Blob {
    if above.is_empty() {
        return blob;
    }
    let size = blob.size;
    let (body, copies) = tee(blob.body, above.len(), tee::Mode::Lossy, Some(size));
    for (handle, copy) in above.iter().zip(copies) {
        let store = handle.store.clone();
        let key = key.clone();
        tokio::spawn(async move {
            let to = store.tier();
            match store.put(&key, Some(size), copy).await {
                Ok(_) => {
                    metrics::counter!(
                        "mammoth_tier_promotions_total",
                        "from" => from.as_str(),
                        "to" => to.as_str(),
                    )
                    .increment(1);
                }
                Err(error) => tracing::debug!(%key, %from, %to, %error, "promotion abandoned"),
            }
        });
    }
    Blob { size, body }
}

/// Uploads stored entries to the bucket, reading each one back from the deepest local tier.
/// On `stop`, closes the queue and finishes the keys already in it.
async fn upload_worker(
    queue: mpsc::Receiver<CacheKey>,
    source: Arc<dyn BlobStore>,
    bucket: Arc<dyn BlobStore>,
    pending: Arc<AtomicU64>,
    stop: Arc<Notify>,
) {
    let keys = stream::unfold((queue, stop), |(mut queue, stop)| async move {
        let key = if queue.is_closed() {
            queue.recv().await?
        } else {
            tokio::select! {
                key = queue.recv() => key?,
                () = stop.notified() => {
                    queue.close();
                    queue.recv().await?
                }
            }
        };
        metrics::gauge!("mammoth_bucket_upload_queue_entries").decrement(1.0);
        Some((key, (queue, stop)))
    });
    keys.for_each_concurrent(UPLOAD_CONCURRENCY, |key| {
        let (source, bucket, pending) = (&source, &bucket, &pending);
        async move {
            let result = match source.get(&key).await {
                Ok(Some(blob)) => bucket.put(&key, Some(blob.size), blob.body).await.map(Some),
                Ok(None) => Ok(None),
                Err(e) => Err(e),
            };
            let outcome = match result {
                Ok(Some(_)) => "ok",
                Ok(None) => {
                    tracing::debug!(%key, "entry left the local tiers before its upload");
                    "skipped"
                }
                Err(error) => {
                    tracing::warn!(%key, %error, "bucket upload failed");
                    "error"
                }
            };
            metrics::counter!("mammoth_bucket_uploads_total", "result" => outcome).increment(1);
            pending.fetch_sub(1, Ordering::Relaxed);
        }
    })
    .await;
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
            .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
            .max_entry_size(limit)
            .oversize(policy)
            .build()
    }

    fn cache(limit: u64) -> Cache {
        cache_with(limit, OversizePolicy::Drop)
    }

    #[tokio::test]
    async fn memory_only_caps_entries_at_the_memory_limit() {
        // A 1 KiB memory store keeps entries of at most 16 bytes, below the 1 KiB limit.
        let cache = Cache::builder()
            .bounded_tier(Arc::new(MemoryStore::new(1024)))
            .max_entry_size(1024)
            .build();
        let k = key("abc123");
        let outcome = cache.put(&k, Some(17), body(&[&[0; 17]])).await.unwrap();
        assert_eq!(outcome, StoreOutcome::Dropped);
        assert!(cache.get(&k).await.unwrap().is_none());
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

    mod tiers {
        use std::time::Duration;

        use object_store::{memory::InMemory, path::Path};

        use super::*;
        use crate::store::{
            bucket::BucketStore,
            disk::{DiskConfig, DiskStore},
        };

        struct Tiers {
            memory: Arc<MemoryStore>,
            disk: Arc<DiskStore>,
            bucket: Arc<BucketStore>,
            _dir: tempfile::TempDir,
        }

        async fn tiers() -> Tiers {
            let dir = tempfile::tempdir().unwrap();
            let disk = DiskStore::open(DiskConfig {
                path: dir.path().to_path_buf(),
                capacity_bytes: 1 << 20,
                max_entry_age: None,
                free_space_buffer: 0,
            })
            .await
            .unwrap();
            Tiers {
                memory: Arc::new(MemoryStore::new(1 << 20)),
                disk: Arc::new(disk),
                bucket: Arc::new(BucketStore::with_store(
                    Arc::new(InMemory::new()),
                    Path::from("p"),
                    0,
                )),
                _dir: dir,
            }
        }

        fn cache(t: &Tiers, write: BucketWrite) -> Cache {
            Cache::builder()
                .bounded_tier(t.memory.clone())
                .bounded_tier(t.disk.clone())
                .bucket_tier(t.bucket.clone(), write)
                .build()
        }

        /// Waits for background work (promotions, uploads) to land in `store`.
        async fn eventually_has(store: &dyn BlobStore, k: &CacheKey) {
            for _ in 0..200 {
                if store.head(k).await.unwrap().is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("{k} never reached the {} tier", store.tier());
        }

        #[tokio::test]
        async fn flushing_finishes_queued_uploads_and_takes_no_more() {
            let t = tiers().await;
            let cache = cache(&t, BucketWrite::Async { queue: 100 });
            let keys: Vec<_> = (0..20).map(|i| key(&format!("{i:02x}"))).collect();
            for k in &keys {
                cache.put(k, Some(3), body(&[b"abc"])).await.unwrap();
            }
            cache.flush_uploads().await;
            assert_eq!(cache.pending_uploads(), 0);
            for k in &keys {
                assert!(
                    t.bucket.head(k).await.unwrap().is_some(),
                    "{k} not uploaded"
                );
            }

            // After the flush, stores still succeed locally but are not uploaded.
            let late = key("ff");
            let outcome = cache.put(&late, Some(3), body(&[b"abc"])).await.unwrap();
            assert_eq!(outcome, StoreOutcome::Stored);
            assert!(t.disk.head(&late).await.unwrap().is_some());
            assert_eq!(t.bucket.head(&late).await.unwrap(), None);
        }

        #[tokio::test]
        async fn health_reports_local_tiers_only() {
            let t = tiers().await;
            let cache = cache(&t, BucketWrite::Sync);
            let names: Vec<_> = cache.health().iter().map(|c| c.name).collect();
            assert_eq!(names, ["storage-writable", "storage-free-space"]);
            assert!(!cache.is_shutting_down());
            cache.begin_shutdown();
            assert!(cache.is_shutting_down());
        }

        #[tokio::test]
        async fn put_writes_every_local_tier_and_uploads_in_the_background() {
            let t = tiers().await;
            let cache = cache(&t, BucketWrite::Async { queue: 8 });
            let k = key("ab12");
            let outcome = cache.put(&k, None, body(&[b"hello ", b"tiers"])).await;
            assert_eq!(outcome.unwrap(), StoreOutcome::Stored);
            assert_eq!(t.memory.head(&k).await.unwrap(), Some(11));
            assert_eq!(t.disk.head(&k).await.unwrap(), Some(11));
            eventually_has(t.bucket.as_ref(), &k).await;
        }

        #[tokio::test]
        async fn sync_bucket_writes_before_answering() {
            let t = tiers().await;
            let cache = cache(&t, BucketWrite::Sync);
            let k = key("cd34");
            cache.put(&k, None, body(&[b"now"])).await.unwrap();
            assert_eq!(t.bucket.head(&k).await.unwrap(), Some(3));
        }

        #[tokio::test]
        async fn disk_hits_are_promoted_to_memory() {
            let t = tiers().await;
            let cache = cache(&t, BucketWrite::Sync);
            let k = key("ef56");
            t.disk.put(&k, None, body(&[b"warm"])).await.unwrap();

            let hit = cache.get(&k).await.unwrap().unwrap();
            assert_eq!(hit.tier, Tier::Disk);
            assert_eq!(read_all(hit.blob).await, b"warm");
            eventually_has(t.memory.as_ref(), &k).await;
            assert_eq!(cache.get(&k).await.unwrap().unwrap().tier, Tier::Memory);
        }

        #[tokio::test]
        async fn bucket_hits_are_promoted_to_disk_and_memory() {
            let t = tiers().await;
            let cache = cache(&t, BucketWrite::Sync);
            let k = key("0123");
            t.bucket
                .put(&k, None, body(&[b"from ", b"elsewhere"]))
                .await
                .unwrap();

            let hit = cache.get(&k).await.unwrap().unwrap();
            assert_eq!(hit.tier, Tier::Bucket);
            assert_eq!(read_all(hit.blob).await, b"from elsewhere");
            eventually_has(t.disk.as_ref(), &k).await;
            eventually_has(t.memory.as_ref(), &k).await;
        }

        #[tokio::test]
        async fn an_abandoned_read_promotes_nothing() {
            let t = tiers().await;
            let cache = cache(&t, BucketWrite::Sync);
            let k = key("4567");
            // Several disk read chunks long.
            let big: Vec<io::Result<Bytes>> = vec![Ok(Bytes::from(vec![7_u8; 200_000]))];
            t.disk
                .put(&k, None, Box::pin(stream::iter(big)))
                .await
                .unwrap();

            let mut hit = cache.get(&k).await.unwrap().unwrap();
            hit.blob.body.next().await.unwrap().unwrap();
            drop(hit);
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(t.memory.head(&k).await.unwrap(), None);
        }

        #[tokio::test]
        async fn an_entry_in_any_tier_is_not_stored_again() {
            let t = tiers().await;
            let cache = cache(&t, BucketWrite::Sync);
            let k = key("89ab");
            t.bucket.put(&k, None, body(&[b"old"])).await.unwrap();
            let outcome = cache.put(&k, None, body(&[b"new"])).await.unwrap();
            assert_eq!(outcome, StoreOutcome::Exists);
            assert_eq!(t.disk.head(&k).await.unwrap(), None);
        }

        #[tokio::test]
        async fn oversized_stores_reach_no_tier() {
            let t = tiers().await;
            let cache = Cache::builder()
                .bounded_tier(t.memory.clone())
                .bounded_tier(t.disk.clone())
                .bucket_tier(t.bucket.clone(), BucketWrite::Sync)
                .max_entry_size(4)
                .build();
            let k = key("cdef");
            let outcome = cache
                .put(&k, None, body(&[b"too", b" long"]))
                .await
                .unwrap();
            assert_eq!(outcome, StoreOutcome::Dropped);
            assert_eq!(cache.head(&k).await.unwrap(), None);
        }

        #[tokio::test]
        async fn a_bucket_only_node_writes_straight_to_the_bucket() {
            let t = tiers().await;
            let cache = Cache::builder()
                .bucket_tier(t.bucket.clone(), BucketWrite::Async { queue: 8 })
                .build();
            let k = key("fedc");
            cache.put(&k, None, body(&[b"direct"])).await.unwrap();
            assert_eq!(t.bucket.head(&k).await.unwrap(), Some(6));
        }
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;

        fn chunked(data: &[u8], cuts: &[usize]) -> ByteStream {
            let mut chunks = Vec::new();
            let mut start = 0;
            for &cut in cuts {
                let end = (start + cut).min(data.len());
                chunks.push(Ok(Bytes::copy_from_slice(&data[start..end])));
                start = end;
            }
            chunks.push(Ok(Bytes::copy_from_slice(&data[start..])));
            Box::pin(stream::iter(chunks))
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            /// However a body is chunked, it is stored whole if it fits the limit and not at all
            /// if it does not.
            #[test]
            fn stores_whole_or_not_at_all(
                data in proptest::collection::vec(any::<u8>(), 0..3000),
                cuts in proptest::collection::vec(0_usize..700, 0..8),
                limit in 0_u64..2500,
            ) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let cache = cache(limit);
                    let k = key("abc");
                    let outcome = cache.put(&k, None, chunked(&data, &cuts)).await.unwrap();
                    let fits = data.len() as u64 <= limit;
                    let expected = if fits { StoreOutcome::Stored } else { StoreOutcome::Dropped };
                    prop_assert_eq!(outcome, expected);
                    match cache.get(&k).await.unwrap() {
                        Some(hit) => prop_assert!(fits && read_all(hit.blob).await == data),
                        None => prop_assert!(!fits),
                    }
                    Ok(())
                })?;
            }
        }
    }
}

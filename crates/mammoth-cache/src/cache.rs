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
//! - **Bucket outages** cost misses, never stalls or errors (see [`BucketLimits`]). A bucket read
//!   that fails or passes its deadline is a miss, and after any bucket failure the bucket is
//!   skipped for a cooldown: reads miss at once and uploads are dropped and counted. The first
//!   call after the cooldown probes the bucket again. Only the change between up and down is
//!   logged, and the `mammoth_tier_up{tier="bucket"}` gauge follows it.

use std::{
    collections::HashMap,
    fmt, io,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::{StreamExt, future, stream};
use tokio::{
    sync::{Notify, mpsc, watch},
    task::JoinHandle,
};

mod gate;

use gate::Gate;

use crate::{
    key::{CacheKey, Namespace},
    store::{
        Blob, BlobStore, BoundedStore, BoxFuture, ByteStream, HealthCheck, PutOutcome, Tier,
        TierUsage,
    },
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

/// Limits on calls to the bucket (`bucket.timeout` and `bucket.cooldown` in `mammoth.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketLimits {
    /// The longest a bucket read may take: a `get` until its body starts, or a `head`. A read
    /// that takes longer is a miss and counts as a bucket failure.
    pub timeout: Duration,
    /// After a bucket failure, how long to skip the bucket. The first call after it goes to the
    /// bucket as a probe. Zero turns the cooldown off, so every call tries the bucket.
    pub cooldown: Duration,
}

impl Default for BucketLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(2),
            cooldown: Duration::from_secs(30),
        }
    }
}

/// Whether the bucket is answering, and the cooldown after a failure. Shared by the read path
/// and the upload worker.
#[derive(Debug)]
struct BucketHealth {
    limits: BucketLimits,
    /// False from a bucket failure until the next success.
    up: AtomicBool,
    /// While cooling down, when the cooldown ends, in milliseconds since `epoch`; 0 otherwise.
    skip_until: AtomicU64,
    epoch: tokio::time::Instant,
}

impl BucketHealth {
    fn new(limits: BucketLimits) -> Self {
        set_bucket_gauge(true);
        Self {
            limits,
            up: AtomicBool::new(true),
            skip_until: AtomicU64::new(0),
            epoch: tokio::time::Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// When a cooldown starting now ends; never 0.
    fn cooldown_end(&self) -> u64 {
        let cooldown = u64::try_from(self.limits.cooldown.as_millis()).unwrap_or(u64::MAX);
        self.now_ms().saturating_add(cooldown.max(1))
    }

    fn is_up(&self) -> bool {
        self.up.load(Ordering::Relaxed)
    }

    /// Whether the bucket is being skipped after a failure.
    fn cooling_down(&self) -> bool {
        let until = self.skip_until.load(Ordering::Acquire);
        until != 0 && self.now_ms() < until
    }

    /// Whether a read may go to the bucket. The first read after a cooldown is let through as
    /// the probe; the cooldown is extended for the others until it reports back.
    fn admit_read(&self) -> bool {
        let until = self.skip_until.load(Ordering::Acquire);
        if until == 0 {
            return true;
        }
        if self.now_ms() < until {
            return false;
        }
        self.skip_until
            .compare_exchange(
                until,
                self.cooldown_end(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Records a bucket call that the bucket answered, "not found" included.
    fn succeeded(&self) {
        self.skip_until.store(0, Ordering::Release);
        if !self.up.swap(true, Ordering::AcqRel) {
            set_bucket_gauge(true);
            tracing::info!("bucket is answering again");
        }
    }

    /// Records a bucket failure: starts the cooldown and, on the first one, warns.
    fn failed(&self, op: &str, key: &CacheKey, error: &io::Error) {
        if !self.limits.cooldown.is_zero() {
            self.skip_until
                .store(self.cooldown_end(), Ordering::Release);
        }
        if self.up.swap(false, Ordering::AcqRel) {
            set_bucket_gauge(false);
            tracing::warn!(
                %key, op, %error, cooldown = ?self.limits.cooldown,
                "bucket failed; serving from the local tiers until it answers again"
            );
        } else {
            tracing::debug!(%key, op, %error, "bucket still failing");
        }
    }
}

fn set_bucket_gauge(up: bool) {
    metrics::gauge!("mammoth_tier_up", "tier" => "bucket").set(if up { 1.0 } else { 0.0 });
}

/// Counts a read against a tier and how long the tier took to answer it.
fn record_read<T>(tier: Tier, started: Instant, result: &io::Result<Option<T>>) {
    let result = match result {
        Ok(Some(_)) => "hit",
        Ok(None) => "miss",
        Err(_) => "error",
    };
    metrics::counter!("mammoth_tier_reads_total", "tier" => tier.as_str(), "result" => result)
        .increment(1);
    metrics::histogram!("mammoth_tier_read_duration_seconds", "tier" => tier.as_str())
        .record(started.elapsed().as_secs_f64());
}

/// Counts a write to a tier, how long it took and the bytes it wrote. A write that failed because
/// of its body (the client's upload failed, it was too large, or its copy was cut short) says
/// nothing about the tier and is not counted.
fn record_write(
    tier: Tier,
    started: Instant,
    outcome: Result<&PutOutcome, &io::Error>,
    bytes: u64,
) {
    let result = match outcome {
        Ok(_) => "ok",
        Err(e) if is_upload_error(e) || is_too_large(e) || tee::is_abandoned(e) => return,
        Err(_) => "error",
    };
    metrics::counter!("mammoth_tier_writes_total", "tier" => tier.as_str(), "result" => result)
        .increment(1);
    metrics::histogram!("mammoth_tier_write_duration_seconds", "tier" => tier.as_str())
        .record(started.elapsed().as_secs_f64());
    if outcome.is_ok_and(|outcome| *outcome == PutOutcome::Stored) {
        metrics::counter!("mammoth_tier_written_bytes_total", "tier" => tier.as_str())
            .increment(bytes);
    }
}

/// Counts the bytes read from `tier` as the entry streams out.
fn metered(tier: Tier, blob: Blob) -> Blob {
    let counter = metrics::counter!("mammoth_tier_read_bytes_total", "tier" => tier.as_str());
    let body = blob.body.inspect(move |chunk| {
        if let Ok(chunk) = chunk {
            counter.increment(chunk.len() as u64);
        }
    });
    Blob {
        size: blob.size,
        body: Box::pin(body),
    }
}

/// Writes an entry to a store, timing it from the start of the write.
async fn timed_put(
    store: &dyn BlobStore,
    key: &CacheKey,
    size_hint: Option<u64>,
    body: ByteStream,
) -> (Instant, io::Result<PutOutcome>) {
    let started = Instant::now();
    (started, store.put(key, size_hint, body).await)
}

/// Counts a bucket upload skipped because the bucket is cooling down after a failure. Logged at
/// debug only: the failure that started the cooldown was already logged.
fn drop_for_bucket_down(key: &CacheKey) {
    metrics::counter!("mammoth_bucket_uploads_dropped_total", "reason" => "bucket-down")
        .increment(1);
    tracing::debug!(%key, "bucket is cooling down; upload dropped");
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
    /// The client's upload failed: it reset, ended before its `Content-Length` or stalled. The
    /// body stream marked the error with [`upload_error`]. Nothing was stored.
    Upload(io::Error),
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
            Self::Upload(e) => write!(f, "the upload failed: {e}"),
            Self::Unavailable(e) => write!(f, "storage unavailable: {e}"),
            Self::Internal(e) => write!(f, "storage error: {e}"),
        }
    }
}

impl std::error::Error for CacheError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TooLarge { .. } => None,
            Self::Upload(e) | Self::Unavailable(e) | Self::Internal(e) => Some(e),
        }
    }
}

impl From<io::Error> for CacheError {
    fn from(e: io::Error) -> Self {
        if is_upload_error(&e) {
            return Self::Upload(e);
        }
        match e.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Self::Unavailable(e),
            _ => Self::Internal(e),
        }
    }
}

/// Marks an error from the client's request body, such as a reset or a truncated upload, so a
/// store that fails on it reports [`CacheError::Upload`] rather than a storage error. Adapters
/// wrap their body stream's errors in it before handing the stream to [`Cache::put`].
pub fn upload_error(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::other(UploadFailed(error.into()))
}

fn is_upload_error(e: &io::Error) -> bool {
    find::<UploadFailed>(e).is_some()
}

/// Finds an error of type `T` anywhere in `e`'s chain, such as a marker wrapped in context.
///
/// `io::Error::source` skips the error an `io::Error` wraps and goes straight to that error's
/// source, so this steps into every `io::Error` it meets with `get_ref` instead.
pub fn find<T: std::error::Error + 'static>(e: &io::Error) -> Option<&T> {
    let mut next: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(current) = next {
        if let Some(found) = current.downcast_ref::<T>() {
            return Some(found);
        }
        next = match current
            .downcast_ref::<io::Error>()
            .and_then(io::Error::get_ref)
        {
            Some(inner) => Some(inner),
            None => current.source(),
        };
    }
    None
}

#[derive(Debug)]
struct UploadFailed(Box<dyn std::error::Error + Send + Sync>);

impl fmt::Display for UploadFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for UploadFailed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.0)
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
    health: Arc<BucketHealth>,
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
    bucket_limits: BucketLimits,
    max_entry_size: u64,
    oversize: OversizePolicy,
}

impl Default for CacheBuilder {
    fn default() -> Self {
        Self {
            tiers: Vec::new(),
            bucket: None,
            bucket_limits: BucketLimits::default(),
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

    /// Sets the bucket's read deadline and cooldown.
    #[must_use]
    pub fn bucket_limits(mut self, limits: BucketLimits) -> Self {
        self.bucket_limits = limits;
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
        // Exported from the start, so a dashboard sees 0 rather than no data.
        metrics::gauge!("mammoth_shutting_down").set(0.0);
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
        let limits = self.bucket_limits;
        let bucket = self.bucket.map(|(store, write)| {
            let health = Arc::new(BucketHealth::new(limits));
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
                        health.clone(),
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
                (BucketWrite::Async { .. }, _) => {
                    tracing::warn!(
                        "bucket writes are async, but no local tier keeps whole entries to \
                         upload from (configure [disk]); writing to the bucket inline instead"
                    );
                    None
                }
                (BucketWrite::Sync, _) => None,
            };
            BucketHandle {
                store,
                health,
                uploads,
            }
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
            gate: Arc::default(),
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
    /// Stores and promotions in progress, per key.
    gate: Arc<Gate>,
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
    /// A hit below the first tier is copied into the local tiers above it as it streams. A tier
    /// that fails is skipped and the next one tried: for a cache, a failed read is a miss. So is
    /// a bucket read past [`BucketLimits::timeout`], or one during a cooldown.
    pub async fn get(&self, key: &CacheKey) -> Result<Option<Hit>, CacheError> {
        let hit = self.get_uncounted(key).await?;
        self.count_lookup(hit.as_ref().map(|hit| hit.tier));
        Ok(hit)
    }

    /// Like [`get`](Self::get), but leaves the lookup uncounted, for a caller that may still find
    /// the entry unusable (a REAPI action result whose outputs are gone) to count with
    /// [`count_lookup`](Self::count_lookup) once it knows. A hit is promoted all the same.
    pub async fn get_uncounted(&self, key: &CacheKey) -> Result<Option<Hit>, CacheError> {
        for (depth, handle) in self.tiers.iter().enumerate() {
            let tier = handle.store.tier();
            let started = Instant::now();
            let result = handle.store.get(key).await;
            record_read(tier, started, &result);
            match result {
                Ok(Some(blob)) => return Ok(Some(self.hit(key, tier, metered(tier, blob), depth))),
                Ok(None) => {}
                Err(error) => tracing::warn!(%key, %tier, %error, "tier read failed; skipping"),
            }
        }
        if let Some(bucket) = &self.bucket
            && let Some(blob) = bucket_read(bucket, key, "get", || bucket.store.get(key)).await
        {
            let blob = metered(Tier::Bucket, blob);
            return Ok(Some(self.hit(key, Tier::Bucket, blob, self.tiers.len())));
        }
        Ok(None)
    }

    /// Counts a load in the `serverStatus` totals and in `mammoth_cache_lookups_total` at once,
    /// so the two cannot disagree. `tier` served the hit; `None` is a miss.
    pub fn count_lookup(&self, tier: Option<Tier>) {
        let (total, result, tier) = match tier {
            Some(tier) => (&self.hits, "hit", tier.as_str()),
            None => (&self.misses, "miss", ""),
        };
        total.fetch_add(1, Ordering::Relaxed);
        metrics::counter!("mammoth_cache_lookups_total", "result" => result, "tier" => tier)
            .increment(1);
    }

    /// Counts an oversized store in the `serverStatus` total and as `too_large`.
    fn count_oversized(&self) {
        self.oversized.fetch_add(1, Ordering::Relaxed);
        metrics::counter!("mammoth_cache_stores_total", "result" => "too_large").increment(1);
    }

    /// A hit at `depth` in the lookup order, promoted into the local tiers above.
    fn hit(&self, key: &CacheKey, tier: Tier, blob: Blob, depth: usize) -> Hit {
        let above = &self.tiers[..depth];
        let blob = if above.is_empty() {
            blob
        } else {
            match self.gate.promotion(key) {
                Some(promotion) => promote(key, tier, blob, above, promotion),
                // A client is storing the key now; its copy is the one to keep.
                None => blob,
            }
        };
        Hit { tier, blob }
    }

    /// Returns an entry's size without reading it. `Ok(None)` is a miss, as for [`get`](Self::get).
    pub async fn head(&self, key: &CacheKey) -> Result<Option<u64>, CacheError> {
        for handle in &self.tiers {
            let started = Instant::now();
            let result = handle.store.head(key).await;
            record_read(handle.store.tier(), started, &result);
            match result {
                Ok(Some(size)) => return Ok(Some(size)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%key, tier = %handle.store.tier(), %error, "tier head failed");
                }
            }
        }
        let Some(bucket) = &self.bucket else {
            return Ok(None);
        };
        Ok(bucket_read(bucket, key, "head", || bucket.store.head(key)).await)
    }

    /// Whether the bucket answered its last call; `None` without a bucket. Mirrors the
    /// `mammoth_tier_up{tier="bucket"}` gauge.
    pub fn bucket_up(&self) -> Option<bool> {
        self.bucket.as_ref().map(|bucket| bucket.health.is_up())
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
        self.count_oversized();
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
    /// A store to an existing immutable (Gradle) key writes nothing, nor does one racing another
    /// store of the same key; one to a mutable (Maven) key replaces the entry.
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
        let received = Arc::new(AtomicU64::new(0));
        let result = self.store(key, declared_size, body, &received).await;
        // Oversized stores were counted where they were found.
        let counted = match &result {
            Ok(StoreOutcome::Stored) => {
                let size = declared_size.unwrap_or_else(|| received.load(Ordering::Relaxed));
                #[allow(clippy::cast_precision_loss)] // A histogram; exact below 2^53 bytes.
                metrics::histogram!("mammoth_cache_entry_size_bytes").record(size as f64);
                Some("stored")
            }
            Ok(StoreOutcome::Exists) => Some("exists"),
            Ok(StoreOutcome::Dropped) | Err(CacheError::TooLarge { .. }) => None,
            // The client's fault, such as a dropped connection, not the node's.
            Err(CacheError::Upload(_)) => Some("client_error"),
            Err(_) => Some("error"),
        };
        if let Some(result) = counted {
            metrics::counter!("mammoth_cache_stores_total", "result" => result).increment(1);
        }
        result
    }

    /// [`put`](Self::put), with `received` counting the body's bytes as they pass.
    async fn store(
        &self,
        key: &CacheKey,
        declared_size: Option<u64>,
        body: ByteStream,
        received: &Arc<AtomicU64>,
    ) -> Result<StoreOutcome, CacheError> {
        match self.admit(key, declared_size) {
            Admission::Accept => {}
            Admission::Drop => return Ok(StoreOutcome::Dropped),
            Admission::Reject(e) => return Err(e),
        }
        let immutable = key.namespace().is_immutable();
        // A promotion in progress shows the entry exists; an immutable one need not wait for it.
        if immutable && self.gate.promoting(key) && self.head(key).await?.is_some() {
            return Ok(StoreOutcome::Exists);
        }
        // Racing stores of an immutable key may carry different bytes (a task that is not
        // reproducible), so they write one at a time, and the later ones find the first's copy.
        // A later one writes only if the first failed.
        let _gate = self.gate.store(key, immutable).await;
        if immutable && self.head(key).await?.is_some() {
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
            // While the bucket cools down, keep the entry local, unless there is nowhere else.
            if self.tiers.is_empty() || !bucket.health.cooling_down() {
                writers.push(&bucket.store);
            } else {
                drop_for_bucket_down(key);
            }
        }
        let Some((primary, others)) = writers.split_first() else {
            return Err(CacheError::Internal(io::Error::other(
                "no storage tiers configured",
            )));
        };

        let limit = self.max_entry_size;
        let drain = self.oversize_policy(key.namespace()) == OversizePolicy::Drop;
        let (body, copies) = tee(
            limit_stream(body, limit, drain, received.clone()),
            others.len(),
            tee::Mode::Lockstep,
            None,
        );
        let secondary = others.iter().zip(copies).map(|(store, copy)| async move {
            let (started, outcome) = timed_put(store.as_ref(), key, declared_size, copy).await;
            (store.tier(), started, outcome)
        });
        let ((started, result), secondary) = future::join(
            timed_put(primary.as_ref(), key, declared_size, body),
            future::join_all(secondary),
        )
        .await;
        let written = received.load(Ordering::Relaxed);
        self.record_writes(
            key,
            (primary.tier(), started),
            result.as_ref(),
            secondary,
            written,
        );

        match result {
            Ok(outcome) => {
                if outcome == PutOutcome::Stored {
                    self.enqueue_upload(key);
                }
                Ok(outcome.into())
            }
            Err(e) if is_too_large(&e) => {
                self.count_oversized();
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

    /// Logs and counts the outcomes of one store's writes of `written` bytes, given the primary
    /// tier's result.
    fn record_writes(
        &self,
        key: &CacheKey,
        (primary, started): (Tier, Instant),
        result: Result<&PutOutcome, &io::Error>,
        secondary: Vec<(Tier, Instant, io::Result<PutOutcome>)>,
        written: u64,
    ) {
        // A secondary write's outcome says something about its tier only when the body was
        // good, which the primary's success shows.
        if result.is_ok() {
            for (tier, started, outcome) in secondary {
                record_write(tier, started, outcome.as_ref(), written);
                match (tier, outcome) {
                    (Tier::Bucket, outcome) => {
                        // A sync upload counts like a background one. Its failure still answers
                        // the client with success: the entry is stored locally, and Gradle would
                        // turn its cache off over an error.
                        let result = if outcome.is_ok() { "ok" } else { "error" };
                        metrics::counter!("mammoth_bucket_uploads_total", "result" => result)
                            .increment(1);
                        self.record_bucket_health(key, outcome.as_ref());
                    }
                    (_, Ok(_)) => {}
                    (tier, Err(error)) => {
                        tracing::warn!(%key, %tier, %error, "write to a secondary tier failed");
                    }
                }
            }
        }
        // A failed primary write may be the client's fault; `record_write` tells them apart.
        record_write(primary, started, result, written);
        if primary == Tier::Bucket && result.is_ok() {
            self.record_bucket_health(key, result);
        }
    }

    /// Records an inline write to the bucket in the bucket's health.
    fn record_bucket_health(&self, key: &CacheKey, outcome: Result<&PutOutcome, &io::Error>) {
        let Some(bucket) = &self.bucket else {
            return;
        };
        match outcome {
            Ok(_) => bucket.health.succeeded(),
            Err(error) => bucket.health.failed("put", key, error),
        }
    }

    /// Queues a background upload to the bucket, when there is one. A full queue drops the upload,
    /// and so does a bucket cooling down after a failure.
    fn enqueue_upload(&self, key: &CacheKey) {
        let Some(bucket) = &self.bucket else {
            return;
        };
        let Some(uploads) = &bucket.uploads else {
            return;
        };
        if bucket.health.cooling_down() {
            drop_for_bucket_down(key);
            return;
        }
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

/// Reads from the bucket under its deadline and cooldown. Failures and timeouts are misses; they
/// are counted, and logged when the bucket changes between up and down.
async fn bucket_read<'a, T>(
    bucket: &'a BucketHandle,
    key: &CacheKey,
    op: &'static str,
    read: impl FnOnce() -> BoxFuture<'a, io::Result<Option<T>>>,
) -> Option<T> {
    let health = &bucket.health;
    if !health.admit_read() {
        return None;
    }
    let timeout = health.limits.timeout;
    let started = Instant::now();
    let result = tokio::time::timeout(timeout, read())
        .await
        .unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no answer within {timeout:?}"),
            ))
        });
    record_read(Tier::Bucket, started, &result);
    match result {
        Ok(found) => {
            health.succeeded();
            found
        }
        Err(error) => {
            health.failed(op, key, &error);
            None
        }
    }
}

/// Copies a hit from a lower tier into the tiers `above` it while it streams to the client. Each
/// copy is written in the background and abandoned if it falls behind the client, or when a store
/// of the key stops the promotion. Its guard holds off stores of the key until every copy is done.
fn promote(
    key: &CacheKey,
    from: Tier,
    blob: Blob,
    above: &[TierHandle],
    promotion: gate::Promotion,
) -> Blob {
    if above.is_empty() {
        return blob;
    }
    let size = blob.size;
    // Skip tiers that would discard the entry anyway.
    let above: Vec<&TierHandle> = above
        .iter()
        .filter(|handle| handle.store.max_entry_size().is_none_or(|max| size <= max))
        .collect();
    if above.is_empty() {
        return blob;
    }
    let (body, copies) = tee(blob.body, above.len(), tee::Mode::Lossy, Some(size));
    let guard = Arc::new(promotion.guard);
    for (handle, copy) in above.into_iter().zip(copies) {
        let store = handle.store.clone();
        let key = key.clone();
        let guard = guard.clone();
        let copy = abortable(copy, promotion.abort.clone());
        tokio::spawn(async move {
            let _guard = guard;
            let to = store.tier();
            let (started, outcome) = timed_put(store.as_ref(), &key, Some(size), copy).await;
            record_write(to, started, outcome.as_ref(), size);
            match outcome {
                Ok(PutOutcome::Stored) => {
                    metrics::counter!(
                        "mammoth_tier_promotions_total",
                        "from" => from.as_str(),
                        "to" => to.as_str(),
                    )
                    .increment(1);
                }
                Ok(_) => {}
                Err(error) => tracing::debug!(%key, %from, %to, %error, "promotion abandoned"),
            }
        });
    }
    Blob { size, body }
}

/// `body`, failing as abandoned once `abort` turns `true`, so the tier writing it stops and cleans
/// up as for a copy that fell behind.
fn abortable(body: ByteStream, abort: watch::Receiver<bool>) -> ByteStream {
    stream::unfold(Some((body, abort)), |state| async move {
        let (mut body, mut abort) = state?;
        let item = tokio::select! {
            biased;
            // The sender lives as long as the promotion's guard, which outlives this copy.
            _ = abort.wait_for(|abort| *abort) => None,
            item = body.next() => Some(item?),
        };
        match item {
            Some(item) => Some((item, Some((body, abort)))),
            None => Some((Err(tee::abandoned()), None)),
        }
    })
    .boxed()
}

/// Uploads stored entries to the bucket, reading each one back from the deepest local tier.
/// On `stop`, closes the queue and finishes the keys already in it.
async fn upload_worker(
    queue: mpsc::Receiver<CacheKey>,
    source: Arc<dyn BlobStore>,
    bucket: Arc<dyn BlobStore>,
    health: Arc<BucketHealth>,
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
    // Keys being uploaded, and those stored again meanwhile. A replaced (Maven or Bazel AC) entry
    // is uploaded again after the upload in progress, never beside it, so the last upload to
    // finish is always the newest copy.
    let in_flight = Mutex::new(HashMap::<CacheKey, bool>::new());
    keys.for_each_concurrent(UPLOAD_CONCURRENCY, |key| {
        let (source, bucket, health) = (&source, &bucket, &health);
        let (pending, in_flight) = (&pending, &in_flight);
        async move {
            if health.cooling_down() {
                // Queued before the bucket failed.
                drop_for_bucket_down(&key);
                pending.fetch_sub(1, Ordering::Relaxed);
                return;
            }
            {
                let mut in_flight = in_flight.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(again) = in_flight.get_mut(&key) {
                    *again = true;
                    pending.fetch_sub(1, Ordering::Relaxed);
                    return;
                }
                in_flight.insert(key.clone(), false);
            }
            loop {
                upload(&key, source.as_ref(), bucket.as_ref(), health).await;
                let mut in_flight = in_flight.lock().unwrap_or_else(PoisonError::into_inner);
                if in_flight.get(&key) == Some(&true) {
                    in_flight.insert(key.clone(), false);
                } else {
                    in_flight.remove(&key);
                    break;
                }
            }
            pending.fetch_sub(1, Ordering::Relaxed);
        }
    })
    .await;
}

/// Uploads one entry from `source` to the bucket, and counts the result. Skipped while the bucket
/// cools down after a failure.
async fn upload(
    key: &CacheKey,
    source: &dyn BlobStore,
    bucket: &dyn BlobStore,
    health: &BucketHealth,
) {
    if health.cooling_down() {
        drop_for_bucket_down(key);
        return;
    }
    let outcome = match source.get(key).await {
        Ok(Some(blob)) => {
            let size = blob.size;
            let (started, outcome) = timed_put(bucket, key, Some(size), blob.body).await;
            record_write(Tier::Bucket, started, outcome.as_ref(), size);
            match outcome {
                Ok(_) => {
                    health.succeeded();
                    "ok"
                }
                // The body comes from a local tier, so a failure is the bucket's. The bucket's
                // health logs it.
                Err(error) => {
                    health.failed("put", key, &error);
                    "error"
                }
            }
        }
        Ok(None) => {
            tracing::debug!(%key, "entry left the local tiers before its upload");
            "skipped"
        }
        Err(error) => {
            tracing::warn!(%key, %error, "reading an entry back for upload failed");
            "error"
        }
    };
    metrics::counter!("mammoth_bucket_uploads_total", "result" => outcome).increment(1);
}

/// Fails the stream once more than `limit` bytes have passed, so the store discards the entry.
/// With `drain`, reads the rest of the body first, so the client finishes sending it. `received`
/// holds the bytes passed so far.
fn limit_stream(body: ByteStream, limit: u64, drain: bool, received: Arc<AtomicU64>) -> ByteStream {
    Box::pin(stream::unfold(Some((body, 0_u64)), move |state| {
        let received = received.clone();
        async move {
            let (mut body, seen) = state?;
            let chunk = match body.next().await? {
                Ok(chunk) => chunk,
                Err(e) => return Some((Err(e), None)),
            };
            let seen = seen + chunk.len() as u64;
            received.store(seen, Ordering::Relaxed);
            if seen <= limit {
                return Some((Ok(chunk), Some((body, seen))));
            }
            if drain {
                while let Some(Ok(_)) = body.next().await {}
            }
            Some((Err(io::Error::other(EntryTooLarge)), None))
        }
    }))
}

fn is_too_large(e: &io::Error) -> bool {
    find::<EntryTooLarge>(e).is_some()
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

    /// Context around an error, the way a tier adds it: an `io::Error` whose inner error has the
    /// wrapped error as its source.
    #[derive(Debug)]
    struct Context(io::Error);

    impl fmt::Display for Context {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("while storing")
        }
    }

    impl std::error::Error for Context {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    fn wrapped(e: io::Error) -> io::Error {
        io::Error::new(e.kind(), Context(e))
    }

    fn classify(e: io::Error) -> &'static str {
        match CacheError::from(e) {
            CacheError::TooLarge { .. } => "too large",
            CacheError::Upload(_) => "upload",
            CacheError::Unavailable(_) => "unavailable",
            CacheError::Internal(_) => "internal",
        }
    }

    #[test]
    fn classification_by_kind() {
        let kinds = [
            (io::ErrorKind::TimedOut, "unavailable"),
            (io::ErrorKind::WouldBlock, "unavailable"),
            (io::ErrorKind::Other, "internal"),
            (io::ErrorKind::PermissionDenied, "internal"),
            (io::ErrorKind::StorageFull, "internal"),
            (io::ErrorKind::UnexpectedEof, "internal"),
        ];
        for (kind, want) in kinds {
            assert_eq!(classify(io::Error::new(kind, "x")), want, "{kind:?}");
            assert_eq!(
                classify(wrapped(io::Error::new(kind, "x"))),
                want,
                "wrapped {kind:?}"
            );
        }
    }

    #[test]
    fn classification_finds_the_upload_marker() {
        assert_eq!(classify(upload_error("reset")), "upload");
        assert_eq!(classify(wrapped(upload_error("reset"))), "upload");
        assert_eq!(classify(wrapped(wrapped(upload_error("reset")))), "upload");
    }

    #[test]
    fn find_reaches_markers_through_context() {
        let too_large = || io::Error::other(EntryTooLarge);
        assert!(is_too_large(&too_large()));
        assert!(is_too_large(&wrapped(too_large())));
        assert!(is_too_large(&wrapped(wrapped(too_large()))));
        assert!(!is_too_large(&io::Error::other("plain")));
        assert!(!is_too_large(&upload_error("reset")));

        // Past the marker to its own source, the way adapters downcast their upload errors.
        let deep = wrapped(upload_error(Context(io::Error::other("deep"))));
        assert!(find::<UploadFailed>(&deep).is_some());
        assert!(find::<Context>(&deep).is_some());
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

    mod bucket_outage {
        use std::{
            sync::atomic::AtomicU8,
            time::{Duration, Instant},
        };

        use super::*;
        use crate::store::BoxFuture;

        const HANG: u8 = 0;
        const FAIL: u8 = 1;
        const ANSWER: u8 = 2;

        /// A bucket that hangs, fails or answers "not found", and counts the calls it gets.
        #[derive(Debug)]
        struct FakeBucket {
            mode: AtomicU8,
            reads: AtomicU64,
            writes: AtomicU64,
        }

        impl FakeBucket {
            fn new(mode: u8) -> Arc<Self> {
                Arc::new(Self {
                    mode: AtomicU8::new(mode),
                    reads: AtomicU64::new(0),
                    writes: AtomicU64::new(0),
                })
            }

            fn set(&self, mode: u8) {
                self.mode.store(mode, Ordering::SeqCst);
            }

            fn reads(&self) -> u64 {
                self.reads.load(Ordering::SeqCst)
            }

            fn answer<'a, T: Send + 'a>(&self) -> BoxFuture<'a, io::Result<Option<T>>> {
                self.reads.fetch_add(1, Ordering::SeqCst);
                match self.mode.load(Ordering::SeqCst) {
                    HANG => Box::pin(std::future::pending()),
                    FAIL => Box::pin(async { Err(io::Error::other("bucket unreachable")) }),
                    _ => Box::pin(async { Ok(None) }),
                }
            }
        }

        impl BlobStore for FakeBucket {
            fn tier(&self) -> Tier {
                Tier::Bucket
            }

            fn get<'a>(&'a self, _: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<Blob>>> {
                self.answer()
            }

            fn head<'a>(&'a self, _: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<u64>>> {
                self.answer()
            }

            fn put<'a>(
                &'a self,
                _: &'a CacheKey,
                _: Option<u64>,
                mut body: ByteStream,
            ) -> BoxFuture<'a, io::Result<PutOutcome>> {
                self.writes.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    while body.next().await.is_some() {}
                    Ok(PutOutcome::Stored)
                })
            }
        }

        const TIMEOUT: Duration = Duration::from_millis(50);
        const COOLDOWN: Duration = Duration::from_millis(200);

        fn cache_over(bucket: &Arc<FakeBucket>, cooldown: Duration) -> Cache {
            Cache::builder()
                .bounded_tier(Arc::new(MemoryStore::new(1 << 20)))
                .bucket_tier(bucket.clone(), BucketWrite::Sync)
                .bucket_limits(BucketLimits {
                    timeout: TIMEOUT,
                    cooldown,
                })
                .build()
        }

        #[tokio::test]
        async fn a_hanging_bucket_is_a_miss_within_the_timeout() {
            let bucket = FakeBucket::new(HANG);
            let cache = cache_over(&bucket, COOLDOWN);
            let started = Instant::now();
            assert!(cache.get(&key("a1")).await.unwrap().is_none());
            let took = started.elapsed();
            assert!(took >= TIMEOUT && took < TIMEOUT * 10, "took {took:?}");
            assert_eq!(cache.bucket_up(), Some(false));
            assert_eq!(bucket.reads(), 1);
        }

        #[tokio::test]
        async fn the_cooldown_skips_the_bucket_then_probes_it() {
            let bucket = FakeBucket::new(FAIL);
            let cache = cache_over(&bucket, COOLDOWN);
            let k = key("a1");
            assert!(cache.get(&k).await.unwrap().is_none());
            assert_eq!((bucket.reads(), cache.bucket_up()), (1, Some(false)));

            // Inside the cooldown: misses without calling the bucket, and stores stay local.
            assert!(cache.get(&k).await.unwrap().is_none());
            assert_eq!(cache.head(&k).await.unwrap(), None);
            assert_eq!(bucket.reads(), 1);
            let stored = cache.put(&key("b2"), Some(1), body(&[b"x"])).await.unwrap();
            assert_eq!(stored, StoreOutcome::Stored);
            assert_eq!(bucket.writes.load(Ordering::SeqCst), 0);

            // After it, the first call probes; a failed probe starts a new cooldown.
            tokio::time::sleep(COOLDOWN + TIMEOUT).await;
            assert_eq!(cache.head(&k).await.unwrap(), None);
            assert_eq!(bucket.reads(), 2);
            assert_eq!(cache.head(&k).await.unwrap(), None);
            assert_eq!(bucket.reads(), 2);

            // A probe that succeeds brings the bucket back.
            bucket.set(ANSWER);
            tokio::time::sleep(COOLDOWN + TIMEOUT).await;
            assert!(cache.get(&k).await.unwrap().is_none());
            assert_eq!((bucket.reads(), cache.bucket_up()), (3, Some(true)));
            assert_eq!(cache.head(&k).await.unwrap(), None);
            assert_eq!(bucket.reads(), 4);
            cache.put(&key("c3"), Some(1), body(&[b"x"])).await.unwrap();
            assert_eq!(bucket.writes.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn without_a_cooldown_every_call_tries_the_bucket() {
            let bucket = FakeBucket::new(FAIL);
            let cache = cache_over(&bucket, Duration::ZERO);
            for _ in 0..3 {
                assert_eq!(cache.head(&key("a1")).await.unwrap(), None);
            }
            assert_eq!((bucket.reads(), cache.bucket_up()), (3, Some(false)));
            bucket.set(ANSWER);
            assert_eq!(cache.head(&key("a1")).await.unwrap(), None);
            assert_eq!(cache.bucket_up(), Some(true));
        }

        #[test]
        fn the_gauge_follows_the_bucket() {
            let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            let gauge = || {
                handle
                    .render()
                    .lines()
                    .find(|line| line.starts_with("mammoth_tier_up{tier=\"bucket\"}"))
                    .map(str::to_owned)
            };
            let k = key("a1");
            let error = io::Error::other("down");
            metrics::with_local_recorder(&recorder, || {
                let health = BucketHealth::new(BucketLimits::default());
                assert_eq!(
                    gauge().as_deref(),
                    Some("mammoth_tier_up{tier=\"bucket\"} 1")
                );
                health.failed("get", &k, &error);
                assert_eq!(
                    gauge().as_deref(),
                    Some("mammoth_tier_up{tier=\"bucket\"} 0")
                );
                health.succeeded();
                assert_eq!(
                    gauge().as_deref(),
                    Some("mammoth_tier_up{tier=\"bucket\"} 1")
                );
            });
        }
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
        async fn racing_stores_of_one_key_write_one_copy() {
            let t = tiers().await;
            let cache = Arc::new(cache(&t, BucketWrite::Sync));
            let k = key("ab12");
            let racers = (0..8_u8).map(|i| {
                let (cache, k) = (cache.clone(), k.clone());
                tokio::spawn(async move {
                    // Slow bodies, so the stores overlap.
                    let chunks = stream::iter([[i; 4], [i; 4]]).then(|chunk| async move {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Ok(Bytes::copy_from_slice(&chunk))
                    });
                    cache.put(&k, Some(8), chunks.boxed()).await.unwrap()
                })
            });
            let outcomes = future::join_all(racers).await;
            let stored = outcomes
                .into_iter()
                .filter(|outcome| *outcome.as_ref().unwrap() == StoreOutcome::Stored)
                .count();
            assert_eq!(stored, 1);
            let memory = read_all(t.memory.get(&k).await.unwrap().unwrap()).await;
            let disk = read_all(t.disk.get(&k).await.unwrap().unwrap()).await;
            let bucket = read_all(t.bucket.get(&k).await.unwrap().unwrap()).await;
            assert_eq!(memory, disk);
            assert_eq!(memory, bucket);
        }

        #[tokio::test]
        async fn a_racing_store_writes_when_the_first_one_fails() {
            let t = tiers().await;
            let cache = Arc::new(cache(&t, BucketWrite::Sync));
            let k = key("ab13");
            // The first store sends half its body, then the client goes away.
            let (fail, failed) = tokio::sync::oneshot::channel::<()>();
            let first =
                stream::iter([Ok(Bytes::from_static(b"1111"))]).chain(stream::once(async move {
                    let _ = failed.await;
                    Err(io::Error::other("client went away"))
                }));
            let first = tokio::spawn({
                let (cache, k) = (cache.clone(), k.clone());
                async move { cache.put(&k, Some(8), first.boxed()).await }
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            let second = tokio::spawn({
                let (cache, k) = (cache.clone(), k.clone());
                async move { cache.put(&k, Some(8), body(&[b"22222222"])).await }
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            fail.send(()).unwrap();

            assert!(first.await.unwrap().is_err());
            // Not told "exists" for an entry that never landed: it waited, then wrote its own.
            assert_eq!(second.await.unwrap().unwrap(), StoreOutcome::Stored);
            let hit = cache.get(&k).await.unwrap().unwrap();
            assert_eq!(read_all(hit.blob).await, b"22222222");
        }

        #[tokio::test]
        async fn a_promotion_never_lands_over_a_newer_store() {
            let t = tiers().await;
            let cache = Arc::new(
                Cache::builder()
                    .bounded_tier(t.memory.clone())
                    .bucket_tier(t.bucket.clone(), BucketWrite::Sync)
                    .build(),
            );
            let k = CacheKey::maven("v1/g/a/buildinfo.xml").unwrap();
            t.bucket.put(&k, Some(3), body(&[b"old"])).await.unwrap();

            // A read hits the bucket and starts promoting the old copy into memory.
            let hit = cache.get(&k).await.unwrap().unwrap();
            assert_eq!(hit.tier, Tier::Bucket);
            // Meanwhile a client stores a new copy.
            let store = tokio::spawn({
                let (cache, k) = (cache.clone(), k.clone());
                async move { cache.put(&k, Some(3), body(&[b"new"])).await }
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(read_all(hit.blob).await, b"old");
            assert_eq!(store.await.unwrap().unwrap(), StoreOutcome::Stored);
            // Let any promotion still running finish.
            tokio::time::sleep(Duration::from_millis(20)).await;

            let memory = read_all(t.memory.get(&k).await.unwrap().unwrap()).await;
            assert_eq!(memory, b"new");
        }

        #[tokio::test]
        async fn a_replaced_entry_uploads_its_newest_copy() {
            let t = tiers().await;
            // Uploads read from the memory tier here, to test their order alone.
            let cache = Cache::builder()
                .bounded_tier(t.memory.clone())
                .bucket_tier(t.bucket.clone(), BucketWrite::Async { queue: 100 })
                .build();
            let k = CacheKey::maven("v1/g/a/buildinfo.xml").unwrap();
            for version in [b"v1", b"v2", b"v3"] {
                cache.put(&k, Some(2), body(&[version])).await.unwrap();
            }
            cache.flush_uploads().await;
            let bucket = read_all(t.bucket.get(&k).await.unwrap().unwrap()).await;
            assert_eq!(bucket, b"v3");
            assert_eq!(cache.pending_uploads(), 0);
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

        /// A read that stalls part way holds a promotion open. A store of the key must not wait
        /// for the client: a mutable store stops the promotion, and an immutable one, which the
        /// promotion shows exists, answers at once.
        #[tokio::test]
        async fn a_stalled_download_does_not_hold_up_stores() {
            let t = tiers().await;
            // A memory tier that takes the entry, so its hit is promoted.
            let memory: MemoryStore =
                MemoryStore::with_config(crate::store::memory::MemoryConfig {
                    max_entry_bytes: 1 << 20,
                    ..crate::store::memory::MemoryConfig::new(4 << 20)
                })
                .unwrap();
            let cache = Arc::new(
                Cache::builder()
                    .bounded_tier(Arc::new(memory))
                    .bounded_tier(t.disk.clone())
                    .build(),
            );
            let big = vec![9_u8; 512 * 1024];
            for k in [
                CacheKey::maven("v1/g/a/buildinfo.xml").unwrap(),
                key("5a11ed"),
            ] {
                t.disk
                    .put(
                        &k,
                        None,
                        stream::iter([Ok(Bytes::from(big.clone()))]).boxed(),
                    )
                    .await
                    .unwrap();
                // The client takes the first chunk, then stops reading.
                let hit = cache.get(&k).await.unwrap().unwrap();
                assert_eq!(hit.tier, Tier::Disk);
                let mut stalled = hit.blob.body;
                stalled.next().await.unwrap().unwrap();
                assert!(cache.gate.promoting(&k));

                let stored = tokio::time::timeout(
                    Duration::from_secs(2),
                    cache.put(&k, Some(3), body(&[b"new"])),
                )
                .await
                .expect("the store waited for the stalled download")
                .unwrap();
                if k.namespace().is_immutable() {
                    assert_eq!(stored, StoreOutcome::Exists);
                } else {
                    assert_eq!(stored, StoreOutcome::Stored);
                    let hit = cache.get(&k).await.unwrap().unwrap();
                    assert_eq!(read_all(hit.blob).await, b"new");
                }
                drop(stalled);
            }
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

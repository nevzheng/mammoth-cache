//! L2: the disk tier.
//!
//! Each entry is one file holding a header, the bytes and a checksum trailer (design doc 0006):
//!
//! ```text
//! magic "MMTHENT\0" | version u16 | checksum algorithm u16 | created u64 | size u64 | key length u32
//! | key | bytes | CRC32C u32
//! ```
//!
//! Integers are little-endian. Files live at `<path>/v1/<namespace>/<h[..2]>/<h>`, where `h` is a
//! hash of the key, so any key maps to a short, safe file name; the header holds the full key.
//! Writes go to `<path>/tmp/` and are renamed into place, so a crash never leaves a partial entry.
//! Files are not fsynced: a cache entry lost to a power cut costs one rebuild.
//!
//! The tier holds an exclusive lock on `<path>/mammoth.lock` while it is open, so a second process
//! pointed at the same directory fails to start instead of deleting the first one's uploads. At
//! startup it deletes only files it recognizes: temporary files named by its own scheme and entry
//! files whose names are entry hashes. Anything else is logged and left alone.
//!
//! An in-memory index tracks every entry's size, metadata and recency. It is rebuilt from the file
//! headers at startup, so a restarted node starts warm. Eviction is least recently used by bytes,
//! plus an optional maximum age. Sizes are counted in whole filesystem blocks, since that is what a
//! file uses on disk.
//!
//! A background task probes the directory every few seconds: it writes and removes a file, and
//! reads the volume's free space. The tier shrinks so that `free_space_buffer` bytes stay free on
//! the volume, and `/readyz` reports both results. A write that finds the volume full evicts
//! entries and carries on once.

use std::{
    collections::{BTreeMap, HashMap},
    fs as std_fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
};

use super::{
    Blob, BlobStore, BoundedStore, BoxFuture, ByteStream, HealthCheck, HealthProblem, PutOutcome,
    Tier, TierUsage,
    meta::{Checksum, EntryMeta, unix_now},
};
use crate::key::CacheKey;

const MAGIC: &[u8; 8] = b"MMTHENT\0";
const FORMAT_VERSION: u16 = 1;
const CHECKSUM_CRC32C: u16 = 1;
/// Header bytes before the key.
const FIXED_HEADER_LEN: usize = 8 + 2 + 2 + 8 + 8 + 4;
/// Offset of the size field, patched once the body has been written.
const SIZE_OFFSET: u64 = 8 + 2 + 2 + 8;
const TRAILER_LEN: u64 = 4;
const READ_CHUNK: usize = 64 * 1024;
/// Entries up to this size are read whole and checked before they are served; larger ones are
/// checked in a first pass, then streamed.
const READ_WHOLE: u64 = 1024 * 1024;
/// How often the directory is probed for writability and free space.
const PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// The allocation unit sizes are rounded up to. Most Linux filesystems use 4 KiB blocks.
const BLOCK_SIZE: u64 = 4096;
/// When a write finds the volume full, the tier shrinks to this percentage of its size (or of
/// its capacity, if smaller), so that the writes that follow do not each have to evict again.
const LOW_WATER_PERCENT: u64 = 90;
/// The lock file held while the tier is open.
const LOCK_FILE: &str = "mammoth.lock";
/// Temporary files are named `<TMP_PREFIX><pid>-<nonce>-<counter><TMP_SUFFIX>`.
const TMP_PREFIX: &str = "mmth-";
const TMP_SUFFIX: &str = ".tmp";
/// The file the probe writes and removes in `tmp/`.
const PROBE_FILE: &str = "probe";

/// Settings for a [`DiskStore`].
#[derive(Debug, Clone)]
pub struct DiskConfig {
    /// The directory the tier owns. Created if missing.
    pub path: PathBuf,
    /// Bytes on disk the tier may use, counting headers and rounding each file up to whole blocks.
    pub capacity_bytes: u64,
    /// Entries older than this are evicted, when set.
    pub max_entry_age: Option<Duration>,
    /// Bytes to leave free on the volume. The tier evicts entries to keep them free.
    pub free_space_buffer: u64,
}

/// The disk tier. Cheap to clone; clones share the same files and index.
#[derive(Debug, Clone)]
pub struct DiskStore {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    entries_dir: PathBuf,
    tmp_dir: PathBuf,
    capacity_bytes: u64,
    max_entry_age: Option<Duration>,
    free_space_buffer: u64,
    /// The most the tier may hold while keeping `free_space_buffer` free, from the last probe.
    space_cap: AtomicU64,
    probe: Mutex<Probe>,
    index: Mutex<Index>,
    /// Makes temporary file names unique to this process: `<TMP_PREFIX><pid>-<nonce>-`.
    tmp_prefix: String,
    next_tmp: AtomicU64,
    /// Held for the life of the tier; dropping it releases the lock.
    _lock: std_fs::File,
    /// Test hook: the n-th piece written from now fails as if the volume were full.
    #[cfg(test)]
    fail_write: AtomicU64,
}

/// What the last probe of the directory found.
#[derive(Debug, Clone, Default)]
struct Probe {
    /// Why a file could not be written and removed, if it could not.
    not_writable: Option<String>,
    /// Free bytes on the volume for unprivileged writers; `None` if unknown.
    available: Option<u64>,
}

#[derive(Debug, Default)]
struct Index {
    entries: HashMap<CacheKey, Slot>,
    /// Recency order: the smallest tick is the least recently used entry.
    lru: BTreeMap<u64, CacheKey>,
    next_tick: u64,
    size_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    meta: EntryMeta,
    /// The file's size rounded up to whole blocks.
    disk_bytes: u64,
    tick: u64,
    /// Unique to this insertion, so a reader can tell whether the entry it looked up is still the
    /// one in the index.
    generation: u64,
}

/// What a lookup found: the entry's metadata and the generation it was stored under.
#[derive(Debug, Clone, Copy)]
struct Found {
    meta: EntryMeta,
    generation: u64,
}

impl Index {
    fn insert(&mut self, key: CacheKey, meta: EntryMeta, file_len: u64) {
        self.remove(&key);
        let tick = self.tick();
        let disk_bytes = disk_usage(file_len);
        self.lru.insert(tick, key.clone());
        self.entries.insert(
            key,
            Slot {
                meta,
                disk_bytes,
                tick,
                generation: tick,
            },
        );
        self.size_bytes += disk_bytes;
    }

    fn remove(&mut self, key: &CacheKey) -> Option<Slot> {
        let slot = self.entries.remove(key)?;
        self.lru.remove(&slot.tick);
        self.size_bytes -= slot.disk_bytes;
        Some(slot)
    }

    /// Removes `key` if it is still the entry stored under `generation`. Returns false if the
    /// index holds a newer entry for the key, which must be left alone.
    fn remove_generation(&mut self, key: &CacheKey, generation: u64) -> bool {
        match self.entries.get(key) {
            Some(slot) if slot.generation == generation => {
                self.remove(key);
                true
            }
            Some(_) => false,
            None => true,
        }
    }

    fn touch(&mut self, key: &CacheKey) -> Option<Found> {
        let tick = self.tick();
        let slot = self.entries.get_mut(key)?;
        self.lru.remove(&slot.tick);
        slot.tick = tick;
        self.lru.insert(tick, key.clone());
        Some(Found {
            meta: slot.meta,
            generation: slot.generation,
        })
    }

    fn tick(&mut self) -> u64 {
        self.next_tick += 1;
        self.next_tick
    }

    /// Removes least recently used entries until the tier fits `capacity`, returning them.
    fn evict_to(&mut self, capacity: u64) -> Vec<CacheKey> {
        let mut evicted = Vec::new();
        while self.size_bytes > capacity {
            let Some((_, key)) = self.lru.pop_first() else {
                break;
            };
            if let Some(slot) = self.entries.remove(&key) {
                self.size_bytes -= slot.disk_bytes;
            }
            evicted.push(key);
        }
        evicted
    }
}

impl DiskStore {
    /// Opens the tier at `config.path`, rebuilding the index from the entry files already there.
    ///
    /// Fails if another process has the directory open. Leftover temporary files and damaged or
    /// expired entries are deleted, and the tier is trimmed to its capacity.
    pub async fn open(config: DiskConfig) -> io::Result<Self> {
        let path = config.path.clone();
        let entries_dir = config.path.join("v1");
        let tmp_dir = config.path.join("tmp");
        let (lock, scanned, entries_dir, tmp_dir) = tokio::task::spawn_blocking(move || {
            std_fs::create_dir_all(&path)?;
            let lock = lock_dir(&path)?;
            std_fs::create_dir_all(&entries_dir)?;
            std_fs::create_dir_all(&tmp_dir)?;
            clean_tmp(&tmp_dir)?;
            let scanned = scan(&entries_dir)?;
            Ok::<_, io::Error>((lock, scanned, entries_dir, tmp_dir))
        })
        .await
        .map_err(io::Error::other)?
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("disk tier at {}: {e}", config.path.display()),
            )
        })?;
        let nonce = getrandom::u32().unwrap_or_else(|_| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        });

        let store = Self {
            inner: Arc::new(Inner {
                entries_dir,
                tmp_dir,
                capacity_bytes: config.capacity_bytes,
                max_entry_age: config.max_entry_age,
                free_space_buffer: config.free_space_buffer,
                space_cap: AtomicU64::new(u64::MAX),
                probe: Mutex::default(),
                index: Mutex::default(),
                tmp_prefix: format!("{TMP_PREFIX}{}-{nonce:08x}-", std::process::id()),
                next_tmp: AtomicU64::new(0),
                _lock: lock,
                #[cfg(test)]
                fail_write: AtomicU64::new(0),
            }),
        };
        let mut expired = Vec::new();
        {
            let mut index = store.inner.lock();
            for found in scanned {
                if store.inner.is_expired(&found.meta) {
                    expired.push(found.key);
                } else {
                    index.insert(found.key, found.meta, found.file_len);
                }
            }
            expired.extend(index.evict_to(store.inner.capacity_bytes));
            tracing::info!(
                entries = index.entries.len(),
                bytes = index.size_bytes,
                "disk tier opened"
            );
        }
        store.inner.delete_files(&expired).await;
        store.inner.refresh().await;
        tokio::spawn(probe_periodically(Arc::downgrade(&store.inner)));
        Ok(store)
    }
}

/// Takes an exclusive lock on the directory's lock file, failing if another process holds it.
#[cfg(unix)]
fn lock_dir(dir: &Path) -> io::Result<std_fs::File> {
    let path = dir.join(LOCK_FILE);
    let file = std_fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "disk.path {} is in use by another mammoth-cache process ({} is locked); each \
                 process needs its own directory",
                dir.display(),
                path.display()
            ),
        )),
        Err(e) => Err(e.into()),
    }
}

/// Opens the lock file. Without `flock`, the directory is not protected from a second process.
#[cfg(not(unix))]
fn lock_dir(dir: &Path) -> io::Result<std_fs::File> {
    std_fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LOCK_FILE))
}

/// Deletes leftover temporary files: only names this tier writes, in case `tmp/` is not ours.
fn clean_tmp(tmp_dir: &Path) -> io::Result<()> {
    for entry in std_fs::read_dir(tmp_dir)? {
        let entry = entry?;
        let ours = entry.file_name().to_str().is_some_and(is_tmp_name);
        if ours && entry.file_type()?.is_file() {
            std_fs::remove_file(entry.path())?;
        } else {
            tracing::warn!(path = %entry.path().display(), "leaving an unknown file in the disk tier");
        }
    }
    Ok(())
}

/// Whether `name` is a temporary file name this tier writes, or wrote before names were unique
/// per process (a bare counter).
fn is_tmp_name(name: &str) -> bool {
    let current = name
        .strip_prefix(TMP_PREFIX)
        .and_then(|n| n.strip_suffix(TMP_SUFFIX));
    current.is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
        || name == PROBE_FILE
        || (!name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether `name` is an entry file name: the first 32 hex digits of a key's hash.
fn is_entry_name(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The size to shrink to when the volume is full: the low-water mark of the tier's size, or of
/// its capacity if that is smaller.
fn low_water(size_bytes: u64, capacity: u64) -> u64 {
    let from = size_bytes.min(capacity);
    from - from / 100 * (100 - LOW_WATER_PERCENT)
}

/// The space a file of `len` bytes uses on disk.
fn disk_usage(len: u64) -> u64 {
    len.div_ceil(BLOCK_SIZE).saturating_mul(BLOCK_SIZE)
}

/// Probes the directory until the store is dropped.
async fn probe_periodically(inner: Weak<Inner>) {
    let mut interval = tokio::time::interval(PROBE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // The first tick is immediate; `open` has just probed.
    loop {
        interval.tick().await;
        let Some(inner) = inner.upgrade() else { return };
        inner.refresh().await;
    }
}

/// Writes and removes a file in `tmp_dir`, and reads the free space under `entries_dir`.
fn probe(tmp_dir: &Path, entries_dir: &Path) -> Probe {
    let file = tmp_dir.join(PROBE_FILE);
    let not_writable = std_fs::write(&file, b"probe")
        .and_then(|()| std_fs::remove_file(&file))
        .err()
        .map(|e| e.to_string());
    Probe {
        not_writable,
        available: available_bytes(entries_dir).ok(),
    }
}

#[cfg(unix)]
fn available_bytes(path: &Path) -> io::Result<u64> {
    let stats = rustix::fs::statvfs(path)?;
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

#[cfg(not(unix))]
fn available_bytes(_path: &Path) -> io::Result<u64> {
    Err(io::ErrorKind::Unsupported.into())
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn last_probe(&self) -> Probe {
        self.probe
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The bytes the tier may hold now: its capacity, or less to keep the free space buffer.
    fn effective_capacity(&self) -> u64 {
        self.capacity_bytes
            .min(self.space_cap.load(Ordering::Relaxed))
    }

    /// Probes the directory, records the result, and evicts entries if free space is short.
    async fn refresh(&self) {
        let (tmp_dir, entries_dir) = (self.tmp_dir.clone(), self.entries_dir.clone());
        let Ok(probe) = tokio::task::spawn_blocking(move || probe(&tmp_dir, &entries_dir)).await
        else {
            return;
        };
        let previous = std::mem::replace(
            &mut *self.probe.lock().unwrap_or_else(PoisonError::into_inner),
            probe.clone(),
        );
        if probe.not_writable != previous.not_writable {
            if let Some(error) = &probe.not_writable {
                tracing::error!(%error, "disk tier is not writable");
            } else {
                tracing::info!("disk tier is writable again");
            }
        }
        let evicted = {
            let mut index = self.lock();
            let cap = probe.available.map_or(u64::MAX, |available| {
                (index.size_bytes + available).saturating_sub(self.free_space_buffer)
            });
            self.space_cap.store(cap, Ordering::Relaxed);
            if let Some(available) = probe.available {
                #[allow(clippy::cast_precision_loss)] // A gauge; exact below 2^53 bytes.
                metrics::gauge!("mammoth_disk_free_bytes").set(available as f64);
            }
            index.evict_to(self.effective_capacity())
        };
        if evicted.is_empty() {
            return;
        }
        tracing::warn!(
            entries = evicted.len(),
            buffer = self.free_space_buffer,
            "evicting disk entries to keep the free space buffer"
        );
        self.delete_files(&evicted).await;
        // Read the free space again, so readiness and the gauge reflect what eviction freed now
        // rather than at the next probe.
        let entries_dir = self.entries_dir.clone();
        if let Ok(Ok(available)) =
            tokio::task::spawn_blocking(move || available_bytes(&entries_dir)).await
        {
            self.probe
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .available = Some(available);
            #[allow(clippy::cast_precision_loss)] // A gauge; exact below 2^53 bytes.
            metrics::gauge!("mammoth_disk_free_bytes").set(available as f64);
        }
    }

    fn is_expired(&self, meta: &EntryMeta) -> bool {
        self.max_entry_age
            .is_some_and(|age| meta.created.saturating_add(age.as_secs()) < unix_now())
    }

    fn entry_path(&self, key: &CacheKey) -> PathBuf {
        let hash = blake3::hash(key.as_str().as_bytes()).to_hex();
        let name = &hash[..32];
        self.entries_dir
            .join(key.namespace().as_str())
            .join(&name[..2])
            .join(name)
    }

    /// Looks an entry up, touching its recency. Expired entries are removed and miss.
    async fn lookup(&self, key: &CacheKey) -> Option<Found> {
        let found = self.lock().touch(key)?;
        if self.is_expired(&found.meta) {
            self.forget(key, found.generation).await;
            return None;
        }
        Some(found)
    }

    /// Treats a damaged entry as a miss: counts it and deletes it.
    async fn damaged(&self, key: &CacheKey, generation: u64, what: &str) {
        tracing::warn!(%key, what, "damaged disk entry; treating it as a miss");
        metrics::counter!("mammoth_cache_integrity_errors_total", "tier" => "disk").increment(1);
        self.forget(key, generation).await;
    }

    /// Drops the entry stored under `generation` from the index and deletes its file. A newer
    /// entry stored for the key since is left alone, file and all.
    async fn forget(&self, key: &CacheKey, generation: u64) {
        let removed = self.lock().remove_generation(key, generation);
        if removed {
            self.delete_file(key).await;
        }
    }

    /// Deletes the files of entries already dropped from the index, except keys stored again since.
    async fn delete_files(&self, keys: &[CacheKey]) {
        for key in keys {
            let stored_again = self.lock().entries.contains_key(key);
            if !stored_again {
                self.delete_file(key).await;
            }
        }
    }

    /// Deletes `key`'s file.
    async fn delete_file(&self, key: &CacheKey) {
        match fs::remove_file(self.entry_path(key)).await {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(%key, %error, "could not delete disk entry"),
        }
    }

    /// Makes room on a full volume: evicts least recently used entries down to the low-water
    /// mark, then probes the volume again, which evicts more if the free space buffer is short.
    async fn make_room(&self) {
        let evicted = {
            let mut index = self.lock();
            let target = low_water(index.size_bytes, self.effective_capacity());
            index.evict_to(target)
        };
        tracing::warn!(
            entries = evicted.len(),
            "disk volume is full; evicting entries to make room"
        );
        self.delete_files(&evicted).await;
        self.refresh().await;
    }

    async fn read(self: Arc<Self>, key: &CacheKey) -> io::Result<Option<Blob>> {
        let Some(Found { generation, .. }) = self.lookup(key).await else {
            return Ok(None);
        };
        let mut file = match fs::File::open(self.entry_path(key)).await {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.lock().remove_generation(key, generation);
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        // The file is checked against its own header and trailer, not the index: a PUT may have
        // replaced it since the lookup, and the new entry is as good an answer as the old one.
        let header = match read_header(&mut file).await {
            Ok(header) if header.key == key.as_str() => header,
            Ok(_) | Err(_) => {
                self.damaged(key, generation, "header").await;
                return Ok(None);
            }
        };
        let size = header.meta.size;

        // The checksum is checked before the first byte is served: once a response has started,
        // a failure can only truncate it, and a truncated download turns Gradle's cache off.
        if size <= READ_WHOLE {
            let mut buf = BytesMut::zeroed(usize::try_from(size).map_err(io::Error::other)? + 4);
            if let Err(e) = file.read_exact(&mut buf).await {
                if e.kind() != io::ErrorKind::UnexpectedEof {
                    return Err(e);
                }
                self.damaged(key, generation, "length").await;
                return Ok(None);
            }
            let trailer = buf.split_off(buf.len() - 4);
            let stored = Checksum(u32::from_le_bytes([
                trailer[0], trailer[1], trailer[2], trailer[3],
            ]));
            if Checksum::of(&buf) != stored {
                self.damaged(key, generation, "checksum").await;
                return Ok(None);
            }
            let body = stream::once(async move { Ok(buf.freeze()) });
            return Ok(Some(Blob {
                size,
                body: Box::pin(body),
            }));
        }
        let body_start = file.stream_position().await?;
        let expected = match verify_body(&mut file, size).await {
            Ok(Some(checksum)) => checksum,
            Ok(None) => {
                self.damaged(key, generation, "checksum").await;
                return Ok(None);
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.damaged(key, generation, "length").await;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        file.seek(io::SeekFrom::Start(body_start)).await?;

        let state = ReadState {
            file,
            remaining: size,
            checksum: Checksum::EMPTY,
            store: self.clone(),
            key: key.clone(),
            generation,
        };
        let body = stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            match state.next_chunk(expected).await {
                Ok(Some(chunk)) => Some((Ok(chunk), Some(state))),
                Ok(None) => None,
                Err(e) => {
                    state.store.forget(&state.key, state.generation).await;
                    Some((Err(e), None))
                }
            }
        });
        Ok(Some(Blob {
            size,
            body: Box::pin(body),
        }))
    }

    async fn write(
        &self,
        key: &CacheKey,
        size_hint: Option<u64>,
        body: ByteStream,
    ) -> io::Result<PutOutcome> {
        if key.namespace().is_immutable() && self.lookup(key).await.is_some() {
            return Ok(PutOutcome::Exists);
        }
        if let Some(size) = size_hint {
            self.reserve(entry_len(key, size)).await;
        }
        let tmp = self.tmp_dir.join(format!(
            "{}{}{TMP_SUFFIX}",
            self.tmp_prefix,
            self.next_tmp.fetch_add(1, Ordering::Relaxed)
        ));
        let written = self.write_entry(&tmp, key, size_hint, body).await;
        let (meta, file_len) = match written {
            Ok(written) => written,
            Err(e) => {
                let _ = fs::remove_file(&tmp).await;
                return Err(e);
            }
        };
        let path = self.entry_path(key);
        if let Err(e) = rename_into_place(&tmp, &path).await {
            let _ = fs::remove_file(&tmp).await;
            return Err(e);
        }
        let evicted = {
            let mut index = self.lock();
            index.insert(key.clone(), meta, file_len);
            index.evict_to(self.effective_capacity())
        };
        self.delete_files(&evicted).await;
        Ok(PutOutcome::Stored)
    }

    /// Before a write of `len` bytes, evicts entries if free space is what limits the tier, so
    /// that the write does not eat into the free space buffer.
    async fn reserve(&self, len: u64) {
        let space_cap = self.space_cap.load(Ordering::Relaxed);
        if space_cap >= self.capacity_bytes {
            return;
        }
        let evicted = self
            .lock()
            .evict_to(space_cap.saturating_sub(disk_usage(len)));
        self.delete_files(&evicted).await;
    }

    /// Streams `body` into a new entry file at `path`, returning its metadata and file length.
    ///
    /// If the volume is full, entries are evicted to make room and the write carries on, once.
    async fn write_entry(
        &self,
        path: &Path,
        key: &CacheKey,
        size_hint: Option<u64>,
        mut body: ByteStream,
    ) -> io::Result<(EntryMeta, u64)> {
        let created = unix_now();
        let key_bytes = key.as_str().as_bytes();
        let key_len = u32::try_from(key_bytes.len()).map_err(io::Error::other)?;

        let mut header = Vec::with_capacity(FIXED_HEADER_LEN + key_bytes.len());
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        header.extend_from_slice(&CHECKSUM_CRC32C.to_le_bytes());
        header.extend_from_slice(&created.to_le_bytes());
        header.extend_from_slice(&size_hint.unwrap_or(0).to_le_bytes());
        header.extend_from_slice(&key_len.to_le_bytes());
        header.extend_from_slice(key_bytes);

        let mut out = EntryFile {
            file: fs::File::create(path).await?,
            offset: 0,
            last: Bytes::new(),
            room_made: false,
        };
        self.append(&mut out, Bytes::from(header)).await?;

        let mut size = 0_u64;
        let mut checksum = Checksum::EMPTY;
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            checksum = checksum.update(&chunk);
            size += chunk.len() as u64;
            self.append(&mut out, chunk).await?;
        }
        let file_len = entry_len(key, size);
        let trailer = Bytes::copy_from_slice(&checksum.0.to_le_bytes());
        self.append(&mut out, trailer).await?;
        let unhinted = (size_hint != Some(size)).then_some(size);
        if let Err(e) = finish(&mut out.file, unhinted).await {
            self.recover(&mut out, e).await?;
            finish(&mut out.file, unhinted).await?;
        }

        let meta = EntryMeta {
            size,
            checksum,
            created,
        };
        Ok((meta, file_len))
    }

    /// Writes `piece` after the pieces already written.
    async fn append(&self, out: &mut EntryFile, piece: Bytes) -> io::Result<()> {
        if let Err(e) = self.write_piece(&mut out.file, &piece).await {
            self.recover(out, e).await?;
            self.write_piece(&mut out.file, &piece).await?;
        }
        out.offset += out.last.len() as u64;
        out.last = piece;
        Ok(())
    }

    /// Recovers from a failed write if the volume was full and no room has been made for this
    /// file yet: evicts entries, then rewrites the last piece, which the failure may have cut
    /// short. Any other failure is returned.
    async fn recover(&self, out: &mut EntryFile, error: io::Error) -> io::Result<()> {
        if error.kind() != io::ErrorKind::StorageFull || out.room_made {
            return Err(error);
        }
        out.room_made = true;
        self.make_room().await;
        out.file.seek(io::SeekFrom::Start(out.offset)).await?;
        self.write_piece(&mut out.file, &out.last).await
    }

    /// Writes one piece of an entry file.
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    async fn write_piece(&self, file: &mut fs::File, piece: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_write.load(Ordering::Relaxed) > 0
            && self.fail_write.fetch_sub(1, Ordering::Relaxed) == 1
        {
            // Half the piece reaches the file, as when a real write runs out of space.
            file.write_all(&piece[..piece.len() / 2]).await?;
            return Err(io::ErrorKind::StorageFull.into());
        }
        file.write_all(piece).await
    }
}

/// An entry file being written.
///
/// A failed write is reported by the file's next operation, so the last piece written is kept
/// until a later write confirms it, to be written again after making room.
struct EntryFile {
    file: fs::File,
    /// Where `last` starts in the file.
    offset: u64,
    last: Bytes,
    /// Whether entries have already been evicted for this file. Room is made once.
    room_made: bool,
}

/// The length of the file for an entry of `size` bytes.
fn entry_len(key: &CacheKey, size: u64) -> u64 {
    (FIXED_HEADER_LEN + key.as_str().len()) as u64 + size.saturating_add(TRAILER_LEN)
}

/// Patches the size field, if the body's size was not known up front, and flushes the file.
async fn finish(file: &mut fs::File, size: Option<u64>) -> io::Result<()> {
    if let Some(size) = size {
        file.seek(io::SeekFrom::Start(SIZE_OFFSET)).await?;
        file.write_all(&size.to_le_bytes()).await?;
    }
    file.flush().await
}

/// Moves a written temporary file to its entry path.
async fn rename_into_place(tmp: &Path, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::rename(tmp, path).await
}

/// Reads `size` body bytes and the trailer. Returns the stored checksum if the body matches it.
async fn verify_body(file: &mut fs::File, size: u64) -> io::Result<Option<Checksum>> {
    let mut buf = vec![0_u8; READ_CHUNK];
    let mut remaining = size;
    let mut checksum = Checksum::EMPTY;
    while remaining > 0 {
        let want = usize::try_from(remaining).map_or(READ_CHUNK, |r| r.min(READ_CHUNK));
        file.read_exact(&mut buf[..want]).await?;
        checksum = checksum.update(&buf[..want]);
        remaining -= want as u64;
    }
    let stored = Checksum(file.read_u32_le().await?);
    Ok((stored == checksum).then_some(stored))
}

struct ReadState {
    file: fs::File,
    remaining: u64,
    checksum: Checksum,
    store: Arc<Inner>,
    key: CacheKey,
    generation: u64,
}

impl ReadState {
    /// The next chunk of the body, or `None` at the end once the checksum has been verified.
    async fn next_chunk(&mut self, expected: Checksum) -> io::Result<Option<Bytes>> {
        if self.remaining == 0 {
            let actual = self.file.read_u32_le().await?;
            if Checksum(actual) != expected || self.checksum != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("disk entry {} failed its checksum", self.key),
                ));
            }
            return Ok(None);
        }
        let want = usize::try_from(self.remaining).map_or(READ_CHUNK, |r| r.min(READ_CHUNK));
        let mut buf = BytesMut::with_capacity(want);
        while buf.len() < want {
            if self.file.read_buf(&mut buf).await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("disk entry {} is truncated", self.key),
                ));
            }
        }
        let chunk = buf.freeze();
        self.remaining -= chunk.len() as u64;
        self.checksum = self.checksum.update(&chunk);
        Ok(Some(chunk))
    }
}

struct Header {
    key: String,
    meta: EntryMeta,
}

async fn read_header(file: &mut fs::File) -> io::Result<Header> {
    let mut magic = [0_u8; 8];
    file.read_exact(&mut magic).await?;
    let version = file.read_u16_le().await?;
    let algorithm = file.read_u16_le().await?;
    if &magic != MAGIC || version != FORMAT_VERSION || algorithm != CHECKSUM_CRC32C {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a mammoth-cache entry",
        ));
    }
    let created = file.read_u64_le().await?;
    let size = file.read_u64_le().await?;
    let key_len = file.read_u32_le().await?;
    if key_len > 4096 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "key too long"));
    }
    let mut key = vec![0_u8; key_len as usize];
    file.read_exact(&mut key).await?;
    let key = String::from_utf8(key).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    // The checksum lives in the trailer; reads verify the body against it.
    Ok(Header {
        key,
        meta: EntryMeta {
            size,
            checksum: Checksum::EMPTY,
            created,
        },
    })
}

struct Scanned {
    key: CacheKey,
    meta: EntryMeta,
    file_len: u64,
}

/// Reads every entry header under `dir`, oldest access first. Files named like entries that are
/// not valid entries are deleted; files with other names are left alone.
fn scan(dir: &Path) -> io::Result<Vec<Scanned>> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std_fs::read_dir(&dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if !file_type.is_file() || !entry.file_name().to_str().is_some_and(is_entry_name)
            {
                tracing::warn!(path = %entry.path().display(), "leaving an unknown file in the disk tier");
            } else {
                match scan_file(&entry.path()) {
                    Ok(scanned) => found.push(scanned),
                    Err(error) => {
                        tracing::warn!(path = %entry.path().display(), %error, "deleting invalid disk entry");
                        let _ = std_fs::remove_file(entry.path());
                    }
                }
            }
        }
    }
    found.sort_by_key(|(_, accessed)| *accessed);
    Ok(found.into_iter().map(|(scanned, _)| scanned).collect())
}

fn scan_file(path: &Path) -> io::Result<(Scanned, std::time::SystemTime)> {
    use std::io::{Read, Seek};

    let mut file = std_fs::File::open(path)?;
    let metadata = file.metadata()?;
    let mut fixed = [0_u8; FIXED_HEADER_LEN];
    file.read_exact(&mut fixed)?;
    let u16_at = |at: usize| u16::from_le_bytes([fixed[at], fixed[at + 1]]);
    let u64_at = |at: usize| {
        let mut b = [0_u8; 8];
        b.copy_from_slice(&fixed[at..at + 8]);
        u64::from_le_bytes(b)
    };
    let invalid = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_owned());
    if &fixed[..8] != MAGIC || u16_at(8) != FORMAT_VERSION || u16_at(10) != CHECKSUM_CRC32C {
        return Err(invalid("not a mammoth-cache entry"));
    }
    let created = u64_at(12);
    let size = u64_at(20);
    let key_len = u32::from_le_bytes([fixed[28], fixed[29], fixed[30], fixed[31]]) as usize;
    if key_len > 4096 {
        return Err(invalid("key too long"));
    }
    let mut key = vec![0_u8; key_len];
    file.read_exact(&mut key)?;
    let key = String::from_utf8(key).map_err(|_| invalid("key is not UTF-8"))?;
    let key = CacheKey::from_stored(&key).ok_or_else(|| invalid("unknown key"))?;

    let file_len = (FIXED_HEADER_LEN + key_len) as u64 + size + TRAILER_LEN;
    if metadata.len() != file_len {
        return Err(invalid("length does not match header"));
    }
    file.seek(io::SeekFrom::Start(file_len - TRAILER_LEN))?;
    let mut trailer = [0_u8; 4];
    file.read_exact(&mut trailer)?;
    let meta = EntryMeta {
        size,
        checksum: Checksum(u32::from_le_bytes(trailer)),
        created,
    };
    let accessed = metadata
        .accessed()
        .or_else(|_| metadata.modified())
        .unwrap_or(std::time::UNIX_EPOCH);
    Ok((
        Scanned {
            key,
            meta,
            file_len,
        },
        accessed,
    ))
}

impl BlobStore for DiskStore {
    fn tier(&self) -> Tier {
        Tier::Disk
    }

    fn get<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<Blob>>> {
        Box::pin(self.inner.clone().read(key))
    }

    fn head<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<u64>>> {
        Box::pin(async move { Ok(self.inner.lookup(key).await.map(|found| found.meta.size)) })
    }

    fn put<'a>(
        &'a self,
        key: &'a CacheKey,
        size_hint: Option<u64>,
        body: ByteStream,
    ) -> BoxFuture<'a, io::Result<PutOutcome>> {
        Box::pin(self.inner.write(key, size_hint, body))
    }
}

impl BoundedStore for DiskStore {
    fn usage(&self) -> TierUsage {
        let index = self.inner.lock();
        TierUsage {
            entries: index.entries.len() as u64,
            size_bytes: index.size_bytes,
            capacity_bytes: self.inner.capacity_bytes,
        }
    }

    fn health(&self) -> Vec<HealthCheck> {
        let probe = self.inner.last_probe();
        let buffer = self.inner.free_space_buffer;
        let writable = probe.not_writable.map(|error| HealthProblem {
            reason: "STORAGE_NOT_WRITABLE",
            message: format!(
                "The disk tier at {} is not writable: {error}.",
                self.inner.entries_dir.display()
            ),
            metadata: vec![("path", self.inner.entries_dir.display().to_string())],
        });
        let free_space = probe
            .available
            .filter(|&available| available < buffer)
            .map(|available| HealthProblem {
                reason: "STORAGE_FREE_SPACE_LOW",
                message: format!(
                    "Free space is {available} bytes, below the {buffer} byte buffer."
                ),
                metadata: vec![
                    ("freeBytes", available.to_string()),
                    ("bufferBytes", buffer.to_string()),
                ],
            });
        vec![
            HealthCheck {
                name: "storage-writable",
                problem: writable,
            },
            HealthCheck {
                name: "storage-free-space",
                problem: free_space,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use futures_util::TryStreamExt;

    use super::*;
    use crate::key::Namespace;

    fn config(dir: &Path, capacity_bytes: u64) -> DiskConfig {
        DiskConfig {
            path: dir.to_path_buf(),
            capacity_bytes,
            max_entry_age: None,
            free_space_buffer: 0,
        }
    }

    fn body(chunks: &[&'static [u8]]) -> ByteStream {
        let chunks: Vec<io::Result<Bytes>> =
            chunks.iter().map(|c| Ok(Bytes::from_static(c))).collect();
        Box::pin(stream::iter(chunks))
    }

    async fn read_all(store: &DiskStore, key: &CacheKey) -> io::Result<Option<Vec<u8>>> {
        let Some(blob) = store.get(key).await? else {
            return Ok(None);
        };
        let chunks: Vec<Bytes> = blob.body.try_collect().await?;
        Ok(Some(chunks.concat()))
    }

    fn gradle(key: &str) -> CacheKey {
        CacheKey::gradle(key).unwrap()
    }

    #[tokio::test]
    async fn round_trip_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let key = gradle("abc123");
        {
            let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
            let outcome = store.put(&key, None, body(&[b"hello ", b"disk"])).await;
            assert_eq!(outcome.unwrap(), PutOutcome::Stored);
            assert_eq!(store.head(&key).await.unwrap(), Some(10));
            let again = store.put(&key, Some(3), body(&[b"new"])).await.unwrap();
            assert_eq!(again, PutOutcome::Exists);
        }
        let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
        assert_eq!(store.usage().entries, 1);
        let bytes = read_all(&store, &key).await.unwrap();
        assert_eq!(bytes.as_deref(), Some(&b"hello disk"[..]));
        assert_eq!(store.head(&gradle("ffff")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn every_namespace_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let keys = [
            gradle("abc123"),
            CacheKey::maven("v1.1/g/a/cs/buildinfo.xml").unwrap(),
            CacheKey::bazel(Namespace::BazelAc, digest).unwrap(),
            CacheKey::bazel(Namespace::BazelCas, digest).unwrap(),
        ];
        {
            let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
            for key in &keys {
                let outcome = store
                    .put(
                        key,
                        None,
                        Box::pin(stream::iter([Ok(Bytes::copy_from_slice(
                            key.as_str().as_bytes(),
                        ))])),
                    )
                    .await;
                assert_eq!(outcome.unwrap(), PutOutcome::Stored, "{key}");
            }
        }
        let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
        assert_eq!(store.usage().entries, keys.len() as u64);
        for key in &keys {
            let bytes = read_all(&store, key).await.unwrap();
            assert_eq!(bytes.as_deref(), Some(key.as_str().as_bytes()), "{key}");
        }
    }

    #[tokio::test]
    async fn maven_entries_are_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
        let key = CacheKey::maven("v1.1/g/a/cs/buildinfo.xml").unwrap();
        store.put(&key, Some(3), body(&[b"old"])).await.unwrap();
        let outcome = store.put(&key, Some(5), body(&[b"newer"])).await.unwrap();
        assert_eq!(outcome, PutOutcome::Stored);
        let bytes = read_all(&store, &key).await.unwrap();
        assert_eq!(bytes.as_deref(), Some(&b"newer"[..]));
        assert_eq!(store.usage().entries, 1);
    }

    #[tokio::test]
    async fn corruption_is_a_clean_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(config(dir.path(), 1 << 30)).await.unwrap();
        // One entry read whole, one checked in a first pass and then streamed.
        let small: &'static [u8] = b"precious bytes";
        let large: &'static [u8] = Box::leak(vec![3_u8; 3 * 1024 * 1024].into_boxed_slice());
        for (name, bytes) in [("c0ffee", small), ("b16b16", large)] {
            let key = gradle(name);
            store.put(&key, None, body(&[bytes])).await.unwrap();
            assert_eq!(
                read_all(&store, &key).await.unwrap().as_deref(),
                Some(bytes)
            );

            let path = store.inner.entry_path(&key);
            let mut raw = std_fs::read(&path).unwrap();
            let in_body = raw.len() - 4 - 3;
            raw[in_body] ^= 0xff;
            std_fs::write(&path, raw).unwrap();

            // A miss before any byte is served, not a failed stream.
            assert!(store.get(&key).await.unwrap().is_none(), "{name}");
            assert_eq!(store.head(&key).await.unwrap(), None);
            assert!(!path.exists());
        }
    }

    #[tokio::test]
    async fn a_truncated_file_is_a_clean_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
        let key = gradle("5407");
        store.put(&key, None, body(&[b"cut short"])).await.unwrap();
        let path = store.inner.entry_path(&key);
        let raw = std_fs::read(&path).unwrap();
        std_fs::write(&path, &raw[..raw.len() - 6]).unwrap();
        assert!(store.get(&key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn failed_upload_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
        let key = gradle("dead");
        let broken: ByteStream = Box::pin(stream::iter(vec![
            Ok(Bytes::from_static(b"partial")),
            Err(io::Error::other("client went away")),
        ]));
        assert!(store.put(&key, None, broken).await.is_err());
        assert_eq!(store.head(&key).await.unwrap(), None);
        assert_eq!(std_fs::read_dir(dir.path().join("tmp")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn evicts_least_recently_used_by_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (gradle("aa"), gradle("bb"), gradle("cc"));
        // Each entry is header + 9-byte key + 100 bytes + trailer = 145 bytes, which takes one
        // 4 KiB block on disk; two fit.
        let store = DiskStore::open(config(dir.path(), 3 * BLOCK_SIZE - 1))
            .await
            .unwrap();
        let hundred: &'static [u8] = &[7; 100];
        store.put(&a, None, body(&[hundred])).await.unwrap();
        store.put(&b, None, body(&[hundred])).await.unwrap();
        assert!(store.head(&a).await.unwrap().is_some()); // a is now more recent than b
        store.put(&c, None, body(&[hundred])).await.unwrap();

        assert!(store.head(&a).await.unwrap().is_some());
        assert_eq!(store.head(&b).await.unwrap(), None);
        assert!(store.head(&c).await.unwrap().is_some());
        let usage = store.usage();
        assert_eq!((usage.entries, usage.size_bytes), (2, 2 * BLOCK_SIZE));
        assert!(!store.inner.entry_path(&b).exists());
    }

    #[tokio::test]
    async fn a_healthy_directory_passes_both_checks() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
        let checks = store.health();
        let names: Vec<_> = checks.iter().map(|c| c.name).collect();
        assert_eq!(names, ["storage-writable", "storage-free-space"]);
        assert!(checks.iter().all(|c| c.problem.is_none()), "{checks:?}");
    }

    #[tokio::test]
    async fn the_free_space_buffer_fails_readiness_and_evicts() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (gradle("aa"), gradle("bb"));
        let hundred: &'static [u8] = &[7; 100];
        let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
        store.put(&a, None, body(&[hundred])).await.unwrap();
        drop(store);

        // A buffer larger than any volume: the tier may hold nothing.
        let short = DiskConfig {
            free_space_buffer: u64::MAX / 2,
            ..config(dir.path(), 1 << 20)
        };
        let store = DiskStore::open(short).await.unwrap();
        let problem = store.health()[1].problem.clone().unwrap();
        assert_eq!(problem.reason, "STORAGE_FREE_SPACE_LOW");
        assert_eq!(store.usage().entries, 0);
        store.put(&b, None, body(&[hundred])).await.unwrap();
        assert_eq!(store.head(&b).await.unwrap(), None);
    }

    #[tokio::test]
    async fn expired_entries_miss_and_are_dropped_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let key = gradle("01d");
        let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
        store.put(&key, None, body(&[b"x"])).await.unwrap();
        // Back-date the entry by rewriting its created field.
        let path = store.inner.entry_path(&key);
        let mut raw = std_fs::read(&path).unwrap();
        raw[12..20].copy_from_slice(&(unix_now() - 7200).to_le_bytes());
        std_fs::write(&path, raw).unwrap();
        drop(store);

        let aged = DiskConfig {
            max_entry_age: Some(Duration::from_secs(3600)),
            ..config(dir.path(), 1 << 20)
        };
        let store = DiskStore::open(aged).await.unwrap();
        assert_eq!(store.usage().entries, 0);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn open_deletes_only_its_own_files() {
        let dir = tempfile::tempdir().unwrap();
        let shard = dir.path().join("v1/gradle/zz");
        std_fs::create_dir_all(&shard).unwrap();
        let damaged = shard.join("0123456789abcdef0123456789abcdef");
        std_fs::write(&damaged, b"not an entry").unwrap();
        let unknown = shard.join("notes.txt");
        std_fs::write(&unknown, b"someone else's").unwrap();
        let tmp = dir.path().join("tmp");
        std_fs::create_dir_all(&tmp).unwrap();
        for name in ["7", "probe", "mmth-12-0badf00d-3.tmp"] {
            std_fs::write(tmp.join(name), b"half written").unwrap();
        }
        std_fs::write(tmp.join("backup.tar"), b"someone else's").unwrap();

        let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
        assert_eq!(store.usage().entries, 0);
        assert!(!damaged.exists());
        assert!(unknown.exists());
        let left: Vec<_> = std_fs::read_dir(&tmp)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["backup.tar"]);
    }

    #[tokio::test]
    async fn a_second_store_cannot_open_the_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
        let err = DiskStore::open(config(dir.path(), 1 << 20))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert!(err.to_string().contains("in use"), "{err}");
        assert!(
            store
                .inner
                .tmp_prefix
                .starts_with(&format!("{TMP_PREFIX}{}-", std::process::id()))
        );
        drop(store);
        DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
    }

    /// Writes a new entry for `key` and renames it over the stored one without telling the index,
    /// as a PUT does just before it records the new entry.
    async fn replace_behind_the_index(store: &DiskStore, key: &CacheKey, bytes: &'static [u8]) {
        let tmp = store.inner.tmp_dir.join("replacement");
        let (meta, file_len) = store
            .inner
            .write_entry(&tmp, key, None, body(&[bytes]))
            .await
            .unwrap();
        std_fs::rename(&tmp, store.inner.entry_path(key)).unwrap();
        // What the PUT does next.
        store.inner.lock().insert(key.clone(), meta, file_len);
    }

    #[tokio::test]
    async fn a_get_racing_a_replacing_put_keeps_the_new_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
        // A new size, then the same size with other bytes.
        for (old, new) in [(&b"old"[..], &b"newer"[..]), (b"abc", b"xyz")] {
            let key = CacheKey::maven("v1.1/g/a/cs/buildinfo.xml").unwrap();
            store.put(&key, None, body(&[old])).await.unwrap();
            let Found { generation, .. } = store.inner.lookup(&key).await.unwrap();

            let tmp = store.inner.tmp_dir.join("replacement");
            store
                .inner
                .write_entry(&tmp, &key, None, body(&[new]))
                .await
                .unwrap();
            std_fs::rename(&tmp, store.inner.entry_path(&key)).unwrap();

            // The GET took the old metadata from the index and opens the new file.
            let read = read_all(&store, &key).await.unwrap();
            assert_eq!(read.as_deref(), Some(new));
            assert!(store.inner.entry_path(&key).exists());

            // A damaged old entry is only forgotten if the index still holds it.
            replace_behind_the_index(&store, &key, new).await;
            store.inner.damaged(&key, generation, "test").await;
            assert!(store.inner.entry_path(&key).exists());
            assert_eq!(read_all(&store, &key).await.unwrap().as_deref(), Some(new));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn readers_never_lose_an_entry_being_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(config(dir.path(), 1 << 30)).await.unwrap();
        let key = CacheKey::maven("v1.1/g/a/cs/buildinfo.xml").unwrap();
        store.put(&key, None, body(&[b"first"])).await.unwrap();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let readers: Vec<_> = (0..6)
            .map(|_| {
                let (store, key, done) = (store.clone(), key.clone(), done.clone());
                tokio::spawn(async move {
                    while !done.load(Ordering::Relaxed) {
                        let _ = read_all(&store, &key).await;
                        tokio::task::yield_now().await;
                    }
                })
            })
            .collect();
        let mut lost = 0;
        for i in 0..500_u32 {
            // Sizes vary, so a stale size cannot match by luck.
            let bytes = vec![b'x'; 1 + (i % 7) as usize];
            let chunks: ByteStream = Box::pin(stream::iter([Ok(Bytes::from(bytes))]));
            let stored = store.put(&key, None, chunks).await.unwrap();
            assert_eq!(stored, PutOutcome::Stored);
            if read_all(&store, &key).await.unwrap().is_none() {
                lost += 1;
            }
        }
        done.store(true, Ordering::Relaxed);
        for reader in readers {
            reader.await.unwrap();
        }
        assert_eq!(lost, 0, "entries missing right after a successful put");
    }

    #[test]
    fn a_full_volume_shrinks_to_the_low_water_mark() {
        assert_eq!(low_water(1000, u64::MAX), 900);
        assert_eq!(low_water(1000, 500), 450);
        assert_eq!(low_water(0, 500), 0);
    }

    #[tokio::test]
    async fn a_full_volume_evicts_and_carries_on() {
        // Fail the header, each body chunk, and the trailer in turn.
        for piece in 1..=4 {
            let dir = tempfile::tempdir().unwrap();
            let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
            // Twenty one-block entries, oldest first.
            let old: Vec<_> = (10..30).map(|i| gradle(&format!("{i}"))).collect();
            let hundred: &'static [u8] = &[7; 100];
            for key in &old {
                store.put(key, Some(100), body(&[hundred])).await.unwrap();
            }
            let c = gradle("cc");

            store.inner.fail_write.store(piece, Ordering::Relaxed);
            let outcome = store.put(&c, None, body(&[b"hello ", b"full disk"])).await;
            assert_eq!(outcome.unwrap(), PutOutcome::Stored, "piece {piece}");
            // Down to 90 % of 20 blocks: the two oldest go, not just one.
            for key in &old[..2] {
                assert_eq!(store.head(key).await.unwrap(), None, "piece {piece}");
                assert!(!store.inner.entry_path(key).exists());
            }
            for key in &old[2..] {
                assert!(store.head(key).await.unwrap().is_some(), "piece {piece}");
            }
            let bytes = read_all(&store, &c).await.unwrap();
            assert_eq!(
                bytes.as_deref(),
                Some(&b"hello full disk"[..]),
                "piece {piece}"
            );
            drop(store);

            // The file is whole: it survives a reopen.
            let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
            let bytes = read_all(&store, &c).await.unwrap();
            assert_eq!(
                bytes.as_deref(),
                Some(&b"hello full disk"[..]),
                "piece {piece}"
            );
        }
    }

    #[tokio::test]
    async fn a_failed_rename_removes_the_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
        let key = gradle("abc123");
        // A file where the entry's directory should be.
        let shard = store.inner.entry_path(&key).parent().unwrap().to_path_buf();
        std_fs::create_dir_all(shard.parent().unwrap()).unwrap();
        std_fs::write(&shard, b"in the way").unwrap();

        assert!(store.put(&key, None, body(&[b"bytes"])).await.is_err());
        assert_eq!(store.head(&key).await.unwrap(), None);
        assert_eq!(std_fs::read_dir(dir.path().join("tmp")).unwrap().count(), 0);
    }
}

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
//! An in-memory index tracks every entry's size, metadata and recency. It is rebuilt from the file
//! headers at startup, so a restarted node starts warm. Eviction is least recently used by bytes,
//! plus an optional maximum age.

use std::{
    collections::{BTreeMap, HashMap},
    fs as std_fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
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
    Blob, BlobStore, BoundedStore, BoxFuture, ByteStream, PutOutcome, Tier, TierUsage,
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

/// Settings for a [`DiskStore`].
#[derive(Debug, Clone)]
pub struct DiskConfig {
    /// The directory the tier owns. Created if missing.
    pub path: PathBuf,
    /// Bytes on disk the tier may use, counting headers.
    pub capacity_bytes: u64,
    /// Entries older than this are evicted, when set.
    pub max_entry_age: Option<Duration>,
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
    index: Mutex<Index>,
    next_tmp: AtomicU64,
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
    file_len: u64,
    tick: u64,
}

impl Index {
    fn insert(&mut self, key: CacheKey, meta: EntryMeta, file_len: u64) {
        self.remove(&key);
        let tick = self.tick();
        self.lru.insert(tick, key.clone());
        self.entries.insert(
            key,
            Slot {
                meta,
                file_len,
                tick,
            },
        );
        self.size_bytes += file_len;
    }

    fn remove(&mut self, key: &CacheKey) -> Option<Slot> {
        let slot = self.entries.remove(key)?;
        self.lru.remove(&slot.tick);
        self.size_bytes -= slot.file_len;
        Some(slot)
    }

    fn touch(&mut self, key: &CacheKey) -> Option<EntryMeta> {
        let tick = self.tick();
        let slot = self.entries.get_mut(key)?;
        self.lru.remove(&slot.tick);
        slot.tick = tick;
        self.lru.insert(tick, key.clone());
        Some(slot.meta)
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
                self.size_bytes -= slot.file_len;
            }
            evicted.push(key);
        }
        evicted
    }
}

impl DiskStore {
    /// Opens the tier at `config.path`, rebuilding the index from the entry files already there.
    ///
    /// Leftover temporary files and damaged or expired entries are deleted, and the tier is
    /// trimmed to its capacity.
    pub async fn open(config: DiskConfig) -> io::Result<Self> {
        let entries_dir = config.path.join("v1");
        let tmp_dir = config.path.join("tmp");
        let (scanned, entries_dir, tmp_dir) = tokio::task::spawn_blocking(move || {
            std_fs::create_dir_all(&entries_dir)?;
            if tmp_dir.exists() {
                std_fs::remove_dir_all(&tmp_dir)?;
            }
            std_fs::create_dir_all(&tmp_dir)?;
            let scanned = scan(&entries_dir)?;
            Ok::<_, io::Error>((scanned, entries_dir, tmp_dir))
        })
        .await
        .map_err(io::Error::other)??;

        let store = Self {
            inner: Arc::new(Inner {
                entries_dir,
                tmp_dir,
                capacity_bytes: config.capacity_bytes,
                max_entry_age: config.max_entry_age,
                index: Mutex::default(),
                next_tmp: AtomicU64::new(0),
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
        Ok(store)
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(PoisonError::into_inner)
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
    async fn lookup(&self, key: &CacheKey) -> Option<EntryMeta> {
        let meta = self.lock().touch(key)?;
        if self.is_expired(&meta) {
            self.forget(key).await;
            return None;
        }
        Some(meta)
    }

    /// Treats a damaged entry as a miss: counts it and deletes it.
    async fn damaged(&self, key: &CacheKey, what: &str) {
        tracing::warn!(%key, what, "damaged disk entry; treating it as a miss");
        metrics::counter!("mammoth_cache_integrity_errors_total", "tier" => "disk").increment(1);
        self.forget(key).await;
    }

    /// Drops an entry from the index and deletes its file.
    async fn forget(&self, key: &CacheKey) {
        self.lock().remove(key);
        self.delete_files(std::slice::from_ref(key)).await;
    }

    async fn delete_files(&self, keys: &[CacheKey]) {
        for key in keys {
            match fs::remove_file(self.entry_path(key)).await {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(error) => tracing::warn!(%key, %error, "could not delete disk entry"),
            }
        }
    }

    async fn read(self: Arc<Self>, key: &CacheKey) -> io::Result<Option<Blob>> {
        let Some(meta) = self.lookup(key).await else {
            return Ok(None);
        };
        let mut file = match fs::File::open(self.entry_path(key)).await {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.lock().remove(key);
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let header = match read_header(&mut file).await {
            Ok(header) if header.key == key.as_str() && header.meta.size == meta.size => header,
            Ok(_) | Err(_) => {
                self.damaged(key, "header").await;
                return Ok(None);
            }
        };
        let size = header.meta.size;
        let expected = meta.checksum;

        // The checksum is checked before the first byte is served: once a response has started,
        // a failure can only truncate it, and a truncated download turns Gradle's cache off.
        if size <= READ_WHOLE {
            let mut buf = BytesMut::zeroed(usize::try_from(size).map_err(io::Error::other)? + 4);
            if let Err(e) = file.read_exact(&mut buf).await {
                if e.kind() != io::ErrorKind::UnexpectedEof {
                    return Err(e);
                }
                self.damaged(key, "length").await;
                return Ok(None);
            }
            let trailer = buf.split_off(buf.len() - 4);
            let stored = Checksum(u32::from_le_bytes([
                trailer[0], trailer[1], trailer[2], trailer[3],
            ]));
            if stored != expected || Checksum::of(&buf) != expected {
                self.damaged(key, "checksum").await;
                return Ok(None);
            }
            let body = stream::once(async move { Ok(buf.freeze()) });
            return Ok(Some(Blob {
                size,
                body: Box::pin(body),
            }));
        }
        let body_start = file.stream_position().await?;
        match verify_body(&mut file, size, expected).await {
            Ok(true) => {}
            Ok(false) => {
                self.damaged(key, "checksum").await;
                return Ok(None);
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.damaged(key, "length").await;
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
        file.seek(io::SeekFrom::Start(body_start)).await?;

        let state = ReadState {
            file,
            remaining: size,
            checksum: Checksum::EMPTY,
            store: self.clone(),
            key: key.clone(),
        };
        let body = stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            match state.next_chunk(expected).await {
                Ok(Some(chunk)) => Some((Ok(chunk), Some(state))),
                Ok(None) => None,
                Err(e) => {
                    state.store.forget(&state.key).await;
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
        let tmp = self
            .tmp_dir
            .join(self.next_tmp.fetch_add(1, Ordering::Relaxed).to_string());
        let written = write_entry(&tmp, key, size_hint, body).await;
        let (meta, file_len) = match written {
            Ok(written) => written,
            Err(e) => {
                let _ = fs::remove_file(&tmp).await;
                return Err(e);
            }
        };
        let path = self.entry_path(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::rename(&tmp, &path).await?;

        let evicted = {
            let mut index = self.lock();
            index.insert(key.clone(), meta, file_len);
            index.evict_to(self.capacity_bytes)
        };
        self.delete_files(&evicted).await;
        Ok(PutOutcome::Stored)
    }
}

/// Reads `size` body bytes and the trailer, and reports whether both match `expected`.
async fn verify_body(file: &mut fs::File, size: u64, expected: Checksum) -> io::Result<bool> {
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
    Ok(stored == expected && checksum == expected)
}

struct ReadState {
    file: fs::File,
    remaining: u64,
    checksum: Checksum,
    store: Arc<Inner>,
    key: CacheKey,
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
    // The checksum lives in the trailer; reads verify against the index's copy.
    Ok(Header {
        key,
        meta: EntryMeta {
            size,
            checksum: Checksum::EMPTY,
            created,
        },
    })
}

/// Streams `body` into a new entry file at `path`, returning its metadata and file length.
async fn write_entry(
    path: &Path,
    key: &CacheKey,
    size_hint: Option<u64>,
    mut body: ByteStream,
) -> io::Result<(EntryMeta, u64)> {
    let created = unix_now();
    let key_bytes = key.as_str().as_bytes();
    let key_len = u32::try_from(key_bytes.len()).map_err(io::Error::other)?;

    let mut file = fs::File::create(path).await?;
    let mut header = Vec::with_capacity(FIXED_HEADER_LEN + key_bytes.len());
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    header.extend_from_slice(&CHECKSUM_CRC32C.to_le_bytes());
    header.extend_from_slice(&created.to_le_bytes());
    header.extend_from_slice(&size_hint.unwrap_or(0).to_le_bytes());
    header.extend_from_slice(&key_len.to_le_bytes());
    header.extend_from_slice(key_bytes);
    file.write_all(&header).await?;

    let mut size = 0_u64;
    let mut checksum = Checksum::EMPTY;
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        checksum = checksum.update(&chunk);
        size += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.write_all(&checksum.0.to_le_bytes()).await?;
    if size_hint != Some(size) {
        file.seek(io::SeekFrom::Start(SIZE_OFFSET)).await?;
        file.write_all(&size.to_le_bytes()).await?;
    }
    file.flush().await?;

    let meta = EntryMeta {
        size,
        checksum,
        created,
    };
    Ok((meta, header.len() as u64 + size + TRAILER_LEN))
}

struct Scanned {
    key: CacheKey,
    meta: EntryMeta,
    file_len: u64,
}

/// Reads every entry header under `dir`, oldest access first. Files that are not valid entries
/// are deleted.
fn scan(dir: &Path) -> io::Result<Vec<Scanned>> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std_fs::read_dir(&dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
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
    let key = parse_stored_key(&key).ok_or_else(|| invalid("unknown key"))?;

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

fn parse_stored_key(stored: &str) -> Option<CacheKey> {
    let (namespace, name) = stored.split_once('/')?;
    match namespace {
        "gradle" => CacheKey::gradle(name).ok(),
        "maven" => CacheKey::maven(name).ok(),
        _ => None,
    }
}

impl BlobStore for DiskStore {
    fn tier(&self) -> Tier {
        Tier::Disk
    }

    fn get<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<Blob>>> {
        Box::pin(self.inner.clone().read(key))
    }

    fn head<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<u64>>> {
        Box::pin(async move { Ok(self.inner.lookup(key).await.map(|meta| meta.size)) })
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
}

#[cfg(test)]
mod tests {
    use futures_util::TryStreamExt;

    use super::*;

    fn config(dir: &Path, capacity_bytes: u64) -> DiskConfig {
        DiskConfig {
            path: dir.to_path_buf(),
            capacity_bytes,
            max_entry_age: None,
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
        // Each entry is header + 9-byte key + 100 bytes + trailer = 145 bytes; two fit.
        let store = DiskStore::open(config(dir.path(), 300)).await.unwrap();
        let hundred: &'static [u8] = &[7; 100];
        store.put(&a, None, body(&[hundred])).await.unwrap();
        store.put(&b, None, body(&[hundred])).await.unwrap();
        assert!(store.head(&a).await.unwrap().is_some()); // a is now more recent than b
        store.put(&c, None, body(&[hundred])).await.unwrap();

        assert!(store.head(&a).await.unwrap().is_some());
        assert_eq!(store.head(&b).await.unwrap(), None);
        assert!(store.head(&c).await.unwrap().is_some());
        let usage = store.usage();
        assert_eq!((usage.entries, usage.size_bytes), (2, 290));
        assert!(!store.inner.entry_path(&b).exists());
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
    async fn open_deletes_files_that_are_not_entries() {
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("v1/gradle/zz/junk");
        std_fs::create_dir_all(junk.parent().unwrap()).unwrap();
        std_fs::write(&junk, b"not an entry").unwrap();
        std_fs::create_dir_all(dir.path().join("tmp")).unwrap();
        std_fs::write(dir.path().join("tmp/7"), b"half written").unwrap();

        let store = DiskStore::open(config(dir.path(), 1 << 20)).await.unwrap();
        assert_eq!(store.usage().entries, 0);
        assert!(!junk.exists());
        assert_eq!(std_fs::read_dir(dir.path().join("tmp")).unwrap().count(), 0);
    }
}

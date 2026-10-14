//! The blob store port and its adapters.
//!
//! Every tier (memory, disk, bucket) is a key-value store of immutable blobs and implements
//! [`BlobStore`]. The local tiers also implement [`BoundedStore`]. The core depends only on these
//! traits, never on a file system or cloud SDK (design doc 0003).

pub mod memory;

use std::{fmt, future::Future, io, pin::Pin};

use bytes::Bytes;
use futures_core::Stream;

use crate::key::CacheKey;

/// A boxed, sendable future, so the port can be used as `dyn BlobStore`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An entry's bytes, streamed in chunks. Blobs are never buffered whole on the way through.
pub type ByteStream = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send + 'static>>;

/// A storage tier, in lookup order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tier {
    /// L1: memory.
    Memory,
    /// L2: local disk.
    Disk,
    /// L3: object-store bucket.
    Bucket,
}

impl Tier {
    /// The tier's name as used in metric labels.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Disk => "disk",
            Self::Bucket => "bucket",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An entry read from a store.
pub struct Blob {
    /// Size of the entry in bytes, known before the body is read.
    pub size: u64,
    /// The entry's bytes.
    pub body: ByteStream,
}

impl fmt::Debug for Blob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Blob")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// What a store did with a `put`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The entry was written.
    Stored,
    /// The key was already present; nothing was written.
    Exists,
}

/// The blob store port, implemented by every tier.
///
/// Entries are immutable: a `put` for a key that exists writes nothing. A `put` whose body stream
/// fails must leave nothing behind, so a partial upload never becomes a hit.
pub trait BlobStore: Send + Sync + fmt::Debug {
    /// Which tier this store is.
    fn tier(&self) -> Tier;

    /// Reads an entry. `Ok(None)` is a miss.
    fn get<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<Blob>>>;

    /// Returns an entry's size without reading it. `Ok(None)` is a miss.
    fn head<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<u64>>>;

    /// Writes an entry from a stream. `size_hint` is the declared length, when known.
    fn put<'a>(
        &'a self,
        key: &'a CacheKey,
        size_hint: Option<u64>,
        body: ByteStream,
    ) -> BoxFuture<'a, io::Result<PutOutcome>>;
}

/// Population of a bounded tier, for `/v1/serverStatus` and the `mammoth_tier_*` gauges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TierUsage {
    /// Entries held.
    pub entries: u64,
    /// Bytes held.
    pub size_bytes: u64,
    /// The configured limit, in bytes.
    pub capacity_bytes: u64,
}

/// What only the local tiers have: a capacity and a current size. The bucket does not implement
/// it, because its expiry is lifecycle rules.
pub trait BoundedStore: BlobStore {
    /// Current population and limit.
    fn usage(&self) -> TierUsage;
}

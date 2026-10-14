//! L1: the memory tier.
//!
//! A placeholder: a plain map with no eviction, enough to run the server and test the core and
//! HTTP adapters. The real tier (admission-aware, bounded by bytes, design doc 0003) replaces it.

use std::{
    collections::{HashMap, hash_map::Entry},
    io,
    sync::{PoisonError, RwLock},
};

use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};

use super::{Blob, BlobStore, BoundedStore, BoxFuture, ByteStream, PutOutcome, Tier, TierUsage};
use crate::key::CacheKey;

/// An in-memory blob store. Entries are shared `Bytes`, so a hit is a reference-count bump.
#[derive(Debug, Default)]
pub struct MemoryStore {
    entries: RwLock<HashMap<CacheKey, Bytes>>,
    capacity_bytes: u64,
}

impl MemoryStore {
    /// Creates an empty store. `capacity_bytes` is reported but not yet enforced.
    #[must_use]
    pub fn new(capacity_bytes: u64) -> Self {
        Self {
            entries: RwLock::default(),
            capacity_bytes,
        }
    }

    fn lookup(&self, key: &CacheKey) -> Option<Bytes> {
        let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        entries.get(key).cloned()
    }
}

impl BlobStore for MemoryStore {
    fn tier(&self) -> Tier {
        Tier::Memory
    }

    fn get<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<Blob>>> {
        Box::pin(async move {
            Ok(self.lookup(key).map(|bytes| Blob {
                size: bytes.len() as u64,
                body: Box::pin(stream::once(async move { Ok(bytes) })),
            }))
        })
    }

    fn head<'a>(&'a self, key: &'a CacheKey) -> BoxFuture<'a, io::Result<Option<u64>>> {
        Box::pin(async move { Ok(self.lookup(key).map(|bytes| bytes.len() as u64)) })
    }

    fn put<'a>(
        &'a self,
        key: &'a CacheKey,
        size_hint: Option<u64>,
        mut body: ByteStream,
    ) -> BoxFuture<'a, io::Result<PutOutcome>> {
        Box::pin(async move {
            let immutable = key.namespace().is_immutable();
            if immutable && self.lookup(key).is_some() {
                return Ok(PutOutcome::Exists);
            }
            let capacity = size_hint.and_then(|n| usize::try_from(n).ok()).unwrap_or(0);
            let mut buf = BytesMut::with_capacity(capacity);
            // An error mid-stream returns before the insert, so nothing partial is kept.
            while let Some(chunk) = body.next().await {
                buf.extend_from_slice(&chunk?);
            }
            let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
            Ok(match entries.entry(key.clone()) {
                Entry::Occupied(_) if immutable => PutOutcome::Exists,
                Entry::Occupied(mut slot) => {
                    slot.insert(buf.freeze());
                    PutOutcome::Stored
                }
                Entry::Vacant(slot) => {
                    slot.insert(buf.freeze());
                    PutOutcome::Stored
                }
            })
        })
    }
}

impl BoundedStore for MemoryStore {
    fn usage(&self) -> TierUsage {
        let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        TierUsage {
            entries: entries.len() as u64,
            size_bytes: entries.values().map(|b| b.len() as u64).sum(),
            capacity_bytes: self.capacity_bytes,
        }
    }
}

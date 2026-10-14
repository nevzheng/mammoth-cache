//! Per-key coordination of the writes the core makes: stores from clients and promotions of hits.
//!
//! - Only one store of an immutable key writes at a time. A second one waits for the first to
//!   finish, then finds the entry, or writes it itself if the first failed.
//! - A promotion never runs beside a store of the same key, and a store of a mutable key waits for
//!   a promotion in progress. Otherwise an old copy promoted from the bucket could land after, and
//!   over, a newer copy a client just stored.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

use tokio::sync::watch;

use crate::key::CacheKey;

#[derive(Debug, Default)]
pub(super) struct Gate {
    keys: Mutex<HashMap<CacheKey, Writes>>,
}

/// The writes in progress for one key.
#[derive(Debug)]
struct Writes {
    stores: usize,
    promotions: usize,
    /// Waiters subscribe; it is dropped, waking them, when the key's last write finishes.
    done: watch::Sender<()>,
}

impl Gate {
    /// Waits until a store of `key` may start, and holds it until the guard is dropped.
    /// `immutable` stores of a key run one at a time; mutable ones only wait for promotions.
    pub(super) async fn store(self: &Arc<Self>, key: &CacheKey, immutable: bool) -> Guard {
        loop {
            let admit = self.admit(key, |writes| {
                let busy = writes.promotions > 0 || (immutable && writes.stores > 0);
                if !busy {
                    writes.stores += 1;
                }
                !busy
            });
            match admit {
                Ok(()) => return self.guard(key, Kind::Store),
                // Errs once the writes in progress finish and drop the sender.
                Err(mut done) => drop(done.changed().await),
            }
        }
    }

    /// Starts a promotion of `key`, or `None` if a store of it is in progress: the store's copy is
    /// at least as new as the hit's.
    pub(super) fn promotion(self: &Arc<Self>, key: &CacheKey) -> Option<Guard> {
        let admit = self.admit(key, |writes| {
            let free = writes.stores == 0;
            if free {
                writes.promotions += 1;
            }
            free
        });
        admit.ok().map(|()| self.guard(key, Kind::Promotion))
    }

    fn guard(self: &Arc<Self>, key: &CacheKey, kind: Kind) -> Guard {
        Guard {
            gate: self.clone(),
            key: key.clone(),
            kind,
        }
    }

    /// Runs `claim` on the key's writes under the lock. It returns whether it took a slot; if not,
    /// the caller gets a receiver that wakes when the writes in progress finish.
    fn admit(
        &self,
        key: &CacheKey,
        claim: impl FnOnce(&mut Writes) -> bool,
    ) -> Result<(), watch::Receiver<()>> {
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        let writes = keys.entry(key.clone()).or_insert_with(|| Writes {
            stores: 0,
            promotions: 0,
            done: watch::Sender::new(()),
        });
        if claim(writes) {
            Ok(())
        } else {
            Err(writes.done.subscribe())
        }
    }

    fn release(&self, key: &CacheKey, kind: Kind) {
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(writes) = keys.get_mut(key) else {
            return;
        };
        match kind {
            Kind::Store => writes.stores -= 1,
            Kind::Promotion => writes.promotions -= 1,
        }
        if writes.stores == 0 && writes.promotions == 0 {
            // Dropping the sender wakes every waiter.
            keys.remove(key);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Store,
    Promotion,
}

/// Holds a write's slot until dropped.
pub(super) struct Guard {
    gate: Arc<Gate>,
    key: CacheKey,
    kind: Kind,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.gate.release(&self.key, self.kind);
    }
}

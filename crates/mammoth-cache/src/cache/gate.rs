//! Per-key coordination of the writes the core makes: stores from clients and promotions of hits.
//!
//! - Only one store of an immutable key writes at a time. A second one waits for the first to
//!   finish, then finds the entry, or writes it itself if the first failed.
//! - A promotion never runs beside a store of the same key. A store that finds promotions of its
//!   key in progress stops them and waits for them to wind down: they copy at the pace of the
//!   client reading the hit, which may be slow or stalled, and the store's copy is at least as
//!   new. Otherwise an old copy promoted from the bucket could land after, and over, a newer copy a
//!   client just stored.
//! - No promotion starts while a store waits, so repeated reads cannot starve a store.

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

/// The writes in progress for one key, and the stores waiting to start.
#[derive(Debug)]
struct Writes {
    stores: usize,
    promotions: usize,
    waiting: usize,
    /// Bumped whenever a write finishes; waiting stores subscribe and check again.
    done: watch::Sender<()>,
    /// Set to stop the promotions in progress.
    abort: watch::Sender<bool>,
}

impl Writes {
    fn new() -> Self {
        Self {
            stores: 0,
            promotions: 0,
            waiting: 0,
            done: watch::Sender::new(()),
            abort: watch::Sender::new(false),
        }
    }

    fn idle(&self) -> bool {
        self.stores == 0 && self.promotions == 0 && self.waiting == 0
    }
}

impl Gate {
    /// Waits until a store of `key` may start, and holds it until the guard is dropped.
    /// `immutable` stores of a key run one at a time; any store stops the key's promotions and
    /// waits for them to end.
    pub(super) async fn store(self: &Arc<Self>, key: &CacheKey, immutable: bool) -> Guard {
        let mut waiting: Option<Waiting> = None;
        loop {
            let admit = self.admit(key, |writes| {
                let busy = writes.promotions > 0 || (immutable && writes.stores > 0);
                if busy {
                    if waiting.is_none() {
                        writes.waiting += 1;
                    }
                    if writes.promotions > 0 {
                        writes.abort.send_replace(true);
                    }
                } else {
                    if waiting.take().is_some_and(Waiting::disarm) {
                        writes.waiting -= 1;
                    }
                    writes.stores += 1;
                }
                !busy
            });
            match admit {
                Ok(()) => return self.guard(key, Kind::Store),
                Err(mut done) => {
                    waiting.get_or_insert_with(|| Waiting {
                        gate: self.clone(),
                        key: key.clone(),
                        armed: true,
                    });
                    drop(done.changed().await);
                }
            }
        }
    }

    /// Whether a promotion of `key` is in progress, which shows some tier holds it.
    pub(super) fn promoting(&self, key: &CacheKey) -> bool {
        let keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        keys.get(key).is_some_and(|writes| writes.promotions > 0)
    }

    /// Starts a promotion of `key`, or `None` if a store of it is in progress or waiting: the
    /// store's copy is at least as new as the hit's.
    pub(super) fn promotion(self: &Arc<Self>, key: &CacheKey) -> Option<Promotion> {
        let mut abort = None;
        let admit = self.admit(key, |writes| {
            let free = writes.stores == 0 && writes.waiting == 0;
            if free {
                writes.promotions += 1;
                abort = Some(writes.abort.subscribe());
            }
            free
        });
        admit.ok()?;
        Some(Promotion {
            guard: self.guard(key, Kind::Promotion),
            abort: abort?,
        })
    }

    fn guard(self: &Arc<Self>, key: &CacheKey, kind: Kind) -> Guard {
        Guard {
            gate: self.clone(),
            key: key.clone(),
            kind,
        }
    }

    /// Runs `claim` on the key's writes under the lock. It returns whether it took a slot; if not,
    /// the caller gets a receiver that wakes when a write in progress finishes.
    fn admit(
        &self,
        key: &CacheKey,
        claim: impl FnOnce(&mut Writes) -> bool,
    ) -> Result<(), watch::Receiver<()>> {
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        let writes = keys.entry(key.clone()).or_insert_with(Writes::new);
        let done = writes.done.subscribe();
        if claim(writes) {
            if writes.idle() {
                keys.remove(key);
            }
            Ok(())
        } else {
            Err(done)
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
        writes.done.send_replace(());
        if writes.idle() {
            keys.remove(key);
        }
    }

    /// A waiting store gave up, as when its client went away.
    fn stop_waiting(&self, key: &CacheKey) {
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(writes) = keys.get_mut(key) else {
            return;
        };
        writes.waiting -= 1;
        if writes.idle() {
            keys.remove(key);
        }
    }
}

/// A store counted as waiting; uncounts it if dropped while armed.
struct Waiting {
    gate: Arc<Gate>,
    key: CacheKey,
    armed: bool,
}

impl Waiting {
    /// Hands the count back to the caller, which uncounts it under the lock.
    fn disarm(mut self) -> bool {
        std::mem::replace(&mut self.armed, false)
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if self.armed {
            self.gate.stop_waiting(&self.key);
        }
    }
}

/// A promotion's slot, and the signal to stop it.
pub(super) struct Promotion {
    pub(super) guard: Guard,
    /// Turns `true` when a store of the key wants the promotion stopped.
    pub(super) abort: watch::Receiver<bool>,
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn key() -> CacheKey {
        CacheKey::maven("v1/g/a/maven-metadata.xml").unwrap()
    }

    /// A store waiting on a promotion asks it to stop, and no new promotion starts until the
    /// store has run, however many reads arrive meanwhile.
    #[tokio::test]
    async fn a_waiting_store_stops_promotions_and_is_not_starved() {
        let gate = Arc::new(Gate::default());
        let promotion = gate.promotion(&key()).unwrap();
        assert!(!*promotion.abort.borrow());

        let store = tokio::spawn({
            let gate = gate.clone();
            async move { drop(gate.store(&key(), false).await) }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            *promotion.abort.borrow(),
            "the store did not stop the promotion"
        );
        for _ in 0..10 {
            assert!(
                gate.promotion(&key()).is_none(),
                "a promotion jumped the store"
            );
        }

        drop(promotion);
        tokio::time::timeout(Duration::from_secs(1), store)
            .await
            .unwrap()
            .unwrap();
        // All done: promotions start again, and the key's state is gone.
        assert!(gate.promotion(&key()).is_some());
        assert!(gate.keys.lock().unwrap().is_empty());
    }

    /// A store that gives up while waiting leaves nothing behind that blocks promotions.
    #[tokio::test]
    async fn a_store_that_stops_waiting_is_uncounted() {
        let gate = Arc::new(Gate::default());
        let promotion = gate.promotion(&key()).unwrap();
        let waiting =
            tokio::time::timeout(Duration::from_millis(20), gate.store(&key(), true)).await;
        assert!(waiting.is_err());
        drop(promotion);
        assert!(gate.keys.lock().unwrap().is_empty());
        assert!(gate.promotion(&key()).is_some());
    }
}

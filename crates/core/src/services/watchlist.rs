//! Watchlist management (D8): add/remove/list per target.

use std::sync::{
    atomic::{
        AtomicU64,
        Ordering,
    },
    Arc,
};

use crate::{
    domain::{
        ItemRef,
        NewWatchlistEntry,
        TargetId,
        WatchlistEntry,
    },
    error::StoreError,
    ports::WatchlistRepository,
};

/// Watchlist management (D8): add/remove/list per target.
pub struct WatchlistService {
    repo: Arc<dyn WatchlistRepository>,
    clock: Arc<dyn crate::ports::ClockPort>,
    /// Bumped by every successful add/remove (any target): the change signal
    /// derived data keyed to the watchlist — the cached slim feed — is
    /// validated against, so no mutation path can leave it stale.
    revision: AtomicU64,
}

impl WatchlistService {
    /// Wires the service to the watchlist repository and the clock used to
    /// stamp insertions (the repository stamps `added_at` itself).
    pub fn new(
        repo: Arc<dyn WatchlistRepository>,
        clock: Arc<dyn crate::ports::ClockPort>,
    ) -> Self {
        Self { repo, clock, revision: AtomicU64::new(0) }
    }

    /// How many watchlist changes (adds and removals, across every target)
    /// have gone through this service. Derived data that depends on the
    /// watchlist stores the revision it was built from and rebuilds when the
    /// two disagree.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// Marks every watchlist-derived copy as outdated.
    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::Release);
    }

    /// Adds an item to a target's watchlist.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Conflict` when the target already watches the
    /// item, and `StoreError::Backend` when the entry cannot be written.
    pub fn add(
        &self,
        target: &TargetId,
        item: ItemRef,
        note: Option<String>,
    ) -> Result<WatchlistEntry, StoreError> {
        self.repo
            .add(&NewWatchlistEntry { target: target.clone(), item, note })
            .inspect(|_| self.bump())
    }

    /// Removes an item from a target's watchlist; returns whether an entry
    /// was removed.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when entries cannot be deleted.
    pub fn remove(&self, target: &TargetId, item: &ItemRef) -> Result<bool, StoreError> {
        self.repo.remove(target, item).inspect(|removed| {
            if *removed {
                self.bump();
            }
        })
    }

    /// Lists a target's watchlist in insertion order.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when entries cannot be read.
    pub fn list(&self, target: &TargetId) -> Result<Vec<WatchlistEntry>, StoreError> {
        self.repo.list(target)
    }

    /// Current wall-clock second — exposed for callers stamping UI output.
    #[must_use]
    pub fn now(&self) -> i64 {
        self.clock.now()
    }
}

//! Watchlist management (D8): add/remove/list per target.

use std::sync::Arc;

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
}

impl WatchlistService {
    /// Wires the service to the watchlist repository and the clock used to
    /// stamp insertions (the repository stamps `added_at` itself).
    pub fn new(
        repo: Arc<dyn WatchlistRepository>,
        clock: Arc<dyn crate::ports::ClockPort>,
    ) -> Self {
        Self { repo, clock }
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
        self.repo.add(&NewWatchlistEntry { target: target.clone(), item, note })
    }

    /// Removes an item from a target's watchlist; returns whether an entry
    /// was removed.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when entries cannot be deleted.
    pub fn remove(&self, target: &TargetId, item: &ItemRef) -> Result<bool, StoreError> {
        self.repo.remove(target, item)
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

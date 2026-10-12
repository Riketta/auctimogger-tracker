//! Watchlist management (D8): add/remove/list per target.

use std::sync::Arc;

use crate::domain::{ItemRef, NewWatchlistEntry, TargetId, WatchlistEntry};
use crate::error::StoreError;
use crate::ports::WatchlistRepository;

pub struct WatchlistService {
    repo: Arc<dyn WatchlistRepository>,
    clock: Arc<dyn crate::ports::ClockPort>,
}

impl WatchlistService {
    pub fn new(
        repo: Arc<dyn WatchlistRepository>,
        clock: Arc<dyn crate::ports::ClockPort>,
    ) -> Self {
        Self { repo, clock }
    }

    pub fn add(
        &self,
        target: &TargetId,
        item: ItemRef,
        note: Option<String>,
    ) -> Result<WatchlistEntry, StoreError> {
        self.repo.add(&NewWatchlistEntry {
            target: target.clone(),
            item,
            note,
        })
    }

    pub fn remove(&self, target: &TargetId, item: &ItemRef) -> Result<bool, StoreError> {
        self.repo.remove(target, item)
    }

    pub fn list(&self, target: &TargetId) -> Result<Vec<WatchlistEntry>, StoreError> {
        self.repo.list(target)
    }

    /// Current wall-clock second — exposed for callers stamping UI output.
    pub fn now(&self) -> i64 {
        self.clock.now()
    }
}

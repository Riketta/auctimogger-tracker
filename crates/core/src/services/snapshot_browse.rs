//! Snapshot viewer service: resolves which stored snapshot a browse request
//! addresses and serves one filterable, sorted, paginated page of its
//! listings. The listing reads are pure port calls; the value this adds is
//! resolution (latest-vs-pinned) and page clamping, so the web layer cannot
//! mis-address a snapshot or ask the store for an out-of-range page.

use std::sync::Arc;

use crate::{
    domain::{
        BrowseError,
        ClassSummary,
        Flavor,
        ListingPage,
        ListingQuery,
        PayloadKind,
        SnapshotMeta,
        TargetId,
    },
    error::StoreError,
    ports::SnapshotRepository,
};

/// Browses the listings of stored snapshots (the web UI's snapshot viewer
/// and the `/api/browse` endpoint).
pub struct SnapshotBrowseService {
    snapshots: Arc<dyn SnapshotRepository>,
}

impl SnapshotBrowseService {
    /// Builds the service over the snapshot repository.
    pub fn new(snapshots: Arc<dyn SnapshotRepository>) -> Self {
        Self { snapshots }
    }

    /// Resolves the snapshot the viewer addresses: an explicit id must exist
    /// and belong to the addressed (target, kind) pair — a mismatched link is
    /// a client error, not silently-redirected data — while an absent id
    /// serves the newest snapshot of the pair (the "live AH" default view).
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::NoSnapshots`] when no snapshot was requested
    /// and none exists, [`BrowseError::UnknownSnapshot`] for unknown ids,
    /// [`BrowseError::SnapshotMismatch`] when the id belongs to another
    /// pair, and [`BrowseError::Store`] on backend failures.
    pub fn resolve(
        &self,
        target: &TargetId,
        kind: PayloadKind,
        snapshot_id: Option<i64>,
    ) -> Result<SnapshotMeta, BrowseError> {
        match snapshot_id {
            Some(id) => {
                let Some(meta) = self.snapshots.snapshot_by_id(id)? else {
                    return Err(BrowseError::UnknownSnapshot);
                };
                if meta.target != *target || meta.kind != kind {
                    return Err(BrowseError::SnapshotMismatch);
                }
                Ok(meta)
            }
            None => self.snapshots.latest(target, kind)?.ok_or(BrowseError::NoSnapshots),
        }
    }

    /// One `LISTING_PAGE_SIZE`-sized page of a snapshot's listings under one
    /// catalog flavor. The page index is clamped to the non-empty range
    /// (below 1 → page 1; beyond the last page → the last page, so stale
    /// "next" links land on real data instead of an empty table).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the query cannot run.
    pub fn page(
        &self,
        flavor: &Flavor,
        snapshot_id: i64,
        mut query: ListingQuery,
    ) -> Result<ListingPage, StoreError> {
        let mut page = self.snapshots.listings_page(flavor, snapshot_id, &query)?;
        if page.pages == 0 {
            page.page = 1;
            return Ok(page);
        }
        let clamped = query.page.max(1).min(page.pages);
        // The SQLite adapter already clamps `page` before returning, so the
        // re-query below never fires with it; it exists so port
        // implementations that report an out-of-range page verbatim still
        // yield a clamped view with real rows.
        if page.page != clamped {
            query.page = clamped;
            page = self.snapshots.listings_page(flavor, snapshot_id, &query)?;
        }
        Ok(page)
    }

    /// Per-class listing counts of a snapshot (the class filter dropdown).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the summary cannot be read.
    pub fn classes(
        &self,
        flavor: &Flavor,
        snapshot_id: i64,
    ) -> Result<Vec<ClassSummary>, StoreError> {
        self.snapshots.snapshot_class_summary(flavor, snapshot_id)
    }
}

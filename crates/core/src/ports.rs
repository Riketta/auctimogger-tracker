//! Port traits — the hexagonal seams. `infra` implements the driven ports;
//! `server`'s driving adapters call the application services, which depend
//! only on these traits.

use crate::config::TargetConfig;
use crate::domain::{
    FeedFormat, FeedSnapshot, FetchOutcome, ItemRef, MetricBlock, NameRecord, NewSnapshot,
    NewWatchlistEntry, NormalizedListing, PayloadKind, PublishedFeed, SearchHit, SnapshotMeta,
    Span, TargetId, WatchlistEntry,
};
use crate::error::{RenderError, SourceError, StoreError};
use crate::fold::DayFold;

/// Time injection.
pub trait ClockPort: Send + Sync {
    fn now(&self) -> i64;
}

/// Outbound: fetch one payload for a target. The adapter owns the wire format
/// ACL (Blizzard shape today, any conforming endpoint via config) and returns
/// normalized listings plus raw bytes for archival.
pub trait AuctionSourcePort: Send + Sync {
    fn fetch(
        &self,
        target: &TargetConfig,
        kind: PayloadKind,
        if_modified_since: Option<i64>,
    ) -> Result<FetchOutcome, SourceError>;
}

/// Outbound: item name/class/quality lookups (cache-aside handled by callers).
pub trait ItemInfoPort: Send + Sync {
    fn describe(&self, item_id: i64) -> Result<Option<NameRecord>, SourceError>;
}

/// Outbound: snapshot metadata + raw payload archival.
pub trait SnapshotRepository: Send + Sync {
    /// Persists metadata and the raw bytes; returns the snapshot id.
    fn record(&self, snapshot: &NewSnapshot, raw: &[u8]) -> Result<i64, StoreError>;
    /// Newest snapshot for a target/kind, if any.
    fn latest(
        &self,
        target: &TargetId,
        kind: PayloadKind,
    ) -> Result<Option<SnapshotMeta>, StoreError>;
}

/// Outbound: per-day unique-auction fold state (the long-term memory).
pub trait PriceHistoryRepository: Send + Sync {
    fn load_fold(
        &self,
        target: &TargetId,
        item: &ItemRef,
        day: i64,
    ) -> Result<Option<DayFold>, StoreError>;
    fn save_fold(
        &self,
        target: &TargetId,
        item: &ItemRef,
        day: i64,
        fold: &DayFold,
    ) -> Result<(), StoreError>;
    /// Drops the raw listing working set older than the given UTC day.
    /// Returns the number of raw rows removed. Snapshot metadata, folds and
    /// sale credits are permanent.
    fn prune_raw_before(&self, target: &TargetId, day: i64) -> Result<u64, StoreError>;
}

/// Outbound: auction sighting bookkeeping for sell-through (R3).
pub trait SalesRepository: Send + Sync {
    /// Registers sightings for all listings of one snapshot.
    fn observe(
        &self,
        target: &TargetId,
        listings: &[NormalizedListing],
        snapshot_id: i64,
    ) -> Result<(), StoreError>;
    /// Closes sightings absent from the newest snapshot; returns credits
    /// granted (vanishing earlier than the band's lower bound).
    fn close_missing(
        &self,
        target: &TargetId,
        snapshot_id: i64,
        at: i64,
    ) -> Result<Vec<crate::domain::SaleCredit>, StoreError>;
    /// Fraction of listings credited as sold within the trailing day window.
    fn sale_rate(
        &self,
        target: &TargetId,
        item: &ItemRef,
        days: i64,
    ) -> Result<Option<f64>, StoreError>;
}

/// Outbound: memoized span metrics (invalidated per target on ingest).
pub trait MetricsCacheRepository: Send + Sync {
    fn get(
        &self,
        target: &TargetId,
        item: &ItemRef,
        span: Span,
    ) -> Result<Option<MetricBlock>, StoreError>;
    fn put(
        &self,
        target: &TargetId,
        item: &ItemRef,
        span: Span,
        block: &MetricBlock,
    ) -> Result<(), StoreError>;
    fn invalidate(&self, target: &TargetId) -> Result<(), StoreError>;
}

/// Outbound: item name/class/quality catalog backing search + completions.
pub trait ItemCatalogRepository: Send + Sync {
    fn upsert(&self, records: &[NameRecord]) -> Result<(), StoreError>;
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, StoreError>;
    fn by_id(&self, item_id: i64) -> Result<Option<NameRecord>, StoreError>;
}

/// Outbound: watchlist persistence (`Conflict` on duplicates).
pub trait WatchlistRepository: Send + Sync {
    fn add(&self, entry: &NewWatchlistEntry) -> Result<WatchlistEntry, StoreError>;
    fn remove(&self, target: &TargetId, item: &ItemRef) -> Result<bool, StoreError>;
    fn list(&self, target: &TargetId) -> Result<Vec<WatchlistEntry>, StoreError>;
}

/// Outbound: feed rendering (lua / json).
pub trait FeedRenderer: Send + Sync {
    fn render(&self, feed: &FeedSnapshot, format: FeedFormat) -> Result<Vec<u8>, RenderError>;
}

/// Outbound: feed publication (disk + in-memory for HTTP).
pub trait FeedPublisher: Send + Sync {
    fn publish(
        &self,
        target: &TargetId,
        format: FeedFormat,
        bytes: &[u8],
    ) -> Result<(), StoreError>;
    fn last_published(
        &self,
        target: &TargetId,
        format: FeedFormat,
    ) -> Result<Option<PublishedFeed>, StoreError>;
}

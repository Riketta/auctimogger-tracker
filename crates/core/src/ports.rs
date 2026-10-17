//! Port traits — the hexagonal seams. `infra` implements the driven ports;
//! `server`'s driving adapters call the application services, which depend
//! only on these traits.

use crate::{
    config::TargetConfig,
    domain::{
        BrowseCursor,
        CatalogCounts,
        FeedFormat,
        FeedSnapshot,
        FetchOutcome,
        ItemRef,
        MetricBlock,
        NameRecord,
        NewSnapshot,
        NewWatchlistEntry,
        NormalizedListing,
        PayloadKind,
        PetNameRecord,
        PublishedFeed,
        SearchHit,
        SnapshotMeta,
        Span,
        TargetId,
        WatchlistEntry,
    },
    error::{
        RenderError,
        SourceError,
        StoreError,
    },
    fold::DayFold,
};

/// Time injection.
pub trait ClockPort: Send + Sync {
    /// Current time in unix seconds. The core never reads the wall clock
    /// directly; everything flows through this port so tests can freeze it.
    fn now(&self) -> i64;
}

/// Outbound: fetch one payload for a target. The adapter owns the wire format
/// ACL (Blizzard shape today, any conforming endpoint via config) and returns
/// normalized listings plus raw bytes for archival.
pub trait AuctionSourcePort: Send + Sync {
    /// Fetches `kind` for `target`, using `if_modified_since` (unix seconds)
    /// for a conditional GET when present.
    ///
    /// # Errors
    ///
    /// Returns `SourceError::Http` on transport failures, `SourceError::Parse`
    /// when the payload cannot be normalized, and `SourceError::Config` when
    /// the target configuration is unusable for this request.
    fn fetch(
        &self,
        target: &TargetConfig,
        kind: PayloadKind,
        if_modified_since: Option<i64>,
    ) -> Result<FetchOutcome, SourceError>;
}

/// Outbound: item/pet name lookups (cache-aside handled by callers).
pub trait ItemInfoPort: Send + Sync {
    /// Looks up one item in the source item API; `None` when the item does
    /// not exist there.
    ///
    /// # Errors
    ///
    /// Returns `SourceError::Http` on transport failures and
    /// `SourceError::Parse` when the response cannot be decoded.
    fn describe(&self, item_id: i64) -> Result<Option<NameRecord>, SourceError>;
    /// Looks up one battle-pet species in the source pet API; `None` when
    /// the species does not exist there.
    ///
    /// # Errors
    ///
    /// Returns `SourceError::Http` on transport failures and
    /// `SourceError::Parse` when the response cannot be decoded.
    fn describe_pet(&self, species_id: i64) -> Result<Option<PetNameRecord>, SourceError>;
}

/// Outbound: snapshot metadata, raw payload archival, and the raw listing
/// working set — persisted atomically in one call.
pub trait SnapshotRepository: Send + Sync {
    /// Persists metadata, the listing batch, and the raw bytes; returns the
    /// snapshot id.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when metadata, listings or the raw
    /// archive cannot be written; the store must stay untouched then.
    fn record(
        &self,
        snapshot: &NewSnapshot,
        listings: &[NormalizedListing],
        raw: &[u8],
    ) -> Result<i64, StoreError>;
    /// Newest snapshot for a target/kind, if any.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the metadata cannot be read.
    fn latest(
        &self,
        target: &TargetId,
        kind: PayloadKind,
    ) -> Result<Option<SnapshotMeta>, StoreError>;
    /// The `limit` newest snapshots for a target/kind, newest first (the
    /// snapshot browser's history view).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the metadata cannot be read.
    fn list_recent(
        &self,
        target: &TargetId,
        kind: PayloadKind,
        limit: usize,
    ) -> Result<Vec<SnapshotMeta>, StoreError>;
}

/// Outbound: per-day unique-auction fold state (the long-term memory).
pub trait PriceHistoryRepository: Send + Sync {
    /// Loads the fold for one (target, item, day); `None` when no fold has
    /// been saved for that day yet.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the fold cannot be read or
    /// deserialized.
    fn load_fold(
        &self,
        target: &TargetId,
        item: &ItemRef,
        day: i64,
    ) -> Result<Option<DayFold>, StoreError>;
    /// Persists the fold for one (target, item, day), replacing any previous
    /// state for that day.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the fold cannot be written.
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
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the raw rows cannot be deleted.
    fn prune_raw_before(&self, target: &TargetId, day: i64) -> Result<u64, StoreError>;
    /// Distinct items that have fold data on/after the given UTC day.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the fold index cannot be read.
    fn items_with_data(
        &self,
        target: &TargetId,
        since_day: i64,
    ) -> Result<Vec<ItemRef>, StoreError>;
    /// Per-item-day aggregate rows on/after the given UTC day (ranking fuel
    /// for the opportunities/overview views).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the aggregates cannot be computed.
    fn item_day_rows(
        &self,
        target: &TargetId,
        since_day: i64,
    ) -> Result<Vec<crate::domain::ItemDayRow>, StoreError>;
    /// Per-item-day aggregate rows on/after `since_day` for exactly one item
    /// identity (the item detail page): SQL-scoped instead of loading the
    /// whole market's rows. The identity must match the canonical `item_ref`
    /// tuple (`kind` + `item_id` + pet fields, see `split_item_ref`).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the aggregates cannot be computed.
    fn item_day_rows_for_item(
        &self,
        target: &TargetId,
        item: &ItemRef,
        since_day: i64,
    ) -> Result<Vec<crate::domain::ItemDayRow>, StoreError>;
}

/// Outbound: auction sighting bookkeeping for sell-through (R3).
pub trait SalesRepository: Send + Sync {
    /// Registers/refreshes sightings for all listings of one snapshot.
    /// Sightings are scoped per payload kind: commodity and item auction ids
    /// live in separate id namespaces, so the same numeric id may legitimately
    /// appear in both. `at` is the cycle timestamp (unix seconds).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when sightings cannot be written.
    fn observe(
        &self,
        target: &TargetId,
        kind: PayloadKind,
        listings: &[NormalizedListing],
        snapshot_id: i64,
        at: i64,
    ) -> Result<(), StoreError>;
    /// Closes sightings of one payload kind that are absent from the newest
    /// snapshot of that kind; returns credits granted (vanished earlier than
    /// the band's lower bound).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when sightings cannot be updated or
    /// credits cannot be recorded.
    fn close_missing(
        &self,
        target: &TargetId,
        kind: PayloadKind,
        snapshot_id: i64,
        at: i64,
    ) -> Result<Vec<crate::domain::SaleCredit>, StoreError>;
    /// Fraction of listings credited as sold within the trailing day window.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the credit statistics cannot be
    /// read.
    fn sale_rate(
        &self,
        target: &TargetId,
        item: &ItemRef,
        days: i64,
    ) -> Result<Option<f64>, StoreError>;
    /// Sale rate summed over several item identities (pet variants of one
    /// species share their market). `Ok(None)` when none of the items has
    /// sales rows.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the credit statistics cannot be
    /// read.
    fn sale_rate_multi(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        days: i64,
    ) -> Result<Option<f64>, StoreError> {
        let _ = (target, items, days);
        Ok(None)
    }
    /// Sale rate for EACH of the given identities, positionally aligned
    /// (`rates[i]` belongs to `items[i]`); `None` entries have no sales rows.
    /// Batched replacement for per-item `sale_rate` loops over candidate
    /// lists.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the credit statistics cannot be
    /// read.
    fn sale_rate_each(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        days: i64,
    ) -> Result<Vec<Option<f64>>, StoreError> {
        // Correct-by-default fallback: one query per identity.
        let mut rates = Vec::with_capacity(items.len());
        for item in items {
            rates.push(self.sale_rate(target, item, days)?);
        }
        Ok(rates)
    }
}

/// Outbound: memoized span metrics (invalidated per target on ingest).
pub trait MetricsCacheRepository: Send + Sync {
    /// The cached block for one (target, item, span), if one is stored.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cache cannot be read.
    fn get(
        &self,
        target: &TargetId,
        item: &ItemRef,
        span: Span,
    ) -> Result<Option<crate::domain::CachedBlock>, StoreError>;
    /// Stores a freshly computed block with its computation timestamp.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the block cannot be written.
    fn put(
        &self,
        target: &TargetId,
        item: &ItemRef,
        span: Span,
        block: &MetricBlock,
        computed_at: i64,
    ) -> Result<(), StoreError>;
    /// Drops all cached blocks for a target (called when fold state changes).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cache cannot be cleared.
    fn invalidate(&self, target: &TargetId) -> Result<(), StoreError>;
}

/// Outbound: item/pet name catalog backing search + completions + browsing.
pub trait ItemCatalogRepository: Send + Sync {
    /// Inserts or updates the given catalog records.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when records cannot be written.
    fn upsert(&self, records: &[NameRecord]) -> Result<(), StoreError>;
    /// Inserts or updates the given pet-species records.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when records cannot be written.
    fn upsert_pets(&self, records: &[PetNameRecord]) -> Result<(), StoreError>;
    /// Kind-aware substring search over known names, at most `limit` hits
    /// (prefix matches first, then alphabetical).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, StoreError>;
    /// The catalog record for one item id, if known.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    fn by_id(&self, item_id: i64) -> Result<Option<NameRecord>, StoreError>;
    /// The catalog record for one pet species id, if known.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    fn pet_by_species(&self, species_id: i64) -> Result<Option<PetNameRecord>, StoreError>;
    /// Alphabetical catalog browsing, keyset-paginated: rows strictly after
    /// the `after` cursor (name, kind, id — `None` = from the top), at most
    /// `limit` rows. `has_more` tells the UI whether a next page exists.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    fn browse(
        &self,
        after: Option<&BrowseCursor>,
        limit: usize,
    ) -> Result<(Vec<SearchHit>, bool), StoreError>;
    /// How many named records the catalog holds, per kind (dashboard KPIs).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    fn counts(&self) -> Result<CatalogCounts, StoreError>;
}

/// Outbound: watchlist persistence (`Conflict` on duplicates).
pub trait WatchlistRepository: Send + Sync {
    /// Stores a new entry and returns the stamped version.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Conflict` when the target already watches the
    /// item, and `StoreError::Backend` when the entry cannot be written.
    fn add(&self, entry: &NewWatchlistEntry) -> Result<WatchlistEntry, StoreError>;
    /// Removes every entry matching (target, item); returns whether anything
    /// was removed.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when entries cannot be deleted.
    fn remove(&self, target: &TargetId, item: &ItemRef) -> Result<bool, StoreError>;
    /// All entries of a target, in insertion order.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when entries cannot be read.
    fn list(&self, target: &TargetId) -> Result<Vec<WatchlistEntry>, StoreError>;
}

/// Outbound: feed rendering (lua / json).
pub trait FeedRenderer: Send + Sync {
    /// Serializes `feed` into `format`.
    ///
    /// # Errors
    ///
    /// Returns `RenderError` when the snapshot cannot be represented in the
    /// requested serialization.
    fn render(&self, feed: &FeedSnapshot, format: FeedFormat) -> Result<Vec<u8>, RenderError>;
}

/// Outbound: feed publication (disk + in-memory for HTTP).
pub trait FeedPublisher: Send + Sync {
    /// Stores the rendered bytes for a target/format (disk and memory).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the bytes cannot be persisted.
    fn publish(
        &self,
        target: &TargetId,
        format: FeedFormat,
        bytes: &[u8],
    ) -> Result<(), StoreError>;
    /// Publication stamp of the most recent publish, if any.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the stamp cannot be read.
    fn last_published(
        &self,
        target: &TargetId,
        format: FeedFormat,
    ) -> Result<Option<PublishedFeed>, StoreError>;
    /// Bytes of the last published feed (serving /feed.lua and friends).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the stored bytes cannot be read.
    fn read(&self, target: &TargetId, format: FeedFormat) -> Result<Option<Vec<u8>>, StoreError>;
}

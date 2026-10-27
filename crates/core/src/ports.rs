//! Port traits — the hexagonal seams. `infra` implements the driven ports;
//! `server`'s driving adapters call the application services, which depend
//! only on these traits.

use crate::{
    config::TargetConfig,
    domain::{
        BrowseCursor,
        CatalogCounts,
        Category,
        FeedFormat,
        FeedSnapshot,
        FetchOutcome,
        Flavor,
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

/// Outbound: item/pet name lookups (cache-aside handled by callers). Sources
/// are flavor-aware: the same numeric id may resolve against different game
/// versions' endpoints and yield different names.
pub trait ItemInfoPort: Send + Sync {
    /// Looks up one item in the source item API for the given flavor; `None`
    /// when the item does not exist there.
    ///
    /// # Errors
    ///
    /// Returns `SourceError::Http` (or `SourceError::RateLimited` when the
    /// source said 429) on transport failures and `SourceError::Parse` when
    /// the response cannot be decoded.
    fn describe(&self, flavor: &Flavor, item_id: i64) -> Result<Option<NameRecord>, SourceError>;
    /// Looks up one battle-pet species in the source pet API for the
    /// flavor; `None` when the species does not exist there.
    ///
    /// # Errors
    ///
    /// Returns `SourceError::Http` (or `SourceError::RateLimited` when the
    /// source said 429) on transport failures and `SourceError::Parse` when
    /// the response cannot be decoded.
    fn describe_pet(
        &self,
        flavor: &Flavor,
        species_id: i64,
    ) -> Result<Option<PetNameRecord>, SourceError>;
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
    /// Loads the fold for one (target, item, day, category); `None` when no
    /// fold has been saved for that day yet.
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
        category: Category,
    ) -> Result<Option<DayFold>, StoreError>;
    /// Day folds for many identities at once, positionally aligned:
    /// `folds[i]` holds the `(day, fold)` pairs stored for `items[i]` within
    /// the inclusive `[first_day, last_day]` window (pairs may arrive in any
    /// order; empty when the item has no folds in the window). Batched fuel
    /// for feed builds: one chunked query replaces per-item-per-day loads.
    /// Only folds of the given category are returned.
    ///
    /// Callers must pass DISTINCT identities: adapters resolve positionally
    /// by identity, so a repeated identity would read back empty for its
    /// second occurrence. (All current callers dedupe by construction.)
    ///
    /// The default falls back to per-item [`PriceHistoryRepository::load_fold`]
    /// reads — correct for any adapter, just less efficient.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when folds cannot be read or
    /// deserialized.
    fn load_folds(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        first_day: i64,
        last_day: i64,
        category: Category,
    ) -> Result<Vec<Vec<(i64, DayFold)>>, StoreError> {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let mut per_item = Vec::new();
            for day in first_day..=last_day {
                if let Some(fold) = self.load_fold(target, item, day, category)? {
                    per_item.push((day, fold));
                }
            }
            out.push(per_item);
        }
        Ok(out)
    }
    /// Persists the fold for one (target, item, day, category), replacing
    /// any previous state for that day and category.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the fold cannot be written.
    fn save_fold(
        &self,
        target: &TargetId,
        item: &ItemRef,
        day: i64,
        category: Category,
        fold: &DayFold,
    ) -> Result<(), StoreError>;
    /// Persists many folds in one bulk pass: tuple = (item, day, category,
    /// fold), each replacing any previous state for that (item, day,
    /// category). Batched fuel for the ingest cycle — adapters should write
    /// the whole batch in one transaction instead of one commit per fold
    /// (market-scale dumps fold five-digit identity counts per cycle).
    ///
    /// The default falls back to per-item [`PriceHistoryRepository::save_fold`]
    /// writes — correct for any adapter, just less efficient.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when any fold cannot be written.
    fn save_folds(
        &self,
        target: &TargetId,
        folds: &[(ItemRef, i64, Category, DayFold)],
    ) -> Result<(), StoreError> {
        for (item, day, category, fold) in folds {
            self.save_fold(target, item, *day, *category, fold)?;
        }
        Ok(())
    }
    /// Drops the raw listing working set older than the given UTC day.
    /// Returns the number of raw rows removed. Snapshot metadata, folds and
    /// sale credits are permanent.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the raw rows cannot be deleted.
    fn prune_raw_before(&self, target: &TargetId, day: i64) -> Result<u64, StoreError>;
    /// Deletes CLOSED sell-through sightings closed before the given UTC day
    /// (their aggregates already live in the sales ledger); returns the rows
    /// removed. Open sights and anything newer stay untouched. Retention
    /// counterpart to [`PriceHistoryRepository::prune_raw_before`] — callers
    /// gate it on their own config (`0` = never call).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the sights cannot be deleted.
    fn prune_sights_before(&self, target: &TargetId, day: i64) -> Result<u64, StoreError> {
        let _ = (target, day);
        Ok(0)
    }
    /// Distinct items that have fold data of the given category on/after the
    /// given UTC day.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the fold index cannot be read.
    fn items_with_data(
        &self,
        target: &TargetId,
        since_day: i64,
        category: Category,
    ) -> Result<Vec<ItemRef>, StoreError>;
    /// Per-item-day aggregate rows on/after the given UTC day, scoped to one
    /// category (ranking fuel for the opportunities/overview views).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the aggregates cannot be computed.
    fn item_day_rows(
        &self,
        target: &TargetId,
        since_day: i64,
        category: Category,
    ) -> Result<Vec<crate::domain::ItemDayRow>, StoreError>;
    /// Per-item-day aggregate rows on/after `since_day` for exactly one item
    /// identity and category (the item detail page): SQL-scoped instead of
    /// loading the whole market's rows. The identity must match the canonical
    /// `item_ref` tuple (`kind` + `item_id` + pet fields, see
    /// `split_item_ref`).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the aggregates cannot be computed.
    fn item_day_rows_for_item(
        &self,
        target: &TargetId,
        item: &ItemRef,
        since_day: i64,
        category: Category,
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
    /// Fraction of listings credited as sold within the trailing day window,
    /// for one category (commodity and item-AH sale evidence are separate
    /// ledgers; pets split off the item-AH rows).
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
        category: Category,
    ) -> Result<Option<f64>, StoreError>;
    /// Sale rate summed over several item identities (pet variants of one
    /// species share their market) within one category. `Ok(None)` when none
    /// of the items has sales rows.
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
        category: Category,
    ) -> Result<Option<f64>, StoreError> {
        let _ = (target, items, days, category);
        Ok(None)
    }
    /// Sale rate for EACH of the given identities, positionally aligned
    /// (`rates[i]` belongs to `items[i]`); `None` entries have no sales rows.
    /// Batched replacement for per-item `sale_rate` loops over candidate
    /// lists. Scoped to one category.
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
        category: Category,
    ) -> Result<Vec<Option<f64>>, StoreError> {
        // Correct-by-default fallback: one query per identity.
        let mut rates = Vec::with_capacity(items.len());
        for item in items {
            rates.push(self.sale_rate(target, item, days, category)?);
        }
        Ok(rates)
    }
}

/// Outbound: memoized span metrics (invalidated per item on ingest).
pub trait MetricsCacheRepository: Send + Sync {
    /// The cached block for one (target, item, category, span), if one is
    /// stored.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cache cannot be read.
    fn get(
        &self,
        target: &TargetId,
        item: &ItemRef,
        category: Category,
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
        category: Category,
        span: Span,
        block: &MetricBlock,
        computed_at: i64,
    ) -> Result<(), StoreError>;
    /// Drops all cached blocks for a target (every category). Called when
    /// fold state changes beyond what a per-item rule can see: the rolling
    /// span windows slide at UTC day boundaries, so the first cycle after a
    /// day roll sweeps all of a target's memoized blocks.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cache cannot be cleared.
    fn invalidate(&self, target: &TargetId) -> Result<(), StoreError>;
    /// Drops cached blocks for just the given items of a target and category
    /// (called after a cycle that touched only those items' folds or sale
    /// state).
    ///
    /// The default falls back to the full sweep — correct for any adapter,
    /// just less precise.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cache cannot be cleared.
    fn invalidate_items(
        &self,
        target: &TargetId,
        _category: Category,
        _items: &[ItemRef],
    ) -> Result<(), StoreError> {
        self.invalidate(target)
    }
    /// Cached blocks for several identities of one target/category/span,
    /// positionally aligned (`blocks[i]` belongs to `items[i]`); `None`
    /// entries have no cached block. Batched fuel for whole-watchlist views.
    ///
    /// Callers must pass DISTINCT identities (same positional-resolution
    /// contract as [`PriceHistoryRepository::load_folds`]).
    ///
    /// The default falls back to per-item reads — correct for any adapter,
    /// just less efficient.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cache cannot be read.
    fn get_many(
        &self,
        target: &TargetId,
        category: Category,
        span: Span,
        items: &[ItemRef],
    ) -> Result<Vec<Option<crate::domain::CachedBlock>>, StoreError> {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            out.push(self.get(target, item, category, span)?);
        }
        Ok(out)
    }
    /// Stores freshly computed blocks for several identities of one
    /// target/category/span, positionally aligned (`blocks[i]` belongs to
    /// `items[i]`; `None` entries are skipped — empty blocks are never
    /// cached). Batched tail of whole-feed recomputation.
    ///
    /// The default falls back to per-item [`MetricsCacheRepository::put`]
    /// writes — correct for any adapter, just less efficient.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when a block cannot be written.
    fn put_many(
        &self,
        target: &TargetId,
        category: Category,
        span: Span,
        items: &[ItemRef],
        blocks: &[Option<MetricBlock>],
        computed_at: i64,
    ) -> Result<(), StoreError> {
        for (item, block) in items.iter().zip(blocks.iter()) {
            if let Some(block) = block {
                self.put(target, item, category, span, block, computed_at)?;
            }
        }
        Ok(())
    }
}

/// Outbound: item/pet name catalog backing search + completions + browsing.
/// Rows are keyed per [`Flavor`] (see `domain::Flavor`); the read-only
/// search/browse surface spans all flavors and dedupes by identity.
pub trait ItemCatalogRepository: Send + Sync {
    /// Inserts or updates the given catalog records for one flavor.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when records cannot be written.
    fn upsert(&self, flavor: &Flavor, records: &[NameRecord]) -> Result<(), StoreError>;
    /// Inserts or updates the given pet-species records for one flavor.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when records cannot be written.
    fn upsert_pets(&self, flavor: &Flavor, records: &[PetNameRecord]) -> Result<(), StoreError>;
    /// Kind-aware substring search over known names, at most `limit` hits
    /// starting at `offset` (prefix matches first, then alphabetical). The
    /// paging pair for completion boxes and browse pages alike.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    fn search(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SearchHit>, StoreError>;
    /// The catalog record for one item id in one flavor, if known.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    fn by_id(&self, flavor: &Flavor, item_id: i64) -> Result<Option<NameRecord>, StoreError>;
    /// The catalog record for one pet species id in one flavor, if known.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    fn pet_by_species(
        &self,
        flavor: &Flavor,
        species_id: i64,
    ) -> Result<Option<PetNameRecord>, StoreError>;
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

/// Outbound: alert rule persistence (the rule set the evaluator runs).
pub trait AlertRuleRepository: Send + Sync {
    /// Stores a new rule and returns the stamped version.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the rule cannot be written.
    fn add(
        &self,
        rule: &crate::domain::NewAlertRule,
    ) -> Result<crate::domain::AlertRule, StoreError>;
    /// Removes the rule by id; returns whether anything was removed.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the rule cannot be deleted.
    fn remove(&self, rule_id: i64) -> Result<bool, StoreError>;
    /// All rules of a target, in insertion order.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when rules cannot be read.
    fn list(&self, target: &TargetId) -> Result<Vec<crate::domain::AlertRule>, StoreError>;
    /// Atomically persists one fired alert (event row + the rule's cooldown
    /// anchor) and returns the event id. The two writes share one transaction:
    /// an event recorded without its anchor would make the next pass re-fire
    /// and re-dispatch a duplicate. The anchor stamp is a no-op for unknown
    /// rule ids.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the event or the anchor cannot be
    /// written.
    fn record_firing(&self, event: &crate::domain::NewAlertEvent) -> Result<i64, StoreError>;
}

/// Outbound: fired-alert history (read side — writes go through
/// [`AlertRuleRepository::record_firing`], which books the event and the
/// cooldown anchor atomically).
pub trait AlertEventRepository: Send + Sync {
    /// The `limit` newest events of a target, newest first.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when events cannot be read.
    fn recent(
        &self,
        target: &TargetId,
        limit: usize,
    ) -> Result<Vec<crate::domain::AlertEvent>, StoreError>;
}

/// Outbound: alert delivery (Discord/Telegram/log adapters). Implementations
/// must be cheap and side-effect-light: the evaluator records the event FIRST
/// (delivery is best effort — a failed dispatch is counted and logged, never
/// retried from here and never blocks evaluation).
pub trait AlertDispatchPort: Send + Sync {
    /// Delivers one fired alert.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::AlertDispatchError`] when the message could
    /// not be delivered; the caller counts and logs it.
    fn dispatch(
        &self,
        event: &crate::domain::AlertEvent,
    ) -> Result<(), crate::error::AlertDispatchError>;
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

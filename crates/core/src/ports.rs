//! Port traits — the hexagonal seams. `infra` implements the driven ports;
//! `server`'s driving adapters call the application services, which depend
//! only on these traits.

use std::collections::HashMap;

use crate::{
    config::TargetConfig,
    domain::{
        BrowseCursor,
        CatalogCounts,
        Category,
        ClassSummary,
        DatabaseSnapshot,
        FeedFormat,
        FetchOutcome,
        Flavor,
        ItemDayLow,
        ItemRef,
        ListingPage,
        ListingQuery,
        MetricBlock,
        NameRecord,
        NewSnapshot,
        NewWatchlistEntry,
        NormalizedListing,
        PayloadKind,
        PetNameRecord,
        PublishedFeed,
        SaleCounts,
        SearchHit,
        SnapshotFloor,
        SnapshotListing,
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
    /// Live listing summary of every item in one stored snapshot — what was
    /// listed, and at which floor, at the moment of that fetch. `snapshot_id`
    /// must belong to `target`/`kind` (callers resolve it through
    /// [`Self::latest`]). The default answers empty: correct for adapters
    /// without a raw working set (live-deal views then read as "nothing
    /// listed").
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the summary cannot be read.
    fn floors(
        &self,
        target: &TargetId,
        kind: PayloadKind,
        snapshot_id: i64,
    ) -> Result<HashMap<ItemRef, SnapshotFloor>, StoreError> {
        let _ = (target, kind, snapshot_id);
        Ok(HashMap::new())
    }
    /// [`Self::floors`] narrowed to one identity: `None` when the item has no
    /// listing in the snapshot. The default answers `None` (see
    /// [`Self::floors`]).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the summary cannot be read.
    fn floor(
        &self,
        target: &TargetId,
        item: &ItemRef,
        snapshot_id: i64,
    ) -> Result<Option<SnapshotFloor>, StoreError> {
        let _ = (target, item, snapshot_id);
        Ok(None)
    }
    /// Every listing of one identity in one stored snapshot, each joined to
    /// its sighting bookkeeping (`first_seen_*` is `None` without a sighting
    /// row). The per-listing view behind the sniper alert conditions; the
    /// default answers empty.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the listings cannot be read.
    fn listings_for_item(
        &self,
        target: &TargetId,
        item: &ItemRef,
        kind: PayloadKind,
        snapshot_id: i64,
    ) -> Result<Vec<SnapshotListing>, StoreError> {
        let _ = (target, item, kind, snapshot_id);
        Ok(Vec::new())
    }
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
    /// Metadata for one snapshot id, any target/kind (the snapshot viewer's
    /// pinned views); `None` when the id is unknown.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the metadata cannot be read.
    fn snapshot_by_id(&self, id: i64) -> Result<Option<SnapshotMeta>, StoreError>;
    /// One page of stored listings for a snapshot, joined to their flavor-
    /// scoped catalog identity (the snapshot viewer's table). Ordering follows
    /// `query.sort`/`query.desc` with `auction_id` as the stable tiebreak;
    /// `query.page` addresses [`LISTING_PAGE_SIZE`]-sized pages and the
    /// filter fields apply as exact/prefix matches (see [`ListingQuery`]).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the query cannot run; an unknown
    /// snapshot id reads as an empty page (callers resolve ids first via
    /// [`SnapshotRepository::snapshot_by_id`]).
    fn listings_page(
        &self,
        flavor: &Flavor,
        snapshot_id: i64,
        query: &ListingQuery,
    ) -> Result<ListingPage, StoreError>;
    /// Per-item-class listing counts for one snapshot under one flavor, most-
    /// listed class first (the viewer's class filter dropdown). Only
    /// enriched listings are counted; sparse identities appear only in the
    /// unfiltered view. Counts are cached per snapshot against a catalog
    /// watermark and recomputed when enrichment moves it, so classes gained
    /// by later enrichment surface in the dropdown; pruning a snapshot's
    /// listings drops its cache rows with them.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the summary cannot be read.
    fn snapshot_class_summary(
        &self,
        flavor: &Flavor,
        snapshot_id: i64,
    ) -> Result<Vec<ClassSummary>, StoreError>;
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
    /// Intraday hour cells for one identity on/after the given UTC day, from
    /// the PERMANENT `item_hour` fold (not the raw working set): one
    /// [`ItemHourRow`](crate::domain::ItemHourRow) per observed UTC
    /// `(day, hour)` cell with the hour's per-snapshot minimums. Fuel for the
    /// intraday buy-timing view; the horizon is bounded only by the fold's
    /// own retention, not by raw-listing retention.
    ///
    /// The identity must match the canonical `item_ref` tuple; the payload
    /// kind scopes the market (see [`Category::payload_kind`]).
    ///
    /// The default answers empty — correct for adapters without an intraday
    /// fold (the view degrades to "no intraday data").
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cells cannot be read.
    fn item_hour_cells(
        &self,
        target: &TargetId,
        item: &ItemRef,
        kind: PayloadKind,
        since_day: i64,
    ) -> Result<Vec<crate::domain::ItemHourRow>, StoreError> {
        let _ = (target, item, kind, since_day);
        Ok(Vec::new())
    }
    /// Drops intraday hour-fold cells older than the given UTC day; returns
    /// the rows removed. Retention counterpart to
    /// [`PriceHistoryRepository::prune_raw_before`] on the fold side — callers
    /// gate it on their own config (`0` = never call).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cells cannot be deleted.
    fn prune_item_hours_before(&self, target: &TargetId, day: i64) -> Result<u64, StoreError> {
        let _ = (target, day);
        Ok(0)
    }
    /// Clears every intraday hour-fold cell of the target from the given UTC
    /// day on (the repair counterpart of
    /// [`PriceHistoryRepository::prune_item_hours_before`], which drops the
    /// days *before* its cutoff): a subsequent [`Self::backfill_item_hours`]
    /// rebuilds exactly those hours from the raw window. Callers must pass a
    /// day inside the raw window — cells without raw rows cannot come back.
    /// Returns the rows removed.
    ///
    /// The default is a no-op returning `0`.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cells cannot be deleted.
    fn clear_item_hours_from(&self, target: &TargetId, day: i64) -> Result<u64, StoreError> {
        let _ = (target, day);
        Ok(0)
    }
    /// Rebuilds intraday hour-fold cells from the raw listing working set:
    /// every `(target, kind, day, hour)` with `day >= since_day` that still
    /// has a snapshot but no fold cell yet is rewritten as the exact
    /// aggregate of ALL its raw-retained listings (one atomic statement per
    /// hour, so an hour that already holds any cell is reconciled and skipped
    /// — only genuinely missing hours, e.g. after an interrupted pass, pay
    /// the rebuild). Hours before `since_day` are never visited: snapshot
    /// rows outlive their listings, so without the horizon every hour past
    /// the raw window would be re-examined on every call, forever. Callers
    /// pass the first day of the rebuildable window
    /// (`today - storage.fold_rebuild_window_days() + 1`). Boot-time/CLI
    /// migration so a fresh fold starts with the raw window's history instead
    /// of an empty chart. Returns the number of cells written.
    ///
    /// The default is a no-op returning `0` — adapters without a raw working
    /// set have nothing to backfill from.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the fold cannot be rebuilt.
    fn backfill_item_hours(&self, since_day: i64) -> Result<u64, StoreError> {
        let _ = since_day;
        Ok(0)
    }
    /// Per-identity daily lowest asks from the PERMANENT intraday fold on/after
    /// `since_day`: for every identity with hour cells in the window, one row
    /// per UTC day it was seen, carrying the minimum of that day's
    /// per-snapshot floors. Narrow rows — no day-fold JSON to parse — so a
    /// whole market's month of lows reads in a fraction of the day rows'
    /// cost. `kind` scopes the market (items and pets share the auctions
    /// payload).
    ///
    /// The default answers empty — correct for adapters without an intraday
    /// fold (the 30-day-low indicator then reads as "no history").
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cells cannot be read.
    fn daily_lows(
        &self,
        target: &TargetId,
        kind: PayloadKind,
        since_day: i64,
    ) -> Result<Vec<ItemDayLow>, StoreError> {
        let _ = (target, kind, since_day);
        Ok(Vec::new())
    }
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
    /// (`rates[i]` belongs to `items[i]`); `None` entries have no
    /// credit-eligible evidence in the window. Batched replacement for per-item
    /// `sale_rate` loops over candidate lists. Scoped to one category.
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
    /// The ledger counts behind the sale rate for EACH of the given identities,
    /// positionally aligned (`counts[i]` belongs to `items[i]`); `None`
    /// entries have no credit-eligible close in the window — the same "no
    /// evidence" contract as [`Self::sale_rate`], whose figure is exactly
    /// `sold / (sold + expired)`. Scoped to one category.
    ///
    /// The default answers `None` for every identity — correct for adapters
    /// without a ledger.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the credit statistics cannot be
    /// read.
    fn sale_counts_each(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        days: i64,
        category: Category,
    ) -> Result<Vec<Option<SaleCounts>>, StoreError> {
        let _ = (target, days, category);
        Ok(vec![None; items.len()])
    }
    /// The ledger counts behind the sale rate for one identity and category
    /// (see [`Self::sale_counts_each`]); `None` when the window holds no
    /// credit-eligible close.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the credit statistics cannot be
    /// read.
    fn sale_counts(
        &self,
        target: &TargetId,
        item: &ItemRef,
        days: i64,
        category: Category,
    ) -> Result<Option<SaleCounts>, StoreError> {
        Ok(self
            .sale_counts_each(target, std::slice::from_ref(item), days, category)?
            .into_iter()
            .next()
            .flatten())
    }
    /// Credited sale PRICES for each of the given identities, positionally
    /// aligned (`sold[i]` belongs to `items[i]`): the window's priced sold
    /// units as `(unit price, quantity)` buckets sorted by price. A credited
    /// vanish counts at the unit price its sight was last seen asking, a
    /// commodity partial fill at the listing's posted price. Empty when
    /// nothing priced was sold in the window (no sales, or a ledger that
    /// predates sold-price recording). Scoped to one category.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the credit statistics cannot be
    /// read.
    fn sold_price_each(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        days: i64,
        category: Category,
    ) -> Result<Vec<Vec<(i64, i64)>>, StoreError> {
        // Correct-by-default: no priced sales, like an empty ledger.
        let _ = (target, days, category);
        Ok(vec![Vec::new(); items.len()])
    }
    /// Units credited as sold within the trailing day window, for one
    /// identity and category — credited vanish quantities plus commodity
    /// partial-fill deltas. `Ok(None)` when the identity has no sales rows
    /// at all; `Ok(Some(0))` means rows exist but nothing sold.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the ledger cannot be read.
    fn qty_sold(
        &self,
        target: &TargetId,
        item: &ItemRef,
        days: i64,
        category: Category,
    ) -> Result<Option<i64>, StoreError>;
    /// Units sold for EACH of the given identities, positionally aligned
    /// (`units[i]` belongs to `items[i]`); `None` entries have no sales
    /// rows. Batched twin of [`SalesRepository::qty_sold`]. Scoped to one
    /// category.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the ledger cannot be read.
    fn qty_sold_each(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        days: i64,
        category: Category,
    ) -> Result<Vec<Option<i64>>, StoreError> {
        // Correct-by-default fallback: one query per identity.
        let mut units = Vec::with_capacity(items.len());
        for item in items {
            units.push(self.qty_sold(target, item, days, category)?);
        }
        Ok(units)
    }
    /// Units sold summed over several item identities (pet variants of one
    /// species share their market) within one category. `Ok(None)` when
    /// none of the items has sales rows.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the ledger cannot be read.
    fn qty_sold_multi(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        days: i64,
        category: Category,
    ) -> Result<Option<i64>, StoreError> {
        // Correct-by-default fallback: pool the per-identity readings.
        let mut total = 0i64;
        let mut any = false;
        for units in self.qty_sold_each(target, items, days, category)?.into_iter().flatten() {
            any = true;
            // Saturating: one absurd day must not overflow the species
            // sum (debug builds panic on overflow).
            total = total.saturating_add(units);
        }
        Ok(if any { Some(total) } else { None })
    }
    /// Ledger recency for one identity: the last credited-sale day and the
    /// net closures since, in one category. `Ok(None)` when no sale was ever
    /// credited (items with sales rows but zero credits read as `None` too —
    /// "days since last sale" is meaningless without a sale).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the ledger cannot be read.
    fn sale_recency(
        &self,
        target: &TargetId,
        item: &ItemRef,
        category: Category,
    ) -> Result<Option<crate::domain::SaleRecency>, StoreError> {
        let _ = (target, item, category);
        Ok(None)
    }
    /// Ledger recency for EACH of the given identities, positionally aligned
    /// (`recencies[i]` belongs to `items[i]`). Batched twin of
    /// [`SalesRepository::sale_recency`]. Scoped to one category.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the ledger cannot be read.
    fn sale_recency_each(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        category: Category,
    ) -> Result<Vec<Option<crate::domain::SaleRecency>>, StoreError> {
        // Correct-by-default fallback: one query per identity.
        let mut recencies = Vec::with_capacity(items.len());
        for item in items {
            recencies.push(self.sale_recency(target, item, category)?);
        }
        Ok(recencies)
    }
    /// Ledger recency pooled over several item identities (pet variants of
    /// one species share their market): the MOST RECENT member's last-sale
    /// day — any variant selling resets the species' clock — and the summed
    /// closures strictly after that day across all members. `Ok(None)` when
    /// no member ever credited a sale.
    ///
    /// The default loop pools the per-identity readings, so its closure sum
    /// is an approximation: closures that fall between the pooled day and a
    /// member's own last-sale day are only visible to a pooled backend (the
    /// SQLite store overrides this with the exact sum).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the ledger cannot be read.
    fn sale_recency_multi(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        category: Category,
    ) -> Result<Option<crate::domain::SaleRecency>, StoreError> {
        // Correct-by-default fallback: pool the per-identity readings.
        let mut last_sale_day: Option<i64> = None;
        let mut closures_since = 0i64;
        let mut sold_any = false;
        for recency in self.sale_recency_each(target, items, category)? {
            let Some(recency) = recency else { continue };
            sold_any = true;
            last_sale_day = Some(match last_sale_day {
                Some(day) => day.max(recency.last_sale_day),
                None => recency.last_sale_day,
            });
            closures_since += recency.closures_since;
        }
        if !sold_any {
            return Ok(None);
        }
        Ok(Some(crate::domain::SaleRecency {
            last_sale_day: last_sale_day.unwrap_or_default(),
            closures_since,
        }))
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
    /// Vendor sell prices (copper) of the given item ids under one flavor —
    /// only ids with a recorded, positive price; an id the API reports no
    /// price for is absent. The hard floor under an item's market (players
    /// vendor rather than list cheaper), behind the deal ranking's at-vendor
    /// marker. Ids may repeat; the answer is a set.
    ///
    /// The default loops [`Self::by_id`] — correct for any adapter, just less
    /// efficient.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    fn sell_prices(
        &self,
        flavor: &Flavor,
        item_ids: &[i64],
    ) -> Result<HashMap<i64, i64>, StoreError> {
        let mut out = HashMap::new();
        for item_id in item_ids {
            if let Some(price) = self
                .by_id(flavor, *item_id)?
                .and_then(|record| record.sell_price)
                .filter(|price| *price > 0)
            {
                out.insert(*item_id, price);
            }
        }
        Ok(out)
    }
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

/// Outbound: database feed rendering (the v4 carrier and its JSON twin).
pub trait FeedRenderer: Send + Sync {
    /// Serializes `snapshot` into `format`.
    ///
    /// # Errors
    ///
    /// Returns `RenderError` when the snapshot cannot be represented in the
    /// requested serialization: an identity filed under the wrong table, a
    /// pet below species grain, a record without a span, or keys out of order.
    fn render(
        &self,
        snapshot: &DatabaseSnapshot,
        format: FeedFormat,
    ) -> Result<Vec<u8>, RenderError>;
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

/// File-level space usage of the backing store, for logging and
/// reclamation decisions. Counts are SQLite page accounting: the total is
/// what the file occupies on disk, the freelist is pages freed by deletes
/// that are reused before the file grows again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpaceStats {
    /// SQLite page size in bytes.
    pub page_size: i64,
    /// Total pages in the database file.
    pub page_count: i64,
    /// Pages freed by deletes and currently unused.
    pub freelist_count: i64,
}

impl SpaceStats {
    /// Bytes the database file occupies on disk (excluding the WAL).
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        u64::try_from(self.page_size.saturating_mul(self.page_count)).unwrap_or(u64::MAX)
    }

    /// Bytes currently in the freelist (reclaimable without a rebuild).
    #[must_use]
    pub fn free_bytes(&self) -> u64 {
        u64::try_from(self.page_size.saturating_mul(self.freelist_count)).unwrap_or(u64::MAX)
    }

    /// Fraction of the file that is free pages (`0.0..=1.0`); `0.0` when
    /// the file has no pages.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // page counts far below 2^53: exact
    pub fn free_ratio(&self) -> f64 {
        if self.page_count == 0 {
            return 0.0;
        }
        self.freelist_count as f64 / self.page_count as f64
    }
}

/// Outbound: storage-file maintenance — space accounting and reclamation.
/// Implemented by the SQLite store; driven by the scheduler's maintenance
/// pass and the `vacuum` CLI command. Every method is a short standalone
/// operation (the store serializes internally), so callers may interleave
/// them with normal traffic; only `VACUUM`-based operations block writers
/// for their duration.
pub trait SpaceMaintenance: Send + Sync {
    /// Page-level space accounting of the database file.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the pragmas cannot be read.
    fn space_stats(&self) -> Result<SpaceStats, StoreError>;

    /// The database header's auto-vacuum mode: `0` none, `1` full, `2`
    /// incremental. A `0` header ignores the in-memory pragma until a full
    /// `VACUUM` rebuilds the file.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the pragma cannot be read.
    fn auto_vacuum_mode(&self) -> Result<i64, StoreError>;

    /// Switches the header to incremental auto-vacuum and compacts once
    /// (the header change only takes effect through a full `VACUUM`).
    /// Blocks writers for the duration of the rebuild.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the pragma or the `VACUUM` fails
    /// (e.g. another process holds a conflicting lock).
    fn enable_incremental_auto_vacuum(&self) -> Result<(), StoreError>;

    /// Returns all freelist pages to the filesystem
    /// (`PRAGMA incremental_vacuum`). Cheap no-op on an empty freelist.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the pragma fails.
    fn incremental_vacuum(&self) -> Result<(), StoreError>;

    /// Rebuilds the database file (`VACUUM`): compacts every free page and
    /// defragments. Blocks writers for its duration and needs free disk
    /// roughly the size of the database.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the `VACUUM` fails.
    fn full_vacuum(&self) -> Result<(), StoreError>;

    /// Checkpoints and truncates the WAL file
    /// (`PRAGMA wal_checkpoint(TRUNCATE)`), returning its bytes to the
    /// filesystem after write bursts.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when readers block the checkpoint.
    fn checkpoint_wal(&self) -> Result<(), StoreError>;
}

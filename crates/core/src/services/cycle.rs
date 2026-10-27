//! Ingestion cycle: fetch → record snapshot → fold day histograms.
//! Stages run sequentially and stop at the first failure — earlier writes of
//! a cycle stay in place (cycles are NOT transactional end-to-end; see the
//! `run_cycle` doc for the exact per-stage failure semantics).

use std::{
    collections::HashMap,
    sync::{
        Arc,
        Mutex,
    },
};

use crate::{
    config::TargetConfig,
    domain::{
        utc_day,
        Category,
        FetchOutcome,
        ItemRef,
        ListingKind,
        NewSnapshot,
        PayloadKind,
        SaleCredit,
        SnapshotStatus,
        TargetId,
    },
    error::{
        CycleError,
        StoreError,
    },
    fold::{
        group_by_item,
        DayFold,
    },
    ports::{
        AuctionSourcePort,
        ClockPort,
        MetricsCacheRepository,
        PriceHistoryRepository,
        SalesRepository,
        SnapshotRepository,
    },
};

/// Runs the fetch → record → fold pipeline for configured targets.
pub struct IngestCycleService {
    source: Arc<dyn AuctionSourcePort>,
    snapshots: Arc<dyn SnapshotRepository>,
    history: Arc<dyn PriceHistoryRepository>,
    cache: Arc<dyn MetricsCacheRepository>,
    sales: Arc<dyn SalesRepository>,
    clock: Arc<dyn ClockPort>,
    /// Last UTC day each target's cache got a full sweep. Per-cycle
    /// invalidation is item-targeted; the sweep covers the rolling-window
    /// slide at UTC day boundaries, which no per-item rule can see.
    last_sweep_day: Mutex<HashMap<TargetId, i64>>,
}

/// Terminal state of one cycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CycleOutcome {
    /// A fresh payload was fetched, persisted and folded.
    Fetched,
    /// The source answered 304 — nothing was written.
    NotModified,
    /// A fresh payload parsed valid but carried zero listings. The snapshot is
    /// archived, but sighting bookkeeping is skipped: mass-closing every open
    /// sight against an empty dump (realm maintenance, emulator hiccup) would
    /// credit phantom sales.
    Empty,
}

/// Per-cycle counters and ids, for logs and HTTP responses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CycleReport {
    /// The target the cycle ran for.
    pub target: TargetId,
    /// Which payload kind the cycle fetched.
    pub kind: PayloadKind,
    /// Whether a fresh payload was ingested.
    pub outcome: CycleOutcome,
    /// Listings that survived the ACL pass (0 for 304).
    pub entries: u64,
    /// Listings dropped by the ACL pass (0 for 304).
    pub dropped: u64,
    /// The new snapshot id (absent for 304 or failed cycles).
    pub snapshot_id: Option<i64>,
    /// Distinct items whose day fold was updated.
    pub items_folded: usize,
    /// Sale credits granted by `close_missing` in this cycle.
    pub sale_credits: usize,
}

impl IngestCycleService {
    /// Wires the cycle to its ports (all driven adapters injected here).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: Arc<dyn AuctionSourcePort>,
        snapshots: Arc<dyn SnapshotRepository>,
        history: Arc<dyn PriceHistoryRepository>,
        cache: Arc<dyn MetricsCacheRepository>,
        sales: Arc<dyn SalesRepository>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self {
            source,
            snapshots,
            history,
            cache,
            sales,
            clock,
            last_sweep_day: Mutex::new(HashMap::new()),
        }
    }

    /// Runs one fetch+ingest cycle for a target and payload kind.
    ///
    /// # Errors
    ///
    /// Returns `CycleError::Source` when the fetch or parse fails (nothing is
    /// persisted then), and `CycleError::Store` when any persistence stage
    /// fails — the cycle stops at the first failing write, leaving earlier
    /// writes of this cycle in place.
    pub fn run_cycle(
        &self,
        target: &TargetConfig,
        kind: PayloadKind,
    ) -> Result<CycleReport, CycleError> {
        let if_modified_since =
            self.snapshots.latest(&target.id, kind)?.and_then(|meta| meta.source_last_modified);

        match self.source.fetch(target, kind, if_modified_since)? {
            FetchOutcome::NotModified => Ok(CycleReport {
                target: target.id.clone(),
                kind,
                outcome: CycleOutcome::NotModified,
                entries: 0,
                dropped: 0,
                snapshot_id: None,
                items_folded: 0,
                sale_credits: 0,
            }),
            FetchOutcome::Payload(payload) => {
                let now = self.clock.now();
                if payload.listings.is_empty() {
                    let snapshot_id = self.snapshots.record(
                        &NewSnapshot {
                            target: target.id.clone(),
                            kind,
                            fetched_at: now,
                            source_last_modified: payload.source_last_modified,
                            entry_count: 0,
                            dropped_count: payload.dropped_count,
                            dropped_reasons: payload.dropped_reasons.clone(),
                            status: SnapshotStatus::Ok,
                        },
                        &payload.listings,
                        &payload.raw,
                    )?;
                    // No logging here (the core never logs): the scheduler
                    // upgrades `CycleOutcome::Empty` reports to a warning.
                    return Ok(CycleReport {
                        target: target.id.clone(),
                        kind,
                        outcome: CycleOutcome::Empty,
                        entries: 0,
                        dropped: payload.dropped_count,
                        snapshot_id: Some(snapshot_id),
                        items_folded: 0,
                        sale_credits: 0,
                    });
                }
                let day = utc_day(now);
                let snapshot_id = self.snapshots.record(
                    &NewSnapshot {
                        target: target.id.clone(),
                        kind,
                        fetched_at: now,
                        source_last_modified: payload.source_last_modified,
                        entry_count: payload.listings.len() as u64,
                        dropped_count: payload.dropped_count,
                        dropped_reasons: payload.dropped_reasons,
                        status: SnapshotStatus::Ok,
                    },
                    &payload.listings,
                    &payload.raw,
                )?;
                let grouped = group_by_item(&payload.listings);
                // The payload kind fixes the auction house; the identity
                // shape splits the item-AH payload's rows into item and pet
                // categories, so one batch can touch two categories.
                let listing_kind = match kind {
                    PayloadKind::Commodities => ListingKind::Commodity,
                    PayloadKind::Auctions => ListingKind::Item,
                };
                let mut affected: Vec<(ItemRef, Category)> = Vec::with_capacity(grouped.len());
                for (item_ref, listings) in &grouped {
                    let category = Category::derive(listing_kind, item_ref);
                    let mut fold = self
                        .history
                        .load_fold(&target.id, item_ref, day, category)?
                        .unwrap_or_else(|| DayFold::new(item_ref.clone()));
                    fold.apply(listings);
                    self.history.save_fold(&target.id, item_ref, day, category, &fold)?;
                    affected.push((item_ref.clone(), category));
                }

                // Sell-through bookkeeping: register sightings, then close
                // the ones absent from the newest snapshot of this kind.
                self.sales.observe(&target.id, kind, &payload.listings, snapshot_id, now)?;
                let credits: Vec<SaleCredit> =
                    self.sales.close_missing(&target.id, kind, snapshot_id, now)?;
                affected.extend(credits.iter().map(|credit| {
                    (credit.item.clone(), Category::derive(listing_kind, &credit.item))
                }));

                // Fold + sale state changed for the affected items only:
                // drop just their memoized blocks (cache is an optimization —
                // the read-side TTL heals any entry left stale by a failed
                // cycle). The rolling span windows slide at UTC day
                // boundaries — a change no per-item rule can see — so the
                // first cycle after a day roll sweeps the whole target cache
                // instead. The sweep marker advances only AFTER the
                // invalidation succeeds: a failed sweep retries next cycle
                // rather than trusting a stamp that never did anything. A
                // poisoned mutex degrades to one extra sweep.
                self.invalidate_after_cycle(&target.id, day, &affected)?;

                Ok(CycleReport {
                    target: target.id.clone(),
                    kind,
                    outcome: CycleOutcome::Fetched,
                    entries: payload.listings.len() as u64,
                    dropped: payload.dropped_count,
                    snapshot_id: Some(snapshot_id),
                    items_folded: grouped.len(),
                    sale_credits: credits.len(),
                })
            }
        }
    }

    /// Drops the memoized metric blocks a cycle made stale. Cache rows are
    /// keyed per category, so the affected items are grouped and invalidated
    /// per category (one or two groups in practice: an item-AH payload can
    /// carry item and pet rows, a commodities payload only one). The rolling
    /// span windows slide at UTC day boundaries — a change no per-item rule
    /// can see — so the first cycle after a day roll sweeps the whole target
    /// cache instead. The sweep marker advances only AFTER the invalidation
    /// succeeds: a failed sweep retries next cycle rather than trusting a
    /// stamp that never did anything. A poisoned mutex degrades to one extra
    /// sweep.
    fn invalidate_after_cycle(
        &self,
        target: &TargetId,
        day: i64,
        affected: &[(ItemRef, Category)],
    ) -> Result<(), StoreError> {
        let sweep = self
            .last_sweep_day
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(target)
            != Some(&day);
        if sweep {
            self.cache.invalidate(target)?;
            self.last_sweep_day
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(target.clone(), day);
            return Ok(());
        }
        let mut by_category: HashMap<Category, Vec<ItemRef>> = HashMap::new();
        for (item, category) in affected {
            by_category.entry(*category).or_default().push(item.clone());
        }
        for (category, items) in &by_category {
            self.cache.invalidate_items(target, *category, items)?;
        }
        Ok(())
    }
}

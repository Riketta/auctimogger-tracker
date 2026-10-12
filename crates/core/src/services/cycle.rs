//! Ingestion cycle: fetch → record snapshot → fold day histograms.
//! One cycle is transactional per snapshot — a failed stage leaves the store
//! untouched (integrity first, plan §6).

use std::sync::Arc;

use crate::config::TargetConfig;
use crate::domain::{
    utc_day, FetchOutcome, NewSnapshot, PayloadKind, SaleCredit, SnapshotStatus, TargetId,
};
use crate::error::CycleError;
use crate::fold::{group_by_item, DayFold};
use crate::ports::{
    AuctionSourcePort, ClockPort, MetricsCacheRepository, PriceHistoryRepository, SalesRepository,
    SnapshotRepository,
};

pub struct IngestCycleService {
    source: Arc<dyn AuctionSourcePort>,
    snapshots: Arc<dyn SnapshotRepository>,
    history: Arc<dyn PriceHistoryRepository>,
    cache: Arc<dyn MetricsCacheRepository>,
    sales: Arc<dyn SalesRepository>,
    clock: Arc<dyn ClockPort>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CycleOutcome {
    Fetched,
    NotModified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CycleReport {
    pub target: TargetId,
    pub kind: PayloadKind,
    pub outcome: CycleOutcome,
    pub entries: u64,
    pub dropped: u64,
    pub snapshot_id: Option<i64>,
    pub items_folded: usize,
    pub sale_credits: usize,
}

impl IngestCycleService {
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
        }
    }

    /// Runs one fetch+ingest cycle for a target and payload kind.
    pub fn run_cycle(
        &self,
        target: &TargetConfig,
        kind: PayloadKind,
    ) -> Result<CycleReport, CycleError> {
        let if_modified_since = self
            .snapshots
            .latest(&target.id, kind)?
            .and_then(|meta| meta.source_last_modified);

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
                let day = utc_day(now);
                let snapshot_id = self.snapshots.record(
                    &NewSnapshot {
                        target: target.id.clone(),
                        kind,
                        fetched_at: now,
                        source_last_modified: payload.source_last_modified,
                        entry_count: payload.listings.len() as u64,
                        dropped_count: payload.dropped_count,
                        status: SnapshotStatus::Ok,
                    },
                    &payload.listings,
                    &payload.raw,
                )?;
                let grouped = group_by_item(&payload.listings);
                for (item_ref, listings) in &grouped {
                    let mut fold = self
                        .history
                        .load_fold(&target.id, item_ref, day)?
                        .unwrap_or_else(|| DayFold::new(item_ref.clone()));
                    fold.apply(listings);
                    self.history.save_fold(&target.id, item_ref, day, &fold)?;
                }
                // Fold state changed: memoized span metrics are stale.
                self.cache.invalidate(&target.id)?;

                // Sell-through bookkeeping: register sightings, then close
                // the ones absent from the newest snapshot (plan §4.2).
                self.sales
                    .observe(&target.id, &payload.listings, snapshot_id, now)?;
                let credits: Vec<SaleCredit> =
                    self.sales.close_missing(&target.id, snapshot_id, now)?;

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
}

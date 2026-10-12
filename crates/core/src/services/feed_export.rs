//! Feed export (tracker feed v2): gathers per-item span metrics and shapes
//! them into the frozen cross-addon contract (`AuctiMogger` `PLAN.md §2.1`).
//! Rendering and publication are outbound ports; this service owns the
//! selection and freshness rules.

use std::{
    collections::BTreeMap,
    sync::Arc,
};

use crate::{
    domain::{
        FeedEntry,
        FeedSnapshot,
        ItemRef,
        PayloadKind,
        Span,
        TargetId,
    },
    error::StoreError,
    ports::{
        ClockPort,
        PriceHistoryRepository,
        SnapshotRepository,
        WatchlistRepository,
    },
    services::metrics::MetricsService,
};

/// Feed build options.
#[derive(Clone, Debug)]
pub struct ExportOptions {
    /// The spans to include in every entry's metric map.
    pub spans: Vec<Span>,
    /// Slim mode: export only the target's watchlist items.
    pub watchlist_only: bool,
}

/// Builds feed snapshots from cached metrics and history.
pub struct FeedExportService {
    snapshots: Arc<dyn SnapshotRepository>,
    history: Arc<dyn PriceHistoryRepository>,
    watchlist: Arc<dyn WatchlistRepository>,
    metrics: Arc<MetricsService>,
    clock: Arc<dyn ClockPort>,
}

impl FeedExportService {
    /// Wires the service to its ports. `metrics` is shared with the query
    /// side so export hits the same cache.
    pub fn new(
        snapshots: Arc<dyn SnapshotRepository>,
        history: Arc<dyn PriceHistoryRepository>,
        watchlist: Arc<dyn WatchlistRepository>,
        metrics: Arc<MetricsService>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self { snapshots, history, watchlist, metrics, clock }
    }

    /// Builds the feed snapshot for a target. `generatedAt` is the newest
    /// source `Last-Modified` across the target's payload kinds (falling back
    /// to publish time when unknown), so the addon's 24-hour freshness gate
    /// reflects actual data age, not export time.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when snapshot metadata, the watchlist
    /// or a metric block cannot be read.
    pub fn build_feed(
        &self,
        target: &TargetId,
        options: &ExportOptions,
    ) -> Result<FeedSnapshot, StoreError> {
        let generated_at = self.freshness(target)?;
        let today = utc_day_of(self.clock.now());
        let window_days = options.spans.iter().map(Span::days).max().unwrap_or(1);

        let items: Vec<ItemRef> = if options.watchlist_only {
            self.watchlist.list(target)?.into_iter().map(|entry| entry.item).collect()
        } else {
            self.history.items_with_data(target, today - window_days + 1)?
        };

        let mut entries = Vec::new();
        for item in items {
            let mut metrics = BTreeMap::new();
            for span in &options.spans {
                if let Some(block) = self.metrics.block(target, &item, *span)? {
                    metrics.insert(*span, block);
                }
            }
            // Entries without any valid span would be rejected by the addon's
            // strict ingestor — omit them.
            if !metrics.is_empty() {
                entries.push(FeedEntry { item, metrics });
            }
        }
        entries.sort_by(|a, b| a.item.cmp(&b.item));
        Ok(FeedSnapshot { target: target.clone(), generated_at, entries })
    }

    fn freshness(&self, target: &TargetId) -> Result<i64, StoreError> {
        let mut newest = 0i64;
        for kind in [PayloadKind::Commodities, PayloadKind::Auctions] {
            if let Some(meta) = self.snapshots.latest(target, kind)? {
                if let Some(lm) = meta.source_last_modified {
                    newest = newest.max(lm);
                }
            }
        }
        if newest == 0 {
            Ok(self.clock.now())
        } else {
            Ok(newest)
        }
    }
}

fn utc_day_of(unix_ts: i64) -> i64 {
    unix_ts.div_euclid(86_400)
}

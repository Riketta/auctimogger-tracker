//! Watchlist report: one batched join of a target's watchlist to its current
//! market numbers — metric blocks per requested span, trailing sale rates and
//! per-day trend rows. The fuel behind the live watchlist page and the
//! `/api/watchlist/metrics` contract; both consumers share this service so
//! the JSON shape and the HTML stay computed from the same reads.

use std::sync::Arc;

use crate::{
    domain::{
        Category,
        ItemDayRow,
        ItemRef,
        MetricBlock,
        SaleRecency,
        Span,
        TargetId,
        WatchlistEntry,
    },
    error::StoreError,
    ports::{
        ClockPort,
        PriceHistoryRepository,
        SalesRepository,
        WatchlistRepository,
    },
    services::metrics::MetricsService,
};

/// One watchlist entry joined to its current market numbers.
pub struct WatchlistReportEntry {
    /// The watchlist entry itself (item, note, `added_at`).
    pub entry: WatchlistEntry,
    /// Metric blocks for the requested spans, positionally aligned;
    /// `None` = no data in the window.
    pub blocks: Vec<Option<MetricBlock>>,
    /// Trailing-window sale rate; `None` = no sales rows yet.
    pub sale_rate: Option<f64>,
    /// Ledger recency; `None` = no credited sale ever (the "stopped
    /// selling" signal behind the watchlist page and metrics API).
    pub recency: Option<SaleRecency>,
    /// Per-day aggregate rows over the trend window, ordered by day.
    pub trend: Vec<ItemDayRow>,
}

/// Builds whole-watchlist reports from batched reads.
pub struct WatchlistReportService {
    watchlist: Arc<dyn WatchlistRepository>,
    metrics: Arc<MetricsService>,
    sales: Arc<dyn SalesRepository>,
    history: Arc<dyn PriceHistoryRepository>,
    clock: Arc<dyn ClockPort>,
}

impl WatchlistReportService {
    /// Wires the report to the watchlist repository, the metrics service
    /// (batched block reads), sale rates, trend rows and the clock.
    pub fn new(
        watchlist: Arc<dyn WatchlistRepository>,
        metrics: Arc<MetricsService>,
        sales: Arc<dyn SalesRepository>,
        history: Arc<dyn PriceHistoryRepository>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self { watchlist, metrics, sales, history, clock }
    }

    /// Joins every entry of a target's watchlist to its metric blocks, sale
    /// rate and trend rows, all within one market category. `blocks[i]` of
    /// each entry corresponds to `spans[i]`; trend rows cover the last
    /// `trend_days` UTC days inclusive of today; the sale rate covers
    /// `sale_days`.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when any of the joined reads fails.
    pub fn report(
        &self,
        target: &TargetId,
        category: Category,
        spans: &[Span],
        sale_days: i64,
        trend_days: i64,
    ) -> Result<Vec<WatchlistReportEntry>, StoreError> {
        let entries = self.watchlist.list(target)?;
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let items: Vec<ItemRef> = entries.iter().map(|entry| entry.item.clone()).collect();
        let since_day = crate::domain::utc_day(self.clock.now()) - (trend_days - 1);
        // One batched read per span, then one sale-rate query for all items.
        // `per_span[s]` is aligned with `items`; the loop below transposes it
        // into per-entry span lists without indexing.
        let per_span = spans
            .iter()
            .map(|span| self.metrics.blocks(target, &items, category, *span))
            .collect::<Result<Vec<_>, _>>()?;
        let rates = self.sales.sale_rate_each(target, &items, sale_days, category)?;
        let recencies = self.sales.sale_recency_each(target, &items, category)?;
        let mut out = Vec::with_capacity(entries.len());
        for (i, entry) in entries.into_iter().enumerate() {
            let rate = rates.get(i).copied().flatten();
            let recency = recencies.get(i).copied().flatten();
            let blocks =
                per_span.iter().map(|spans_items| spans_items.get(i).cloned().flatten()).collect();
            let trend =
                self.history.item_day_rows_for_item(target, &entry.item, since_day, category)?;
            out.push(WatchlistReportEntry { entry, blocks, sale_rate: rate, recency, trend });
        }
        Ok(out)
    }
}

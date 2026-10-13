//! Span metrics queries: cache-aside reads over the day-fold history.

use std::{
    collections::BTreeMap,
    sync::Arc,
};

use crate::{
    domain::{
        utc_day,
        ItemDayHistogram,
        ItemRef,
        MetricBlock,
        PriceBucket,
        Span,
        TargetId,
    },
    error::StoreError,
    metrics::block_from_histograms,
    ports::{
        ClockPort,
        MetricsCacheRepository,
        PriceHistoryRepository,
        SalesRepository,
    },
};

/// How long a memoized block may serve queries before recomputation. Ingest
/// invalidates the whole target cache immediately; this bound only covers
/// intra-day fold growth.
const MAX_CACHE_AGE_SECONDS: i64 = 300;

/// Window for the sell-through rate attached to every block.
const SALE_RATE_WINDOW_DAYS: i64 = 14;

/// Serves per-span metric blocks with cache-aside recomputation.
pub struct MetricsService {
    history: Arc<dyn PriceHistoryRepository>,
    cache: Arc<dyn MetricsCacheRepository>,
    sales: Arc<dyn SalesRepository>,
    clock: Arc<dyn ClockPort>,
}

impl MetricsService {
    /// Wires the service to history, cache, sales and clock ports.
    pub fn new(
        history: Arc<dyn PriceHistoryRepository>,
        cache: Arc<dyn MetricsCacheRepository>,
        sales: Arc<dyn SalesRepository>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self { history, cache, sales, clock }
    }

    /// Computes (or serves from cache) the metric block for one item/span.
    /// `None` = no data in the window.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when folds, the cache or the sale rate
    /// cannot be read, or a freshly computed block cannot be cached.
    pub fn block(
        &self,
        target: &TargetId,
        item: &ItemRef,
        span: Span,
    ) -> Result<Option<MetricBlock>, StoreError> {
        let now = self.clock.now();
        if let Some(cached) = self.cache.get(target, item, span)? {
            if now - cached.computed_at <= MAX_CACHE_AGE_SECONDS {
                return Ok(Some(cached.block));
            }
        }

        let today = utc_day(now);
        let mut histograms = Vec::new();
        for offset in 0..span.days() {
            let day = today - offset;
            if let Some(fold) = self.history.load_fold(target, item, day)? {
                let histogram = fold.histogram();
                if histogram.listing_count > 0 {
                    histograms.push(histogram);
                }
            }
        }
        let sale_rate = self.sales.sale_rate(target, item, SALE_RATE_WINDOW_DAYS)?;
        let block = block_from_histograms(&histograms, sale_rate);
        if let Some(block) = &block {
            self.cache.put(target, item, span, block, now)?;
        }
        Ok(block)
    }

    /// Computes the metric block for several item identities merged into one
    /// population (pet variants of one species share their market). Histogram
    /// buckets are summed before the (parity-frozen) summarizer runs, and the
    /// sale rate aggregates credited/listings across all identities. Merged
    /// blocks bypass the per-item cache: exports run on a slow cadence and
    /// the cache schema is keyed per item.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when folds or sale rates cannot be read.
    pub fn block_merged(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        span: Span,
    ) -> Result<Option<MetricBlock>, StoreError> {
        if items.is_empty() {
            return Ok(None);
        }
        let today = utc_day(self.clock.now());
        let mut merged =
            ItemDayHistogram { buckets: BTreeMap::new(), listing_count: 0, qty_sum: 0 };
        for item in items {
            for offset in 0..span.days() {
                let day = today - offset;
                if let Some(fold) = self.history.load_fold(target, item, day)? {
                    let histogram = fold.histogram();
                    if histogram.listing_count == 0 {
                        continue;
                    }
                    for (price, bucket) in histogram.buckets {
                        let merged_bucket = merged
                            .buckets
                            .entry(price)
                            .or_insert(PriceBucket { listings: 0, qty: 0 });
                        merged_bucket.listings += bucket.listings;
                        merged_bucket.qty += bucket.qty;
                    }
                    merged.listing_count += histogram.listing_count;
                    merged.qty_sum += histogram.qty_sum;
                }
            }
        }
        if merged.listing_count == 0 {
            return Ok(None);
        }
        let sale_rate = self.sales.sale_rate_multi(target, items, SALE_RATE_WINDOW_DAYS)?;
        Ok(block_from_histograms(std::slice::from_ref(&merged), sale_rate))
    }
}

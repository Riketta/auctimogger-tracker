//! Span metrics queries: cache-aside reads over the day-fold history.

use std::sync::Arc;

use crate::domain::{utc_day, ItemRef, MetricBlock, Span, TargetId};
use crate::error::StoreError;
use crate::metrics::block_from_histograms;
use crate::ports::{ClockPort, MetricsCacheRepository, PriceHistoryRepository, SalesRepository};

/// How long a memoized block may serve queries before recomputation. Ingest
/// invalidates the whole target cache immediately; this bound only covers
/// intra-day fold growth.
const MAX_CACHE_AGE_SECONDS: i64 = 300;

/// Window for the sell-through rate attached to every block.
const SALE_RATE_WINDOW_DAYS: i64 = 14;

pub struct MetricsService {
    history: Arc<dyn PriceHistoryRepository>,
    cache: Arc<dyn MetricsCacheRepository>,
    sales: Arc<dyn SalesRepository>,
    clock: Arc<dyn ClockPort>,
}

impl MetricsService {
    pub fn new(
        history: Arc<dyn PriceHistoryRepository>,
        cache: Arc<dyn MetricsCacheRepository>,
        sales: Arc<dyn SalesRepository>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self {
            history,
            cache,
            sales,
            clock,
        }
    }

    /// Computes (or serves from cache) the metric block for one item/span.
    /// `None` = no data in the window.
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
}

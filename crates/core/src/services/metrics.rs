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
/// invalidates the touched items' cache entries immediately (full sweep on a
/// UTC day roll); this bound covers everything that escapes those rules —
/// above all intra-day fold growth for untouched items.
const MAX_CACHE_AGE_SECONDS: i64 = 300;

/// Window for the sell-through rate attached to every block.
const SALE_RATE_WINDOW_DAYS: i64 = 14;

/// Identities per bulk fold/sale pass. Bounds the fold state one batch
/// materializes (500 items × up to 30 day-folds) while keeping a market-scale
/// recomputation at a handful of chunked queries per span.
const BLOCK_CHUNK: usize = 500;

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
        Ok(self.blocks(target, std::slice::from_ref(item), span)?.into_iter().next().flatten())
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
        Ok(self
            .blocks_merged(target, std::slice::from_ref(&items), span)?
            .into_iter()
            .next()
            .flatten())
    }

    /// Cached-first metric blocks for several identities of one span,
    /// positionally aligned with `items`; missing or expired entries are
    /// recomputed in bounded batches — one bulk fold read and one bulk sale
    /// rate read per chunk instead of per-item-per-day queries — and
    /// re-cached through one bulk write.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cache or any recomputation
    /// fails.
    pub fn blocks(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        span: Span,
    ) -> Result<Vec<Option<MetricBlock>>, StoreError> {
        let now = self.clock.now();
        let mut out = Vec::with_capacity(items.len());
        let pairs: Vec<(ItemRef, Option<crate::domain::CachedBlock>)> =
            items.iter().cloned().zip(self.cache.get_many(target, span, items)?).collect();
        for chunk in pairs.chunks(BLOCK_CHUNK) {
            let mut row = Vec::with_capacity(chunk.len());
            let mut cold = Vec::new();
            for (item, cached) in chunk {
                if cached
                    .as_ref()
                    .is_some_and(|cached| now - cached.computed_at <= MAX_CACHE_AGE_SECONDS)
                {
                    row.push(cached.as_ref().map(|cached| cached.block.clone()));
                } else {
                    row.push(None);
                    cold.push(item.clone());
                }
            }
            if !cold.is_empty() {
                let computed = self.compute_blocks(target, &cold, span, now)?;
                let mut computed_iter = computed.into_iter();
                for slot in &mut row {
                    if slot.is_none() {
                        *slot = computed_iter.next().flatten();
                    }
                }
            }
            out.extend(row);
        }
        Ok(out)
    }

    /// Merged-population metric blocks for several groups of one span,
    /// positionally aligned with `groups`; each group's members are folded
    /// into one histogram population and their sale credits pooled. Bulk
    /// twin of [`MetricsService::block_merged`]: fold state is loaded in
    /// bounded batches across all groups instead of per member per day.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when folds or sale rates cannot be read.
    pub fn blocks_merged(
        &self,
        target: &TargetId,
        groups: &[&[ItemRef]],
        span: Span,
    ) -> Result<Vec<Option<MetricBlock>>, StoreError> {
        let now = self.clock.now();
        let mut out = Vec::with_capacity(groups.len());
        let mut batch: Vec<&[ItemRef]> = Vec::new();
        let mut batch_members = 0usize;
        for group in groups {
            batch.push(group);
            batch_members += group.len();
            if batch_members >= BLOCK_CHUNK {
                out.extend(self.compute_merged_batch(target, &batch, span, now)?);
                batch.clear();
                batch_members = 0;
            }
        }
        if !batch.is_empty() {
            out.extend(self.compute_merged_batch(target, &batch, span, now)?);
        }
        Ok(out)
    }

    /// Recomputes one chunk of cold identities: bulk folds in the span
    /// window, per-identity sale rates, then the (parity-frozen) summarizer;
    /// non-empty results are written back through one bulk cache put.
    fn compute_blocks(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        span: Span,
        now: i64,
    ) -> Result<Vec<Option<MetricBlock>>, StoreError> {
        let today = utc_day(now);
        let folds = self.history.load_folds(target, items, today - span.days() + 1, today)?;
        let rates = self.sales.sale_rate_each(target, items, SALE_RATE_WINDOW_DAYS)?;
        let mut blocks = Vec::with_capacity(items.len());
        for (per_item, rate) in folds.into_iter().zip(rates) {
            let mut histograms = Vec::new();
            for (_, fold) in per_item {
                let histogram = fold.histogram();
                if histogram.listing_count > 0 {
                    histograms.push(histogram);
                }
            }
            blocks.push(block_from_histograms(&histograms, rate));
        }
        self.cache.put_many(target, span, items, &blocks, now)?;
        Ok(blocks)
    }

    /// Merges one bounded batch of groups: the batch's members share a single
    /// bulk fold read; each group's population is then summed in memory and
    /// its sale rate pooled exactly as [`MetricsService::block_merged`] does.
    fn compute_merged_batch(
        &self,
        target: &TargetId,
        groups: &[&[ItemRef]],
        span: Span,
        now: i64,
    ) -> Result<Vec<Option<MetricBlock>>, StoreError> {
        let today = utc_day(now);
        let flat: Vec<ItemRef> = groups.iter().flat_map(|group| group.iter()).cloned().collect();
        if flat.is_empty() {
            return Ok(vec![None; groups.len()]);
        }
        let folds = self.history.load_folds(target, &flat, today - span.days() + 1, today)?;
        let mut per_member = folds.into_iter();
        let mut out = Vec::with_capacity(groups.len());
        for group in groups {
            let mut merged =
                ItemDayHistogram { buckets: BTreeMap::new(), listing_count: 0, qty_sum: 0 };
            // One fold batch per member: `per_member` is positionally
            // aligned with the flattened group members.
            for _ in 0..group.len() {
                if let Some(per_day) = per_member.next() {
                    for (_, fold) in per_day {
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
                out.push(None);
                continue;
            }
            let sale_rate = self.sales.sale_rate_multi(target, group, SALE_RATE_WINDOW_DAYS)?;
            out.push(block_from_histograms(std::slice::from_ref(&merged), sale_rate));
        }
        Ok(out)
    }
}

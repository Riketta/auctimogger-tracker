//! Opportunity ranking and market overview ("what should I actually snipe"
//! and "how is the market moving"). All scoring components are transparent
//! and 0..1-normalized; the weighted score is for RANKING only — deal gates
//! never read it (they use exact metric blocks).

use std::{
    collections::{
        BTreeMap,
        HashMap,
    },
    sync::{
        Arc,
        Mutex,
        MutexGuard,
    },
};

use crate::{
    config::TrackerConfig,
    domain::{
        utc_day,
        Category,
        ItemDayRow,
        ItemRef,
        SaleRecency,
        TargetId,
    },
    error::StoreError,
    ports::{
        ClockPort,
        PriceHistoryRepository,
        SalesRepository,
    },
};

/// Scan window for ranking and overview (days).
const SCAN_WINDOW_DAYS: i64 = 7;
/// Sale-rate window backing the reliability component (days).
const SALE_RATE_WINDOW_DAYS: i64 = 14;
/// How long a prepared scan stays fresh before the scheduler refreshes it;
/// stale snapshots keep serving until the refreshed one lands.
pub const SCAN_TTL_SECONDS: i64 = 300;
/// Hard staleness backstop: past this age a request recomputes inline even
/// though a snapshot exists, so ranking self-heals (as it did before the
/// background scheduler existed) if that thread ever dies or persistently
/// fails. Six TTLs keeps this far above normal refresh jitter.
const SCAN_MAX_STALENESS_SECONDS: i64 = SCAN_TTL_SECONDS * 6;
/// Minimum age a cached scan must reach before [`OpportunityService::refresh`]
/// recomputes it, deduplicating back-to-back post-cycle refreshes (commodity
/// and auction cycles can land in the same scheduler pass) and races with a
/// manual `/api/fetch` refresh.
const SCAN_REFRESH_DEBOUNCE_SECONDS: i64 = 30;
/// Normalization anchor for the churn component: this many unique listings
/// per day scores as a full 1.0 (config could override later).
const CHURN_ANCHOR: f64 = 50.0;

/// One ranked snipe candidate. All components are 0..1-normalized except the
/// raw price fields; `score` is ranking-only and never gates deals. Rows are
/// per market category: an item trading in both auction houses ranks
/// independently in each, on that category's evidence alone.
#[derive(Clone, Debug, PartialEq)]
pub struct Opportunity {
    /// The ranked item or pet.
    pub item: ItemRef,
    /// Which market the row ranks in.
    pub category: Category,
    /// Weighted median of the window's per-day medians (each day weighted by
    /// its listing count; copper). An approximation of a true count-weighted
    /// median — chosen for parity with the addon's math.
    pub median7: i64,
    /// Cheapest unit price of the newest day in the window (copper).
    pub floor_now: i64,
    /// `1 - p05(newest day) / median7`, clamped to [0, 1] for scoring — the
    /// robust floor keeps one outlier listing from faking a deep discount.
    pub discount: f64,
    /// 14-day sale rate: share of vanished listings credited as sold.
    pub reliability: f64,
    /// Average unique listings per day across the whole window — days the
    /// item did not appear count as zero, so a single-day spike cannot read
    /// as sustained volume.
    pub churn_per_day: f64,
    /// Churn component: `churn_per_day` normalized against the anchor,
    /// capped at 1.
    pub churn_score: f64,
    /// `1 - min(1, mad7 / median7)`: how much the median itself can be trusted.
    pub stability: f64,
    /// Unique listings seen over the window (evidence size).
    pub samples7: i64,
    /// `min(1, samples7 / min_samples)` — thin evidence means thin trust.
    pub confidence: f64,
    /// Weighted sum of the components (config weights).
    pub score: f64,
    /// Whole days since the ledger last credited a sale; `None` = no sale
    /// ever credited (a young ledger is not evidence of a stalled market).
    pub days_since_last_sale: Option<i64>,
    /// Net listings that closed after the last credited sale — the "stopped
    /// selling" churn signal riding with `days_since_last_sale`.
    pub closures_since_last_sale: Option<i64>,
}

/// Row ordering for [`OpportunityService::opportunities`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpportunitySort {
    /// Weighted score, descending (the default).
    Score,
    /// Stalest first: `days_since_last_sale` descending, never-sold rows
    /// last (no sale evidence is not evidence of a long quiet streak), ties
    /// broken by score descending. The value-add surfacing of the TSM
    /// `NumExpires` lesson.
    LastSale,
}

impl OpportunitySort {
    /// Lowercase wire spelling (query param).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            OpportunitySort::Score => "score",
            OpportunitySort::LastSale => "last_sale",
        }
    }

    /// Parses the lowercase spelling; `None` for unknown values.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "score" => Some(OpportunitySort::Score),
            "last_sale" => Some(OpportunitySort::LastSale),
            _ => None,
        }
    }
}

/// One overview row: an item's current median vs its window median, within
/// one market category.
#[derive(Clone, Debug, PartialEq)]
pub struct MoverRow {
    /// The item or pet that moved.
    pub item: ItemRef,
    /// Which market the row belongs to.
    pub category: Category,
    /// Median unit price of the newest day in the window (copper).
    pub median_now: i64,
    /// Count-weighted median unit price over the window (copper).
    pub median_prev: f64,
    /// `median_now / median_prev - 1` (fraction, may be negative).
    pub change: f64,
    /// Average unique listings per day across the whole window (zero days
    /// count as zero).
    pub churn_per_day: f64,
}

/// The market overview response: movers split by direction plus the
/// most-active items by churn.
#[derive(Clone, Debug, PartialEq)]
pub struct OverviewData {
    /// Items whose median rose vs the window median.
    pub movers_up: Vec<MoverRow>,
    /// Items whose median fell vs the window median.
    pub movers_down: Vec<MoverRow>,
    /// Highest-churn items over the window.
    pub most_active: Vec<MoverRow>,
}

/// Immutable result of one full-market scan, prepared by
/// [`OpportunityService::refresh`] (scheduler) or the cold-start inline
/// fallback and shared across requests until the next refresh replaces it.
#[derive(Clone, Debug)]
pub struct ScanSnapshot {
    /// When the scan was computed (unix seconds).
    pub computed_at: i64,
    /// How many distinct items the scan covered.
    pub items: usize,
    rows: Arc<Vec<ItemScan>>,
    /// 14-day sale rates positionally aligned with `rows` (`None` = the item
    /// has no sales evidence in the window).
    rates: Arc<Vec<Option<f64>>>,
    /// Ledger recencies positionally aligned with `rows` (`None` = no
    /// credited sale ever).
    recencies: Arc<Vec<Option<SaleRecency>>>,
}

/// Per-item intermediate scan row (pre-filter, pre-ranking).
#[derive(Clone, Debug)]
struct ItemScan {
    item: ItemRef,
    samples7: i64,
    churn_per_day: f64,
    floor_now: i64,
    /// Newest day's p05 — the floor discount is scored against.
    floor_p05: i64,
    median_now: i64,
    median7: f64,
    mad7: f64,
}

/// Cached scan per (target, category): computed-at stamp plus the shared
/// snapshot. Keyed by both so alternating requests and categories don't
/// evict each other.
type ScanCache = HashMap<(TargetId, Category), (i64, Arc<ScanSnapshot>)>;

/// Builds ranked opportunities and the market overview from prepared
/// full-market scans: the scheduler refreshes them in the background and
/// requests only read the snapshot; a completely cold cache falls back to an
/// inline compute.
pub struct OpportunityService {
    history: Arc<dyn PriceHistoryRepository>,
    sales: Arc<dyn SalesRepository>,
    clock: Arc<dyn ClockPort>,
    weights: crate::config::ScoreWeights,
    min_samples: u32,
    cache: Mutex<ScanCache>,
}

impl OpportunityService {
    /// Creates the service, reading scoring weights and the evidence floor
    /// from `[opportunities]` in the config.
    pub fn new(
        history: Arc<dyn PriceHistoryRepository>,
        sales: Arc<dyn SalesRepository>,
        clock: Arc<dyn ClockPort>,
        config: &TrackerConfig,
    ) -> Self {
        Self {
            history,
            sales,
            clock,
            weights: config.opportunities.weights.clone(),
            min_samples: config.opportunities.min_samples,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Ranked snipe candidates: `min_samples` evidence floor, optional churn
    /// floor, sorted by score descending. Served entirely from the prepared
    /// scan — reliability ships with the snapshot, so steady-state requests
    /// touch no store.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cold-start inline scan cannot
    /// read history rows or sale rates, or the scan cache is poisoned.
    pub fn opportunities(
        &self,
        target: &TargetId,
        category: Category,
        min_churn_per_day: f64,
        limit: usize,
        sort: OpportunitySort,
    ) -> Result<Vec<Opportunity>, StoreError> {
        let scan = self.scan(target, category)?;
        let mut out: Vec<Opportunity> = Vec::new();
        // Sale rates and recencies come with the prepared snapshot,
        // positionally aligned with the rows; requests never re-query the
        // sales store.
        for ((row, rate), recency) in
            scan.rows.iter().zip(scan.rates.iter()).zip(scan.recencies.iter())
        {
            let Some(opportunity) =
                self.opportunity_for(row, rate.unwrap_or(0.0), *recency, category)
            else {
                continue;
            };
            if opportunity.samples7 >= i64::from(self.min_samples)
                && opportunity.churn_per_day >= min_churn_per_day
            {
                out.push(opportunity);
            }
        }
        match sort {
            OpportunitySort::Score => apply_sort(&mut out, OpportunitySort::Score),
            OpportunitySort::LastSale => apply_sort(&mut out, OpportunitySort::LastSale),
        }
        out.truncate(limit.max(1));
        Ok(out)
    }

    /// Market overview: movers vs their window median, plus most-active.
    /// Served from the prepared scan; nothing is read from the store unless
    /// the cache is completely cold.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cold-start inline scan cannot
    /// read history rows or sale rates, or the scan cache is poisoned.
    pub fn overview(
        &self,
        target: &TargetId,
        category: Category,
        limit: usize,
    ) -> Result<OverviewData, StoreError> {
        let scan = self.scan(target, category)?;
        let mut movers: Vec<MoverRow> = scan
            .rows
            .iter()
            .filter_map(|row| Self::mover_for(row, category))
            .filter(|m| m.churn_per_day >= 1.0 && m.median_prev > 0.0)
            .collect();
        movers.sort_by(|a, b| b.change.partial_cmp(&a.change).unwrap_or(std::cmp::Ordering::Equal));
        let movers_up: Vec<MoverRow> =
            movers.iter().filter(|m| m.change > 0.0).take(limit).cloned().collect();
        let movers_down: Vec<MoverRow> =
            movers.iter().rev().filter(|m| m.change < 0.0).take(limit).cloned().collect();
        let mut most_active: Vec<MoverRow> = movers.clone();
        most_active.sort_by(|a, b| {
            b.churn_per_day.partial_cmp(&a.churn_per_day).unwrap_or(std::cmp::Ordering::Equal)
        });
        most_active.truncate(limit);
        Ok(OverviewData { movers_up, movers_down, most_active })
    }

    /// Recomputes the full scan artifact for one target — history read,
    /// per-item aggregation, and one batched sale-rate lookup — and stores it
    /// as the snapshot requests are served from. Meant for the background
    /// scheduler: after startup, after each successful ingest cycle, and when
    /// a prepared scan outlives [`SCAN_TTL_SECONDS`]. Skips all work while the
    /// cached snapshot is younger than [`SCAN_REFRESH_DEBOUNCE_SECONDS`], so a
    /// post-cycle refresh and the TTL tick never double-compute.
    ///
    /// Returns the number of items the scan covered (the cached count when
    /// skipped).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when history rows or sale rates cannot be
    /// read, or the scan cache is poisoned.
    pub fn refresh(&self, target: &TargetId, category: Category) -> Result<usize, StoreError> {
        let now = self.clock.now();
        {
            let cache = self.scan_cache()?;
            if let Some((at, snapshot)) = cache.get(&(target.clone(), category)) {
                if now - at <= SCAN_REFRESH_DEBOUNCE_SECONDS {
                    return Ok(snapshot.items);
                }
            }
        }
        let snapshot = Arc::new(self.compute_scan(target, category)?);
        let covered = snapshot.items;
        self.scan_cache()?
            .insert((target.clone(), category), (snapshot.computed_at, Arc::clone(&snapshot)));
        Ok(covered)
    }

    /// Age of the prepared snapshot in seconds (via the injected clock);
    /// `None` when nothing has been prepared for the target's category yet.
    #[must_use]
    pub fn snapshot_age_secs(&self, target: &TargetId, category: Category) -> Option<i64> {
        let cache = self.scan_cache().ok()?;
        let (computed_at, _) = cache.get(&(target.clone(), category))?;
        Some(self.clock.now() - computed_at)
    }

    /// Serves the prepared scan for a target: any snapshot younger than
    /// [`SCAN_MAX_STALENESS_SECONDS`] is returned as-is (stale-while-
    /// revalidate — the scheduler keeps refreshing it in the background, so
    /// staleness is bounded by the refresh cadence, not by request traffic).
    /// Only a completely cold cache (e.g. the first request right after
    /// startup, before the scheduler's initial refresh) or a snapshot past
    /// the hard backstop (the scheduler is dead or persistently failing)
    /// computes inline as a fallback.
    fn scan(&self, target: &TargetId, category: Category) -> Result<Arc<ScanSnapshot>, StoreError> {
        let now = self.clock.now();
        {
            let cache = self.scan_cache()?;
            if let Some((at, snapshot)) = cache.get(&(target.clone(), category)) {
                if now - at <= SCAN_MAX_STALENESS_SECONDS {
                    return Ok(Arc::clone(snapshot));
                }
            }
        }
        let snapshot = Arc::new(self.compute_scan(target, category)?);
        self.scan_cache()?
            .insert((target.clone(), category), (snapshot.computed_at, Arc::clone(&snapshot)));
        Ok(snapshot)
    }

    /// Builds one full-market scan artifact in a single store pass: day rows,
    /// per-item aggregation, then one batched sale-rate lookup aligned with
    /// the aggregated rows. This is the whole per-target cost, paid by the
    /// scheduler (or the cold-start fallback) instead of request threads.
    fn compute_scan(
        &self,
        target: &TargetId,
        category: Category,
    ) -> Result<ScanSnapshot, StoreError> {
        let now = self.clock.now();
        let today = utc_day(now);
        let rows = self.history.item_day_rows(target, today - SCAN_WINDOW_DAYS + 1, category)?;

        let mut by_item: BTreeMap<ItemRef, Vec<ItemDayRow>> = BTreeMap::new();
        for row in rows {
            by_item.entry(row.item.clone()).or_default().push(row.clone());
        }

        let mut scans: Vec<ItemScan> = Vec::with_capacity(by_item.len());
        for (item, day_rows) in &by_item {
            scans.push(Self::scan_item(item, day_rows));
        }
        // BTreeMap iteration is sorted, so the batched rates line up
        // positionally with the stored rows by construction.
        let items: Vec<ItemRef> = scans.iter().map(|scan| scan.item.clone()).collect();
        let rates = self.sales.sale_rate_each(target, &items, SALE_RATE_WINDOW_DAYS, category)?;
        let recencies = self.sales.sale_recency_each(target, &items, category)?;
        debug_assert_eq!(
            scans.len(),
            rates.len(),
            "sale_rate_each must stay positionally aligned with the scan rows"
        );
        debug_assert_eq!(
            scans.len(),
            recencies.len(),
            "sale_recency_each must stay positionally aligned with the scan rows"
        );
        Ok(ScanSnapshot {
            computed_at: now,
            items: scans.len(),
            rows: Arc::new(scans),
            rates: Arc::new(rates),
            recencies: Arc::new(recencies),
        })
    }

    /// Locks the scan cache. A poisoned mutex (another thread panicked while
    /// holding it) is reported as a backend error instead of panicking here.
    fn scan_cache(&self) -> Result<MutexGuard<'_, ScanCache>, StoreError> {
        self.cache.lock().map_err(|_| StoreError::Backend("opportunity scan cache poisoned".into()))
    }

    /// Aggregates one item's day rows. Listing counts are narrowed u64 → i64
    /// for the ranking math; real counts never approach 2^63, so saturating
    /// at `i64::MAX` on absurd input is harmless here.
    // Counts/prices feed f64 scoring components; auction-scale magnitudes sit
    // far inside f64's exact-integer range, so precision loss is intended.
    #[allow(clippy::cast_precision_loss)]
    fn scan_item(item: &ItemRef, day_rows: &[ItemDayRow]) -> ItemScan {
        let mut samples7 = 0i64;
        let mut churn_sum = 0.0f64;
        let mut weighted: Vec<(i64, i64)> = Vec::new(); // (median, listing_count)
        let mut mad_weight_sum = 0i64;
        let mut mad_weighted = 0.0f64;
        let mut newest: Option<&ItemDayRow> = None;
        for row in day_rows {
            let count = i64::try_from(row.listing_count).unwrap_or(i64::MAX);
            // Saturating: one absurd listing_count must not overflow the
            // whole sum (debug builds panic on overflow).
            samples7 = samples7.saturating_add(count);
            churn_sum += row.listing_count as f64;
            weighted.push((row.median, count));
            mad_weight_sum = mad_weight_sum.saturating_add(count);
            mad_weighted += row.mad as f64 * row.listing_count as f64;
            match newest {
                Some(latest) if latest.day >= row.day => {}
                _ => newest = Some(row),
            }
        }
        weighted.sort_by_key(|(median, _)| *median);
        let median7 = weighted_median(&weighted);
        let mad7 = if mad_weight_sum > 0 { mad_weighted / mad_weight_sum as f64 } else { 0.0 };
        let (floor_now, floor_p05, median_now) =
            newest.map_or((0, 0, 0), |row| (row.min, row.p05, row.median));
        // Window-denominator churn: days without a fold count as zero, so a
        // single busy day reads as that day's count spread over the window,
        // not as sustained per-day volume (the docs and the `min_churn`
        // filter both describe window semantics).
        #[allow(clippy::cast_precision_loss)] // const 7 is exact in f64
        let churn_per_day = churn_sum / SCAN_WINDOW_DAYS as f64;
        ItemScan {
            item: item.clone(),
            samples7,
            churn_per_day,
            floor_now,
            floor_p05,
            median_now,
            median7,
            mad7,
        }
    }

    /// Builds one opportunity from a scan row. `Ok(None)` = the row is not
    /// rankable (no price evidence). `reliability` comes from the sale rates
    /// prepared with the snapshot; a failing sales store surfaces while the
    /// snapshot is computed (in `refresh` or the cold-start fallback), never
    /// per request.
    fn opportunity_for(
        &self,
        scan: &ItemScan,
        reliability: f64,
        recency: Option<SaleRecency>,
        category: Category,
    ) -> Option<Opportunity> {
        if scan.median7 <= 0.0 || scan.floor_now <= 0 {
            return None;
        }
        // Discount is rounded to 3 decimals for feed/UI stability. Scored
        // against the newest day's p05, not the raw min: one outlier listing
        // cannot fake a deep discount, while days with ≤ 20 listings
        // degenerate p05 → min and keep full snipe sensitivity.
        #[allow(clippy::cast_precision_loss)]
        let discount = ((1.0 - scan.floor_p05 as f64 / scan.median7).clamp(0.0, 1.0) * 1000.0)
            .round()
            / 1000.0;
        let churn_score = (scan.churn_per_day / CHURN_ANCHOR).min(1.0);
        let stability = 1.0 - (scan.mad7 / scan.median7).min(1.0);
        // Confidence compares evidence against the u32 floor; f64 is exact at
        // these magnitudes.
        #[allow(clippy::cast_precision_loss)]
        let confidence = (scan.samples7 as f64 / f64::from(self.min_samples)).min(1.0);
        let score = self.weights.discount * discount
            + self.weights.sale * reliability
            + self.weights.churn * churn_score
            + self.weights.stability * stability.clamp(0.0, 1.0);
        // median7 is a half-precision weighted mean; the published field is
        // the floored integer median, matching the addon's display rule.
        #[allow(clippy::cast_possible_truncation)]
        let median7 = scan.median7 as i64;
        let today = utc_day(self.clock.now());
        Some(Opportunity {
            item: scan.item.clone(),
            category,
            median7,
            floor_now: scan.floor_now,
            discount,
            reliability,
            churn_per_day: scan.churn_per_day,
            churn_score,
            stability: stability.clamp(0.0, 1.0),
            samples7: scan.samples7,
            confidence,
            score,
            days_since_last_sale: recency.map(|recency| today - recency.last_sale_day),
            closures_since_last_sale: recency.map(|recency| recency.closures_since),
        })
    }

    fn mover_for(scan: &ItemScan, category: Category) -> Option<MoverRow> {
        if scan.median_now <= 0 || scan.median7 <= 0.0 {
            return None;
        }
        // Relative change only — magnitudes cancel, precision loss is fine.
        #[allow(clippy::cast_precision_loss)]
        let change = scan.median_now as f64 / scan.median7 - 1.0;
        Some(MoverRow {
            item: scan.item.clone(),
            category,
            median_now: scan.median_now,
            median_prev: scan.median7,
            change,
            churn_per_day: scan.churn_per_day,
        })
    }
}

/// Applies one row ordering in place. `LastSale` sorts stalest first, ranks
/// never-sold identities last (no sale evidence is not evidence of a long
/// quiet streak) and breaks ties by score descending.
fn apply_sort(out: &mut [Opportunity], sort: OpportunitySort) {
    match sort {
        OpportunitySort::Score => {
            out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        }
        OpportunitySort::LastSale => {
            out.sort_by(|a, b| {
                b.days_since_last_sale.cmp(&a.days_since_last_sale).then_with(|| {
                    b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)
                })
            });
        }
    }
}

/// Count-weighted median over per-day medians (approximation for ranking —
/// deal gates always use the exact blocks from the metrics engine).
// Copper prices go into ranking math only; f64 keeps 2^53 integers exact.
#[allow(clippy::cast_precision_loss)]
fn weighted_median(pairs: &[(i64, i64)]) -> f64 {
    // Saturating fold: corrupt/absurd counts must not overflow the total.
    let total: i64 =
        pairs.iter().map(|(_, count)| count).fold(0i64, |acc, count| acc.saturating_add(*count));
    if total == 0 {
        return 0.0;
    }
    let mid_floor = total.saturating_add(1) / 2;
    let mid_ceil = total / 2 + 1;
    let value_at = |rank: i64| -> i64 {
        let mut seen = 0i64;
        for (median, count) in pairs {
            seen = seen.saturating_add(*count);
            if seen >= rank {
                return *median;
            }
        }
        pairs.last().map_or(0, |(median, _)| *median)
    };
    let low = value_at(mid_floor);
    let high = value_at(mid_ceil);
    // Sum in i128 first: `low + high` can overflow i64, and the cast keeps
    // the .5 fraction the old float division produced.
    ((i128::from(low) + i128::from(high)) as f64) / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_median_matches_nearest_rank_pair_averaging() {
        // counts: 100@100, 1@900 → total 101; floor rank 51 → 100; ceil rank 51 → 100
        assert_eq!(weighted_median(&[(100, 100), (900, 1)]), 100.0);
        // counts: 1@100, 1@300 → total 2; ranks 1 and 2 → (100+300)/2 = 200
        assert_eq!(weighted_median(&[(100, 1), (300, 1)]), 200.0);
        assert_eq!(weighted_median(&[]), 0.0);
    }

    /// `last_sale` ordering: stalest first, never-sold last, ties by score.
    #[test]
    #[allow(clippy::indexing_slicing)] // fixed synthetic rows, not hostile data
    fn last_sale_sort_orders_stalest_first_and_never_sold_last() {
        // score is the tiebreaker only; the fabricated rows are otherwise
        // identical shells.
        let row = |score: f64, days: Option<i64>| Opportunity {
            item: ItemRef::Item { item_id: 1 },
            category: Category::Commodity,
            median7: 100,
            floor_now: 100,
            discount: 0.0,
            reliability: 0.0,
            churn_per_day: 0.0,
            churn_score: 0.0,
            stability: 0.0,
            samples7: 10,
            confidence: 1.0,
            score,
            days_since_last_sale: days,
            closures_since_last_sale: None,
        };
        let mut rows = vec![
            row(0.9, Some(2)),  // freshest
            row(0.1, None),     // never sold
            row(0.5, Some(30)), // stalest, ties with the next by score
            row(0.7, Some(30)), // stalest, higher score
        ];
        apply_sort(&mut rows, OpportunitySort::LastSale);
        assert_eq!(rows[0].days_since_last_sale, Some(30));
        assert_eq!(rows[0].score, 0.7, "tie on staleness breaks by score");
        assert_eq!(rows[1].days_since_last_sale, Some(30));
        assert_eq!(rows[1].score, 0.5);
        assert_eq!(rows[2].days_since_last_sale, Some(2));
        assert_eq!(rows[3].days_since_last_sale, None, "never sold ranks last");
    }

    #[test]
    fn scan_item_picks_the_newest_day_for_the_floor() {
        let rows = vec![
            ItemDayRow {
                category: Category::Commodity,
                item: item(1),
                day: 7,
                listing_count: 900,
                qty_sum: 9_000,
                min: 45_000,
                p05: 45_000,
                median: 90_000,
                mad: 500,
            },
            ItemDayRow {
                category: Category::Commodity,
                item: item(1),
                day: 8,
                listing_count: 1_100,
                qty_sum: 11_000,
                min: 55_000,
                p05: 55_000,
                median: 110_000,
                mad: 600,
            },
        ];
        let scan = OpportunityService::scan_item(&item(1), &rows);
        assert_eq!(scan.floor_now, 55_000);
        assert_eq!(scan.floor_p05, 55_000);
        assert_eq!(scan.samples7, 2_000);
        // Window denominator: 2 000 listings across a 7-day window, even
        // though only two of those days have folds.
        assert!((scan.churn_per_day - 2_000.0 / 7.0).abs() < 1e-9);
    }

    #[test]
    fn churn_uses_the_window_denominator_not_days_with_data() {
        // A single-day spike must read as spike-over-window, not as sustained
        // per-day volume: 50 listings on one day are 50/7 per day, not 50.
        let rows = vec![ItemDayRow {
            item: item(2),
            category: Category::Commodity,
            day: 9,
            listing_count: 50,
            qty_sum: 50,
            min: 100,
            p05: 100,
            median: 100,
            mad: 0,
        }];
        let scan = OpportunityService::scan_item(&item(2), &rows);
        assert!((scan.churn_per_day - 50.0 / 7.0).abs() < 1e-9);
    }

    // ---- helpers ---------------------------------------------------------

    use crate::domain::ItemDayRow;

    fn item(id: i64) -> ItemRef {
        ItemRef::Item { item_id: id }
    }
}

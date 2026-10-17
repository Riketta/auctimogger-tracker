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
        ItemDayRow,
        ItemRef,
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
/// How long a computed scan may serve requests before recomputation.
const SCAN_TTL_SECONDS: i64 = 300;
/// Normalization anchor for the churn component: this many unique listings
/// per day scores as a full 1.0 (config could override later).
const CHURN_ANCHOR: f64 = 50.0;

/// One ranked snipe candidate. All components are 0..1-normalized except the
/// raw price fields; `score` is ranking-only and never gates deals.
#[derive(Clone, Debug, PartialEq)]
pub struct Opportunity {
    /// The ranked item or pet.
    pub item: ItemRef,
    /// Count-weighted median unit price over the window (copper).
    pub median7: i64,
    /// Cheapest unit price of the newest day in the window (copper).
    pub floor_now: i64,
    /// `1 - floor_now / median7`, clamped to [0, 1] for scoring.
    pub discount: f64,
    /// 14-day sale rate: share of vanished listings credited as sold.
    pub reliability: f64,
    /// Average unique listings per day over the window.
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
}

/// One overview row: an item's current median vs its window median.
#[derive(Clone, Debug, PartialEq)]
pub struct MoverRow {
    /// The item or pet that moved.
    pub item: ItemRef,
    /// Median unit price of the newest day in the window (copper).
    pub median_now: i64,
    /// Count-weighted median unit price over the window (copper).
    pub median_prev: f64,
    /// `median_now / median_prev - 1` (fraction, may be negative).
    pub change: f64,
    /// Average unique listings per day over the window.
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

/// Immutable result of one full-market scan, shared across requests until the
/// TTL expires.
#[derive(Clone, Debug)]
pub struct ScanSnapshot {
    /// When the scan was computed (unix seconds).
    pub computed_at: i64,
    /// How many distinct items the scan covered.
    pub items: usize,
    rows: Arc<Vec<ItemScan>>,
}

/// Per-item intermediate scan row (pre-filter, pre-ranking).
#[derive(Clone, Debug)]
struct ItemScan {
    item: ItemRef,
    samples7: i64,
    churn_per_day: f64,
    floor_now: i64,
    median_now: i64,
    median7: f64,
    mad7: f64,
}

/// Cached scan per target: computed-at stamp plus the shared snapshot.
/// Keyed by target so alternating requests don't evict each other.
type ScanCache = HashMap<TargetId, (i64, Arc<ScanSnapshot>)>;

/// Builds ranked opportunities and the market overview from day-fold history.
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
    /// floor, sorted by score descending.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when history rows cannot be read, a
    /// sale-rate lookup fails, or the scan cache is poisoned.
    pub fn opportunities(
        &self,
        target: &TargetId,
        min_churn_per_day: f64,
        limit: usize,
    ) -> Result<Vec<Opportunity>, StoreError> {
        let scan = self.scan(target)?;
        // One batched lookup instead of a per-candidate query (scan recompute
        // serves whole markets; the N+1 variant dominated its latency).
        let items: Vec<ItemRef> = scan.rows.iter().map(|row| row.item.clone()).collect();
        let rates = self.sales.sale_rate_each(target, &items, 14)?;
        let mut out: Vec<Opportunity> = Vec::new();
        for (row, rate) in scan.rows.iter().zip(rates) {
            let Some(opportunity) = self.opportunity_for(row, rate.unwrap_or(0.0)) else {
                continue;
            };
            if opportunity.samples7 >= i64::from(self.min_samples)
                && opportunity.churn_per_day >= min_churn_per_day
            {
                out.push(opportunity);
            }
        }
        out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(limit.max(1));
        Ok(out)
    }

    /// Market overview: movers vs their window median, plus most-active.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when history rows cannot be read or the
    /// scan cache is poisoned.
    pub fn overview(&self, target: &TargetId, limit: usize) -> Result<OverviewData, StoreError> {
        let scan = self.scan(target)?;
        let mut movers: Vec<MoverRow> = scan
            .rows
            .iter()
            .filter_map(|row| Self::mover_for(row, target))
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

    /// Computes (or serves from cache) the full per-item scan.
    fn scan(&self, target: &TargetId) -> Result<Arc<ScanSnapshot>, StoreError> {
        let now = self.clock.now();
        {
            let cache = self.scan_cache()?;
            if let Some((at, snapshot)) = cache.get(target) {
                if now - at <= SCAN_TTL_SECONDS {
                    return Ok(Arc::clone(snapshot));
                }
            }
        }
        let today = utc_day(now);
        let rows = self.history.item_day_rows(target, today - SCAN_WINDOW_DAYS + 1)?;

        let mut by_item: BTreeMap<ItemRef, Vec<ItemDayRow>> = BTreeMap::new();
        for row in rows {
            by_item.entry(row.item.clone()).or_default().push(row.clone());
        }

        let mut scans: Vec<ItemScan> = Vec::with_capacity(by_item.len());
        for (item, day_rows) in &by_item {
            scans.push(Self::scan_item(item, day_rows));
        }
        let snapshot =
            Arc::new(ScanSnapshot { computed_at: now, items: scans.len(), rows: Arc::new(scans) });
        self.scan_cache()?.insert(target.clone(), (now, Arc::clone(&snapshot)));
        Ok(snapshot)
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
            samples7 += count;
            churn_sum += row.listing_count as f64;
            weighted.push((row.median, count));
            mad_weight_sum += count;
            mad_weighted += row.mad as f64 * row.listing_count as f64;
            match newest {
                Some(latest) if latest.day >= row.day => {}
                _ => newest = Some(row),
            }
        }
        weighted.sort_by_key(|(median, _)| *median);
        let median7 = weighted_median(&weighted);
        let mad7 = if mad_weight_sum > 0 { mad_weighted / mad_weight_sum as f64 } else { 0.0 };
        let (floor_now, median_now) = newest.map_or((0, 0), |row| (row.min, row.median));
        ItemScan {
            item: item.clone(),
            samples7,
            churn_per_day: if day_rows.is_empty() {
                0.0
            } else {
                churn_sum / day_rows.len() as f64
            },
            floor_now,
            median_now,
            median7,
            mad7,
        }
    }

    /// Builds one opportunity from a scan row. `Ok(None)` = the row is not
    /// rankable (no price evidence). `reliability` comes from the batched
    /// `sale_rate_each` lookup; a failing sales store zeroes it visibly (the
    /// error propagates from the batch call, not silently per row).
    fn opportunity_for(&self, scan: &ItemScan, reliability: f64) -> Option<Opportunity> {
        if scan.median7 <= 0.0 || scan.floor_now <= 0 {
            return None;
        }
        // Discount is rounded to 3 decimals for feed/UI stability.
        #[allow(clippy::cast_precision_loss)]
        let discount = ((1.0 - scan.floor_now as f64 / scan.median7).clamp(0.0, 1.0) * 1000.0)
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
        Some(Opportunity {
            item: scan.item.clone(),
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
        })
    }

    fn mover_for(scan: &ItemScan, _target: &TargetId) -> Option<MoverRow> {
        if scan.median_now <= 0 || scan.median7 <= 0.0 {
            return None;
        }
        // Relative change only — magnitudes cancel, precision loss is fine.
        #[allow(clippy::cast_precision_loss)]
        let change = scan.median_now as f64 / scan.median7 - 1.0;
        Some(MoverRow {
            item: scan.item.clone(),
            median_now: scan.median_now,
            median_prev: scan.median7,
            change,
            churn_per_day: scan.churn_per_day,
        })
    }
}

/// Count-weighted median over per-day medians (approximation for ranking —
/// deal gates always use the exact blocks from the metrics engine).
// Copper prices go into ranking math only; f64 keeps 2^53 integers exact.
#[allow(clippy::cast_precision_loss)]
fn weighted_median(pairs: &[(i64, i64)]) -> f64 {
    let total: i64 = pairs.iter().map(|(_, count)| count).sum();
    if total == 0 {
        return 0.0;
    }
    let mid_floor = (total + 1) / 2;
    let mid_ceil = total / 2 + 1;
    let value_at = |rank: i64| -> i64 {
        let mut seen = 0i64;
        for (median, count) in pairs {
            seen += count;
            if seen >= rank {
                return *median;
            }
        }
        pairs.last().map_or(0, |(median, _)| *median)
    };
    let low = value_at(mid_floor);
    let high = value_at(mid_ceil);
    (low + high) as f64 / 2.0
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

    #[test]
    fn scan_item_picks_the_newest_day_for_the_floor() {
        let rows = vec![
            ItemDayRow {
                item: item(1),
                day: 7,
                listing_count: 900,
                qty_sum: 9_000,
                min: 45_000,
                median: 90_000,
                mad: 500,
            },
            ItemDayRow {
                item: item(1),
                day: 8,
                listing_count: 1_100,
                qty_sum: 11_000,
                min: 55_000,
                median: 110_000,
                mad: 600,
            },
        ];
        let scan = OpportunityService::scan_item(&item(1), &rows);
        assert_eq!(scan.floor_now, 55_000);
        assert_eq!(scan.samples7, 2_000);
        assert!((scan.churn_per_day - 1_000.0).abs() < 1e-9);
    }

    // ---- helpers ---------------------------------------------------------

    use crate::domain::ItemDayRow;

    fn item(id: i64) -> ItemRef {
        ItemRef::Item { item_id: id }
    }
}

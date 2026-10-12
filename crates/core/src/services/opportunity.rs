//! Opportunity ranking and market overview ("what should I actually snipe"
//! and "how is the market moving"). All scoring components are transparent
//! and 0..1-normalized; the weighted score is for RANKING only — deal gates
//! never read it (they use exact metric blocks).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::config::TrackerConfig;
use crate::domain::{utc_day, ItemDayRow, ItemRef, TargetId};
use crate::error::StoreError;
use crate::ports::{ClockPort, PriceHistoryRepository, SalesRepository};

/// Scan window for ranking and overview (days).
const SCAN_WINDOW_DAYS: i64 = 7;
/// How long a computed scan may serve requests before recomputation.
const SCAN_TTL_SECONDS: i64 = 300;
/// Normalization anchor for the churn component: this many unique listings
/// per day scores as a full 1.0 (config could override later).
const CHURN_ANCHOR: f64 = 50.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Opportunity {
    pub item: ItemRef,
    pub median7: i64,
    pub floor_now: i64,
    /// `1 - floor_now / median7`, clamped to [0, 1] for scoring.
    pub discount: f64,
    /// 14-day sale rate: share of vanished listings credited as sold.
    pub reliability: f64,
    /// Average unique listings per day over the window.
    pub churn_per_day: f64,
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

#[derive(Clone, Debug, PartialEq)]
pub struct MoverRow {
    pub item: ItemRef,
    pub median_now: i64,
    pub median_prev: f64,
    /// `median_now / median_prev - 1` (fraction, may be negative).
    pub change: f64,
    pub churn_per_day: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverviewData {
    pub movers_up: Vec<MoverRow>,
    pub movers_down: Vec<MoverRow>,
    pub most_active: Vec<MoverRow>,
}

#[derive(Clone, Debug)]
pub struct ScanSnapshot {
    pub computed_at: i64,
    pub items: usize,
    rows: Arc<Vec<ItemScan>>,
}

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

pub struct OpportunityService {
    history: Arc<dyn PriceHistoryRepository>,
    sales: Arc<dyn SalesRepository>,
    clock: Arc<dyn ClockPort>,
    weights: crate::config::ScoreWeights,
    min_samples: u32,
    cache: Mutex<Option<(i64, TargetId, Arc<ScanSnapshot>)>>,
}

impl OpportunityService {
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
            cache: Mutex::new(None),
        }
    }

    /// Ranked snipe candidates: `min_samples` evidence floor, optional churn
    /// floor, sorted by score descending.
    pub fn opportunities(
        &self,
        target: &TargetId,
        min_churn_per_day: f64,
        limit: usize,
    ) -> Result<Vec<Opportunity>, StoreError> {
        let scan = self.scan(target)?;
        let mut out: Vec<Opportunity> = scan
            .rows
            .iter()
            .filter_map(|scan| self.opportunity_for(scan, target))
            .filter(|o| {
                o.samples7 >= self.min_samples as i64 && o.churn_per_day >= min_churn_per_day
            })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(limit.max(1));
        Ok(out)
    }

    /// Market overview: movers vs their window median, plus most-active.
    pub fn overview(&self, target: &TargetId, limit: usize) -> Result<OverviewData, StoreError> {
        let scan = self.scan(target)?;
        let mut movers: Vec<MoverRow> = scan
            .rows
            .iter()
            .filter_map(|row| self.mover_for(row, target))
            .filter(|m| m.churn_per_day >= 1.0 && m.median_prev > 0.0)
            .collect();
        movers.sort_by(|a, b| {
            b.change
                .partial_cmp(&a.change)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let movers_up: Vec<MoverRow> = movers
            .iter()
            .filter(|m| m.change > 0.0)
            .take(limit)
            .cloned()
            .collect();
        let movers_down: Vec<MoverRow> = movers
            .iter()
            .rev()
            .filter(|m| m.change < 0.0)
            .take(limit)
            .cloned()
            .collect();
        let mut most_active: Vec<MoverRow> = movers.clone();
        most_active.sort_by(|a, b| {
            b.churn_per_day
                .partial_cmp(&a.churn_per_day)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        most_active.truncate(limit);
        Ok(OverviewData {
            movers_up,
            movers_down,
            most_active,
        })
    }

    /// Computes (or serves from cache) the full per-item scan.
    fn scan(&self, target: &TargetId) -> Result<Arc<ScanSnapshot>, StoreError> {
        let now = self.clock.now();
        if let Some((at, cached_target, snapshot)) = self.cache.lock().unwrap().as_ref() {
            if now - at <= SCAN_TTL_SECONDS && cached_target == target {
                return Ok(Arc::clone(snapshot));
            }
        }
        let today = utc_day(now);
        let rows = self
            .history
            .item_day_rows(target, today - SCAN_WINDOW_DAYS + 1)?;

        let mut by_item: BTreeMap<ItemRef, Vec<ItemDayRow>> = BTreeMap::new();
        for row in rows {
            by_item
                .entry(row.item.clone())
                .or_default()
                .push(row.clone());
        }

        let mut scans: Vec<ItemScan> = Vec::with_capacity(by_item.len());
        for (item, day_rows) in &by_item {
            scans.push(self.scan_item(item, day_rows));
        }
        let snapshot = Arc::new(ScanSnapshot {
            computed_at: now,
            items: scans.len(),
            rows: Arc::new(scans),
        });
        *self.cache.lock().unwrap() = Some((now, target.clone(), Arc::clone(&snapshot)));
        Ok(snapshot)
    }

    fn scan_item(&self, item: &ItemRef, day_rows: &[ItemDayRow]) -> ItemScan {
        let mut samples7 = 0i64;
        let mut churn_sum = 0.0f64;
        let mut weighted: Vec<(i64, i64)> = Vec::new(); // (median, listing_count)
        let mut mad_weight_sum = 0i64;
        let mut mad_weighted = 0.0f64;
        let mut newest: Option<&ItemDayRow> = None;
        for row in day_rows {
            samples7 += row.listing_count as i64;
            churn_sum += row.listing_count as f64;
            weighted.push((row.median, row.listing_count as i64));
            mad_weight_sum += row.listing_count as i64;
            mad_weighted += row.mad as f64 * row.listing_count as f64;
            match newest {
                Some(latest) if latest.day >= row.day => {}
                _ => newest = Some(row),
            }
        }
        weighted.sort_by_key(|(median, _)| *median);
        let median7 = weighted_median(&weighted);
        let mad7 = if mad_weight_sum > 0 {
            mad_weighted / mad_weight_sum as f64
        } else {
            0.0
        };
        let (floor_now, median_now) = newest.map(|row| (row.min, row.median)).unwrap_or((0, 0));
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

    fn opportunity_for(&self, scan: &ItemScan, target: &TargetId) -> Option<Opportunity> {
        if scan.median7 <= 0.0 || scan.floor_now <= 0 {
            return None;
        }
        let discount = ((1.0 - scan.floor_now as f64 / scan.median7).clamp(0.0, 1.0) * 1000.0)
            .round()
            / 1000.0;
        let reliability = self
            .sales
            .sale_rate(target, &scan.item, 14)
            .ok()
            .flatten()
            .unwrap_or(0.0);
        let churn_score = (scan.churn_per_day / CHURN_ANCHOR).min(1.0);
        let stability = 1.0 - (scan.mad7 / scan.median7).min(1.0);
        let confidence = (scan.samples7 as f64 / self.min_samples as f64).min(1.0);
        let score = self.weights.discount * discount
            + self.weights.sale * reliability
            + self.weights.churn * churn_score
            + self.weights.stability * stability.clamp(0.0, 1.0);
        Some(Opportunity {
            item: scan.item.clone(),
            median7: scan.median7 as i64,
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

    fn mover_for(&self, scan: &ItemScan, _target: &TargetId) -> Option<MoverRow> {
        if scan.median_now <= 0 || scan.median7 <= 0.0 {
            return None;
        }
        Some(MoverRow {
            item: scan.item.clone(),
            median_now: scan.median_now,
            median_prev: scan.median7,
            change: scan.median_now as f64 / scan.median7 - 1.0,
            churn_per_day: scan.churn_per_day,
        })
    }
}

/// Count-weighted median over per-day medians (approximation for ranking —
/// deal gates always use the exact blocks from the metrics engine).
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
        pairs.last().map(|(median, _)| *median).unwrap_or(0)
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
        let clock = Arc::new(FixedClock(9 * 86_400 + 5));
        let sales = Arc::new(NoSales);
        let history = Arc::new(StaticHistory);
        let config = test_config();
        let service = OpportunityService::new(history, sales, clock, &config);
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
        let scan = service.scan_item(&item(1), &rows);
        assert_eq!(scan.floor_now, 55_000);
        assert_eq!(scan.samples7, 2_000);
        assert!((scan.churn_per_day - 1_000.0).abs() < 1e-9);
    }

    // ---- tiny fakes ------------------------------------------------------

    use crate::config::TrackerConfig;
    use crate::domain::{NormalizedListing, SaleCredit, Span};
    use crate::ports::{ClockPort, SalesRepository};

    fn item(id: i64) -> ItemRef {
        ItemRef::Item { item_id: id }
    }

    fn test_config() -> TrackerConfig {
        TrackerConfig {
            server: crate::config::ServerConfig {
                bind: "127.0.0.1:0".into(),
                token_env: None,
            },
            storage: crate::config::StorageConfig {
                database: String::new(),
                raw_listing_days: 7,
                archive_dir: String::new(),
            },
            metrics: crate::config::MetricsConfig {
                spans: vec![Span::D1, Span::D7, Span::D14, Span::D30],
            },
            opportunities: crate::config::OpportunitiesConfig {
                min_samples: 10,
                weights: crate::config::ScoreWeights {
                    discount: 0.4,
                    sale: 0.3,
                    churn: 0.2,
                    stability: 0.1,
                },
            },
            targets: Vec::new(),
        }
    }

    struct FixedClock(i64);
    impl ClockPort for FixedClock {
        fn now(&self) -> i64 {
            self.0
        }
    }

    struct NoSales;
    impl SalesRepository for NoSales {
        fn observe(
            &self,
            _target: &TargetId,
            _listings: &[NormalizedListing],
            _snapshot_id: i64,
            _at: i64,
        ) -> Result<(), StoreError> {
            Ok(())
        }
        fn close_missing(
            &self,
            _target: &TargetId,
            _snapshot_id: i64,
            _at: i64,
        ) -> Result<Vec<SaleCredit>, StoreError> {
            Ok(Vec::new())
        }
        fn sale_rate(
            &self,
            _target: &TargetId,
            _item: &ItemRef,
            _days: i64,
        ) -> Result<Option<f64>, StoreError> {
            Ok(Some(0.5))
        }
    }

    struct StaticHistory;
    impl PriceHistoryRepository for StaticHistory {
        fn load_fold(
            &self,
            _target: &TargetId,
            _item: &ItemRef,
            _day: i64,
        ) -> Result<Option<crate::fold::DayFold>, StoreError> {
            Ok(None)
        }
        fn save_fold(
            &self,
            _target: &TargetId,
            _item: &ItemRef,
            _day: i64,
            _fold: &crate::fold::DayFold,
        ) -> Result<(), StoreError> {
            Ok(())
        }
        fn prune_raw_before(&self, _target: &TargetId, _day: i64) -> Result<u64, StoreError> {
            Ok(0)
        }
        fn items_with_data(
            &self,
            _target: &TargetId,
            _since_day: i64,
        ) -> Result<Vec<ItemRef>, StoreError> {
            Ok(Vec::new())
        }
        fn item_day_rows(
            &self,
            _target: &TargetId,
            _since_day: i64,
        ) -> Result<Vec<ItemDayRow>, StoreError> {
            Ok(Vec::new())
        }
    }
}

//! Sell-priority ranking over a target's watchlist: which favourites to
//! sell first, scored by liquidity (does it move), momentum (is it trading
//! above its weekly norm) and recency (is the market still alive). All
//! inputs come from the same cached metric blocks the UI already shows, so
//! the ranking costs no new store reads and no schema change.
//!
//! The fuel behind the `/priorities` page and the `/api/priorities`
//! contract; both consumers share this service so page and API can never
//! disagree about the order.

use std::sync::Arc;

use crate::{
    domain::{
        utc_day,
        Category,
        ItemDayRow,
        MetricBlock,
        Span,
        TargetId,
        WatchlistEntry,
    },
    error::StoreError,
    ports::{
        ClockPort,
        PriceHistoryRepository,
        WatchlistRepository,
    },
    services::metrics::MetricsService,
};

/// Evidence floor for the `thin_data` flag, in unique auctions per window —
/// the same spirit as `[opportunities].min_samples` (30). A thinner history
/// still ranks, but the UI marks it so a lucky median is not mistaken for a
/// signal.
const THIN_DATA_SAMPLES: i64 = 30;

/// Days of per-day rows behind the trend sparkline (matches the watchlist
/// page's window).
const TREND_DAYS: i64 = 30;

/// Score components behind [`PriorityScore::value`]: liquidity dominates
/// (a fish nobody buys cannot be a priority), momentum and recency modulate
/// it. The weights are deliberate constants, not config: the page shows the
/// components next to the score, so a disagreeing trader re-sorts manually
/// instead of tuning knobs.
const WEIGHT_LIQUIDITY: f64 = 0.5;
const WEIGHT_MOMENTUM: f64 = 0.3;
const WEIGHT_RECENCY: f64 = 0.2;

/// The sell-priority score of one watchlist entry, components exposed so
/// the UI can show *why* an item ranks where it does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PriorityScore {
    /// 0..=100: `100 · (0.5·liquidity + 0.3·momentum⁺ + 0.2·recency)`.
    /// Negative momentum contributes 0 (it can lower a rank but not go
    /// negative itself — below-norm prices mean "wait", not "dump").
    pub value: f64,
    /// Trailing-14-day share of vanished listings credited as sold,
    /// 0..=1 (`None` in the ledger → 0).
    pub liquidity: f64,
    /// `(d1 median − d7 median) / d7 median`, clamped to −1..=1
    /// (missing week → 0).
    pub momentum: f64,
    /// `1 / (1 + days since last credited sale)`; never-sold → 0.
    pub recency: f64,
}

/// One ranked watchlist entry: the score, the numbers behind it, and the
/// per-day trend rows for the page's sparkline.
#[derive(Clone, Debug)]
pub struct RankedFavourite {
    /// The watchlist entry itself (item, note, `added_at`).
    pub entry: WatchlistEntry,
    /// `None` only when the identity has no fold data in either window.
    pub score: Option<PriorityScore>,
    /// The d7 window lacks samples or the d1 window is missing: the rank
    /// rests on little evidence. Never `true` without data to back the
    /// flag — the UI renders it as a marker, not a filter.
    pub thin_data: bool,
    /// Current-day median unit price (`d1`, thin right after midnight UTC).
    pub now: Option<i64>,
    /// The trailing-7-day metric block (median/p05/sale-rate context).
    pub week: Option<MetricBlock>,
    /// Per-day aggregate rows over the trend window, ordered by day.
    pub trend: Vec<ItemDayRow>,
}

/// Ranks a target's watchlist for selling.
pub struct SellPriorityService {
    watchlist: Arc<dyn WatchlistRepository>,
    metrics: Arc<MetricsService>,
    history: Arc<dyn PriceHistoryRepository>,
    clock: Arc<dyn ClockPort>,
}

impl SellPriorityService {
    /// Wires the ranking to the watchlist repository, the metrics service
    /// (batched block reads) and the history port (trend rows).
    pub fn new(
        watchlist: Arc<dyn WatchlistRepository>,
        metrics: Arc<MetricsService>,
        history: Arc<dyn PriceHistoryRepository>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self { watchlist, metrics, history, clock }
    }

    /// Ranks every entry of a target's watchlist within one market
    /// category, best sell first: score descending, entries without any
    /// fold data last. Trend rows cover the last [`TREND_DAYS`] UTC days
    /// inclusive of today.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when any of the joined reads fails.
    pub fn ranked(
        &self,
        target: &TargetId,
        category: Category,
    ) -> Result<Vec<RankedFavourite>, StoreError> {
        let entries = self.watchlist.list(target)?;
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let items: Vec<crate::domain::ItemRef> =
            entries.iter().map(|entry| entry.item.clone()).collect();
        // One batched block read per span for the whole watchlist; both
        // blocks carry the span-independent ledger signals (sale rate,
        // last-sale recency), so d1 alone is enough when d7 is missing.
        let d1 = self.metrics.blocks(target, &items, category, Span::D1)?;
        let d7 = self.metrics.blocks(target, &items, category, Span::D7)?;
        let today = utc_day(self.clock.now());
        let since_day = today - (TREND_DAYS - 1);
        let mut out: Vec<RankedFavourite> = Vec::with_capacity(entries.len());
        for (i, entry) in entries.into_iter().enumerate() {
            let now_block = d1.get(i).cloned().flatten();
            let week = d7.get(i).cloned().flatten();
            let now = now_block.as_ref().map(|block| block.median);
            let thin_data = now_block.as_ref().is_none_or(|block| block.n < THIN_DATA_SAMPLES)
                || week.as_ref().is_none_or(|block| block.n < THIN_DATA_SAMPLES);
            let mut trend =
                self.history.item_day_rows_for_item(target, &entry.item, since_day, category)?;
            trend.sort_unstable_by_key(|row| row.day);
            let score = priority_score(now_block.as_ref(), week.as_ref());
            out.push(RankedFavourite { entry, score, thin_data, now, week, trend });
        }
        // Best sell first: scored entries descending, unscored last, ties
        // broken newest-first (`added_at`) so a re-watch surfaces.
        out.sort_by(|a, b| {
            let score = |r: &RankedFavourite| r.score.as_ref().map(|s| s.value);
            match (score(a), score(b)) {
                (Some(a_score), Some(b_score)) => b_score
                    .total_cmp(&a_score)
                    .then_with(|| b.entry.added_at.cmp(&a.entry.added_at)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => b.entry.added_at.cmp(&a.entry.added_at),
            }
        });
        Ok(out)
    }
}

/// The score behind [`SellPriorityService::ranked`]: `None` only when the
/// identity has no fold data in either window (nothing to rank on). A d1
/// window with no data still scores — momentum simply reads 0 and the
/// thin-data flag marks the rank as resting on the week alone.
/// Display-only f64 math (ranking, not decisions about money).
#[allow(clippy::cast_precision_loss)]
fn priority_score(now: Option<&MetricBlock>, week: Option<&MetricBlock>) -> Option<PriorityScore> {
    if now.is_none() && week.is_none() {
        return None;
    }
    // Both blocks carry the span-independent ledger signals (sale rate,
    // last-sale recency); prefer the fresher d1 read when present.
    let liquidity = now
        .and_then(|block| block.sale_rate)
        .or_else(|| week.and_then(|block| block.sale_rate))
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let momentum = match (now, week) {
        (Some(now_block), Some(week)) if week.median > 0 => {
            let now = now_block.median as f64;
            let week = week.median as f64;
            ((now - week) / week).clamp(-1.0, 1.0)
        }
        _ => 0.0,
    };
    let recency = now
        .and_then(|block| block.days_since_last_sale)
        .or_else(|| week.and_then(|block| block.days_since_last_sale))
        .map_or(0.0, |days| 1.0 / (1.0 + days as f64));
    let value = 100.0
        * (WEIGHT_LIQUIDITY * liquidity
            + WEIGHT_MOMENTUM * momentum.max(0.0)
            + WEIGHT_RECENCY * recency);
    Some(PriorityScore { value, liquidity, momentum, recency })
}

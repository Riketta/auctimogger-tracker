//! Sell-priority ranking over a target's watchlist: which favourites to
//! sell first, scored by liquidity (does it move), momentum (is it trading
//! above its weekly norm) and recency (is the market still alive). All
//! inputs come from the same cached metric blocks the UI already shows, so
//! the ranking costs no new store reads and no schema change.
//!
//! The fuel behind the `/priorities` page and the `/api/priorities`
//! contract; both consumers share this service so page and API can never
//! disagree about the order.

use std::{
    collections::HashSet,
    sync::Arc,
};

use crate::{
    domain::{
        utc_day,
        Category,
        ItemDayRow,
        ItemRef,
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

// ------------------------------------------------------------------ compare

/// One watched identity's cell in the cross-target matrix: the market
/// numbers on one target and whether that target actually watches it.
#[derive(Clone, Copy, Debug)]
pub struct CompareCell {
    /// The target watches this identity (the `only on X` marker).
    pub watched: bool,
    /// Current-day median unit price on this target, when any fold exists.
    pub now: Option<i64>,
    /// Trailing-7-day median unit price on this target.
    pub week_median: Option<i64>,
}

/// One identity across all targets: cells align with the report's targets.
#[derive(Clone, Debug)]
pub struct CompareRow {
    /// The identity (item id, or full pet variant).
    pub item: ItemRef,
    /// How many of the compared targets watch this identity — the rows
    /// sort by this descending, so the broadly-watched fish surface first.
    pub coverage: usize,
    /// Cells aligned with [`CompareReport::targets`].
    pub cells: Vec<CompareCell>,
    /// Index of the best sell market: the target with the highest
    /// current-day median. `None` when no target traded the identity today —
    /// a cell shown as "—" is never the best market.
    pub best: Option<usize>,
}

/// The cross-target comparison behind the `/compare` page and the
/// `/api/watchlist/compare` contract.
#[derive(Clone, Debug)]
pub struct CompareReport {
    /// The compared targets, aligned with every row's `cells`.
    pub targets: Vec<TargetId>,
    /// The market category the numbers come from.
    pub category: Category,
    /// The union of all compared watchlists, broadest coverage first.
    pub rows: Vec<CompareRow>,
}

impl SellPriorityService {
    /// Compares every watched identity across all given targets: one
    /// watchlist read and two batched block reads per target, cells aligned
    /// to the `targets` order. A target without the identity in its own
    /// watchlist still gets market cells (the market exists regardless of
    /// who watches it) with `watched = false`.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when any of the joined reads fails.
    pub fn compare(
        &self,
        targets: &[TargetId],
        category: Category,
    ) -> Result<CompareReport, StoreError> {
        // Union of watched identities. The per-target sets iterate in hash
        // order, so this list is NOT the row order — the sort below is.
        let mut items: Vec<ItemRef> = Vec::new();
        let mut seen: HashSet<ItemRef> = HashSet::new();
        let mut watched: Vec<HashSet<ItemRef>> = Vec::with_capacity(targets.len());
        for target in targets {
            let entries = self.watchlist.list(target)?;
            let set: HashSet<ItemRef> = entries.into_iter().map(|entry| entry.item).collect();
            for item in &set {
                if seen.insert(item.clone()) {
                    items.push(item.clone());
                }
            }
            watched.push(set);
        }

        // One batched d1/d7 read per target over the whole union, then the
        // per-row transpose below — T×2 reads, not T×N×2.
        let mut per_target: Vec<TargetBlocks> = Vec::with_capacity(targets.len());
        for target in targets {
            per_target.push(TargetBlocks {
                now: self.metrics.blocks(target, &items, category, Span::D1)?,
                week: self.metrics.blocks(target, &items, category, Span::D7)?,
            });
        }

        let mut rows: Vec<CompareRow> = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let mut cells = Vec::with_capacity(targets.len());
            let mut coverage = 0;
            for ((_, watched_set), blocks) in targets.iter().zip(&watched).zip(&per_target) {
                let is_watched = watched_set.contains(item);
                if is_watched {
                    coverage += 1;
                }
                cells.push(CompareCell {
                    watched: is_watched,
                    now: blocks.now.get(index).cloned().flatten().map(|block| block.median),
                    week_median: blocks
                        .week
                        .get(index)
                        .cloned()
                        .flatten()
                        .map(|block| block.median),
                });
            }
            let best = argmax_price(&cells);
            rows.push(CompareRow { item: item.clone(), coverage, cells, best });
        }
        // Broadest coverage first, then the full identity order (items by id,
        // then pets by species, level, quality and breed): the union itself
        // comes out of hash-set iteration, so without a total order the pet
        // variants of one species would shuffle between requests.
        rows.sort_by(|a, b| b.coverage.cmp(&a.coverage).then_with(|| a.item.cmp(&b.item)));
        Ok(CompareReport { targets: targets.to_vec(), category, rows })
    }
}

/// One target's batched d1/d7 block lists over the union of items — the
/// `now`/`week` vectors align with the union order.
struct TargetBlocks {
    now: Vec<Option<MetricBlock>>,
    week: Vec<Option<MetricBlock>>,
}

/// Highest CURRENT-day median among the targets that traded the identity
/// today; ties prefer the earlier target (deterministic). A cell without a
/// current price renders as "—", so it is never the best market — and a row
/// nothing traded anywhere today has no best at all (the 7-day medians still
/// read in their cells).
fn argmax_price(cells: &[CompareCell]) -> Option<usize> {
    let mut best: Option<(usize, i64)> = None;
    for (index, cell) in cells.iter().enumerate() {
        if let Some(value) = cell.now {
            best = match best {
                Some((_, best_value)) if best_value >= value => best,
                _ => Some((index, value)),
            };
        }
    }
    best.map(|(index, _)| index)
}

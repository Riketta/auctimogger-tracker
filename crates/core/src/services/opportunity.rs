//! Opportunity ranking and market overview ("what should I actually snipe"
//! and "how is the market moving"). Deals are LIVE: a row ranks only while its
//! floor is listed in the newest snapshot, and it is discounted against the
//! item's quantity-weighted 7-day value. Listings seen today but already gone
//! are reported separately — the day fold never forgets a listing, so a
//! bargain bought minutes after it appeared must not pass as buyable. All
//! scoring components are transparent and 0..1-normalized; the weighted score
//! is for RANKING only — deal gates never read it (they use exact metric
//! blocks).

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
        SnapshotFloor,
        TargetId,
    },
    error::StoreError,
    ports::{
        ClockPort,
        PriceHistoryRepository,
        SalesRepository,
        SnapshotRepository,
    },
};

/// Scan window for ranking and overview (days). The value-relative alert
/// conditions read the same window, so an alert and the ranking never
/// disagree about what an item is worth.
pub const SCAN_WINDOW_DAYS: i64 = 7;
/// Sale-rate window backing the reliability component (days). The one
/// definition every 14-day sale figure reads — metric blocks, alerts and the
/// ranking must describe the same fortnight.
pub(crate) const SALE_RATE_WINDOW_DAYS: i64 = 14;
/// Sold-price window backing the reference value (days): the sale-rate window,
/// so "what buyers paid" and "how often it sells" describe the same
/// fortnight.
pub(crate) const SOLD_PRICE_WINDOW_DAYS: i64 = SALE_RATE_WINDOW_DAYS;
/// Priced units that must have sold in the window before the sold median
/// replaces the listing value as an item's reference: a thinner sample is one
/// bargain or one outlier, not a market.
pub(crate) const SOLD_REFERENCE_MIN_UNITS: i64 = 3;
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
/// Default floor for the gone-today list: every sold listing leaves today's
/// cumulative floor under the live one, and only a deep gap separates a deal
/// from ordinary turnover.
pub const DEFAULT_MIN_MISSED_DISCOUNT: f64 = 0.25;

/// Which of an item's two values its discount is measured against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueSource {
    /// The 14-day sold median: enough priced units sold.
    Sold,
    /// The 7-day listing value: too little sold evidence.
    Listed,
}

impl ValueSource {
    /// Wire/UI spelling of the source.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sold => "sold",
            Self::Listed => "listed",
        }
    }

    /// The reference it names, for alert messages.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Sold => "14-day sold median",
            Self::Listed => "7-day value",
        }
    }
}

/// One ranked live snipe candidate. All components are 0..1-normalized except
/// the raw price fields; `score` is ranking-only and never gates deals. Rows
/// are per market category: an item trading in both auction houses ranks
/// independently in each, on that category's evidence alone.
#[derive(Clone, Debug, PartialEq)]
pub struct Opportunity {
    /// The ranked item or pet.
    pub item: ItemRef,
    /// Which market the row ranks in.
    pub category: Category,
    /// Count-weighted median of the window's per-day listing-level medians
    /// (each day weighted by its listing count; copper) — where the median
    /// LISTING asks, kept for parity with the addon's math and behind the
    /// stability component.
    pub median7: i64,
    /// Cheapest unit price listed for the item in the newest snapshot
    /// (copper) — buyable right now, not merely seen earlier today.
    pub floor_now: i64,
    /// Quantity listed at exactly `floor_now` — how deep the deal is.
    pub floor_qty: i64,
    /// Listings of the item in the newest snapshot.
    pub listed_now: u64,
    /// Quantity-weighted 7-day median LISTING unit price (copper): the price
    /// half the listed quantity sits at — the bulk price for stackable goods,
    /// the listing median for single units. The reference whenever too few
    /// priced units sold (see `sold_median14`).
    pub value7: i64,
    /// 14-day quantity-weighted median of SOLD unit prices (copper); `None`
    /// when fewer than `SOLD_REFERENCE_MIN_UNITS` priced units sold in the
    /// window, or the ledger predates sold-price recording.
    pub sold_median14: Option<i64>,
    /// Which value `discount` is measured against: what buyers paid when
    /// enough of them did, else what sellers ask.
    pub value_source: ValueSource,
    /// `1 - floor_now / reference`, clamped to [0, 1] and rounded to 3
    /// decimals — `sold_median14` when known, else `value7`.
    pub discount: f64,
    /// 14-day sale rate: share of credit-eligible listings credited as sold.
    /// `None` = no credit-eligible evidence; the score then ranks the item at
    /// the scan's median known rate instead of at zero.
    pub reliability: Option<f64>,
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
    /// `min(1, samples7 / min_samples)` — always 1.0 on ranked rows, which
    /// the evidence floor already admits; kept for feed/API compatibility.
    pub confidence: f64,
    /// Weighted geometric mean of the four components (the config weights,
    /// normalized, as exponents): a component at zero zeroes the score, so a
    /// liquid but undiscounted market cannot outrank a real deal.
    pub score: f64,
    /// Whole days since the ledger last credited a sale; `None` = no sale
    /// ever credited (a young ledger is not evidence of a stalled market).
    pub days_since_last_sale: Option<i64>,
    /// Net listings that closed after the last credited sale — the "stopped
    /// selling" churn signal riding with `days_since_last_sale`.
    pub closures_since_last_sale: Option<i64>,
}

/// A deal seen earlier today that is no longer listed: today's cumulative day
/// fold holds a listing cheaper than anything the newest snapshot lists. The
/// fold never drops a listing once seen, so a bargain bought minutes after it
/// appeared keeps showing under the day's floor — this row says so instead of
/// passing it off as buyable.
#[derive(Clone, Debug, PartialEq)]
pub struct MissedDeal {
    /// The item or pet the gone listing offered.
    pub item: ItemRef,
    /// Which market the deal was seen in.
    pub category: Category,
    /// Today's cumulative fold minimum (copper) — what the gone listing asked.
    pub seen_floor: i64,
    /// Quantity that listing offered.
    pub seen_floor_qty: i64,
    /// Cheapest listing of the item in the newest snapshot (copper); `None`
    /// when nothing of the item is listed at all.
    pub live_floor: Option<i64>,
    /// Quantity-weighted 7-day median listing price (copper), as on live rows.
    pub value7: i64,
    /// 14-day sold-price median (copper), as on live rows.
    pub sold_median14: Option<i64>,
    /// Which value `discount` is measured against, as on live rows.
    pub value_source: ValueSource,
    /// `1 - seen_floor / reference`, clamped to [0, 1] and rounded to 3
    /// decimals (the same reference rule as live rows).
    pub discount: f64,
    /// 14-day sale rate (`None` = no credit-eligible evidence).
    pub reliability: Option<f64>,
    /// Unique listings seen over the window (evidence behind `value7`).
    pub samples7: i64,
}

/// The stored snapshot a prepared scan's live floors describe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveSnapshot {
    /// Storage-assigned snapshot id.
    pub id: i64,
    /// Unix seconds of the fetch.
    pub fetched_at: i64,
    /// The source's `Last-Modified`, when it reported one.
    pub source_last_modified: Option<i64>,
}

/// One item's market facts from a prepared scan — the read-only fuel of the
/// crafting view, which needs every reagent and output priced the same way
/// the deal ranking prices them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarketValue {
    /// Quantity-weighted 7-day median listing unit price (copper); 0 without
    /// evidence.
    pub value7: i64,
    /// 14-day quantity-weighted median of SOLD unit prices (copper); `None`
    /// under the sold-evidence floor (see [`Opportunity::sold_median14`]).
    pub sold_median14: Option<i64>,
    /// Auction-days of evidence behind `value7`.
    pub samples7: i64,
    /// 14-day sale rate (`None` = no credit-eligible evidence).
    pub sale_rate: Option<f64>,
    /// The item's listing summary in the newest snapshot (`None` = not
    /// listed there).
    pub live: Option<SnapshotFloor>,
}

impl MarketValue {
    /// The pricing reference — the value a deal's discount is measured
    /// against and a craft's output is assumed to fetch: what buyers paid
    /// when enough priced units sold, else what sellers ask.
    #[must_use]
    pub fn reference(&self) -> i64 {
        self.sold_median14.filter(|median| *median > 0).unwrap_or(self.value7)
    }

    /// Which of the two values [`Self::reference`] resolved to.
    #[must_use]
    pub fn value_source(&self) -> ValueSource {
        if self.sold_median14.is_some_and(|median| median > 0) {
            ValueSource::Sold
        } else {
            ValueSource::Listed
        }
    }
}

/// Read-only view of one category's prepared scan, keyed by identity.
#[derive(Clone, Debug)]
pub struct MarketView {
    scan: Arc<ScanSnapshot>,
}

impl MarketView {
    /// One item's market facts; `None` when the scan holds no row for it.
    #[must_use]
    pub fn get(&self, item: &ItemRef) -> Option<MarketValue> {
        // `rows` is built in BTreeMap order, so it is sorted by identity and
        // the aligned `rates` vector shares the index.
        let index = self.scan.rows.binary_search_by(|row| row.item.cmp(item)).ok()?;
        Some(MarketValue {
            value7: as_copper(self.scan.rows.get(index)?.value7),
            sold_median14: self.scan.rows.get(index)?.sold_median,
            samples7: self.scan.rows.get(index)?.samples7,
            sale_rate: self.scan.rates.get(index).copied().flatten(),
            live: self.scan.floors.get(item).copied(),
        })
    }

    /// When the scan was computed (unix seconds).
    #[must_use]
    pub fn computed_at(&self) -> i64 {
        self.scan.computed_at
    }

    /// The snapshot the view's live floors describe, when one is stored.
    #[must_use]
    pub fn snapshot(&self) -> Option<LiveSnapshot> {
        self.scan.live
    }
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
    /// has no credit-eligible evidence in the window).
    rates: Arc<Vec<Option<f64>>>,
    /// Ledger recencies positionally aligned with `rows` (`None` = no
    /// credited sale ever).
    recencies: Arc<Vec<Option<SaleRecency>>>,
    /// Live floors per item in the category's newest snapshot (empty when the
    /// target has no snapshot of that payload kind).
    floors: Arc<HashMap<ItemRef, SnapshotFloor>>,
    /// The snapshot `floors` describe; `None` when none is stored.
    live: Option<LiveSnapshot>,
    /// Median of the known 14-day sale rates in this scan — the neutral
    /// stand-in that ranks items without evidence.
    reliability_prior: f64,
}

/// Per-item intermediate scan row (pre-filter, pre-ranking).
#[derive(Clone, Debug)]
struct ItemScan {
    item: ItemRef,
    samples7: i64,
    churn_per_day: f64,
    /// Today's cumulative fold floor and the quantity that listing offered;
    /// `None` when the item has no fold today.
    seen_today: Option<(i64, i64)>,
    median_now: i64,
    median7: f64,
    /// Count-weighted median of the days BEFORE today — the movers' baseline.
    median_prev: f64,
    value7: f64,
    /// 14-day sold median (copper) once enough priced units sold; stamps in
    /// after the batched sold-price read, like the sale rates.
    sold_median: Option<i64>,
    mad7: f64,
}

impl ItemScan {
    /// The discount reference: the sold median when the window holds one, else
    /// the 7-day listing value.
    #[allow(clippy::cast_precision_loss)] // copper prices into ranking math only
    fn reference(&self) -> f64 {
        self.sold_median.filter(|median| *median > 0).map_or(self.value7, |median| median as f64)
    }

    /// Which of the two values [`Self::reference`] resolved to.
    fn value_source(&self) -> ValueSource {
        if self.sold_median.is_some_and(|median| median > 0) {
            ValueSource::Sold
        } else {
            ValueSource::Listed
        }
    }
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
    snapshots: Arc<dyn SnapshotRepository>,
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
        snapshots: Arc<dyn SnapshotRepository>,
        clock: Arc<dyn ClockPort>,
        config: &TrackerConfig,
    ) -> Self {
        Self {
            history,
            sales,
            snapshots,
            clock,
            weights: config.opportunities.weights.clone(),
            min_samples: config.opportunities.min_samples,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Ranked live snipe candidates: only items the newest snapshot lists,
    /// `min_samples` evidence floor, optional churn floor, sorted by score
    /// descending. Served entirely from the prepared scan — live floors and
    /// reliability ship with the snapshot, so steady-state requests touch no
    /// store.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cold-start inline scan cannot
    /// read history rows, live floors or sale rates, or the scan cache is
    /// poisoned.
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
            // Live only: nothing of the item in the newest snapshot, nothing
            // to buy (its day-fold floor is a memory, see `missed_deals`).
            let Some(floor) = scan.floors.get(&row.item) else {
                continue;
            };
            let Some(opportunity) =
                self.opportunity_for(row, floor, *rate, scan.reliability_prior, *recency, category)
            else {
                continue;
            };
            if opportunity.samples7 >= i64::from(self.min_samples)
                && opportunity.churn_per_day >= min_churn_per_day
            {
                out.push(opportunity);
            }
        }
        apply_sort(&mut out, sort);
        out.truncate(limit.max(1));
        Ok(out)
    }

    /// Deals seen today that the newest snapshot no longer lists, deepest
    /// discount first: today's fold floor undercuts the live floor (or the
    /// item is not listed at all) by at least `min_discount` against the same
    /// value the ranking uses. Same evidence floor as the ranking, so every
    /// listed price has a value worth comparing against.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cold-start inline scan cannot
    /// read history rows, live floors or sale rates, or the scan cache is
    /// poisoned.
    pub fn missed_deals(
        &self,
        target: &TargetId,
        category: Category,
        min_discount: f64,
        limit: usize,
    ) -> Result<Vec<MissedDeal>, StoreError> {
        let scan = self.scan(target, category)?;
        // An empty newest snapshot (a maintenance dump) proves nothing: with
        // nothing listed, every sighting would read as "gone".
        if scan.floors.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<MissedDeal> = Vec::new();
        for (row, rate) in scan.rows.iter().zip(scan.rates.iter()) {
            let Some((seen_floor, seen_floor_qty)) = row.seen_today else {
                continue;
            };
            let live_floor = scan.floors.get(&row.item).map(|floor| floor.floor_price);
            // Gone exactly when today's cheapest sighting undercuts
            // everything listed now; equal floors mean it is still there.
            if live_floor.is_some_and(|live| seen_floor >= live) {
                continue;
            }
            let reference = row.reference();
            if row.samples7 < i64::from(self.min_samples) || reference <= 0.0 || seen_floor <= 0 {
                continue;
            }
            let discount = discount_of(seen_floor, reference);
            if discount < min_discount {
                continue;
            }
            out.push(MissedDeal {
                item: row.item.clone(),
                category,
                seen_floor,
                seen_floor_qty,
                live_floor,
                value7: as_copper(row.value7),
                sold_median14: row.sold_median,
                value_source: row.value_source(),
                discount,
                reliability: *rate,
                samples7: row.samples7,
            });
        }
        out.sort_by(|a, b| {
            b.discount
                .partial_cmp(&a.discount)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.samples7.cmp(&a.samples7))
        });
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
    /// read history rows, live floors or sale rates, or the scan cache is
    /// poisoned.
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

    /// The stored snapshot the prepared scan's live floors describe; `None`
    /// until a scan exists for the target/category or when the target has no
    /// snapshot of that payload kind.
    #[must_use]
    pub fn live_snapshot(&self, target: &TargetId, category: Category) -> Option<LiveSnapshot> {
        let cache = self.scan_cache().ok()?;
        let (_, snapshot) = cache.get(&(target.clone(), category))?;
        snapshot.live
    }

    /// One category's prepared scan as a read-only market view (value,
    /// evidence, sale rate and live floor per identity). Serves from the
    /// cache like the ranking does; a cold cache computes inline.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cold-start inline scan cannot
    /// read history rows, live floors or sale rates, or the scan cache is
    /// poisoned.
    pub fn market(&self, target: &TargetId, category: Category) -> Result<MarketView, StoreError> {
        Ok(MarketView { scan: self.scan(target, category)? })
    }

    /// Recomputes the full scan artifact for one target — history read,
    /// per-item aggregation, one batched sale-rate lookup and the newest
    /// snapshot's live floors — and stores it as the snapshot requests are
    /// served from. Meant for the background scheduler: after startup, after
    /// each successful ingest cycle, and when a prepared scan outlives
    /// [`SCAN_TTL_SECONDS`]. Skips all work while the cached snapshot is
    /// younger than [`SCAN_REFRESH_DEBOUNCE_SECONDS`], so a post-cycle
    /// refresh and the TTL tick never double-compute.
    ///
    /// Returns the number of items the scan covered (the cached count when
    /// skipped).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when history rows, live floors or sale
    /// rates cannot be read, or the scan cache is poisoned.
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
    /// per-item aggregation, one batched sale-rate lookup aligned with the
    /// aggregated rows, then the newest snapshot's live floors. This is the
    /// whole per-target cost, paid by the scheduler (or the cold-start
    /// fallback) instead of request threads.
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
            scans.push(Self::scan_item(item, day_rows, today));
        }
        // BTreeMap iteration is sorted, so the batched rates line up
        // positionally with the stored rows by construction.
        let items: Vec<ItemRef> = scans.iter().map(|scan| scan.item.clone()).collect();
        let rates = self.sales.sale_rate_each(target, &items, SALE_RATE_WINDOW_DAYS, category)?;
        let recencies = self.sales.sale_recency_each(target, &items, category)?;
        // What buyers actually paid: the discount reference for every item
        // with enough priced sales in the window.
        let sold = self.sales.sold_price_each(target, &items, SOLD_PRICE_WINDOW_DAYS, category)?;
        debug_assert_eq!(
            scans.len(),
            sold.len(),
            "sold_price_each must stay positionally aligned with the scan rows"
        );
        for (scan, histogram) in scans.iter_mut().zip(sold.iter()) {
            scan.sold_median = sold_reference(histogram).map(as_copper);
        }
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
        let reliability_prior = median_known(&rates);
        // The live side: the category's newest snapshot and what it lists.
        let kind = category.payload_kind();
        let live = self.snapshots.latest(target, kind)?.map(|meta| LiveSnapshot {
            id: meta.id,
            fetched_at: meta.fetched_at,
            source_last_modified: meta.source_last_modified,
        });
        let floors = match live {
            Some(live) => self.snapshots.floors(target, kind, live.id)?,
            None => HashMap::new(),
        };
        Ok(ScanSnapshot {
            computed_at: now,
            items: scans.len(),
            rows: Arc::new(scans),
            rates: Arc::new(rates),
            recencies: Arc::new(recencies),
            floors: Arc::new(floors),
            live,
            reliability_prior,
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
    fn scan_item(item: &ItemRef, day_rows: &[ItemDayRow], today: i64) -> ItemScan {
        let mut churn_sum = 0.0f64;
        let mut weighted: Vec<(i64, i64)> = Vec::new(); // (median, listing_count)
                                                        // The movers' baseline: every day before today, same weighting.
        let mut weighted_prev: Vec<(i64, i64)> = Vec::new();
        let mut mad_weight_sum = 0i64;
        let mut mad_weighted = 0.0f64;
        let mut seen_today: Option<(i64, i64)> = None;
        // Only a fold today has a "now"; an item last seen yesterday is not
        // moving, it is absent.
        let mut median_now = 0;
        for row in day_rows {
            let count = i64::try_from(row.listing_count).unwrap_or(i64::MAX);
            churn_sum += row.listing_count as f64;
            weighted.push((row.median, count));
            if row.day < today {
                weighted_prev.push((row.median, count));
            }
            mad_weight_sum = mad_weight_sum.saturating_add(count);
            mad_weighted += row.mad as f64 * row.listing_count as f64;
            if row.day == today {
                seen_today = Some((row.min, row.min_qty.max(0)));
                median_now = row.median;
            }
        }
        weighted.sort_by_key(|(median, _)| *median);
        weighted_prev.sort_by_key(|(median, _)| *median);
        let median7 = weighted_median(&weighted);
        let median_prev = weighted_median(&weighted_prev);
        let mad7 = if mad_weight_sum > 0 { mad_weighted / mad_weight_sum as f64 } else { 0.0 };
        let (value7, samples7) = quantity_value(day_rows).unwrap_or((0.0, 0));
        // Window-denominator churn: days without a fold count as zero, so a
        // single busy day reads as that day's count spread over the window,
        // not as sustained per-day volume (the docs and the `min_churn`
        // filter both describe window semantics).
        let churn_per_day = churn_sum / SCAN_WINDOW_DAYS as f64;
        ItemScan {
            item: item.clone(),
            samples7,
            churn_per_day,
            seen_today,
            median_now,
            median7,
            median_prev,
            value7,
            sold_median: None,
            mad7,
        }
    }

    /// Builds one opportunity from a scan row and its live floor. `Ok(None)`
    /// = the row is not rankable (no value evidence or no live price).
    /// `reliability` comes from the sale rates prepared with the snapshot;
    /// a failing sales store surfaces while the snapshot is computed (in
    /// `refresh` or the cold-start fallback), never per request.
    fn opportunity_for(
        &self,
        scan: &ItemScan,
        floor: &SnapshotFloor,
        rate: Option<f64>,
        prior: f64,
        recency: Option<SaleRecency>,
        category: Category,
    ) -> Option<Opportunity> {
        let reference = scan.reference();
        if scan.median7 <= 0.0 || scan.value7 <= 0.0 || reference <= 0.0 || floor.floor_price <= 0 {
            return None;
        }
        let discount = discount_of(floor.floor_price, reference);
        let churn_score = (scan.churn_per_day / CHURN_ANCHOR).min(1.0);
        let stability = 1.0 - (scan.mad7 / scan.median7).min(1.0);
        // Confidence compares evidence against the u32 floor; f64 is exact at
        // these magnitudes.
        #[allow(clippy::cast_precision_loss)]
        let confidence = (scan.samples7 as f64 / f64::from(self.min_samples)).min(1.0);
        // No sale evidence is not a proven non-seller: rank the item at the
        // scan's median known rate rather than at zero.
        let reliability = rate.unwrap_or(prior);
        let score = weighted_geometric(
            &self.weights,
            discount,
            reliability,
            churn_score,
            stability.clamp(0.0, 1.0),
        );
        // median7/value7 are half-precision weighted medians; the published
        // fields are the floored integers, matching the addon's display rule.
        let median7 = as_copper(scan.median7);
        let value7 = as_copper(scan.value7);
        let today = utc_day(self.clock.now());
        Some(Opportunity {
            item: scan.item.clone(),
            category,
            median7,
            floor_now: floor.floor_price,
            floor_qty: floor.floor_qty.max(0),
            listed_now: floor.listing_count,
            value7,
            sold_median14: scan.sold_median,
            value_source: scan.value_source(),
            discount,
            reliability: rate,
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
        // Today against the days BEFORE it: a baseline that included today
        // divided a real one-day move by the window length (~7x dampening),
        // and an item without a fold today has no "now" to move from.
        if scan.median_now <= 0 || scan.median_prev <= 0.0 {
            return None;
        }
        // Relative change only — magnitudes cancel, precision loss is fine.
        #[allow(clippy::cast_precision_loss)]
        let change = scan.median_now as f64 / scan.median_prev - 1.0;
        Some(MoverRow {
            item: scan.item.clone(),
            category,
            median_now: scan.median_now,
            median_prev: scan.median_prev,
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

/// The discount reference shared by the ranking and the deal alerts: the
/// quantity-weighted median of the per-day quantity-weighted medians (each
/// day weighted by its listed quantity), plus the rows' listing evidence.
/// Days without quantity fall back to their listing-level median and count,
/// so single-unit goods weigh exactly as before. `None` when the rows carry
/// no value at all.
#[must_use]
pub fn quantity_value(rows: &[ItemDayRow]) -> Option<(f64, i64)> {
    let mut qty_weighted: Vec<(i64, i64)> = Vec::with_capacity(rows.len()); // (qty_median, qty_sum)
    let mut samples = 0i64;
    for row in rows {
        let count = i64::try_from(row.listing_count).unwrap_or(i64::MAX);
        samples = samples.saturating_add(count);
        // Saturating: one absurd count must not overflow the whole sum.
        let weight = if row.qty_sum > 0 { row.qty_sum } else { count };
        qty_weighted.push((row.qty_median, weight.max(0)));
    }
    qty_weighted.sort_by_key(|(price, _)| *price);
    let value = weighted_median(&qty_weighted);
    (value > 0.0).then_some((value, samples))
}

/// Weighted geometric mean of the four 0..1 components: the config weights,
/// normalized to sum 1, are the exponents (only ratios matter, as before).
/// Multiplicative on purpose — an item that is liquid but not cheap scores
/// low however good its other components are, where a weighted sum let churn
/// capped at its anchor carry it to the top. A component at zero zeroes the
/// score; a weight at zero drops its component from the product.
#[must_use]
fn weighted_geometric(
    weights: &crate::config::ScoreWeights,
    discount: f64,
    reliability: f64,
    churn_score: f64,
    stability: f64,
) -> f64 {
    // Validated finite and positive-sum by the config.
    let total = weights.discount + weights.sale + weights.churn + weights.stability;
    discount.powf(weights.discount / total)
        * reliability.powf(weights.sale / total)
        * churn_score.powf(weights.churn / total)
        * stability.powf(weights.stability / total)
}

/// `1 - price / value`, clamped to [0, 1] and rounded to 3 decimals for
/// feed/UI stability.
#[allow(clippy::cast_precision_loss)]
fn discount_of(price: i64, value: f64) -> f64 {
    ((1.0 - price as f64 / value).clamp(0.0, 1.0) * 1000.0).round() / 1000.0
}

/// Display rule for the weighted medians: the floored integer copper.
#[allow(clippy::cast_possible_truncation)]
fn as_copper(value: f64) -> i64 {
    value as i64
}

/// Median of the known sale rates (midpoint on even counts); `0.0` when
/// nothing is known.
// `known` is non-empty past the early return, and `mid` is the middle index
// for odd lengths and the upper middle for even ones — both indexes are valid.
// `f64::midpoint` is unstable; sale rates are 0..=1 and cannot overflow.
#[allow(clippy::indexing_slicing, clippy::manual_midpoint)]
fn median_known(rates: &[Option<f64>]) -> f64 {
    let mut known: Vec<f64> =
        rates.iter().flatten().copied().filter(|rate| rate.is_finite()).collect();
    if known.is_empty() {
        return 0.0;
    }
    known.sort_by(f64::total_cmp);
    let mid = known.len() / 2;
    if known.len() % 2 == 1 {
        known[mid]
    } else {
        (known[mid - 1] + known[mid]) / 2.0
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

/// The reference-worthy sold median of a sold-price histogram: the
/// quantity-weighted median of its `(unit price, quantity)` buckets once at
/// least [`SOLD_REFERENCE_MIN_UNITS`] priced units sold, else `None` — one
/// bargain is not a market.
pub(crate) fn sold_reference(sold: &[(i64, i64)]) -> Option<f64> {
    let units = sold.iter().map(|(_, qty)| *qty).fold(0i64, i64::saturating_add);
    if units < SOLD_REFERENCE_MIN_UNITS {
        return None;
    }
    // The store hands buckets over sorted; a defensive sort keeps the
    // weighted median correct whatever a future adapter returns.
    let mut pairs = sold.to_vec();
    pairs.sort_by_key(|(price, _)| *price);
    Some(weighted_median(&pairs))
}

#[cfg(test)]
mod tests {
    // Test construction is allowed to unwrap/expect/index: fixed synthetic
    // inputs, not hostile data (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

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
            floor_qty: 1,
            listed_now: 1,
            value7: 100,
            sold_median14: None,
            value_source: ValueSource::Listed,
            discount: 0.0,
            reliability: None,
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

    /// The scan keeps today's cumulative floor apart from the window stats:
    /// the live ranking reads the snapshot floor, the gone-today list reads
    /// this one.
    #[test]
    fn scan_item_keeps_todays_floor_and_the_window_value_apart() {
        let rows = vec![
            day_row(7, 900, 9_000, 45_000, 45_000, 90_000, 500),
            day_row(8, 1_100, 11_000, 55_000, 55_000, 110_000, 600),
        ];
        let scan = OpportunityService::scan_item(&item(1), &rows, 8);
        assert_eq!(scan.seen_today, Some((55_000, 10)), "day 8 is today: its floor and quantity");
        assert_eq!(scan.samples7, 2_000);
        assert_eq!(scan.median_now, 110_000);
        // Window-denominator: 2 000 listings across a 7-day window, even
        // though only two of those days have folds.
        assert!((scan.churn_per_day - 2_000.0 / 7.0).abs() < 1e-9);
        // Day 8 listed more units, so its per-unit price is the window value.
        assert_eq!(scan.value7, 110_000.0);
        assert_eq!(OpportunityService::scan_item(&item(1), &rows, 9).seen_today, None);
    }

    #[test]
    fn sold_reference_needs_the_evidence_floor_and_weighs_by_quantity() {
        assert_eq!(sold_reference(&[]), None, "nothing sold");
        // Two units: under the floor, whatever their price.
        assert_eq!(sold_reference(&[(100, 2)]), None);
        // Three units, quantity-weighted: the median unit sits at 200.
        assert_eq!(sold_reference(&[(100, 1), (200, 2)]), Some(200.0));
        // Unsorted buckets are still a correct median.
        assert_eq!(sold_reference(&[(300, 2), (100, 1)]), Some(300.0));
        // An even split lands between the two middle prices.
        assert_eq!(sold_reference(&[(100, 2), (300, 2)]), Some(200.0));
    }

    #[test]
    fn movers_measure_today_against_the_days_before_it() {
        let today = 100;
        let row = |day: i64, median: i64| ItemDayRow {
            item: item(1),
            category: Category::Commodity,
            day,
            listing_count: 10,
            qty_sum: 10,
            min: median,
            min_qty: 1,
            p05: median,
            median,
            qty_median: median,
            mad: 0,
        };
        // Yesterday 100, today 150: a +50% day. With today inside its own
        // baseline it read as +20% — the move divided by the window.
        let scan =
            OpportunityService::scan_item(&item(1), &[row(today - 1, 100), row(today, 150)], today);
        assert_eq!(scan.median7, 125.0, "the listing median still spans the whole window");
        let mover = OpportunityService::mover_for(&scan, Category::Commodity).expect("a mover");
        assert!((mover.change - 0.5).abs() < 1e-9, "change = {}", mover.change);
        assert_eq!(mover.median_now, 150);
        assert!((mover.median_prev - 100.0).abs() < 1e-9, "baseline = {}", mover.median_prev);

        // No fold today: nothing is moving, the item is absent.
        let stale = OpportunityService::scan_item(
            &item(1),
            &[row(today - 2, 100), row(today - 1, 90)],
            today,
        );
        assert!(OpportunityService::mover_for(&stale, Category::Commodity).is_none());
    }

    #[test]
    fn churn_uses_the_window_denominator_not_days_with_data() {
        // A single-day spike must read as spike-over-window, not as sustained
        // per-day volume: 50 listings on one day are 50/7 per day, not 50.
        let rows = vec![day_row(9, 50, 50, 100, 100, 100, 0)];
        let scan = OpportunityService::scan_item(&item(2), &rows, 1);
        assert!((scan.churn_per_day - 50.0 / 7.0).abs() < 1e-9);
    }

    /// The discount reference follows quantity: a day of big cheap stacks
    /// outweighs a day of small dear ones, where the listing-level median
    /// would read as the dear day.
    #[test]
    fn quantity_value_weighs_days_by_listed_quantity() {
        let mut bulky = day_row(1, 5, 1_000, 10, 10, 10, 0);
        bulky.qty_median = 10;
        let mut small = day_row(2, 500, 500, 100, 100, 100, 0);
        small.qty_median = 100;
        let (value, samples) = quantity_value(&[bulky, small]).expect("value present");
        assert_eq!(value, 10.0, "1 000 of 1 500 units sit at 10");
        assert_eq!(samples, 505);
        assert_eq!(quantity_value(&[day_row(3, 0, 0, 0, 0, 0, 0)]), None, "no rows, no value");
    }

    /// Multiplicative scoring: zero discount means zero score however liquid
    /// the market, and a capped churn component cannot carry an undiscounted
    /// item past a real deal.
    #[test]
    fn score_is_a_weighted_geometric_mean() {
        let weights =
            crate::config::ScoreWeights { discount: 0.4, sale: 0.3, churn: 0.2, stability: 0.1 };
        assert_eq!(
            weighted_geometric(&weights, 0.0, 1.0, 1.0, 1.0),
            0.0,
            "not cheap: no deal, whatever else is perfect"
        );
        let deal = weighted_geometric(&weights, 0.3, 0.9, 1.0, 0.9);
        let saturated = weighted_geometric(&weights, 0.05, 1.0, 1.0, 1.0);
        assert!(deal > saturated, "a 30% discount beats a 5% one even against perfect markets");
        assert!((weighted_geometric(&weights, 1.0, 1.0, 1.0, 1.0) - 1.0).abs() < 1e-12);
        let no_stability = crate::config::ScoreWeights { stability: 0.0, ..weights };
        assert_eq!(
            weighted_geometric(&no_stability, 1.0, 1.0, 1.0, 0.0),
            1.0,
            "a zero weight drops its component instead of zeroing the product"
        );
    }

    /// The unknown-sale-rate prior is the median of the known rates.
    #[test]
    fn median_known_takes_the_middle_known_rate() {
        assert_eq!(median_known(&[]), 0.0);
        assert_eq!(median_known(&[None, None]), 0.0);
        assert_eq!(median_known(&[Some(0.2), None, Some(0.8)]), 0.5);
        assert!((median_known(&[Some(0.2), Some(0.4), Some(0.4), Some(1.0)]) - 0.4).abs() < 1e-12);
    }

    // ---- helpers ---------------------------------------------------------

    use crate::domain::ItemDayRow;

    fn item(id: i64) -> ItemRef {
        ItemRef::Item { item_id: id }
    }

    #[allow(clippy::too_many_arguments, clippy::cast_possible_truncation)]
    fn day_row(
        day: i64,
        listing_count: u64,
        qty_sum: i64,
        min: i64,
        p05: i64,
        median: i64,
        mad: i64,
    ) -> ItemDayRow {
        ItemDayRow {
            item: item(1),
            category: Category::Commodity,
            day,
            listing_count,
            qty_sum,
            min,
            min_qty: 10,
            p05,
            median,
            qty_median: median,
            mad,
        }
    }
}

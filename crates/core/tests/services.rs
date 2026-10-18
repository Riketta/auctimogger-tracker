//! Application service specs over in-memory port fakes — the hexagon exercised
//! exactly as the real adapters will drive it.

// Test fakes may unwrap/expect/index: fixed synthetic inputs, not hostile
// data (the SecOutfall precedent).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::{
    collections::{
        BTreeMap,
        HashMap,
    },
    sync::{
        Arc,
        Mutex,
    },
};

use amtracker_core::{
    config::{
        TargetConfig,
        TargetKind,
        TrackerConfig,
    },
    domain::{
        utc_day,
        FetchOutcome,
        ItemDayRow,
        ItemRef,
        ListingKind,
        NewWatchlistEntry,
        PayloadKind,
        SnapshotMeta,
        SnapshotStatus,
        Span,
        TargetId,
        TimeLeftBand,
    },
    error::{
        CycleError,
        SourceError,
        StoreError,
    },
    fold::DayFold,
    ports::{
        AlertEventRepository,
        AlertRuleRepository,
        AuctionSourcePort,
        ClockPort,
        MetricsCacheRepository,
        PriceHistoryRepository,
        SalesRepository,
        SnapshotRepository,
        WatchlistRepository,
    },
    services::{
        cycle::{
            CycleOutcome,
            IngestCycleService,
        },
        opportunity::OpportunityService,
        watchlist::WatchlistService,
    },
};

// ------------------------------------------------------------------ helpers

fn target() -> TargetConfig {
    TargetConfig {
        id: TargetId("us-4".into()),
        kind: TargetKind::BlizzardConnectedRealm { region: "us".into(), connected_realm_id: 4 },
        auth: None,
        poll: amtracker_core::config::PollConfig { commodities_secs: 3600, auctions_secs: 1800 },
        flavor: amtracker_core::domain::Flavor::default(),
    }
}

fn listing(
    auction_id: i64,
    item: &ItemRef,
    price: i64,
    qty: i64,
) -> amtracker_core::domain::NormalizedListing {
    amtracker_core::domain::NormalizedListing {
        auction_id,
        item_ref: item.clone(),
        kind: ListingKind::Commodity,
        quantity: qty,
        unit_price: price,
        bid: None,
        time_left: TimeLeftBand::VeryLong,
        context: None,
        bonus_lists: Vec::new(),
    }
}

fn item(id: i64) -> ItemRef {
    ItemRef::Item { item_id: id }
}

fn payload(listings: Vec<amtracker_core::domain::NormalizedListing>) -> FetchOutcome {
    FetchOutcome::Payload(amtracker_core::domain::FetchedPayload {
        kind: PayloadKind::Commodities,
        source_last_modified: None,
        listings,
        dropped_count: 0,
        dropped_reasons: BTreeMap::new(),
        raw: b"raw".to_vec(),
    })
}

type FoldKey = (TargetId, ItemRef, i64);

struct FakeSource {
    outcomes: Mutex<Vec<Result<FetchOutcome, SourceError>>>,
    conditional_gets: Mutex<Vec<Option<i64>>>,
}

impl FakeSource {
    fn new(outcomes: Vec<Result<FetchOutcome, SourceError>>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            conditional_gets: Mutex::new(Vec::new()),
        }
    }

    fn conditional_gets(&self) -> Vec<Option<i64>> {
        self.conditional_gets.lock().unwrap().clone()
    }
}

impl AuctionSourcePort for FakeSource {
    fn fetch(
        &self,
        _target: &TargetConfig,
        _kind: PayloadKind,
        if_modified_since: Option<i64>,
    ) -> Result<FetchOutcome, SourceError> {
        self.conditional_gets.lock().unwrap().push(if_modified_since);
        self.outcomes.lock().unwrap().pop().expect("scripted source outcome missing")
    }
}

#[derive(Default)]
struct FakeSnapshotsInner {
    latest: Option<SnapshotMeta>,
    raws: Vec<Vec<u8>>,
    listing_counts: Vec<u64>,
    next_id: i64,
}

struct FakeSnapshots {
    inner: Mutex<FakeSnapshotsInner>,
}

impl FakeSnapshots {
    fn new() -> Self {
        Self { inner: Mutex::new(FakeSnapshotsInner::default()) }
    }
}

impl SnapshotRepository for FakeSnapshots {
    fn record(
        &self,
        snapshot: &amtracker_core::domain::NewSnapshot,
        listings: &[amtracker_core::domain::NormalizedListing],
        raw: &[u8],
    ) -> Result<i64, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.latest = Some(SnapshotMeta {
            id,
            target: snapshot.target.clone(),
            kind: snapshot.kind,
            fetched_at: snapshot.fetched_at,
            source_last_modified: snapshot.source_last_modified,
            entry_count: snapshot.entry_count,
            dropped_count: snapshot.dropped_count,
            dropped_reasons: snapshot.dropped_reasons.clone(),
            status: snapshot.status,
        });
        inner.raws.push(raw.to_vec());
        inner.listing_counts.push(listings.len() as u64);
        Ok(id)
    }

    fn latest(
        &self,
        _target: &TargetId,
        _kind: PayloadKind,
    ) -> Result<Option<SnapshotMeta>, StoreError> {
        Ok(self.inner.lock().unwrap().latest.clone())
    }

    fn list_recent(
        &self,
        _target: &TargetId,
        _kind: PayloadKind,
        limit: usize,
    ) -> Result<Vec<SnapshotMeta>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.latest.iter().take(limit).cloned().collect())
    }
}

#[derive(Default)]
struct FakeHistory {
    folds: Mutex<HashMap<FoldKey, DayFold>>,
    /// Rows served by `item_day_rows_for_item` (filtered by item and
    /// `since_day`).
    item_rows: Mutex<Vec<ItemDayRow>>,
}

impl PriceHistoryRepository for FakeHistory {
    fn load_fold(
        &self,
        target: &TargetId,
        item: &ItemRef,
        day: i64,
    ) -> Result<Option<DayFold>, StoreError> {
        Ok(self.folds.lock().unwrap().get(&(target.clone(), item.clone(), day)).cloned())
    }

    fn save_fold(
        &self,
        target: &TargetId,
        item: &ItemRef,
        day: i64,
        fold: &DayFold,
    ) -> Result<(), StoreError> {
        self.folds.lock().unwrap().insert((target.clone(), item.clone(), day), fold.clone());
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
    ) -> Result<Vec<amtracker_core::domain::ItemDayRow>, StoreError> {
        Ok(Vec::new())
    }

    fn item_day_rows_for_item(
        &self,
        _target: &TargetId,
        item: &ItemRef,
        since_day: i64,
    ) -> Result<Vec<amtracker_core::domain::ItemDayRow>, StoreError> {
        let rows = self.item_rows.lock().unwrap();
        Ok(rows.iter().filter(|row| &row.item == item && row.day >= since_day).cloned().collect())
    }
}

#[derive(Default)]
struct FakeCache {
    invalidations: Mutex<usize>,
    targeted: Mutex<Vec<Vec<ItemRef>>>,
}

impl FakeCache {
    fn targeted(&self) -> Vec<Vec<ItemRef>> {
        self.targeted.lock().unwrap().clone()
    }
}

impl MetricsCacheRepository for FakeCache {
    fn get(
        &self,
        _target: &TargetId,
        _item: &ItemRef,
        _span: amtracker_core::domain::Span,
    ) -> Result<Option<amtracker_core::domain::CachedBlock>, StoreError> {
        Ok(None)
    }

    fn put(
        &self,
        _target: &TargetId,
        _item: &ItemRef,
        _span: amtracker_core::domain::Span,
        _block: &amtracker_core::domain::MetricBlock,
        _computed_at: i64,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    fn invalidate(&self, _target: &TargetId) -> Result<(), StoreError> {
        *self.invalidations.lock().unwrap() += 1;
        Ok(())
    }

    fn invalidate_items(&self, _target: &TargetId, items: &[ItemRef]) -> Result<(), StoreError> {
        self.targeted.lock().unwrap().push(items.to_vec());
        Ok(())
    }
}

#[derive(Default)]
struct FakeSales {
    observes: Mutex<usize>,
    closes: Mutex<usize>,
    credits: Mutex<Vec<amtracker_core::domain::SaleCredit>>,
    /// Optional per-item rates served by `sale_rate` (default: none).
    rates: Mutex<HashMap<ItemRef, f64>>,
}

impl FakeSales {
    fn with_credits(credits: Vec<amtracker_core::domain::SaleCredit>) -> Self {
        Self {
            observes: Mutex::new(0),
            closes: Mutex::new(0),
            credits: Mutex::new(credits),
            rates: Mutex::new(HashMap::new()),
        }
    }
}

impl SalesRepository for FakeSales {
    fn observe(
        &self,
        _target: &TargetId,
        _kind: amtracker_core::domain::PayloadKind,
        _listings: &[amtracker_core::domain::NormalizedListing],
        _snapshot_id: i64,
        _at: i64,
    ) -> Result<(), StoreError> {
        *self.observes.lock().unwrap() += 1;
        Ok(())
    }

    fn close_missing(
        &self,
        _target: &TargetId,
        _kind: amtracker_core::domain::PayloadKind,
        _snapshot_id: i64,
        _at: i64,
    ) -> Result<Vec<amtracker_core::domain::SaleCredit>, StoreError> {
        *self.closes.lock().unwrap() += 1;
        Ok(self.credits.lock().unwrap().clone())
    }

    fn sale_rate(
        &self,
        _target: &TargetId,
        item: &ItemRef,
        _days: i64,
    ) -> Result<Option<f64>, StoreError> {
        Ok(self.rates.lock().unwrap().get(item).copied())
    }
}

struct FixedClock(i64);

impl ClockPort for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

// ------------------------------------------------------------------ tests

#[test]
fn ingest_cycle_folds_unique_auctions_and_records_the_snapshot() {
    let item1 = item(1);
    let item2 = item(2);
    let source = Arc::new(FakeSource::new(vec![Ok(FetchOutcome::Payload(
        amtracker_core::domain::FetchedPayload {
            kind: PayloadKind::Commodities,
            source_last_modified: Some(999),
            listings: vec![
                listing(10, &item1, 100, 20),
                listing(11, &item1, 100, 5), // same price bucket
                listing(12, &item2, 900, 1),
                listing(10, &item1, 100, 4), // re-sight: last wins, not a new auction
            ],
            dropped_count: 1,
            dropped_reasons: BTreeMap::new(),
            raw: b"raw-bytes".to_vec(),
        },
    ))]));
    let snapshots = Arc::new(FakeSnapshots::new());
    let history = Arc::new(FakeHistory::default());
    let cache = Arc::new(FakeCache::default());
    let sales = Arc::new(FakeSales::with_credits(vec![amtracker_core::domain::SaleCredit {
        auction_id: 10,
        item: item1.clone(),
        quantity: 4,
        at: 1_000_000,
    }]));
    let clock = Arc::new(FixedClock(1_000_000)); // day 11

    let service = IngestCycleService::new(
        source.clone(),
        snapshots.clone(),
        history.clone(),
        cache.clone(),
        sales.clone(),
        clock,
    );
    let report = service.run_cycle(&target(), PayloadKind::Commodities).expect("cycle");

    assert_eq!(report.outcome, CycleOutcome::Fetched);
    assert_eq!(report.entries, 4);
    assert_eq!(report.dropped, 1);
    assert_eq!(report.snapshot_id, Some(1));
    assert_eq!(report.items_folded, 2);
    assert_eq!(report.sale_credits, 1, "credits from close_missing surface in the report");

    // conditional GET saw no previous snapshot
    assert_eq!(source.conditional_gets(), vec![None]);
    assert_eq!(*sales.observes.lock().unwrap(), 1);
    assert_eq!(*sales.closes.lock().unwrap(), 1);

    let stored = snapshots.inner.lock().unwrap();
    assert_eq!(stored.latest.as_ref().expect("snapshot recorded").entry_count, 4);
    assert_eq!(stored.raws[0], b"raw-bytes".to_vec());
    assert_eq!(stored.listing_counts[0], 4, "listings persist with the snapshot");
    drop(stored);

    let folds = history.folds.lock().unwrap();
    let day = amtracker_core::domain::utc_day(1_000_000);
    let item1_fold =
        folds.get(&(TargetId("us-4".into()), item1.clone(), day)).expect("item 1 fold saved");
    assert_eq!(item1_fold.len(), 2, "re-sighted auction folds into one entry");
    let item2_fold =
        folds.get(&(TargetId("us-4".into()), item2.clone(), day)).expect("item 2 fold saved");
    assert_eq!(item2_fold.len(), 1);

    assert_eq!(*cache.invalidations.lock().unwrap(), 1, "fetched data invalidates the cache");
}

#[test]
fn ingest_cycle_passes_conditional_timestamp_and_handles_304() {
    let snapshots = Arc::new(FakeSnapshots::new());
    // Seed a previous snapshot so the cycle sends If-Modified-Since.
    snapshots
        .record(
            &amtracker_core::domain::NewSnapshot {
                target: TargetId("us-4".into()),
                kind: PayloadKind::Commodities,
                fetched_at: 500,
                source_last_modified: Some(500),
                entry_count: 1,
                dropped_count: 0,
                dropped_reasons: BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &[],
            b"old".as_slice(),
        )
        .expect("seed");

    let source = Arc::new(FakeSource::new(vec![Ok(FetchOutcome::NotModified)]));
    let history = Arc::new(FakeHistory::default());
    let cache = Arc::new(FakeCache::default());
    let sales = Arc::new(FakeSales::default());
    let clock = Arc::new(FixedClock(1_000));
    let service = IngestCycleService::new(
        source.clone(),
        snapshots,
        history,
        cache.clone(),
        sales.clone(),
        clock,
    );

    let report = service.run_cycle(&target(), PayloadKind::Commodities).expect("cycle");

    assert_eq!(report.outcome, CycleOutcome::NotModified);
    assert_eq!(report.snapshot_id, None);
    assert_eq!(source.conditional_gets(), vec![Some(500)]);
    assert_eq!(*cache.invalidations.lock().unwrap(), 0, "304 must not invalidate");
    assert_eq!(*sales.observes.lock().unwrap(), 0, "304 must not touch sell-through");
    assert_eq!(report.sale_credits, 0);
}

/// Same-day cycles touch only the items they folded or credited: the cache
/// drops exactly those blocks and keeps the rest of the target's memoized
/// entries (the full sweep is reserved for day rolls).
#[test]
fn second_same_day_cycle_targets_the_affected_items_only() {
    let item1 = item(1);
    let item9 = item(9);
    // FakeSource pops outcomes from the back: cycle 2's payload first.
    let source = Arc::new(FakeSource::new(vec![
        Ok(payload(vec![listing(20, &item1, 100, 2)])),
        Ok(payload(vec![listing(10, &item1, 100, 1)])),
    ]));
    let snapshots = Arc::new(FakeSnapshots::new());
    let history = Arc::new(FakeHistory::default());
    let cache = Arc::new(FakeCache::default());
    // Every cycle's close_missing credits the same vanished item: its cache
    // rows must go too, even though it never appears in the new snapshots.
    let sales = Arc::new(FakeSales::with_credits(vec![amtracker_core::domain::SaleCredit {
        auction_id: 3,
        item: item9.clone(),
        quantity: 1,
        at: 1_030_000,
    }]));
    let clock = MutableClock::new(1_000_000); // day 11
    let service =
        IngestCycleService::new(source, snapshots, history, cache.clone(), sales, clock.clone());

    service.run_cycle(&target(), PayloadKind::Commodities).expect("cycle 1");
    assert_eq!(*cache.invalidations.lock().unwrap(), 1, "first cycle sweeps");
    assert!(cache.targeted().is_empty(), "the sweep is not item-targeted");

    clock.advance(30_000); // still day 11
    let report = service.run_cycle(&target(), PayloadKind::Commodities).expect("cycle 2");

    assert_eq!(*cache.invalidations.lock().unwrap(), 1, "same day: no second sweep");
    let targeted = cache.targeted();
    assert_eq!(targeted.len(), 1, "one targeted drop");
    assert_eq!(targeted[0].len(), 2, "folded item + sale-credited item");
    assert!(targeted[0].contains(&item1));
    assert!(targeted[0].contains(&item9));
    assert_eq!(report.sale_credits, 1);
}

/// The rolling span windows slide at UTC day boundaries — a change no
/// per-item rule can see — so the first cycle after a day roll sweeps the
/// whole target cache instead of dropping individual items.
#[test]
fn the_first_cycle_after_a_day_roll_sweeps_the_whole_target_cache() {
    let item1 = item(1);
    // FakeSource pops outcomes from the back: cycle 2's payload first.
    let source = Arc::new(FakeSource::new(vec![
        Ok(payload(vec![listing(20, &item1, 100, 2)])),
        Ok(payload(vec![listing(10, &item1, 100, 1)])),
    ]));
    let snapshots = Arc::new(FakeSnapshots::new());
    let history = Arc::new(FakeHistory::default());
    let cache = Arc::new(FakeCache::default());
    let clock = MutableClock::new(1_000_000); // day 11
    let service = IngestCycleService::new(
        source,
        snapshots,
        history,
        cache.clone(),
        Arc::new(FakeSales::default()),
        clock.clone(),
    );

    service.run_cycle(&target(), PayloadKind::Commodities).expect("cycle 1");
    assert_eq!(*cache.invalidations.lock().unwrap(), 1, "first cycle sweeps");

    clock.advance(100_000); // day 12: the windows slid
    service.run_cycle(&target(), PayloadKind::Commodities).expect("cycle 2");

    assert_eq!(*cache.invalidations.lock().unwrap(), 2, "day roll sweeps again");
    assert!(cache.targeted().is_empty(), "the sweep is not item-targeted");
}

#[test]
fn ingest_cycle_maps_source_errors_without_touching_storage() {
    let source = Arc::new(FakeSource::new(vec![Err(SourceError::Http("boom".into()))]));
    let snapshots = Arc::new(FakeSnapshots::new());
    let history = Arc::new(FakeHistory::default());
    let clock = Arc::new(FixedClock(0));
    let service = IngestCycleService::new(
        source,
        snapshots.clone(),
        history,
        Arc::new(FakeCache::default()),
        Arc::new(FakeSales::default()),
        clock,
    );

    let error = service.run_cycle(&target(), PayloadKind::Auctions).expect_err("must fail");
    assert!(matches!(error, CycleError::Source(_)));
    assert!(snapshots.inner.lock().unwrap().latest.is_none(), "failed cycle writes nothing");
}

#[derive(Default)]
struct FakeWatchlist {
    entries: Mutex<Vec<amtracker_core::domain::WatchlistEntry>>,
}

impl WatchlistRepository for FakeWatchlist {
    fn add(
        &self,
        entry: &NewWatchlistEntry,
    ) -> Result<amtracker_core::domain::WatchlistEntry, StoreError> {
        let mut entries = self.entries.lock().unwrap();
        if entries.iter().any(|e| e.target == entry.target && e.item == entry.item) {
            return Err(StoreError::Conflict("already watched".into()));
        }
        let stored = amtracker_core::domain::WatchlistEntry {
            target: entry.target.clone(),
            item: entry.item.clone(),
            note: entry.note.clone(),
            added_at: 77,
        };
        entries.push(stored.clone());
        Ok(stored)
    }

    fn remove(&self, target: &TargetId, item: &ItemRef) -> Result<bool, StoreError> {
        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();
        entries.retain(|e| !(e.target == *target && e.item == *item));
        Ok(entries.len() != before)
    }

    fn list(
        &self,
        target: &TargetId,
    ) -> Result<Vec<amtracker_core::domain::WatchlistEntry>, StoreError> {
        Ok(self.entries.lock().unwrap().iter().filter(|e| e.target == *target).cloned().collect())
    }
}

#[test]
fn watchlist_service_adds_lists_and_rejects_duplicates() {
    let repo = Arc::new(FakeWatchlist::default());
    let service = WatchlistService::new(repo.clone(), Arc::new(FixedClock(77)));
    let target = TargetId("us-4".into());
    let pet = ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 };

    let added = service.add(&target, pet.clone(), Some("rare pet".into())).expect("add");
    assert_eq!(added.item, pet);
    assert_eq!(added.added_at, 77);

    let duplicate = service.add(&target, pet.clone(), None);
    assert!(matches!(duplicate, Err(StoreError::Conflict(_))));

    assert_eq!(service.list(&target).expect("list").len(), 1);
    assert!(service.remove(&target, &pet).expect("remove"));
    assert!(!service.remove(&target, &pet).expect("remove again"));
    assert_eq!(service.list(&target).expect("list").len(), 0);
}

#[test]
fn watchlist_report_joins_blocks_rates_and_trends_positionally() {
    let today = 1_000_000; // utc day 11
    let item1 = item(1);
    let pet = ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 };

    let watchlist = Arc::new(FakeWatchlist::default());
    watchlist
        .add(&NewWatchlistEntry {
            target: TargetId("us-4".into()),
            item: item1.clone(),
            note: Some("flip candidate".into()),
        })
        .expect("add item");
    watchlist
        .add(&NewWatchlistEntry { target: TargetId("us-4".into()), item: pet.clone(), note: None })
        .expect("add pet");

    // One fold per day for the last 7 days at price 100; the pet has none.
    let history = Arc::new(FakeHistory::default());
    for offset in 0..7 {
        let mut fold = DayFold::new(item1.clone());
        fold.apply(&[listing(offset, &item1, 100, 1)]);
        history
            .save_fold(&TargetId("us-4".into()), &item1, utc_day(today) - offset, &fold)
            .expect("save fold");
    }
    // Trend rows: one inside the 30-day window, one far outside it.
    *history.item_rows.lock().unwrap() = vec![
        ItemDayRow {
            item: item1.clone(),
            day: utc_day(today),
            listing_count: 1,
            qty_sum: 1,
            min: 90,
            median: 100,
            mad: 0,
        },
        ItemDayRow {
            item: item1.clone(),
            day: utc_day(today) - 40,
            listing_count: 1,
            qty_sum: 1,
            min: 500,
            median: 500,
            mad: 0,
        },
    ];

    let sales = Arc::new(FakeSales::default());
    sales.rates.lock().unwrap().insert(item1.clone(), 0.5);
    let metrics = Arc::new(amtracker_core::services::metrics::MetricsService::new(
        history.clone(),
        Arc::new(FakeCache::default()),
        sales.clone(),
        Arc::new(FixedClock(today)),
    ));
    let report_service = amtracker_core::services::watchlist_report::WatchlistReportService::new(
        watchlist,
        metrics,
        sales,
        history,
        Arc::new(FixedClock(today)),
    );

    let report = report_service
        .report(&TargetId("us-4".into()), &[Span::D1, Span::D7], 14, 30)
        .expect("report");
    assert_eq!(report.len(), 2);

    // Entry order follows the watchlist; blocks align with the spans.
    let first = &report[0];
    assert_eq!(first.entry.item, item1);
    assert_eq!(first.blocks.len(), 2);
    let d1 = first.blocks[0].as_ref().expect("d1 block");
    let d7 = first.blocks[1].as_ref().expect("d7 block");
    assert_eq!(d1.n, 1);
    assert_eq!(d1.median, 100);
    assert_eq!(d7.n, 7);
    assert_eq!(d7.median, 100);
    assert_eq!(first.sale_rate, Some(0.5));
    // The 40-day-old trend row is outside the 30-day window.
    assert_eq!(first.trend.len(), 1);
    assert_eq!(first.trend[0].median, 100);

    // The pet has no folds, no sales and no trend rows — all `None`, but
    // still positionally present.
    let second = &report[1];
    assert_eq!(second.entry.item, pet);
    assert!(second.blocks.iter().all(Option::is_none));
    assert_eq!(second.sale_rate, None);
    assert_eq!(second.trend.len(), 0);
}

#[test]
fn alert_evaluation_fires_records_cools_down_and_isolates_dispatch() {
    use amtracker_core::{
        domain::{
            AlertCondition,
            NewAlertRule,
        },
        ports::{
            AlertEventRepository,
            AlertRuleRepository,
        },
        services::alert::AlertEvaluationService,
    };

    let today = 1_000_000; // utc day 11
    let item1 = item(1);
    let empty_item = item(2); // never gets folds: no data → never fires
    let target = TargetId("us-4".into());

    let history = Arc::new(FakeHistory::default());
    let mut fold = DayFold::new(item1.clone());
    fold.apply(&[listing(1, &item1, 100, 1)]);
    history.save_fold(&target, &item1, utc_day(today), &fold).expect("save fold");

    let rules = Arc::new(FakeAlertRules::default());
    let events = Arc::new(FakeAlertEvents::default());
    rules
        .add(&NewAlertRule {
            target: target.clone(),
            item: item1.clone(),
            condition: AlertCondition::PriceBelow { copper: 150 },
            note: Some("cheap flip".into()),
        })
        .expect("add price rule");
    rules
        .add(&NewAlertRule {
            target: target.clone(),
            item: empty_item,
            condition: AlertCondition::PriceBelow { copper: 150 },
            note: None,
        })
        .expect("add no-data rule");

    let clock = MutableClock::new(today);
    let dispatched = Arc::new(RecordingDispatcher(Mutex::new(0)));
    let service = AlertEvaluationService::new(
        rules.clone(),
        events.clone(),
        Arc::new(amtracker_core::services::metrics::MetricsService::new(
            history.clone(),
            Arc::new(FakeCache::default()),
            Arc::new(FakeSales::default()),
            clock.clone(),
        )),
        Arc::new(FakeSales::default()),
        Some(dispatched.clone()),
        clock.clone(),
    );

    // First pass: the price rule fires (100 ≤ 150), the data-less one skips.
    let run = service.evaluate(&target, 21_600).expect("evaluate");
    assert_eq!(run.evaluated, 2);
    assert_eq!(run.fired, 1);
    assert_eq!(run.dispatched, 1);
    assert_eq!(*dispatched.0.lock().unwrap(), 1);
    let stored = events.recent(&target, 10).expect("recent");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].observed, Some(100.0));
    assert!(stored[0].message.contains("item 1"), "message identifies the item");
    // The fired rule's cooldown anchor is stamped.
    let listed = rules.list(&target).expect("list");
    assert_eq!(listed[0].last_fired_at, Some(today));

    // An immediate second pass is inside the cooldown: nothing fires again.
    let run = service.evaluate(&target, 21_600).expect("evaluate again");
    assert_eq!(run.fired, 0);

    // Past the cooldown the condition still holds, so it fires again — and a
    // failing dispatcher only counts failures, never breaks evaluation.
    clock.advance(21_601);
    let failing = AlertEvaluationService::new(
        rules.clone(),
        events.clone(),
        Arc::new(amtracker_core::services::metrics::MetricsService::new(
            history.clone(),
            Arc::new(FakeCache::default()),
            Arc::new(FakeSales::default()),
            clock.clone(),
        )),
        Arc::new(FakeSales::default()),
        Some(Arc::new(FailingDispatcher)),
        clock.clone(),
    );
    let run = failing.evaluate(&target, 21_600).expect("evaluate after cooldown");
    assert_eq!(run.fired, 1);
    assert_eq!(run.dispatched, 0);
    assert_eq!(run.dispatch_failures, 1);
    assert_eq!(events.recent(&target, 10).expect("recent").len(), 2);

    // A target without rules evaluates to a quiet zero-run.
    let quiet = service.evaluate(&TargetId("eu-9".into()), 21_600).expect("evaluate empty");
    assert_eq!(quiet.evaluated, 0);
}

#[derive(Default)]
struct FakeAlertRules {
    rules: Mutex<Vec<amtracker_core::domain::AlertRule>>,
    next_id: Mutex<i64>,
}

impl AlertRuleRepository for FakeAlertRules {
    fn add(
        &self,
        rule: &amtracker_core::domain::NewAlertRule,
    ) -> Result<amtracker_core::domain::AlertRule, StoreError> {
        let mut next = self.next_id.lock().unwrap();
        *next += 1;
        let id = *next;
        let stored = amtracker_core::domain::AlertRule {
            id,
            target: rule.target.clone(),
            item: rule.item.clone(),
            condition: rule.condition,
            note: rule.note.clone(),
            created_at: 1,
            last_fired_at: None,
        };
        self.rules.lock().unwrap().push(stored.clone());
        Ok(stored)
    }

    fn remove(&self, rule_id: i64) -> Result<bool, StoreError> {
        let mut rules = self.rules.lock().unwrap();
        let before = rules.len();
        rules.retain(|rule| rule.id != rule_id);
        Ok(rules.len() != before)
    }

    fn list(
        &self,
        target: &TargetId,
    ) -> Result<Vec<amtracker_core::domain::AlertRule>, StoreError> {
        Ok(self
            .rules
            .lock()
            .unwrap()
            .iter()
            .filter(|rule| rule.target == *target)
            .cloned()
            .collect())
    }

    fn mark_fired(&self, rule_id: i64, at: i64) -> Result<(), StoreError> {
        for rule in self.rules.lock().unwrap().iter_mut() {
            if rule.id == rule_id {
                rule.last_fired_at = Some(at);
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct FakeAlertEvents {
    events: Mutex<Vec<amtracker_core::domain::AlertEvent>>,
}

impl AlertEventRepository for FakeAlertEvents {
    fn record(&self, event: &amtracker_core::domain::NewAlertEvent) -> Result<i64, StoreError> {
        let mut events = self.events.lock().unwrap();
        let id = i64::try_from(events.len() + 1).unwrap_or(i64::MAX);
        events.push(amtracker_core::domain::AlertEvent {
            id,
            rule_id: event.rule_id,
            target: event.target.clone(),
            item: event.item.clone(),
            condition: event.condition,
            observed: event.observed,
            message: event.message.clone(),
            fired_at: event.fired_at,
        });
        Ok(id)
    }

    fn recent(
        &self,
        target: &TargetId,
        limit: usize,
    ) -> Result<Vec<amtracker_core::domain::AlertEvent>, StoreError> {
        let events = self.events.lock().unwrap();
        Ok(events
            .iter()
            .filter(|event| event.target == *target)
            .rev()
            .take(limit)
            .cloned()
            .collect())
    }
}

/// Dispatcher that records deliveries — for asserting dispatch counts.
struct RecordingDispatcher(Mutex<usize>);

impl amtracker_core::ports::AlertDispatchPort for RecordingDispatcher {
    fn dispatch(
        &self,
        _event: &amtracker_core::domain::AlertEvent,
    ) -> Result<(), amtracker_core::error::AlertDispatchError> {
        *self.0.lock().unwrap() += 1;
        Ok(())
    }
}

/// Dispatcher that always fails — for asserting failure isolation.
struct FailingDispatcher;

impl amtracker_core::ports::AlertDispatchPort for FailingDispatcher {
    fn dispatch(
        &self,
        _event: &amtracker_core::domain::AlertEvent,
    ) -> Result<(), amtracker_core::error::AlertDispatchError> {
        Err(amtracker_core::error::AlertDispatchError::Status(500))
    }
}

// --------------------------------------------------------- opportunity scans

/// Clock the tests can move forward to simulate TTL expiry and debounce
/// windows.
struct MutableClock(Mutex<i64>);

impl MutableClock {
    fn new(now: i64) -> Arc<Self> {
        Arc::new(Self(Mutex::new(now)))
    }

    fn advance(&self, secs: i64) {
        *self.0.lock().unwrap() += secs;
    }
}

impl ClockPort for MutableClock {
    fn now(&self) -> i64 {
        *self.0.lock().unwrap()
    }
}

/// Full-market history fake: scripted day rows plus a read counter, so tests
/// can prove that steady-state requests touch no store.
struct CountingHistory {
    rows: Vec<ItemDayRow>,
    item_day_rows_calls: Mutex<usize>,
}

impl CountingHistory {
    fn new(rows: Vec<ItemDayRow>) -> Self {
        Self { rows, item_day_rows_calls: Mutex::new(0) }
    }

    fn item_day_rows_calls(&self) -> usize {
        *self.item_day_rows_calls.lock().unwrap()
    }
}

impl PriceHistoryRepository for CountingHistory {
    fn load_fold(
        &self,
        _target: &TargetId,
        _item: &ItemRef,
        _day: i64,
    ) -> Result<Option<DayFold>, StoreError> {
        Ok(None)
    }

    fn save_fold(
        &self,
        _target: &TargetId,
        _item: &ItemRef,
        _day: i64,
        _fold: &DayFold,
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
        *self.item_day_rows_calls.lock().unwrap() += 1;
        Ok(self.rows.clone())
    }

    fn item_day_rows_for_item(
        &self,
        _target: &TargetId,
        _item: &ItemRef,
        _since_day: i64,
    ) -> Result<Vec<ItemDayRow>, StoreError> {
        Ok(Vec::new())
    }
}

/// Sales fake: a deterministic per-item rate, a batch-call counter, and an
/// injectable failure mode for the failed-refresh resilience test.
#[derive(Default)]
struct CountingSales {
    sale_rate_each_calls: Mutex<usize>,
    fail_sale_rate_each: Mutex<bool>,
}

impl CountingSales {
    fn sale_rate_each_calls(&self) -> usize {
        *self.sale_rate_each_calls.lock().unwrap()
    }

    fn set_sale_rate_each_failure(&self, fail: bool) {
        *self.fail_sale_rate_each.lock().unwrap() = fail;
    }
}

impl SalesRepository for CountingSales {
    fn observe(
        &self,
        _target: &TargetId,
        _kind: PayloadKind,
        _listings: &[amtracker_core::domain::NormalizedListing],
        _snapshot_id: i64,
        _at: i64,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    fn close_missing(
        &self,
        _target: &TargetId,
        _kind: PayloadKind,
        _snapshot_id: i64,
        _at: i64,
    ) -> Result<Vec<amtracker_core::domain::SaleCredit>, StoreError> {
        Ok(Vec::new())
    }

    fn sale_rate(
        &self,
        _target: &TargetId,
        _item: &ItemRef,
        _days: i64,
    ) -> Result<Option<f64>, StoreError> {
        Ok(None)
    }

    fn sale_rate_each(
        &self,
        _target: &TargetId,
        items: &[ItemRef],
        _days: i64,
    ) -> Result<Vec<Option<f64>>, StoreError> {
        *self.sale_rate_each_calls.lock().unwrap() += 1;
        if *self.fail_sale_rate_each.lock().unwrap() {
            return Err(StoreError::Backend("sales store unavailable".into()));
        }
        Ok(items.iter().map(|i| if *i == item(1) { Some(0.25) } else { None }).collect())
    }
}

/// Minimal-but-valid tracker config for the opportunity service specs.
fn opportunity_config() -> TrackerConfig {
    TrackerConfig {
        server: amtracker_core::config::ServerConfig {
            bind: "127.0.0.1:0".into(),
            token_env: None,
            allow_open: true,
            feed_publish_secs: 300,
        },
        storage: amtracker_core::config::StorageConfig {
            database: "mem.sqlite3".into(),
            raw_listing_days: 7,
            archive_dir: "raw".into(),
            backup_dir: None,
            backup_secs: 0,
            backup_keep: 7,
        },
        metrics: amtracker_core::config::MetricsConfig {
            spans: vec![amtracker_core::domain::Span::D1],
        },
        opportunities: amtracker_core::config::OpportunitiesConfig {
            min_samples: 1,
            weights: amtracker_core::config::ScoreWeights {
                discount: 0.4,
                sale: 0.3,
                churn: 0.2,
                stability: 0.1,
            },
        },
        alerts: amtracker_core::config::AlertsConfig {
            enabled: false,
            cooldown_secs: 21_600,
            delivery: "log".into(),
            webhook_env: None,
            token_env: None,
            chat_id_env: None,
        },
        catalog: amtracker_core::config::CatalogConfig::default(),
        targets: vec![target()],
    }
}

fn day_row(item_id: i64, day: i64, listing_count: u64, min: i64, median: i64) -> ItemDayRow {
    ItemDayRow { item: item(item_id), day, listing_count, qty_sum: 0, min, median, mad: 500 }
}

/// Two ranked items: item 1 with a steep discount and sales evidence, item 2
/// with a modest discount and none.
fn scan_day_rows() -> Vec<ItemDayRow> {
    let today = amtracker_core::domain::utc_day(1_000_000);
    vec![
        day_row(1, today - 1, 900, 45_000, 90_000),
        day_row(1, today, 1_100, 55_000, 110_000),
        day_row(2, today, 300, 80_000, 95_000),
    ]
}

/// A fresh opportunity service over the shared fixture: scripted rows,
/// counting sales fake, movable clock.
fn opportunity_fixture(
    now: i64,
) -> (Arc<OpportunityService>, Arc<CountingHistory>, Arc<CountingSales>, Arc<MutableClock>) {
    let history = Arc::new(CountingHistory::new(scan_day_rows()));
    let sales = Arc::new(CountingSales::default());
    let clock = MutableClock::new(now);
    let service = Arc::new(OpportunityService::new(
        history.clone(),
        sales.clone(),
        clock.clone(),
        &opportunity_config(),
    ));
    (service, history, sales, clock)
}

#[test]
fn refresh_prepares_the_scan_so_requests_read_nothing() {
    let target = TargetId("us-4".into());
    let (service, history, sales, _clock) = opportunity_fixture(1_000_000);

    let covered = service.refresh(&target).expect("refresh");
    assert_eq!(covered, 2, "both scanned items are covered");

    for _ in 0..3 {
        let ranked = service.opportunities(&target, 1.0, 50).expect("ranked");
        assert_eq!(ranked.len(), 2);
        let overview = service.overview(&target, 15).expect("overview");
        assert_eq!(overview.most_active.len(), 2);
    }
    assert_eq!(history.item_day_rows_calls(), 1, "only the refresh reads history");
    assert_eq!(sales.sale_rate_each_calls(), 1, "only the refresh reads sale rates");
}

#[test]
fn prepared_ranking_matches_the_inline_semantics() {
    let target = TargetId("us-4".into());
    // The same data through the cold inline fallback and through a prepared
    // snapshot must produce identical ranking.
    let (cold, _, _, _) = opportunity_fixture(1_000_000);
    let inline = cold.opportunities(&target, 1.0, 50).expect("inline ranked");

    let (prepared, history, sales, _clock) = opportunity_fixture(1_000_000);
    prepared.refresh(&target).expect("refresh");
    let warmed = prepared.opportunities(&target, 1.0, 50).expect("prepared ranked");

    assert_eq!(inline, warmed, "prepared rates must reproduce the inline math");
    assert_eq!(
        warmed.first().map(|o| o.item.clone()),
        Some(item(1)),
        "biggest discount ranks first"
    );
    assert_eq!(
        warmed.first().map(|o| o.reliability),
        Some(0.25),
        "prepared rate feeds reliability"
    );
    assert_eq!(history.item_day_rows_calls(), 1);
    assert_eq!(sales.sale_rate_each_calls(), 1);
}

#[test]
fn stale_snapshot_is_still_served_without_recompute() {
    let target = TargetId("us-4".into());
    let ttl = amtracker_core::services::opportunity::SCAN_TTL_SECONDS;
    let (service, history, sales, clock) = opportunity_fixture(1_000_000);
    service.refresh(&target).expect("refresh");
    assert_eq!(service.snapshot_age_secs(&target), Some(0));

    // Past the scheduler TTL but far inside the staleness backstop: the
    // stale snapshot keeps serving.
    clock.advance(ttl + 60);
    assert_eq!(service.snapshot_age_secs(&target), Some(ttl + 60));

    service.opportunities(&target, 1.0, 50).expect("ranked from the stale snapshot");
    service.overview(&target, 15).expect("overview from the stale snapshot");
    assert_eq!(history.item_day_rows_calls(), 1, "a stale snapshot must not recompute inline");
    assert_eq!(sales.sale_rate_each_calls(), 1);
}

#[test]
fn cold_cache_falls_back_to_inline_compute() {
    let target = TargetId("us-4".into());
    let (service, history, sales, _clock) = opportunity_fixture(1_000_000);

    let ranked = service.opportunities(&target, 1.0, 50).expect("ranked");
    assert_eq!(ranked.len(), 2);
    assert_eq!(history.item_day_rows_calls(), 1, "cold cache computes inline");
    assert_eq!(sales.sale_rate_each_calls(), 1);

    // The inline compute also seeds the cache: the next request reads nothing.
    service.overview(&target, 15).expect("overview");
    assert_eq!(history.item_day_rows_calls(), 1);
    assert_eq!(sales.sale_rate_each_calls(), 1);
}

#[test]
fn refresh_debounces_inside_the_window_and_recomputes_outside() {
    let target = TargetId("us-4".into());
    let (service, history, sales, clock) = opportunity_fixture(1_000_000);
    let covered = service.refresh(&target).expect("first refresh");
    assert_eq!(covered, 2);

    clock.advance(10);
    let covered = service.refresh(&target).expect("debounced refresh");
    assert_eq!(covered, 2, "debounced refresh reports the cached coverage");
    assert_eq!(history.item_day_rows_calls(), 1, "inside the debounce window no work happens");
    assert_eq!(sales.sale_rate_each_calls(), 1);

    clock.advance(21); // 31s after the first refresh
    let covered = service.refresh(&target).expect("post-debounce refresh");
    assert_eq!(covered, 2);
    assert_eq!(history.item_day_rows_calls(), 2, "outside the window the scan recomputes");
    assert_eq!(sales.sale_rate_each_calls(), 2);
}

#[test]
fn a_failed_refresh_keeps_the_previous_snapshot_serving() {
    let target = TargetId("us-4".into());
    let (service, history, sales, clock) = opportunity_fixture(1_000_000);
    service.refresh(&target).expect("first refresh");

    clock.advance(31); // past the debounce, so the next refresh really runs
    sales.set_sale_rate_each_failure(true);
    assert!(service.refresh(&target).is_err(), "the failed sales lookup surfaces as an Err");

    // The failed pass must not evict or poison the cached artifact: ranking
    // and overview keep serving the previous snapshot without store reads.
    let ranked = service.opportunities(&target, 1.0, 50).expect("stale ranked");
    assert_eq!(ranked.len(), 2);
    service.overview(&target, 15).expect("stale overview");
    assert_eq!(
        history.item_day_rows_calls(),
        2,
        "the failed attempt re-read history before the sales step failed"
    );
    assert_eq!(sales.sale_rate_each_calls(), 2, "the failed attempt counted");
}

#[test]
fn snapshot_age_is_none_until_a_scan_exists() {
    let target = TargetId("us-4".into());
    let (service, _history, _sales, _clock) = opportunity_fixture(1_000_000);
    assert_eq!(service.snapshot_age_secs(&target), None, "cold cache has no age");
    service.refresh(&target).expect("refresh");
    assert_eq!(service.snapshot_age_secs(&target), Some(0));
}

#[test]
fn requests_self_heal_once_a_snapshot_passes_the_backstop() {
    let target = TargetId("us-4".into());
    let ttl = amtracker_core::services::opportunity::SCAN_TTL_SECONDS;
    let (service, history, sales, clock) = opportunity_fixture(1_000_000);
    service.refresh(&target).expect("refresh");

    // A dead scheduler would leave the snapshot aging forever; past six TTLs
    // the request path recomputes inline instead of serving it.
    clock.advance(ttl * 6 + 1);
    let ranked = service.opportunities(&target, 1.0, 50).expect("ranked");
    assert_eq!(ranked.len(), 2);
    assert_eq!(history.item_day_rows_calls(), 2, "the backstop recomputed inline");
    assert_eq!(sales.sale_rate_each_calls(), 2);
    assert_eq!(service.snapshot_age_secs(&target), Some(0), "the backstop seeds a fresh scan");
}

/// Builds one fold with a single auction for the given item on no particular
/// day; callers pick the storage day via `save_fold`.
fn fold_with(item: &ItemRef, price: i64, qty: i64) -> DayFold {
    let mut fold = DayFold::new(item.clone());
    fold.apply(&[listing(1, item, price, qty)]);
    fold
}

#[test]
fn bulk_block_paths_match_the_per_item_paths() {
    let today = 1_000_000; // utc day 11
    let target = TargetId("us-4".into());
    let item1 = item(1);
    let item2 = item(2);
    let variant_a = ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 };
    let variant_b = ItemRef::Pet { species: 2546, level: 7, quality: 3, breed: 5 };

    let history = Arc::new(FakeHistory::default());
    // item1: three days at two price points; item2: one day.
    for offset in 0..3 {
        let mut fold = DayFold::new(item1.clone());
        fold.apply(&[listing(offset, &item1, 100, 1), listing(1_000 + offset, &item1, 120, 2)]);
        history.save_fold(&target, &item1, utc_day(today) - offset, &fold).expect("save fold");
    }
    history
        .save_fold(&target, &item2, utc_day(today), &fold_with(&item2, 90, 4))
        .expect("save fold");
    // Pet variants: each contributes to the shared species population, one
    // of them on a previous day.
    history
        .save_fold(&target, &variant_a, utc_day(today), &fold_with(&variant_a, 200, 1))
        .expect("save fold");
    history
        .save_fold(&target, &variant_b, utc_day(today) - 1, &fold_with(&variant_b, 220, 3))
        .expect("save fold");

    let sales = Arc::new(FakeSales::default());
    sales.rates.lock().unwrap().insert(item1.clone(), 0.25);
    let metrics = Arc::new(amtracker_core::services::metrics::MetricsService::new(
        history,
        Arc::new(FakeCache::default()),
        sales,
        Arc::new(FixedClock(today)),
    ));

    for span in [Span::D1, Span::D7, Span::D30] {
        let singles = vec![item1.clone(), item2.clone()];
        let bulk = metrics.blocks(&target, &singles, span).expect("bulk blocks");
        assert_eq!(
            bulk.first().cloned().flatten(),
            metrics.block(&target, &item1, span).expect("block 1")
        );
        assert_eq!(
            bulk.get(1).cloned().flatten(),
            metrics.block(&target, &item2, span).expect("block 2")
        );

        let group = vec![variant_a.clone(), variant_b.clone()];
        let merged_bulk =
            metrics.blocks_merged(&target, &[group.as_slice()], span).expect("bulk merged");
        let merged_one = metrics.block_merged(&target, &group, span).expect("merged");
        assert_eq!(merged_bulk.first().cloned().flatten(), merged_one);
    }

    // The merged pet population pools both variants: d7 folds two auctions
    // at 200×1 and 220×3, so the nearest-rank median is 220.
    let d7 = metrics
        .block_merged(&target, &[variant_a.clone(), variant_b.clone()], Span::D7)
        .expect("merged d7")
        .expect("pet population has data");
    assert_eq!(d7.n, 2);
    assert_eq!(d7.median, 210);
}

#[test]
fn feed_export_builds_canonical_entries_through_the_bulk_paths() {
    let today = 1_000_000; // utc day 11
    let target = TargetId("us-4".into());
    let item1 = item(1);
    let variant_a = ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 };
    let variant_b = ItemRef::Pet { species: 2546, level: 7, quality: 3, breed: 5 };

    let watchlist = Arc::new(FakeWatchlist::default());
    for watched in [&item1, &variant_a, &variant_b] {
        watchlist
            .add(&NewWatchlistEntry { target: target.clone(), item: watched.clone(), note: None })
            .expect("add watch");
    }

    let history = Arc::new(FakeHistory::default());
    let mut fold1 = DayFold::new(item1.clone());
    fold1.apply(&[listing(1, &item1, 100, 1), listing(2, &item1, 100, 2)]);
    history.save_fold(&target, &item1, utc_day(today), &fold1).expect("save fold");
    history
        .save_fold(&target, &variant_a, utc_day(today), &fold_with(&variant_a, 200, 1))
        .expect("save fold");
    history
        .save_fold(&target, &variant_b, utc_day(today), &fold_with(&variant_b, 220, 3))
        .expect("save fold");

    let sales = Arc::new(FakeSales::default());
    sales.rates.lock().unwrap().insert(item1.clone(), 0.5);
    let metrics = Arc::new(amtracker_core::services::metrics::MetricsService::new(
        history.clone(),
        Arc::new(FakeCache::default()),
        sales,
        Arc::new(FixedClock(today)),
    ));
    let feed = amtracker_core::services::feed_export::FeedExportService::new(
        Arc::new(FakeSnapshots::new()),
        history,
        watchlist,
        metrics,
        Arc::new(FixedClock(today)),
    )
    .build_feed(
        &target,
        &amtracker_core::services::feed_export::ExportOptions {
            spans: vec![Span::D1],
            watchlist_only: true,
        },
    )
    .expect("feed");

    assert_eq!(feed.entries.len(), 2, "one item entry + one species-grain pet entry");
    let pet_entry = feed
        .entries
        .iter()
        .find(|entry| matches!(entry.item, ItemRef::Pet { species: 2546, .. }))
        .expect("pet entry");
    assert_eq!(
        pet_entry.item,
        ItemRef::Pet { species: 2546, level: 0, quality: 0, breed: 0 },
        "variants export under the canonical species grain"
    );
    let pet_block = pet_entry.metrics.get(&Span::D1).expect("pet d1 block");
    assert_eq!(pet_block.n, 2, "both variants pooled into one population");
    assert_eq!(pet_block.median, 210);

    let item_entry = feed.entries.iter().find(|entry| entry.item == item1).expect("item entry");
    let item_block = item_entry.metrics.get(&Span::D1).expect("item d1 block");
    assert_eq!(item_block.n, 2);
    assert_eq!(item_block.median, 100);
}

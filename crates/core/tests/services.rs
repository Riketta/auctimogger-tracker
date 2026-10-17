//! Application service specs over in-memory port fakes — the hexagon exercised
//! exactly as the real adapters will drive it.

// Test fakes may unwrap/expect/index: fixed synthetic inputs, not hostile
// data (the SecOutfall precedent).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::{
    collections::HashMap,
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
        FetchOutcome,
        ItemDayRow,
        ItemRef,
        ListingKind,
        NewWatchlistEntry,
        PayloadKind,
        SnapshotMeta,
        SnapshotStatus,
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
}

#[derive(Default)]
struct FakeHistory {
    folds: Mutex<HashMap<FoldKey, DayFold>>,
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
        _item: &ItemRef,
        _since_day: i64,
    ) -> Result<Vec<amtracker_core::domain::ItemDayRow>, StoreError> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
struct FakeCache {
    invalidations: Mutex<usize>,
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
}

#[derive(Default)]
struct FakeSales {
    observes: Mutex<usize>,
    closes: Mutex<usize>,
    credits: Mutex<Vec<amtracker_core::domain::SaleCredit>>,
}

impl FakeSales {
    fn with_credits(credits: Vec<amtracker_core::domain::SaleCredit>) -> Self {
        Self { observes: Mutex::new(0), closes: Mutex::new(0), credits: Mutex::new(credits) }
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
        _item: &ItemRef,
        _days: i64,
    ) -> Result<Option<f64>, StoreError> {
        Ok(None)
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

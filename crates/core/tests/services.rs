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
        BrowseError,
        Category,
        FetchOutcome,
        ItemDayRow,
        ItemRef,
        ListingKind,
        ListingQuery,
        ListingSortKey,
        NewSnapshot,
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
        opportunity::{
            OpportunityService,
            OpportunitySort,
        },
        snapshot_browse::SnapshotBrowseService,
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

type FoldKey = (TargetId, ItemRef, i64, Category);

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
    /// Newest snapshot per (target, kind) — the freshness gate is per kind,
    /// so the fake must scope its reads the way the real store does.
    latest: HashMap<(TargetId, PayloadKind), SnapshotMeta>,
    /// Every recorded snapshot by id (the browse viewer's pinned reads).
    metas: HashMap<i64, SnapshotMeta>,
    /// Recorded listings per snapshot id, in record order.
    listings: HashMap<i64, Vec<amtracker_core::domain::NormalizedListing>>,
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
        let meta = SnapshotMeta {
            id,
            target: snapshot.target.clone(),
            kind: snapshot.kind,
            fetched_at: snapshot.fetched_at,
            source_last_modified: snapshot.source_last_modified,
            entry_count: snapshot.entry_count,
            dropped_count: snapshot.dropped_count,
            dropped_reasons: snapshot.dropped_reasons.clone(),
            status: snapshot.status,
        };
        inner.latest.insert((snapshot.target.clone(), snapshot.kind), meta.clone());
        inner.metas.insert(id, meta);
        inner.listings.insert(id, listings.to_vec());
        inner.raws.push(raw.to_vec());
        inner.listing_counts.push(listings.len() as u64);
        Ok(id)
    }

    fn latest(
        &self,
        target: &TargetId,
        kind: PayloadKind,
    ) -> Result<Option<SnapshotMeta>, StoreError> {
        Ok(self.inner.lock().unwrap().latest.get(&(target.clone(), kind)).cloned())
    }

    fn list_recent(
        &self,
        _target: &TargetId,
        _kind: PayloadKind,
        limit: usize,
    ) -> Result<Vec<SnapshotMeta>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.latest.values().take(limit).cloned().collect())
    }

    fn snapshot_by_id(&self, id: i64) -> Result<Option<SnapshotMeta>, StoreError> {
        Ok(self.inner.lock().unwrap().metas.get(&id).cloned())
    }

    fn listings_page(
        &self,
        _flavor: &amtracker_core::domain::Flavor,
        snapshot_id: i64,
        query: &amtracker_core::domain::ListingQuery,
    ) -> Result<amtracker_core::domain::ListingPage, StoreError> {
        use amtracker_core::domain::{
            ListingPage,
            ListingRow,
            ListingSortKey,
            LISTING_PAGE_SIZE,
        };
        let inner = self.inner.lock().unwrap();
        let mut rows: Vec<ListingRow> = inner
            .listings
            .get(&snapshot_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|listing| ListingRow {
                auction_id: listing.auction_id,
                item: listing.item_ref,
                quantity: listing.quantity,
                unit_price: listing.unit_price,
                bid: listing.bid,
                time_left: listing.time_left,
                name: None,
                quality: None,
                item_level: None,
            })
            .collect();
        rows.retain(|row| {
            query.item_id.is_none_or(|id| match &row.item {
                ItemRef::Item { item_id } => *item_id == id,
                ItemRef::Pet { .. } => false,
            })
        });
        let total = u64::try_from(rows.len()).unwrap_or(u64::MAX);
        let sort_key = |row: &ListingRow| match query.sort {
            ListingSortKey::Price => row.unit_price,
            ListingSortKey::Quantity => row.quantity,
            ListingSortKey::TimeLeft => row.time_left.lower_bound_secs(),
        };
        rows.sort_by(|a, b| {
            let ord = (sort_key(a), a.auction_id).cmp(&(sort_key(b), b.auction_id));
            if query.desc {
                ord.reverse()
            } else {
                ord
            }
        });
        let pages = u32::try_from(total.div_ceil(u64::try_from(LISTING_PAGE_SIZE).unwrap_or(1)))
            .unwrap_or(u32::MAX);
        let page = query.page.max(1).min(pages.max(1));
        let skip = usize::try_from(
            u64::from(page - 1) * u64::try_from(LISTING_PAGE_SIZE).unwrap_or(u64::MAX),
        )
        .unwrap_or(usize::MAX);
        let rows = rows.into_iter().skip(skip).take(LISTING_PAGE_SIZE).collect();
        Ok(ListingPage { rows, total, page, pages })
    }

    fn snapshot_class_summary(
        &self,
        _flavor: &amtracker_core::domain::Flavor,
        _snapshot_id: i64,
    ) -> Result<Vec<amtracker_core::domain::ClassSummary>, StoreError> {
        Ok(Vec::new())
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
        category: amtracker_core::domain::Category,
    ) -> Result<Option<DayFold>, StoreError> {
        Ok(self.folds.lock().unwrap().get(&(target.clone(), item.clone(), day, category)).cloned())
    }

    fn save_fold(
        &self,
        target: &TargetId,
        item: &ItemRef,
        day: i64,
        category: amtracker_core::domain::Category,
        fold: &DayFold,
    ) -> Result<(), StoreError> {
        self.folds
            .lock()
            .unwrap()
            .insert((target.clone(), item.clone(), day, category), fold.clone());
        Ok(())
    }

    fn prune_raw_before(&self, _target: &TargetId, _day: i64) -> Result<u64, StoreError> {
        Ok(0)
    }

    fn items_with_data(
        &self,
        _target: &TargetId,
        _since_day: i64,
        _category: amtracker_core::domain::Category,
    ) -> Result<Vec<ItemRef>, StoreError> {
        Ok(Vec::new())
    }

    fn item_day_rows(
        &self,
        _target: &TargetId,
        _since_day: i64,
        _category: amtracker_core::domain::Category,
    ) -> Result<Vec<amtracker_core::domain::ItemDayRow>, StoreError> {
        Ok(Vec::new())
    }

    fn item_day_rows_for_item(
        &self,
        _target: &TargetId,
        item: &ItemRef,
        since_day: i64,
        _category: amtracker_core::domain::Category,
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
        _category: amtracker_core::domain::Category,
        _span: amtracker_core::domain::Span,
    ) -> Result<Option<amtracker_core::domain::CachedBlock>, StoreError> {
        Ok(None)
    }

    fn put(
        &self,
        _target: &TargetId,
        _item: &ItemRef,
        _category: amtracker_core::domain::Category,
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

    fn invalidate_items(
        &self,
        _target: &TargetId,
        _category: amtracker_core::domain::Category,
        items: &[ItemRef],
    ) -> Result<(), StoreError> {
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
    /// Optional per-item ledger recency served by `sale_recency` (default:
    /// none — never sold).
    recencies: Mutex<HashMap<ItemRef, amtracker_core::domain::SaleRecency>>,
}

impl FakeSales {
    fn with_credits(credits: Vec<amtracker_core::domain::SaleCredit>) -> Self {
        Self {
            observes: Mutex::new(0),
            closes: Mutex::new(0),
            credits: Mutex::new(credits),
            rates: Mutex::new(HashMap::new()),
            recencies: Mutex::new(HashMap::new()),
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
        _category: amtracker_core::domain::Category,
    ) -> Result<Option<f64>, StoreError> {
        Ok(self.rates.lock().unwrap().get(item).copied())
    }

    fn sale_recency(
        &self,
        _target: &TargetId,
        item: &ItemRef,
        _category: amtracker_core::domain::Category,
    ) -> Result<Option<amtracker_core::domain::SaleRecency>, StoreError> {
        Ok(self.recencies.lock().unwrap().get(item).copied())
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
    assert_eq!(
        stored
            .latest
            .get(&(TargetId("us-4".into()), PayloadKind::Commodities))
            .expect("snapshot recorded")
            .entry_count,
        4
    );
    assert_eq!(stored.raws[0], b"raw-bytes".to_vec());
    assert_eq!(stored.listing_counts[0], 4, "listings persist with the snapshot");
    drop(stored);

    let folds = history.folds.lock().unwrap();
    let day = amtracker_core::domain::utc_day(1_000_000);
    let item1_fold = folds
        .get(&(TargetId("us-4".into()), item1.clone(), day, Category::Commodity))
        .expect("item 1 fold saved");
    assert_eq!(item1_fold.len(), 2, "re-sighted auction folds into one entry");
    let item2_fold = folds
        .get(&(TargetId("us-4".into()), item2.clone(), day, Category::Commodity))
        .expect("item 2 fold saved");
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
    assert!(snapshots.inner.lock().unwrap().latest.is_empty(), "failed cycle writes nothing");
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
            .save_fold(
                &TargetId("us-4".into()),
                &item1,
                utc_day(today) - offset,
                Category::Commodity,
                &fold,
            )
            .expect("save fold");
    }
    // Trend rows: one inside the 30-day window, one far outside it.
    *history.item_rows.lock().unwrap() = vec![
        ItemDayRow {
            item: item1.clone(),
            category: Category::Commodity,
            day: utc_day(today),
            listing_count: 1,
            qty_sum: 1,
            min: 90,
            p05: 90,
            median: 100,
            mad: 0,
        },
        ItemDayRow {
            item: item1.clone(),
            category: Category::Commodity,
            day: utc_day(today) - 40,
            listing_count: 1,
            qty_sum: 1,
            min: 500,
            p05: 500,
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
        .report(&TargetId("us-4".into()), Category::Commodity, &[Span::D1, Span::D7], 14, 30)
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
    history
        .save_fold(&target, &item1, utc_day(today), Category::Commodity, &fold)
        .expect("save fold");

    let events = Arc::new(FakeAlertEvents::default());
    let rules = Arc::new(FakeAlertRules {
        rules: Mutex::new(Vec::new()),
        next_id: Mutex::new(0),
        events: events.clone(),
    });
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
        Arc::new(FakeSnapshots::new()),
        Some(dispatched.clone()),
        clock.clone(),
        0,
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
        Arc::new(FakeSnapshots::new()),
        Some(Arc::new(FailingDispatcher)),
        clock.clone(),
        0,
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

#[test]
fn days_since_last_sale_alert_fires_and_never_sold_never_fires() {
    use amtracker_core::{
        domain::{
            AlertCondition,
            NewAlertRule,
            SaleRecency,
        },
        ports::AlertEventRepository,
        services::alert::AlertEvaluationService,
    };

    let today = 1_000_000; // utc day 11
    let target = TargetId("us-4".into());
    let stale_item = item(3); // last sold 12 days ago
    let fresh_item = item(4); // sold today
    let never_sold = item(5);

    let history = Arc::new(FakeHistory::default());
    let events = Arc::new(FakeAlertEvents::default());
    let rules = Arc::new(FakeAlertRules {
        rules: Mutex::new(Vec::new()),
        next_id: Mutex::new(0),
        events: events.clone(),
    });
    let sales = Arc::new(FakeSales::default());
    sales.recencies.lock().unwrap().insert(
        stale_item.clone(),
        SaleRecency {
            last_sale_day: amtracker_core::domain::utc_day(today) - 12,
            closures_since: 7,
        },
    );
    sales.recencies.lock().unwrap().insert(
        fresh_item.clone(),
        SaleRecency { last_sale_day: amtracker_core::domain::utc_day(today), closures_since: 0 },
    );
    for item_ref in [&stale_item, &fresh_item, &never_sold] {
        rules
            .add(&NewAlertRule {
                target: target.clone(),
                item: item_ref.clone(),
                condition: AlertCondition::DaysSinceLastSaleAbove { days: 10 },
                note: None,
            })
            .expect("add rule");
    }

    let clock = MutableClock::new(today);
    let service = AlertEvaluationService::new(
        rules.clone(),
        events.clone(),
        Arc::new(amtracker_core::services::metrics::MetricsService::new(
            history,
            Arc::new(FakeCache::default()),
            sales.clone(),
            clock.clone(),
        )),
        sales,
        Arc::new(FakeSnapshots::new()),
        None,
        clock.clone(),
        0,
    );

    let run = service.evaluate(&target, 21_600).expect("evaluate");
    assert_eq!(run.evaluated, 3);
    assert_eq!(run.fired, 1, "only the 12-day-stale item fires");
    let stored = events.recent(&target, 10).expect("recent");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].observed, Some(12.0));
    assert!(stored[0].message.contains("12 days since last sale"), "{}", stored[0].message);
}

#[test]
fn stale_feed_snapshot_skips_rules_and_counts_them() {
    use amtracker_core::{
        domain::{
            AlertCondition,
            NewAlertRule,
            NewSnapshot,
            PayloadKind,
            SnapshotStatus,
        },
        ports::AlertEventRepository,
        services::alert::AlertEvaluationService,
    };

    let today = 1_000_000;
    let target = TargetId("us-4".into());
    let history = Arc::new(FakeHistory::default());
    let events = Arc::new(FakeAlertEvents::default());
    let rules = Arc::new(FakeAlertRules {
        rules: Mutex::new(Vec::new()),
        next_id: Mutex::new(0),
        events: events.clone(),
    });
    rules
        .add(&NewAlertRule {
            target: target.clone(),
            item: item(1),
            condition: AlertCondition::PriceBelow { copper: 150 },
            note: None,
        })
        .expect("add rule");

    let snapshots = Arc::new(FakeSnapshots::new());
    // One stale feed: the newest snapshot is ~2.3 days old, past the 48h gate.
    snapshots
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: today - 200_000,
                source_last_modified: None,
                entry_count: 1,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::default(),
                status: SnapshotStatus::Ok,
            },
            &[],
            b"".as_slice(),
        )
        .expect("record stale snapshot");

    let clock = MutableClock::new(today);
    let sales = Arc::new(FakeSales::default());
    let service = AlertEvaluationService::new(
        rules.clone(),
        events.clone(),
        Arc::new(amtracker_core::services::metrics::MetricsService::new(
            history,
            Arc::new(FakeCache::default()),
            sales.clone(),
            clock.clone(),
        )),
        sales,
        snapshots,
        None,
        clock.clone(),
        48 * 60 * 60,
    );

    // The rule would fire on its block (the fixture fold is 100 ≤ 150), but
    // the stale gate must refuse — and count the skip.
    let run = service.evaluate(&target, 21_600).expect("evaluate");
    assert_eq!(run.evaluated, 1);
    assert_eq!(run.fired, 0);
    assert_eq!(run.skipped_stale, 1);
    let recent = events.recent(&target, 10).expect("recent");
    assert_eq!(recent.as_slice(), &[], "stale gate must not record events");
}

/// Seeds a `PriceBelow` rule plus its Commodity-category fold so the rule
/// would fire without the freshness gate.
fn price_below_fixture(
    target: &TargetId,
    today: i64,
) -> (Arc<FakeHistory>, Arc<FakeAlertRules>, Arc<FakeAlertEvents>) {
    use amtracker_core::domain::{
        AlertCondition,
        NewAlertRule,
    };

    let history = Arc::new(FakeHistory::default());
    let item1 = item(1);
    let mut fold = DayFold::new(item1.clone());
    fold.apply(&[listing(1, &item1, 100, 1)]);
    history
        .save_fold(
            target,
            &item1,
            amtracker_core::domain::utc_day(today),
            Category::Commodity,
            &fold,
        )
        .expect("save fold");
    let events = Arc::new(FakeAlertEvents::default());
    let rules = Arc::new(FakeAlertRules {
        rules: Mutex::new(Vec::new()),
        next_id: Mutex::new(0),
        events: events.clone(),
    });
    rules
        .add(&NewAlertRule {
            target: target.clone(),
            item: item1,
            condition: AlertCondition::PriceBelow { copper: 150 },
            note: None,
        })
        .expect("add rule");
    (history, rules, events)
}

#[test]
fn freshness_gate_lets_exact_age_pass_and_blocks_beyond_it() {
    use amtracker_core::{
        domain::NewSnapshot,
        ports::AlertEventRepository,
        services::alert::AlertEvaluationService,
    };

    let today = 1_000_000;
    let target = TargetId("us-4".into());
    let gate = 48 * 60 * 60;
    let (history, rules, events) = price_below_fixture(&target, today);

    let snapshots = Arc::new(FakeSnapshots::new());
    // Age exactly == gate: fresh enough (the gate compares strictly older).
    snapshots
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: today - gate,
                source_last_modified: None,
                entry_count: 1,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::default(),
                status: SnapshotStatus::Ok,
            },
            &[],
            b"".as_slice(),
        )
        .expect("record boundary snapshot");

    let clock = MutableClock::new(today);
    let sales = Arc::new(FakeSales::default());
    let service = AlertEvaluationService::new(
        rules.clone(),
        events.clone(),
        Arc::new(amtracker_core::services::metrics::MetricsService::new(
            history,
            Arc::new(FakeCache::default()),
            sales.clone(),
            clock.clone(),
        )),
        sales,
        snapshots,
        None,
        clock.clone(),
        gate,
    );

    let run = service.evaluate(&target, 0).expect("evaluate at the boundary");
    assert_eq!(run.fired, 1, "age == gate still fires");
    assert_eq!(run.skipped_stale, 0);

    // One second older: age > gate now, so the very same rule skips (cooldown
    // disabled to isolate the gate).
    clock.advance(1);
    let run = service.evaluate(&target, 0).expect("evaluate past the boundary");
    assert_eq!(run.evaluated, 1);
    assert_eq!(run.fired, 0);
    assert_eq!(run.skipped_stale, 1);
    let recent = events.recent(&target, 10).expect("recent");
    assert_eq!(recent.len(), 1, "only the boundary pass recorded an event");
}

#[test]
fn freshness_gate_ages_from_source_last_modified() {
    use amtracker_core::{
        domain::NewSnapshot,
        services::alert::AlertEvaluationService,
    };

    let today = 1_000_000;
    let target = TargetId("us-4".into());
    let (history, rules, events) = price_below_fixture(&target, today);

    let snapshots = Arc::new(FakeSnapshots::new());
    // The fetch stamp alone would read as ~4.6 days stale, but the source's
    // Last-Modified is fresh — the gate must anchor on the data's own stamp.
    snapshots
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: today - 400_000,
                source_last_modified: Some(today - 60),
                entry_count: 1,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::default(),
                status: SnapshotStatus::Ok,
            },
            &[],
            b"".as_slice(),
        )
        .expect("record snapshot");

    let clock = MutableClock::new(today);
    let sales = Arc::new(FakeSales::default());
    let service = AlertEvaluationService::new(
        rules.clone(),
        events.clone(),
        Arc::new(amtracker_core::services::metrics::MetricsService::new(
            history,
            Arc::new(FakeCache::default()),
            sales.clone(),
            clock.clone(),
        )),
        sales,
        snapshots,
        None,
        clock.clone(),
        48 * 60 * 60,
    );

    let run = service.evaluate(&target, 0).expect("evaluate");
    assert_eq!(run.fired, 1, "last-modified anchored age is fresh");
    assert_eq!(run.skipped_stale, 0);
}

#[test]
fn no_snapshots_read_as_not_stale() {
    use amtracker_core::services::alert::AlertEvaluationService;

    let today = 1_000_000;
    let target = TargetId("us-4".into());
    let gate = 48 * 60 * 60;
    let (history, rules, events) = price_below_fixture(&target, today);
    let snapshots = Arc::new(FakeSnapshots::new());

    let clock = MutableClock::new(today);
    let sales = Arc::new(FakeSales::default());
    let service = AlertEvaluationService::new(
        rules.clone(),
        events.clone(),
        Arc::new(amtracker_core::services::metrics::MetricsService::new(
            history,
            Arc::new(FakeCache::default()),
            sales.clone(),
            clock.clone(),
        )),
        sales,
        snapshots,
        None,
        clock.clone(),
        gate,
    );

    let run = service.evaluate(&target, 0).expect("evaluate");
    assert_eq!(run.fired, 1, "no snapshots = no data yet, not stale");
    assert_eq!(run.skipped_stale, 0);
}

#[test]
fn freshness_gate_is_per_market_kind() {
    use amtracker_core::{
        domain::{
            AlertCondition,
            Category,
            NewAlertRule,
            NewSnapshot,
        },
        services::alert::AlertEvaluationService,
    };

    let today = 1_000_000;
    let target = TargetId("us-4".into());
    let commodity_item = item(7);
    let auction_item = item(8);

    // Fresh commodity evidence for item 7; fresh-per-fold auctions evidence
    // for item 8 (only its market's feed will be stale).
    let history = Arc::new(FakeHistory::default());
    let mut commodity_fold = DayFold::new(commodity_item.clone());
    commodity_fold.apply(&[listing(1, &commodity_item, 100, 1)]);
    history
        .save_fold(
            &target,
            &commodity_item,
            amtracker_core::domain::utc_day(today),
            Category::Commodity,
            &commodity_fold,
        )
        .expect("save commodity fold");
    let mut auction_fold = DayFold::new(auction_item.clone());
    auction_fold.apply(&[listing(2, &auction_item, 100, 1)]);
    history
        .save_fold(
            &target,
            &auction_item,
            amtracker_core::domain::utc_day(today),
            Category::Item,
            &auction_fold,
        )
        .expect("save item fold");

    let events = Arc::new(FakeAlertEvents::default());
    let rules = Arc::new(FakeAlertRules {
        rules: Mutex::new(Vec::new()),
        next_id: Mutex::new(0),
        events: events.clone(),
    });
    for item_ref in [&commodity_item, &auction_item] {
        rules
            .add(&NewAlertRule {
                target: target.clone(),
                item: item_ref.clone(),
                condition: AlertCondition::PriceBelow { copper: 150 },
                note: None,
            })
            .expect("add rule");
    }

    let snapshots = Arc::new(FakeSnapshots::new());
    let snap = |kind, fetched_at| NewSnapshot {
        target: target.clone(),
        kind,
        fetched_at,
        source_last_modified: None,
        entry_count: 1,
        dropped_count: 0,
        dropped_reasons: std::collections::BTreeMap::default(),
        status: SnapshotStatus::Ok,
    };
    // Commodities fresh (now); auctions stale (~2.3 days). The old
    // newest-of-both gate would have armed BOTH rules on the fresh feed.
    snapshots
        .record(&snap(PayloadKind::Commodities, today), &[], b"".as_slice())
        .expect("record fresh commodities snapshot");
    snapshots
        .record(&snap(PayloadKind::Auctions, today - 200_000), &[], b"".as_slice())
        .expect("record stale auctions snapshot");

    let clock = MutableClock::new(today);
    let sales = Arc::new(FakeSales::default());
    let service = AlertEvaluationService::new(
        rules.clone(),
        events.clone(),
        Arc::new(amtracker_core::services::metrics::MetricsService::new(
            history,
            Arc::new(FakeCache::default()),
            sales.clone(),
            clock.clone(),
        )),
        sales,
        snapshots,
        None,
        clock.clone(),
        48 * 60 * 60,
    );

    let run = service.evaluate(&target, 0).expect("evaluate");
    assert_eq!(run.evaluated, 2);
    assert_eq!(run.fired, 1, "the commodity rule fires on its own fresh feed");
    assert_eq!(
        run.skipped_stale, 1,
        "the auctions rule cannot fire on the sibling feed's freshness"
    );
    let stored = events.recent(&target, 10).expect("recent");
    assert_eq!(stored.len(), 1);
    assert!(stored[0].message.contains("[commodity]"), "{}", stored[0].message);
}

#[test]
fn merged_pet_blocks_pool_the_newest_variant_sale() {
    use amtracker_core::{
        domain::{
            Category,
            SaleRecency,
            Span,
        },
        services::metrics::MetricsService,
    };

    let today = 1_000_000;
    let target = TargetId("us-4".into());
    let variant_a =
        amtracker_core::domain::ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 4 };
    let variant_b =
        amtracker_core::domain::ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 };

    let history = Arc::new(FakeHistory::default());
    for (auction_id, variant) in [(10, &variant_a), (11, &variant_b)] {
        let mut fold = DayFold::new(variant.clone());
        fold.apply(&[listing(auction_id, variant, 900_000, 1)]);
        history
            .save_fold(
                &target,
                variant,
                amtracker_core::domain::utc_day(today),
                Category::Pet,
                &fold,
            )
            .expect("save variant fold");
    }
    let sales = Arc::new(FakeSales::default());
    sales.recencies.lock().unwrap().insert(
        variant_a.clone(),
        SaleRecency {
            last_sale_day: amtracker_core::domain::utc_day(today) - 30,
            closures_since: 5,
        },
    );
    sales.recencies.lock().unwrap().insert(
        variant_b.clone(),
        SaleRecency {
            last_sale_day: amtracker_core::domain::utc_day(today) - 2,
            closures_since: 3,
        },
    );

    let clock = Arc::new(FixedClock(today));
    let metrics =
        MetricsService::new(history, Arc::new(FakeCache::default()), sales.clone(), clock);
    let block = metrics
        .block_merged(&target, &[variant_a.clone(), variant_b.clone()], Category::Pet, Span::D1)
        .expect("merged block")
        .expect("block from seeded folds");
    assert_eq!(
        block.days_since_last_sale,
        Some(2),
        "MAX pooling: the newest variant's sale anchors the species"
    );
    // The port-default pool sums the per-member readings (5 + 3); the exact
    // pooled closure sum is the store's query (see the infra test).
    assert_eq!(block.closures_since_last_sale, Some(8));
}

#[derive(Default)]
struct FakeAlertRules {
    rules: Mutex<Vec<amtracker_core::domain::AlertRule>>,
    next_id: Mutex<i64>,
    events: Arc<FakeAlertEvents>,
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

    fn record_firing(
        &self,
        event: &amtracker_core::domain::NewAlertEvent,
    ) -> Result<i64, StoreError> {
        let event_id = self.events.record(event);
        for rule in self.rules.lock().unwrap().iter_mut() {
            if rule.id == event.rule_id {
                rule.last_fired_at = Some(event.fired_at);
            }
        }
        Ok(event_id)
    }
}

#[derive(Default)]
struct FakeAlertEvents {
    events: Mutex<Vec<amtracker_core::domain::AlertEvent>>,
}

impl FakeAlertEvents {
    fn record(&self, event: &amtracker_core::domain::NewAlertEvent) -> i64 {
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
        id
    }
}

impl AlertEventRepository for FakeAlertEvents {
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
        _category: amtracker_core::domain::Category,
    ) -> Result<Option<DayFold>, StoreError> {
        Ok(None)
    }

    fn save_fold(
        &self,
        _target: &TargetId,
        _item: &ItemRef,
        _day: i64,
        _category: amtracker_core::domain::Category,
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
        _category: amtracker_core::domain::Category,
    ) -> Result<Vec<ItemRef>, StoreError> {
        Ok(Vec::new())
    }

    fn item_day_rows(
        &self,
        _target: &TargetId,
        _since_day: i64,
        _category: amtracker_core::domain::Category,
    ) -> Result<Vec<ItemDayRow>, StoreError> {
        *self.item_day_rows_calls.lock().unwrap() += 1;
        Ok(self.rows.clone())
    }

    fn item_day_rows_for_item(
        &self,
        _target: &TargetId,
        _item: &ItemRef,
        _since_day: i64,
        _category: amtracker_core::domain::Category,
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
        _category: amtracker_core::domain::Category,
    ) -> Result<Option<f64>, StoreError> {
        Ok(None)
    }

    fn sale_rate_each(
        &self,
        _target: &TargetId,
        items: &[ItemRef],
        _days: i64,
        _category: amtracker_core::domain::Category,
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
            sight_retention_days: 0,
            archive_dir: "raw".into(),
            feed_dir: "feeds".into(),
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
            max_data_age_secs: 0,
        },
        catalog: amtracker_core::config::CatalogConfig::default(),
        targets: vec![target()],
    }
}

fn day_row(item_id: i64, day: i64, listing_count: u64, min: i64, median: i64) -> ItemDayRow {
    ItemDayRow {
        item: item(item_id),
        category: Category::Commodity,
        day,
        listing_count,
        qty_sum: 0,
        min,
        p05: min,
        median,
        mad: 500,
    }
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

    let covered = service.refresh(&target, Category::Commodity).expect("refresh");
    assert_eq!(covered, 2, "both scanned items are covered");

    for _ in 0..3 {
        let ranked = service
            .opportunities(&target, Category::Commodity, 1.0, 50, OpportunitySort::Score)
            .expect("ranked");
        assert_eq!(ranked.len(), 2);
        let overview = service.overview(&target, Category::Commodity, 15).expect("overview");
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
    let inline = cold
        .opportunities(&target, Category::Commodity, 1.0, 50, OpportunitySort::Score)
        .expect("inline ranked");

    let (prepared, history, sales, _clock) = opportunity_fixture(1_000_000);
    prepared.refresh(&target, Category::Commodity).expect("refresh");
    let warmed = prepared
        .opportunities(&target, Category::Commodity, 1.0, 50, OpportunitySort::Score)
        .expect("prepared ranked");

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
fn a_single_troll_floor_cannot_fake_a_deep_discount() {
    // One 1-copper listing among a busy day: the displayed floor keeps the
    // true min, but discount scores against the day's p05 so the outlier
    // cannot top the ranking with a fake ~100% discount.
    let target = TargetId("us-4".into());
    let today = amtracker_core::domain::utc_day(1_000_000);
    let rows = vec![ItemDayRow {
        item: item(1),
        category: Category::Commodity,
        day: today,
        listing_count: 1_000,
        qty_sum: 1_000,
        min: 1,
        p05: 90_000,
        median: 100_000,
        mad: 5_000,
    }];
    let history = Arc::new(CountingHistory::new(rows));
    let sales = Arc::new(CountingSales::default());
    let clock = MutableClock::new(1_000_000);
    let service = OpportunityService::new(history, sales, clock, &opportunity_config());

    let ranked = service
        .opportunities(&target, Category::Commodity, 0.0, 50, OpportunitySort::Score)
        .expect("ranked");
    let top = ranked.first().expect("rankable");
    assert_eq!(top.floor_now, 1, "the displayed floor stays the true min");
    assert_eq!(top.median7, 100_000);
    assert_eq!(top.discount, 0.1, "discount reads p05, not the troll min");
}

#[test]
fn stale_snapshot_is_still_served_without_recompute() {
    let target = TargetId("us-4".into());
    let ttl = amtracker_core::services::opportunity::SCAN_TTL_SECONDS;
    let (service, history, sales, clock) = opportunity_fixture(1_000_000);
    service.refresh(&target, Category::Commodity).expect("refresh");
    assert_eq!(service.snapshot_age_secs(&target, Category::Commodity), Some(0));

    // Past the scheduler TTL but far inside the staleness backstop: the
    // stale snapshot keeps serving.
    clock.advance(ttl + 60);
    assert_eq!(service.snapshot_age_secs(&target, Category::Commodity), Some(ttl + 60));

    service
        .opportunities(&target, Category::Commodity, 1.0, 50, OpportunitySort::Score)
        .expect("ranked from the stale snapshot");
    service.overview(&target, Category::Commodity, 15).expect("overview from the stale snapshot");
    assert_eq!(history.item_day_rows_calls(), 1, "a stale snapshot must not recompute inline");
    assert_eq!(sales.sale_rate_each_calls(), 1);
}

#[test]
fn cold_cache_falls_back_to_inline_compute() {
    let target = TargetId("us-4".into());
    let (service, history, sales, _clock) = opportunity_fixture(1_000_000);

    let ranked = service
        .opportunities(&target, Category::Commodity, 1.0, 50, OpportunitySort::Score)
        .expect("ranked");
    assert_eq!(ranked.len(), 2);
    assert_eq!(history.item_day_rows_calls(), 1, "cold cache computes inline");
    assert_eq!(sales.sale_rate_each_calls(), 1);

    // The inline compute also seeds the cache: the next request reads nothing.
    service.overview(&target, Category::Commodity, 15).expect("overview");
    assert_eq!(history.item_day_rows_calls(), 1);
    assert_eq!(sales.sale_rate_each_calls(), 1);
}

#[test]
fn refresh_debounces_inside_the_window_and_recomputes_outside() {
    let target = TargetId("us-4".into());
    let (service, history, sales, clock) = opportunity_fixture(1_000_000);
    let covered = service.refresh(&target, Category::Commodity).expect("first refresh");
    assert_eq!(covered, 2);

    clock.advance(10);
    let covered = service.refresh(&target, Category::Commodity).expect("debounced refresh");
    assert_eq!(covered, 2, "debounced refresh reports the cached coverage");
    assert_eq!(history.item_day_rows_calls(), 1, "inside the debounce window no work happens");
    assert_eq!(sales.sale_rate_each_calls(), 1);

    clock.advance(21); // 31s after the first refresh
    let covered = service.refresh(&target, Category::Commodity).expect("post-debounce refresh");
    assert_eq!(covered, 2);
    assert_eq!(history.item_day_rows_calls(), 2, "outside the window the scan recomputes");
    assert_eq!(sales.sale_rate_each_calls(), 2);
}

#[test]
fn a_failed_refresh_keeps_the_previous_snapshot_serving() {
    let target = TargetId("us-4".into());
    let (service, history, sales, clock) = opportunity_fixture(1_000_000);
    service.refresh(&target, Category::Commodity).expect("first refresh");

    clock.advance(31); // past the debounce, so the next refresh really runs
    sales.set_sale_rate_each_failure(true);
    assert!(
        service.refresh(&target, Category::Commodity).is_err(),
        "the failed sales lookup surfaces as an Err"
    );

    // The failed pass must not evict or poison the cached artifact: ranking
    // and overview keep serving the previous snapshot without store reads.
    let ranked = service
        .opportunities(&target, Category::Commodity, 1.0, 50, OpportunitySort::Score)
        .expect("stale ranked");
    assert_eq!(ranked.len(), 2);
    service.overview(&target, Category::Commodity, 15).expect("stale overview");
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
    assert_eq!(
        service.snapshot_age_secs(&target, Category::Commodity),
        None,
        "cold cache has no age"
    );
    service.refresh(&target, Category::Commodity).expect("refresh");
    assert_eq!(service.snapshot_age_secs(&target, Category::Commodity), Some(0));
}

#[test]
fn requests_self_heal_once_a_snapshot_passes_the_backstop() {
    let target = TargetId("us-4".into());
    let ttl = amtracker_core::services::opportunity::SCAN_TTL_SECONDS;
    let (service, history, sales, clock) = opportunity_fixture(1_000_000);
    service.refresh(&target, Category::Commodity).expect("refresh");

    // A dead scheduler would leave the snapshot aging forever; past six TTLs
    // the request path recomputes inline instead of serving it.
    clock.advance(ttl * 6 + 1);
    let ranked = service
        .opportunities(&target, Category::Commodity, 1.0, 50, OpportunitySort::Score)
        .expect("ranked");
    assert_eq!(ranked.len(), 2);
    assert_eq!(history.item_day_rows_calls(), 2, "the backstop recomputed inline");
    assert_eq!(sales.sale_rate_each_calls(), 2);
    assert_eq!(
        service.snapshot_age_secs(&target, Category::Commodity),
        Some(0),
        "the backstop seeds a fresh scan"
    );
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
        history
            .save_fold(&target, &item1, utc_day(today) - offset, Category::Commodity, &fold)
            .expect("save fold");
    }
    history
        .save_fold(&target, &item2, utc_day(today), Category::Commodity, &fold_with(&item2, 90, 4))
        .expect("save fold");
    // Pet variants: each contributes to the shared species population, one
    // of them on a previous day.
    history
        .save_fold(
            &target,
            &variant_a,
            utc_day(today),
            Category::Pet,
            &fold_with(&variant_a, 200, 1),
        )
        .expect("save fold");
    history
        .save_fold(
            &target,
            &variant_b,
            utc_day(today) - 1,
            Category::Pet,
            &fold_with(&variant_b, 220, 3),
        )
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
        let bulk =
            metrics.blocks(&target, &singles, Category::Commodity, span).expect("bulk blocks");
        assert_eq!(
            bulk.first().cloned().flatten(),
            metrics.block(&target, &item1, Category::Commodity, span).expect("block 1")
        );
        assert_eq!(
            bulk.get(1).cloned().flatten(),
            metrics.block(&target, &item2, Category::Commodity, span).expect("block 2")
        );

        let group = vec![variant_a.clone(), variant_b.clone()];
        let merged_bulk = metrics
            .blocks_merged(&target, &[group.as_slice()], Category::Pet, span)
            .expect("bulk merged");
        let merged_one =
            metrics.block_merged(&target, &group, Category::Pet, span).expect("merged");
        assert_eq!(merged_bulk.first().cloned().flatten(), merged_one);
    }

    // The merged pet population pools both variants: d7 folds two auctions
    // at 200×1 and 220×3, so the nearest-rank median is 220.
    let d7 = metrics
        .block_merged(&target, &[variant_a.clone(), variant_b.clone()], Category::Pet, Span::D7)
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
    history
        .save_fold(&target, &item1, utc_day(today), Category::Commodity, &fold1)
        .expect("save fold");
    history
        .save_fold(
            &target,
            &variant_a,
            utc_day(today),
            Category::Pet,
            &fold_with(&variant_a, 200, 1),
        )
        .expect("save fold");
    history
        .save_fold(
            &target,
            &variant_b,
            utc_day(today),
            Category::Pet,
            &fold_with(&variant_b, 220, 3),
        )
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

#[test]
fn feed_export_attributes_every_span_to_its_own_item_across_many_singles() {
    // More single groups than spans, and two spans: the shape where a
    // span-major matrix consumed as group-major cross-contaminates early
    // entries and silently drops the tail.
    let today = 1_000_000; // utc day 11
    let target = TargetId("us-4".into());
    let item1 = item(1);
    let item2 = item(2);
    let item3 = item(3);
    let yesterday = utc_day(today) - 1;

    let watchlist = Arc::new(FakeWatchlist::default());
    for watched in [&item1, &item2, &item3] {
        watchlist
            .add(&NewWatchlistEntry { target: target.clone(), item: watched.clone(), note: None })
            .expect("add watch");
    }

    let history = Arc::new(FakeHistory::default());
    let save = |item: &ItemRef, day: i64, price: i64, count: i64| {
        let listings: Vec<_> = (0..count).map(|i| listing(1_000 + i, item, price, 1)).collect();
        let mut fold = DayFold::new(item.clone());
        fold.apply(&listings);
        history.save_fold(&target, item, day, Category::Commodity, &fold).expect("save fold");
    };
    save(&item1, utc_day(today), 100, 2);
    save(&item1, yesterday, 100, 1); // d7: n=3, median 100
    save(&item2, utc_day(today), 500, 1);
    save(&item2, yesterday, 900, 1); // d7: n=2, median 700
    save(&item3, utc_day(today), 900, 1); // d7: n=1, median 900

    let sales = Arc::new(FakeSales::default());
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
            spans: vec![Span::D1, Span::D7],
            watchlist_only: true,
        },
    )
    .expect("feed");

    assert_eq!(feed.entries.len(), 3, "every single exports, none dropped");
    let expected = [(1i64, 2i64, 100i64, 3i64, 100i64), (2, 1, 500, 2, 700), (3, 1, 900, 1, 900)];
    for (id, d1_n, d1_median, d7_n, d7_median) in expected {
        let entry = feed
            .entries
            .iter()
            .find(|entry| entry.item == ItemRef::Item { item_id: id })
            .unwrap_or_else(|| panic!("entry for item {id} missing"));
        let d1 = entry.metrics.get(&Span::D1).unwrap_or_else(|| panic!("item {id} d1 missing"));
        assert_eq!((d1.n, d1.median), (d1_n, d1_median), "item {id} d1 block");
        let d7 = entry.metrics.get(&Span::D7).unwrap_or_else(|| panic!("item {id} d7 missing"));
        assert_eq!((d7.n, d7.median), (d7_n, d7_median), "item {id} d7 block");
    }
}

// ------------------------------------------------------- snapshot browsing

fn browse_snapshot(target: &TargetId, kind: PayloadKind, at: i64) -> NewSnapshot {
    NewSnapshot {
        target: target.clone(),
        kind,
        fetched_at: at,
        source_last_modified: None,
        entry_count: 3,
        dropped_count: 0,
        dropped_reasons: BTreeMap::new(),
        status: SnapshotStatus::Ok,
    }
}

fn browse_listing(
    auction_id: i64,
    item: &ItemRef,
    price: i64,
) -> amtracker_core::domain::NormalizedListing {
    amtracker_core::domain::NormalizedListing {
        auction_id,
        item_ref: item.clone(),
        kind: ListingKind::Commodity,
        quantity: 1,
        unit_price: price,
        bid: None,
        time_left: TimeLeftBand::Long,
        context: None,
        bonus_lists: Vec::new(),
    }
}

/// The viewer's snapshot resolution: absent id serves the newest snapshot of
/// the pair, an explicit id must exist AND match the pair, and missing data
/// is a distinct error from a malformed link.
#[test]
fn snapshot_browse_resolves_latest_and_validates_pins() {
    let snapshots = Arc::new(FakeSnapshots::new());
    let t1 = TargetId("us-4".into());
    let t2 = TargetId("eu-9".into());
    let first = snapshots
        .record(&browse_snapshot(&t1, PayloadKind::Commodities, 1_000), &[], b"a")
        .expect("record first");
    let second = snapshots
        .record(&browse_snapshot(&t1, PayloadKind::Commodities, 2_000), &[], b"b")
        .expect("record second");
    snapshots
        .record(&browse_snapshot(&t2, PayloadKind::Auctions, 3_000), &[], b"c")
        .expect("record other pair");
    let service = SnapshotBrowseService::new(snapshots);

    let latest = service.resolve(&t1, PayloadKind::Commodities, None).expect("latest");
    assert_eq!(latest.id, second, "absent id serves the newest snapshot");

    let pinned = service.resolve(&t1, PayloadKind::Commodities, Some(first)).expect("pinned");
    assert_eq!(pinned.id, first, "an explicit id pins the historical view");

    assert_eq!(
        service.resolve(&t1, PayloadKind::Commodities, Some(999)),
        Err(BrowseError::UnknownSnapshot),
        "unknown ids are their own error"
    );
    assert_eq!(
        service.resolve(&t1, PayloadKind::Auctions, Some(first)),
        Err(BrowseError::SnapshotMismatch),
        "a real id for another pair is a malformed link"
    );
    assert_eq!(
        service.resolve(&TargetId("bogus".into()), PayloadKind::Commodities, None),
        Err(BrowseError::NoSnapshots),
        "no data is distinct from a bad link"
    );
}

/// The service clamps pages into the non-empty range (stale next links land
/// on real data) and passes filters/sort/direction through to the port.
#[test]
fn snapshot_browse_clamps_pages_and_passes_the_query_through() {
    let snapshots = Arc::new(FakeSnapshots::new());
    let t1 = TargetId("us-4".into());
    let item = ItemRef::Item { item_id: 2589 };
    let pet = ItemRef::Pet { species: 42, level: 3, quality: 2, breed: 5 };
    let listings = [
        browse_listing(1, &item, 300),
        browse_listing(2, &item, 100),
        browse_listing(3, &pet, 200),
    ];
    let snapshot_id = snapshots
        .record(&browse_snapshot(&t1, PayloadKind::Commodities, 1_000), &listings, b"a")
        .expect("record");
    let service = SnapshotBrowseService::new(snapshots);

    let default_query = ListingQuery {
        name_prefix: None,
        item_id: None,
        class: None,
        quality: None,
        sort: ListingSortKey::Price,
        desc: false,
        page: 1,
    };

    let page = service
        .page(&amtracker_core::domain::Flavor::default(), snapshot_id, default_query.clone())
        .expect("page");
    let prices: Vec<i64> = page.rows.iter().map(|row| row.unit_price).collect();
    assert_eq!(prices, [100, 200, 300], "price ascending is the default order");

    let desc_query = ListingQuery { desc: true, ..default_query.clone() };
    let page = service
        .page(&amtracker_core::domain::Flavor::default(), snapshot_id, desc_query)
        .expect("desc");
    let prices: Vec<i64> = page.rows.iter().map(|row| row.unit_price).collect();
    assert_eq!(prices, [300, 200, 100], "descending flips the order");

    let far_query = ListingQuery { page: 99, ..default_query.clone() };
    let page = service
        .page(&amtracker_core::domain::Flavor::default(), snapshot_id, far_query)
        .expect("far page");
    assert_eq!(page.page, 1, "a page beyond the end clamps to the last page");
    assert_eq!(page.rows.len(), 3, "the clamped page still serves rows");

    let zero_query = ListingQuery { page: 0, ..default_query.clone() };
    let page = service
        .page(&amtracker_core::domain::Flavor::default(), snapshot_id, zero_query)
        .expect("zero page");
    assert_eq!(page.page, 1, "page 0 clamps to page 1");

    let pet_query = ListingQuery { item_id: Some(42), ..default_query };
    let page = service
        .page(&amtracker_core::domain::Flavor::default(), snapshot_id, pet_query)
        .expect("item filter");
    assert_eq!(page.total, 0, "an item-id filter never matches pet identities");
}

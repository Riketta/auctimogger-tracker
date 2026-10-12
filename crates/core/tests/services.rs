//! Application service specs over in-memory port fakes — the hexagon exercised
//! exactly as the real adapters will drive it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use amtracker_core::config::{TargetConfig, TargetKind};
use amtracker_core::domain::{
    FetchOutcome, ItemRef, ListingKind, NewWatchlistEntry, PayloadKind, SnapshotMeta,
    SnapshotStatus, TargetId, TimeLeftBand,
};
use amtracker_core::error::{CycleError, SourceError, StoreError};
use amtracker_core::fold::DayFold;
use amtracker_core::ports::{
    AuctionSourcePort, ClockPort, PriceHistoryRepository, SnapshotRepository, WatchlistRepository,
};
use amtracker_core::services::cycle::{CycleOutcome, IngestCycleService};
use amtracker_core::services::watchlist::WatchlistService;

// ------------------------------------------------------------------ helpers

fn target() -> TargetConfig {
    TargetConfig {
        id: TargetId("us-4".into()),
        kind: TargetKind::BlizzardConnectedRealm {
            region: "us".into(),
            connected_realm_id: 4,
        },
        auth: None,
        poll: amtracker_core::config::PollConfig {
            commodities_secs: 3600,
            auctions_secs: 1800,
        },
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
        self.conditional_gets
            .lock()
            .unwrap()
            .push(if_modified_since);
        self.outcomes
            .lock()
            .unwrap()
            .pop()
            .expect("scripted source outcome missing")
    }
}

#[derive(Default)]
struct FakeSnapshotsInner {
    latest: Option<SnapshotMeta>,
    raws: Vec<Vec<u8>>,
    next_id: i64,
}

struct FakeSnapshots {
    inner: Mutex<FakeSnapshotsInner>,
}

impl FakeSnapshots {
    fn new() -> Self {
        Self {
            inner: Mutex::new(FakeSnapshotsInner::default()),
        }
    }
}

impl SnapshotRepository for FakeSnapshots {
    fn record(
        &self,
        snapshot: &amtracker_core::domain::NewSnapshot,
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
        Ok(self
            .folds
            .lock()
            .unwrap()
            .get(&(target.clone(), item.clone(), day))
            .cloned())
    }

    fn save_fold(
        &self,
        target: &TargetId,
        item: &ItemRef,
        day: i64,
        fold: &DayFold,
    ) -> Result<(), StoreError> {
        self.folds
            .lock()
            .unwrap()
            .insert((target.clone(), item.clone(), day), fold.clone());
        Ok(())
    }

    fn prune_raw_before(&self, _target: &TargetId, _day: i64) -> Result<u64, StoreError> {
        Ok(0)
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
    let clock = Arc::new(FixedClock(1_000_000)); // day 11

    let service =
        IngestCycleService::new(source.clone(), snapshots.clone(), history.clone(), clock);
    let report = service
        .run_cycle(&target(), PayloadKind::Commodities)
        .expect("cycle");

    assert_eq!(report.outcome, CycleOutcome::Fetched);
    assert_eq!(report.entries, 4);
    assert_eq!(report.dropped, 1);
    assert_eq!(report.snapshot_id, Some(1));
    assert_eq!(report.items_folded, 2);

    // conditional GET saw no previous snapshot
    assert_eq!(source.conditional_gets(), vec![None]);

    let stored = snapshots.inner.lock().unwrap();
    assert_eq!(
        stored
            .latest
            .as_ref()
            .expect("snapshot recorded")
            .entry_count,
        4
    );
    assert_eq!(stored.raws[0], b"raw-bytes".to_vec());
    drop(stored);

    let folds = history.folds.lock().unwrap();
    let day = amtracker_core::domain::utc_day(1_000_000);
    let item1_fold = folds
        .get(&(TargetId("us-4".into()), item1.clone(), day))
        .expect("item 1 fold saved");
    assert_eq!(
        item1_fold.len(),
        2,
        "re-sighted auction folds into one entry"
    );
    let item2_fold = folds
        .get(&(TargetId("us-4".into()), item2.clone(), day))
        .expect("item 2 fold saved");
    assert_eq!(item2_fold.len(), 1);
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
            b"old".as_slice(),
        )
        .expect("seed");

    let source = Arc::new(FakeSource::new(vec![Ok(FetchOutcome::NotModified)]));
    let history = Arc::new(FakeHistory::default());
    let clock = Arc::new(FixedClock(1_000));
    let service = IngestCycleService::new(source.clone(), snapshots, history, clock);

    let report = service
        .run_cycle(&target(), PayloadKind::Commodities)
        .expect("cycle");

    assert_eq!(report.outcome, CycleOutcome::NotModified);
    assert_eq!(report.snapshot_id, None);
    assert_eq!(source.conditional_gets(), vec![Some(500)]);
}

#[test]
fn ingest_cycle_maps_source_errors_without_touching_storage() {
    let source = Arc::new(FakeSource::new(vec![Err(SourceError::Http("boom".into()))]));
    let snapshots = Arc::new(FakeSnapshots::new());
    let history = Arc::new(FakeHistory::default());
    let clock = Arc::new(FixedClock(0));
    let service = IngestCycleService::new(source, snapshots.clone(), history, clock);

    let error = service
        .run_cycle(&target(), PayloadKind::Auctions)
        .expect_err("must fail");
    assert!(matches!(error, CycleError::Source(_)));
    assert!(
        snapshots.inner.lock().unwrap().latest.is_none(),
        "failed cycle writes nothing"
    );
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
        if entries
            .iter()
            .any(|e| e.target == entry.target && e.item == entry.item)
        {
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
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.target == *target)
            .cloned()
            .collect())
    }
}

#[test]
fn watchlist_service_adds_lists_and_rejects_duplicates() {
    let repo = Arc::new(FakeWatchlist::default());
    let service = WatchlistService::new(repo.clone(), Arc::new(FixedClock(77)));
    let target = TargetId("us-4".into());
    let pet = ItemRef::Pet {
        species: 2546,
        level: 1,
        quality: 3,
        breed: 11,
    };

    let added = service
        .add(&target, pet.clone(), Some("rare pet".into()))
        .expect("add");
    assert_eq!(added.item, pet);
    assert_eq!(added.added_at, 77);

    let duplicate = service.add(&target, pet.clone(), None);
    assert!(matches!(duplicate, Err(StoreError::Conflict(_))));

    assert_eq!(service.list(&target).expect("list").len(), 1);
    assert!(service.remove(&target, &pet).expect("remove"));
    assert!(!service.remove(&target, &pet).expect("remove again"));
    assert!(service.list(&target).expect("list").is_empty());
}

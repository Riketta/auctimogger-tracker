//! SQLite repository integration tests: atomic snapshot+listing records,
//! fold round-trips through the serialization mirror, retention pruning.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use amtracker_core::domain::{
    ItemRef, ListingKind, NewSnapshot, NormalizedListing, PayloadKind, SnapshotStatus, Span,
    TargetId, TimeLeftBand,
};
use amtracker_core::fold::DayFold;
use amtracker_core::ports::{MetricsCacheRepository, PriceHistoryRepository, SnapshotRepository};
use amtracker_infra::store::SqliteStore;

fn temp_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "amtracker-test-{}-{}-{}",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn listing(auction_id: i64, item: &ItemRef, price: i64, qty: i64) -> NormalizedListing {
    NormalizedListing {
        auction_id,
        item_ref: item.clone(),
        kind: ListingKind::Commodity,
        quantity: qty,
        unit_price: price,
        bid: None,
        time_left: TimeLeftBand::VeryLong,
        context: None,
        bonus_lists: vec![6655],
    }
}

fn item(id: i64) -> ItemRef {
    ItemRef::Item { item_id: id }
}

#[test]
fn record_persists_snapshot_listings_and_latest() {
    let dir = temp_dir("record");
    let store = SqliteStore::open(&dir.join("db.sqlite3"), &dir.join("raw")).expect("open");

    let target = TargetId("us-4".into());
    let snapshot_id = store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: 1_000_000,
                source_last_modified: Some(999_000),
                entry_count: 2,
                dropped_count: 1,
                status: SnapshotStatus::Ok,
            },
            &[
                listing(10, &item(1), 100, 20),
                listing(11, &item(1), 200, 1),
            ],
            b"payload".as_slice(),
        )
        .expect("record");

    assert_eq!(snapshot_id, 1);
    let latest = store
        .latest(&target, PayloadKind::Commodities)
        .expect("latest")
        .expect("snapshot present");
    assert_eq!(latest.entry_count, 2);
    assert_eq!(latest.dropped_count, 1);
    assert_eq!(latest.source_last_modified, Some(999_000));

    // different target/kind combination has nothing
    assert!(store
        .latest(&target, PayloadKind::Auctions)
        .expect("latest")
        .is_none());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fold_round_trips_through_storage() {
    let dir = temp_dir("fold");
    let store = SqliteStore::open(&dir.join("db.sqlite3"), &dir.join("raw")).expect("open");

    let target = TargetId("us-4".into());
    let pet = ItemRef::Pet {
        species: 2546,
        level: 1,
        quality: 3,
        breed: 11,
    };
    let mut fold = DayFold::new(pet.clone());
    fold.apply(&[listing(7, &pet, 900_000, 1), listing(8, &pet, 900_000, 2)]);
    store.save_fold(&target, &pet, 11, &fold).expect("save");

    let loaded = store
        .load_fold(&target, &pet, 11)
        .expect("load")
        .expect("fold present");
    assert_eq!(loaded, fold, "fold survives the serialization round-trip");
    assert_eq!(loaded.histogram().listing_count, 2);

    // unknown item/day has no fold
    assert!(store
        .load_fold(&TargetId("other".into()), &pet, 11)
        .expect("load")
        .is_none());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn prune_removes_only_the_raw_working_set() {
    let dir = temp_dir("prune");
    let store = SqliteStore::open(&dir.join("db.sqlite3"), &dir.join("raw")).expect("open");
    let target = TargetId("us-4".into());

    // old-day snapshot with listings
    store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: 10 * 86_400,
                source_last_modified: None,
                entry_count: 1,
                dropped_count: 0,
                status: SnapshotStatus::Ok,
            },
            &[listing(1, &item(1), 100, 1)],
            b"old-payload".as_slice(),
        )
        .expect("old record");

    // fresh snapshot
    store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: 30 * 86_400,
                source_last_modified: None,
                entry_count: 1,
                dropped_count: 0,
                status: SnapshotStatus::Ok,
            },
            &[listing(2, &item(2), 300, 1)],
            b"new-payload".as_slice(),
        )
        .expect("new record");

    let removed = store.prune_raw_before(&target, 20).expect("prune");
    assert_eq!(removed, 1, "only the old-day raw rows are removed");

    // metadata survives the prune (permanent by design)
    assert!(store
        .latest(&target, PayloadKind::Commodities)
        .expect("latest")
        .is_some());

    // old fold rows are permanent too
    let old_item = item(1);
    store
        .save_fold(&target, &old_item, 10, &DayFold::new(old_item.clone()))
        .expect("save");
    assert!(store
        .load_fold(&target, &old_item, 10)
        .expect("load")
        .is_some());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn metrics_cache_round_trips_and_invalidates() {
    use amtracker_core::domain::MetricBlock;

    let dir = temp_dir("cache");
    let store = SqliteStore::open(&dir.join("db.sqlite3"), &dir.join("raw")).expect("open");
    let target = TargetId("us-4".into());
    let item = item(194784);

    assert!(store.get(&target, &item, Span::D1).expect("get").is_none());

    let block = MetricBlock {
        n: 42,
        min: 100,
        p05: 100,
        p25: 150,
        median: 200,
        p75: 250,
        p95: 300,
        max: 400,
        mad: 30,
        vwap: 210,
        qty_sum: 900,
        listing_count: 42,
        sale_rate: None,
    };
    store
        .put(&target, &item, Span::D1, &block, 1234)
        .expect("put");

    let cached = store
        .get(&target, &item, Span::D1)
        .expect("get")
        .expect("cached");
    assert_eq!(cached.computed_at, 1234);
    assert_eq!(
        cached.block, block,
        "block survives the serialization round-trip"
    );

    store.invalidate(&target).expect("invalidate");
    assert!(store.get(&target, &item, Span::D1).expect("get").is_none());

    std::fs::remove_dir_all(&dir).ok();
}

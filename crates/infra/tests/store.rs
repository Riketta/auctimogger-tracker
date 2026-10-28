//! SQLite repository integration tests: atomic snapshot+listing records,
//! fold round-trips through the serialization mirror, retention pruning.

// Test assertions may unwrap/expect/panic and index: fixture-driven,
// deterministic inputs (the SecOutfall precedent).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::assert_is_empty
)]

use std::{
    path::PathBuf,
    sync::{
        atomic::{
            AtomicU64,
            Ordering,
        },
        Arc,
    },
};

use amtracker_core::{
    domain::{
        Category,
        Flavor,
        ItemRef,
        ListingKind,
        NewSnapshot,
        NormalizedListing,
        PayloadKind,
        SnapshotStatus,
        Span,
        TargetId,
        TimeLeftBand,
    },
    error::StoreError,
    fold::DayFold,
    ports::{
        ClockPort,
        ItemCatalogRepository,
        MetricsCacheRepository,
        PriceHistoryRepository,
        SalesRepository,
        SnapshotRepository,
    },
};
use amtracker_infra::store::SqliteStore;

/// Fixed clock: `sale_rate` windows and watchlist/catalog stamps now flow
/// through the injected port, so tests are deterministic by construction.
struct FixedClock(i64);

impl ClockPort for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn open_store(dir: &std::path::Path, now: i64) -> SqliteStore {
    SqliteStore::open(
        &dir.join("db.sqlite3"),
        &dir.join("raw"),
        Arc::new(FixedClock(now)) as Arc<dyn ClockPort>,
    )
    .expect("open")
}

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
    let store = open_store(&dir, 1_000_000);

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
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &[listing(10, &item(1), 100, 20), listing(11, &item(1), 200, 1)],
            b"payload".as_slice(),
        )
        .expect("record");

    assert_eq!(snapshot_id, 1);
    let latest =
        store.latest(&target, PayloadKind::Commodities).expect("latest").expect("snapshot present");
    assert_eq!(latest.entry_count, 2);
    assert_eq!(latest.dropped_count, 1);
    assert_eq!(latest.source_last_modified, Some(999_000));

    // different target/kind combination has nothing
    assert!(store.latest(&target, PayloadKind::Auctions).expect("latest").is_none());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fold_round_trips_through_storage() {
    let dir = temp_dir("fold");
    let store = open_store(&dir, 1_000_000);

    let target = TargetId("us-4".into());
    let pet = ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 };
    let mut fold = DayFold::new(pet.clone());
    fold.apply(&[listing(7, &pet, 900_000, 1), listing(8, &pet, 900_000, 2)]);
    store.save_fold(&target, &pet, 11, Category::Pet, &fold).expect("save");

    let loaded =
        store.load_fold(&target, &pet, 11, Category::Pet).expect("load").expect("fold present");
    assert_eq!(loaded, fold, "fold survives the serialization round-trip");
    assert_eq!(loaded.histogram().listing_count, 2);

    // unknown item/day has no fold
    assert!(store
        .load_fold(&TargetId("other".into()), &pet, 11, Category::Pet)
        .expect("load")
        .is_none());
    // ...and neither does the same identity under a different category (the
    // pet fold lives in the pet market only).
    assert!(store.load_fold(&target, &pet, 11, Category::Item).expect("load").is_none());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn prune_removes_only_the_raw_working_set() {
    let dir = temp_dir("prune");
    let store = open_store(&dir, 30 * 86_400);
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
                dropped_reasons: std::collections::BTreeMap::new(),
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
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &[listing(2, &item(2), 300, 1)],
            b"new-payload".as_slice(),
        )
        .expect("new record");

    let removed = store.prune_raw_before(&target, 20).expect("prune");
    assert_eq!(removed, 1, "only the old-day raw rows are removed");

    // metadata survives the prune (permanent by design)
    assert!(store.latest(&target, PayloadKind::Commodities).expect("latest").is_some());

    // old fold rows are permanent too
    let old_item = item(1);
    store
        .save_fold(&target, &old_item, 10, Category::Commodity, &DayFold::new(old_item.clone()))
        .expect("save");
    assert!(store.load_fold(&target, &old_item, 10, Category::Commodity).expect("load").is_some());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sell_through_credits_band_violations_only() {
    use amtracker_core::ports::SalesRepository;

    let dir = temp_dir("sales");
    let store = open_store(&dir, 1_000_000 + 1_800 + 60);
    let target = TargetId("us-4".into());
    // Fully deterministic now: the sale_rate window reads the injected clock.
    let t0: i64 = 1_000_000 - 1_200; // cycle 1 timestamp, 20 minutes before cycle 3

    // Three MEDIUM-band listings opened at t0 (band lower bound: 30 min).
    let batch =
        [listing(1, &item(1), 100, 1), listing(2, &item(1), 200, 2), listing(3, &item(1), 300, 3)];
    store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: t0,
                source_last_modified: None,
                entry_count: 3,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &batch,
            b"s1".as_slice(),
        )
        .expect("record s1");
    store.observe(&target, PayloadKind::Commodities, &batch, 1, t0).expect("observe s1");

    // Cycle 2 (snapshot 2) 10 minutes later: auctions 1 and 2 vanished,
    // auction 3 is still present.
    let t1 = t0 + 600;
    let survivor = [listing(3, &item(1), 300, 3)];
    store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: t1,
                source_last_modified: None,
                entry_count: 1,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &survivor,
            b"s2".as_slice(),
        )
        .expect("record s2");
    store.observe(&target, PayloadKind::Commodities, &survivor, 2, t1).expect("observe s2");
    let credits = store.close_missing(&target, PayloadKind::Commodities, 2, t1).expect("close");
    assert_eq!(credits.len(), 2, "vanished after 10 min < 30 min band floor");
    assert!(credits.iter().all(|c| c.item == item(1)));

    // Later closure past the band floor is NOT credited: reopen auction 3's
    // sight artificially? No - use a fresh MEDIUM sight closed after 30 min.
    let t2 = t1 + 1_800 + 60; // just past the MEDIUM lower bound
    let late = [listing(9, &item(2), 500, 1)];
    store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: t2,
                source_last_modified: None,
                entry_count: 1,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &late,
            b"s3".as_slice(),
        )
        .expect("record s3");
    store.observe(&target, PayloadKind::Commodities, &late, 3, t2).expect("observe s3");
    let t3 = t2 + 1_800 + 60;
    store.close_missing(&target, PayloadKind::Commodities, 3, t3).expect("close late");

    // sale_rate over the trailing window: item 1 = 2 credited / 2 listings.
    let rate =
        store.sale_rate(&target, &item(1), 14, Category::Commodity).expect("rate").expect("rate");
    assert!((rate - 1.0).abs() < 1e-9, "all vanishings credited so far");

    // item 2 closed past its band floor: credited stays 0, denominator 1.
    let rate2 =
        store.sale_rate(&target, &item(2), 14, Category::Commodity).expect("rate").expect("rate");
    assert_eq!(rate2, 0.0);

    std::fs::remove_dir_all(&dir).ok();
}

/// F1 regression: interleaving the two payload kinds must close only the
/// vanished kind's sights (never the other kind's), never wedge on the sight
/// primary key, and keep `sale_rate` sane.
#[test]
#[allow(clippy::too_many_lines)]
fn interleaved_kinds_do_not_close_each_others_sights_or_wedge() {
    use amtracker_core::ports::SalesRepository;

    let dir = temp_dir("interleave");
    let store = open_store(&dir, 10_000);
    let target = TargetId("us-4".into());
    let shared_auction_id = 777; // deliberately the SAME numeric id in both kinds

    let commodities = [listing(shared_auction_id, &item(1), 100, 1)];
    let auctions = [{
        let mut l = listing(shared_auction_id, &item(2), 200, 1);
        l.kind = ListingKind::Item;
        l
    }];

    // Cycle 1: both kinds observe their (id-clashing) listings.
    for (kind, batch, tag) in [
        (PayloadKind::Commodities, &commodities[..], b"c1".as_slice()),
        (PayloadKind::Auctions, &auctions[..], b"a1".as_slice()),
    ] {
        let id = store
            .record(
                &NewSnapshot {
                    target: target.clone(),
                    kind,
                    fetched_at: 10_000,
                    source_last_modified: None,
                    entry_count: 1,
                    dropped_count: 0,
                    dropped_reasons: std::collections::BTreeMap::new(),
                    status: SnapshotStatus::Ok,
                },
                batch,
                tag,
            )
            .expect("record");
        store.observe(&target, kind, batch, id, 10_000).expect("observe");
    }

    // Cycle 2: fresh snapshots for both kinds, same listings still present.
    for (kind, batch, tag) in [
        (PayloadKind::Commodities, &commodities[..], b"c2".as_slice()),
        (PayloadKind::Auctions, &auctions[..], b"a2".as_slice()),
    ] {
        let id = store
            .record(
                &NewSnapshot {
                    target: target.clone(),
                    kind,
                    fetched_at: 11_000,
                    source_last_modified: None,
                    entry_count: 1,
                    dropped_count: 0,
                    dropped_reasons: std::collections::BTreeMap::new(),
                    status: SnapshotStatus::Ok,
                },
                batch,
                tag,
            )
            .expect("record");
        store.observe(&target, kind, batch, id, 11_000).expect("re-observe");
        // Before the fix this closed the OTHER kind's sights (their
        // last_seen_snapshot differs) and then PK-viad the re-observe.
        let credits = store.close_missing(&target, kind, id, 11_000).expect("close");
        assert!(credits.is_empty(), "nothing vanished; cross-kind mass-close must not happen");
    }
    // BOTH sights must still be open after each kind's close pass: the
    // close UPDATE carries the same kind predicate as the SELECT.
    {
        let conn = rusqlite::Connection::open(dir.join("db.sqlite3")).expect("inspect db");
        for kind in ["commodities", "auctions"] {
            let closed_at: Option<i64> = conn
                .query_row(
                    "SELECT closed_at FROM sight
                     WHERE target_id = ?1 AND auction_id = ?2 AND kind = ?3",
                    rusqlite::params![target.as_str(), shared_auction_id, kind],
                    |row| row.get(0),
                )
                .expect("sight row present");
            assert!(closed_at.is_none(), "{kind} sight must stay open after its own close pass");
        }
    }

    // Vanish the commodities listing for real: close, credit, then re-observe
    // it later — the sight must reopen (not PK-wedge) and the phantom credit
    // must be reversed.
    let t_close = 12_000;
    let id3 = store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: t_close,
                source_last_modified: None,
                entry_count: 0,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &[],
            b"c3".as_slice(),
        )
        .expect("record c3");
    let credits =
        store.close_missing(&target, PayloadKind::Commodities, id3, t_close).expect("close c3");
    assert_eq!(
        credits.len(),
        1,
        "a 16-minute vanish with VERY_LONG remaining is a phantom sale candidate (gap < band floor)"
    );
    assert_eq!(
        credits.first().map(|c| c.item.clone()),
        Some(item(1)),
        "only the commodities sight closed; auctions untouched"
    );
    // The close must be scoped to its own kind: the OTHER kind's sight stays
    // open (with the old UPDATE this is where the auctions sight was closed
    // and its phantom credit polluted item 2's ledger).
    {
        let conn = rusqlite::Connection::open(dir.join("db.sqlite3")).expect("inspect db");
        let auctions_closed: Option<i64> = conn
            .query_row(
                "SELECT closed_at FROM sight
                 WHERE target_id = ?1 AND auction_id = ?2 AND kind = 'auctions'",
                rusqlite::params![target.as_str(), shared_auction_id],
                |row| row.get(0),
            )
            .expect("auctions sight present");
        assert!(auctions_closed.is_none(), "the OTHER kind's sight must stay open");
        let commodities_closed: Option<i64> = conn
            .query_row(
                "SELECT closed_at FROM sight
                 WHERE target_id = ?1 AND auction_id = ?2 AND kind = 'commodities'",
                rusqlite::params![target.as_str(), shared_auction_id],
                |row| row.get(0),
            )
            .expect("commodities sight present");
        assert!(commodities_closed.is_some(), "the vanished kind's sight IS closed");
    }

    let t_back = 13_000;
    let id4 = store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: t_back,
                source_last_modified: None,
                entry_count: 1,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &commodities,
            b"c4".as_slice(),
        )
        .expect("record c4");
    store
        .observe(&target, PayloadKind::Commodities, &commodities, id4, t_back)
        .expect("re-observe after close must reopen, not wedge");

    // Sale ledger: 1 listing opened, 1 closed (not credited), reopen undid the
    // close. Nothing credited.
    let rate = store.sale_rate(&target, &item(1), 14, Category::Commodity).expect("rate");
    assert!(rate.is_none() || rate == Some(0.0), "no phantom sales: got {rate:?}");

    // Sales aggregates must never go negative after the close/reopen cycle
    // (a phantom credit reversal that underflowed the counters would corrupt
    // every rate computed afterwards).
    {
        let conn = rusqlite::Connection::open(dir.join("db.sqlite3")).expect("inspect db");
        let rows: Vec<(i64, i64, i64, i64)> = conn
            .prepare("SELECT listings, closed, credited, qty_sold FROM sales")
            .expect("prepare sales scan")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
            .expect("query sales")
            .filter_map(Result::ok)
            .collect();
        assert!(!rows.is_empty(), "sales rows exist after the cycle");
        for (listings, closed, credited, qty_sold) in rows {
            assert!(listings >= 0 && closed >= 0 && credited >= 0 && qty_sold >= 0);
        }
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// T11 companion: a larger synthetic batch through record/observe/close must
/// produce identical bookkeeping to the small-path tests (the batched
/// prepared statements cover the per-listing work).
#[test]
fn larger_batches_record_observe_and_close_without_drift() {
    use amtracker_core::ports::SalesRepository;

    let dir = temp_dir("batch");
    let store = open_store(&dir, 20_000);
    let target = TargetId("us-4".into());

    // 400 listings across 4 items, distinct auction ids in one namespace.
    let batch: Vec<NormalizedListing> =
        (0..400).map(|i| listing(i, &item((i % 4) + 1), 100 + i, 1)).collect();
    let snapshot_id = store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: 20_000,
                source_last_modified: None,
                entry_count: batch.len() as u64,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &batch,
            b"batch-1".as_slice(),
        )
        .expect("record batch");
    store
        .observe(&target, PayloadKind::Commodities, &batch, snapshot_id, 20_000)
        .expect("observe batch");

    // Every sight opens exactly once per item: 100 new sights each.
    {
        let conn = rusqlite::Connection::open(dir.join("db.sqlite3")).expect("inspect db");
        let total: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(listings), 0) FROM sales WHERE target_id = ?1",
                rusqlite::params![target.as_str()],
                |row| row.get(0),
            )
            .expect("sales sum");
        assert_eq!(total, 400, "one new-sight listing counted per observed row");
    }

    // A snapshot without any of them closes and credits the whole batch.
    let next = store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: 21_000,
                source_last_modified: None,
                entry_count: 0,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &[],
            b"batch-2".as_slice(),
        )
        .expect("record empty");
    let credits =
        store.close_missing(&target, PayloadKind::Commodities, next, 21_000).expect("close all");
    assert_eq!(credits.len(), 400, "every vanished VERY_LONG listing credits");

    std::fs::remove_dir_all(&dir).ok();
}

/// A schema-v1 database (sight table without `kind`) must be refused with a
/// rebuild hint instead of silently fabricating history.
#[test]
fn legacy_v1_database_is_refused() {
    use rusqlite::Connection;

    let dir = temp_dir("legacy");
    let db_path = dir.join("db.sqlite3");
    {
        let conn = Connection::open(&db_path).expect("open legacy");
        conn.execute_batch(
            "CREATE TABLE sight (
                 target_id TEXT NOT NULL,
                 auction_id INTEGER NOT NULL,
                 item_ref_id INTEGER NOT NULL,
                 first_seen_snapshot INTEGER NOT NULL,
                 last_seen_snapshot INTEGER NOT NULL,
                 first_seen_at INTEGER NOT NULL,
                 last_seen_at INTEGER NOT NULL,
                 band TEXT NOT NULL,
                 quantity INTEGER NOT NULL,
                 closed_at INTEGER,
                 credited INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (target_id, auction_id)
             );",
        )
        .expect("create legacy sight");
    }
    let error = expect_legacy_refusal(&dir, &db_path);
    assert!(error.to_string().contains("delete the database"), "message: {error}");

    std::fs::remove_dir_all(&dir).ok();
}

#[allow(clippy::panic)]
fn expect_legacy_refusal(
    dir: &std::path::Path,
    db_path: &std::path::Path,
) -> amtracker_core::error::StoreError {
    match SqliteStore::open(
        db_path,
        &dir.join("raw"),
        Arc::new(FixedClock(0)) as Arc<dyn ClockPort>,
    ) {
        Err(error) => error,
        Ok(_) => panic!("legacy v1 database must be refused"),
    }
}

#[test]
fn metrics_cache_round_trips_and_invalidates() {
    use amtracker_core::domain::MetricBlock;

    let dir = temp_dir("cache");
    let store = open_store(&dir, 1_000_000);
    let target = TargetId("us-4".into());
    let item = item(194_784);

    assert!(store.get(&target, &item, Category::Commodity, Span::D1).expect("get").is_none());

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
        sale_rate_180d: None,
        days_since_last_sale: None,
        closures_since_last_sale: None,
    };
    store.put(&target, &item, Category::Commodity, Span::D1, &block, 1234).expect("put");

    let cached =
        store.get(&target, &item, Category::Commodity, Span::D1).expect("get").expect("cached");
    assert_eq!(cached.computed_at, 1234);
    assert_eq!(cached.block, block, "block survives the serialization round-trip");

    store.invalidate(&target).expect("invalidate");
    assert!(store.get(&target, &item, Category::Commodity, Span::D1).expect("get").is_none());

    std::fs::remove_dir_all(&dir).ok();
}

/// The per-cycle targeted drop removes every cached span of the named items
/// (all spans derive from the same folds/sales, so all go stale together);
/// other items survive. Unknown identities are a no-op: they cannot have
/// cache rows.
#[test]
fn metrics_cache_invalidation_targets_only_named_items() {
    use amtracker_core::domain::MetricBlock;

    let dir = temp_dir("cache-targeted");
    let store = open_store(&dir, 1_000_000);
    let target = TargetId("us-4".into());
    let item_a = item(194_784);
    let item_b = item(194_785);
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
        sale_rate_180d: None,
        days_since_last_sale: None,
        closures_since_last_sale: None,
    };
    store.put(&target, &item_a, Category::Commodity, Span::D1, &block, 1234).expect("put a d1");
    store.put(&target, &item_b, Category::Commodity, Span::D1, &block, 1234).expect("put b d1");
    store.put(&target, &item_a, Category::Commodity, Span::D7, &block, 1234).expect("put a d7");

    store
        .invalidate_items(&target, Category::Commodity, std::slice::from_ref(&item_a))
        .expect("invalidate a");
    assert!(store
        .get(&target, &item_a, Category::Commodity, Span::D1)
        .expect("get a d1")
        .is_none());
    assert!(store
        .get(&target, &item_a, Category::Commodity, Span::D7)
        .expect("get a d7")
        .is_none());
    assert!(store
        .get(&target, &item_b, Category::Commodity, Span::D1)
        .expect("get b d1")
        .is_some());

    store
        .invalidate_items(&target, Category::Commodity, &[item(999_999_999)])
        .expect("unknown identity");
    assert!(store
        .get(&target, &item_b, Category::Commodity, Span::D1)
        .expect("get b d1")
        .is_some());

    std::fs::remove_dir_all(&dir).ok();
}

/// `sale_rates_by_id` runs chunked (500 ids per query), so the driver's
/// 32 766 bound-parameter ceiling is unreachable by construction. This test
/// pins that results merge correctly ACROSS chunk boundaries: 1200 items →
/// three chunks (500/500/200), with credited and uncredited items planted on
/// both sides of each boundary.
#[test]
fn sale_rates_chunking_merges_results_across_chunk_boundaries() {
    const N: i64 = 1200;
    const CHUNK: i64 = 500; // mirrors sale_rates_by_id's SALE_RATE_CHUNK

    let dir = temp_dir("sale-chunks");
    let store = open_store(&dir, 1_000_000);
    let target = TargetId("us-4".into());
    let item = |i: i64| item(1000 + i);

    // Cycle 1: every item is sighted once.
    let batch_a: Vec<NormalizedListing> =
        (0..N).map(|i| listing(900_000 + i, &item(i), 100, 1)).collect();
    store
        .observe(&target, PayloadKind::Commodities, &batch_a, 1, 1_000_000)
        .expect("observe batch a");

    // Cycle 2: everything survives except items 600 and 1190..N — planted
    // deliberately so credited rows exist in all three chunks.
    let dropped: Vec<i64> = [600].into_iter().chain(1190..N).collect();
    let batch_b: Vec<NormalizedListing> = (0..N)
        .filter(|i| !dropped.contains(i))
        .map(|i| listing(900_000 + i, &item(i), 100, 1))
        .collect();
    store
        .observe(&target, PayloadKind::Commodities, &batch_b, 2, 1_000_600)
        .expect("observe batch b");
    let credits = store
        .close_missing(&target, PayloadKind::Commodities, 2, 1_001_200)
        .expect("close missing");
    assert_eq!(credits.len(), dropped.len(), "every vanished listing is credited");

    let items: Vec<ItemRef> = (0..N).map(&item).collect();
    let rates = store.sale_rate_each(&target, &items, 14, Category::Commodity).expect("rates");
    let expected_len = usize::try_from(N).unwrap();
    assert_eq!(rates.len(), expected_len, "positionally aligned across all chunks");
    for (index, rate) in rates.iter().enumerate() {
        let index = i64::try_from(index).unwrap();
        let expected = if dropped.contains(&index) { 1.0 } else { 0.0 };
        assert_eq!(*rate, Some(expected), "index {index} (chunk {})", index / CHUNK);
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// Pruning must not reclaim an archive path that a RECENT snapshot still
/// references: identical raw payloads collapse to one `<sha>.json`, so the
/// old snapshot's path can simultaneously be the live archive of a new one.
#[test]
fn prune_keeps_archives_shared_with_recent_snapshots() {
    let dir = temp_dir("prune-shared-sha");
    let store = open_store(&dir, 1_000_000);
    let target = TargetId("us-4".into());

    let snapshot = |fetched_at: i64| NewSnapshot {
        target: target.clone(),
        kind: PayloadKind::Commodities,
        fetched_at,
        source_last_modified: None,
        entry_count: 0,
        dropped_count: 0,
        dropped_reasons: std::collections::BTreeMap::new(),
        status: SnapshotStatus::Ok,
    };
    store.record(&snapshot(50_000), &[], b"shared-payload").expect("old shared");
    store.record(&snapshot(60_000), &[], b"unique-payload").expect("old unique");
    store.record(&snapshot(500_000), &[], b"shared-payload").expect("recent shared");

    // Cutoff day 2 → 172 800: both old snapshots prune, the recent one (and
    // its shared archive) must survive.
    let removed = store.prune_raw_before(&target, 2).expect("prune");
    assert_eq!(removed, 0, "no listing rows were written");

    // Exactly one archive file survives — the shared payload. Before the
    // shared-path guard this was 0: the old snapshot's prune deleted the
    // file the recent snapshot still referenced.
    let kind_dir = dir.join("raw").join("us-4").join("commodities");
    let survivors: Vec<String> = std::fs::read_dir(&kind_dir)
        .expect("archive dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(survivors.len(), 1, "only the shared archive survives: {survivors:?}");

    std::fs::remove_dir_all(&dir).ok();
}

/// `list_recent` returns the newest-first snapshot history, bounded by the
/// limit, and stays scoped to one target/kind.
#[test]
fn list_recent_returns_newest_first_bounded_and_scoped() {
    let dir = temp_dir("snapshots-recent");
    let store = open_store(&dir, 1_000_000);
    let target = TargetId("us-4".into());
    let other = TargetId("eu-1".into());

    for i in 0..5_i64 {
        store
            .record(
                &NewSnapshot {
                    target: target.clone(),
                    kind: PayloadKind::Commodities,
                    fetched_at: 1_000_000 + i,
                    source_last_modified: Some(900_000 + i),
                    entry_count: 100 + u64::try_from(i).unwrap_or(0),
                    dropped_count: 0,
                    dropped_reasons: std::collections::BTreeMap::new(),
                    status: SnapshotStatus::Ok,
                },
                &[],
                br"[]",
            )
            .expect("record");
    }
    // Noise in other namespaces must never leak into the history.
    store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Auctions,
                fetched_at: 1_000_009,
                source_last_modified: None,
                entry_count: 1,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &[],
            br"[]",
        )
        .expect("record");
    store
        .record(
            &NewSnapshot {
                target: other,
                kind: PayloadKind::Commodities,
                fetched_at: 1_000_009,
                source_last_modified: None,
                entry_count: 1,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &[],
            br"[]",
        )
        .expect("record");

    let recent = store.list_recent(&target, PayloadKind::Commodities, 3).expect("list_recent");
    assert_eq!(recent.len(), 3, "limit is respected");
    assert_eq!(recent.first().map(|meta| meta.fetched_at), Some(1_000_004), "newest first");
    assert_eq!(recent.get(2).map(|meta| meta.fetched_at), Some(1_000_002), "bounded window");
    assert!(recent.iter().all(|meta| meta.kind == PayloadKind::Commodities));

    let all = store.list_recent(&target, PayloadKind::Commodities, 100).expect("list_recent");
    assert_eq!(all.len(), 5, "all five commodity snapshots are retained");

    std::fs::remove_dir_all(&dir).ok();
}

/// Pet catalog records round-trip, and the unified search/browse surfaces
/// mix items and pets with correct kinds and ordering.
#[test]
fn pet_catalog_round_trips_through_search_browse_and_counts() {
    use amtracker_core::{
        domain::{
            BrowseCursor,
            CatalogCounts,
            Flavor,
            NameRecord,
            PetNameRecord,
            SearchHit,
        },
        ports::ItemCatalogRepository,
    };

    let dir = temp_dir("pet-catalog");
    let store = open_store(&dir, 1_000_000);
    let flavor = Flavor::default();

    store
        .upsert(
            &flavor,
            &[NameRecord {
                item_id: 2589,
                name: "Linen Cloth".into(),
                item_class: Some("Cloth".into()),
                item_subclass: None,
                inventory_slot: None,
                quality: Some("Common".into()),
                icon: None,
                sell_price: None,
                item_level: None,
                fetched_at: 0,
            }],
        )
        .expect("upsert item");
    store
        .upsert_pets(
            &flavor,
            &[
                PetNameRecord { species_id: 264, name: "Mechanical Squirrel Box".into() },
                PetNameRecord { species_id: 42, name: "Linen-labeled Pet".into() },
            ],
        )
        .expect("upsert pets");

    // by-species reads.
    let pet = store.pet_by_species(&flavor, 264).expect("by_species").expect("named");
    assert_eq!(pet.name, "Mechanical Squirrel Box");
    assert!(store.pet_by_species(&flavor, 999_999).expect("by_species").is_none());

    // Kind-aware search: both pets share the substring with the item.
    let hits = store.search("linen", 10, 0).expect("search");
    assert_eq!(hits.len(), 2, "item hit plus pet hit");
    assert!(hits
        .iter()
        .any(|hit| matches!(hit, SearchHit::Item(record) if record.item_id == 2589)));
    assert!(hits
        .iter()
        .any(|hit| matches!(hit, SearchHit::Pet(record) if record.species_id == 42)));
    let pet_hits = store.search("squirrel", 10, 0).expect("search");
    assert_eq!(pet_hits.len(), 1, "pets are searchable by name");
    assert!(matches!(pet_hits[0], SearchHit::Pet(ref record) if record.species_id == 264));

    // Alphabetical browse across both kinds, keyset-paginated.
    let (page1, has_more) = store.browse(None, 2).expect("browse");
    assert_eq!(page1.len(), 2);
    assert!(has_more, "a next page exists");
    assert_eq!(page1[0].name(), "Linen Cloth", "ties break item before pet");
    assert_eq!(page1[1].name(), "Linen-labeled Pet");

    let last = match &page1[1] {
        SearchHit::Pet(record) => {
            BrowseCursor { name: record.name.clone(), kind: "pet".into(), id: record.species_id }
        }
        other @ SearchHit::Item(_) => panic!("expected pet hit, got {other:?}"),
    };
    let (page2, has_more) = store.browse(Some(&last), 2).expect("browse");
    assert_eq!(page2.len(), 1);
    assert!(!has_more, "end of catalog");
    assert_eq!(page2[0].name(), "Mechanical Squirrel Box");

    let counts = store.counts().expect("counts");
    assert_eq!(counts, CatalogCounts { items: 1, pets: 2 }, "per-kind totals");

    std::fs::remove_dir_all(&dir).ok();
}

/// Search pagination: `offset` skips the leading hits so clients can page
/// through a catalog with `(limit, offset)` pairs.
#[test]
fn search_offset_pages_through_hits() {
    use amtracker_core::{
        domain::{
            Flavor,
            NameRecord,
        },
        ports::ItemCatalogRepository,
    };

    let dir = temp_dir("search-offset");
    let store = open_store(&dir, 1_000_000);
    let record = |id: i64, name: &str| NameRecord {
        item_id: id,
        name: name.into(),
        item_class: None,
        item_subclass: None,
        inventory_slot: None,
        quality: None,
        icon: None,
        sell_price: None,
        item_level: None,
        fetched_at: 0,
    };
    store
        .upsert(
            &Flavor::default(),
            &[
                record(1, "Linen Cloth"),
                record(2, "Bolts of Linen Cloth"),
                record(3, "Heavy Linen Glove"),
            ],
        )
        .expect("upsert");

    let page1 = store.search("linen", 2, 0).expect("page 1");
    let page2 = store.search("linen", 2, 2).expect("page 2");
    assert_eq!(page1.len(), 2);
    assert_eq!(page2.len(), 1, "the trailing hit lands on the second page");
    let id = |hits: &[amtracker_core::domain::SearchHit]| -> Vec<i64> {
        hits.iter()
            .map(|hit| match hit {
                amtracker_core::domain::SearchHit::Item(record) => record.item_id,
                amtracker_core::domain::SearchHit::Pet(record) => record.species_id,
            })
            .collect()
    };
    assert_eq!(id(&page1), vec![1, 2], "prefix match first, then alphabetical");
    assert_eq!(id(&page2), vec![3]);

    std::fs::remove_dir_all(&dir).ok();
}

/// Catalog rows are scoped per flavor: the same numeric id can exist in two
/// game versions with different names, while the search/browse surfaces stay
/// identity-deduplicated and counts stay per-identity.
#[test]
fn catalog_rows_are_scoped_per_flavor() {
    use amtracker_core::{
        domain::{
            Flavor,
            NameRecord,
        },
        ports::ItemCatalogRepository,
    };

    let dir = temp_dir("catalog-flavors");
    let store = open_store(&dir, 1_000_000);
    let retail = Flavor("retail".into());
    let forever = Flavor("forever".into());
    let record = |id: i64, name: &str| NameRecord {
        item_id: id,
        name: name.into(),
        item_class: None,
        item_subclass: None,
        inventory_slot: None,
        quality: None,
        icon: None,
        sell_price: None,
        item_level: None,
        fetched_at: 0,
    };

    store.upsert(&retail, &[record(7, "Retail Name")]).expect("upsert retail");
    store.upsert(&forever, &[record(7, "Forever Name")]).expect("upsert forever");

    let retail_row = store.by_id(&retail, 7).expect("by id").expect("named");
    assert_eq!(retail_row.name, "Retail Name");
    let forever_row = store.by_id(&forever, 7).expect("by id").expect("named");
    assert_eq!(forever_row.name, "Forever Name");
    assert!(store.by_id(&retail, 8).expect("by id").is_none());

    // Search and counts stay per-identity, not per-flavor.
    let hits = store.search("name", 10, 0).expect("search");
    assert_eq!(hits.len(), 1, "one hit per identity, not per flavor");
    let counts = store.counts().expect("counts");
    assert_eq!(counts.items, 1, "flavor twins count once");

    std::fs::remove_dir_all(&dir).ok();
}

/// Batched cache reads: `get_many` returns one slot per requested identity,
/// positionally aligned, with unknown identities reading back as `None`.
#[test]
fn metrics_cache_get_many_aligns_blocks_with_identities() {
    let dir = temp_dir("cache-get-many");
    let store = open_store(&dir, 1_000_000);
    let target = TargetId("us-4".into());
    let item1 = ItemRef::Item { item_id: 1 };
    let item2 = ItemRef::Item { item_id: 2 };
    let unknown = ItemRef::Item { item_id: 999 };
    let block = |median: i64| amtracker_core::domain::MetricBlock {
        n: 1,
        min: median,
        p05: median,
        p25: median,
        median,
        p75: median,
        p95: median,
        max: median,
        mad: 0,
        vwap: median,
        qty_sum: 1,
        listing_count: 1,
        sale_rate: None,
        sale_rate_180d: None,
        days_since_last_sale: None,
        closures_since_last_sale: None,
    };

    store.put(&target, &item1, Category::Item, Span::D7, &block(100), 999).expect("put item1");
    store.put(&target, &item2, Category::Item, Span::D7, &block(200), 999).expect("put item2");

    let items = vec![item1.clone(), unknown.clone(), item2.clone()];
    let blocks = store.get_many(&target, Category::Item, Span::D7, &items).expect("get_many");
    assert_eq!(blocks.len(), 3, "one slot per requested identity");
    assert_eq!(blocks[0].as_ref().expect("item1 cached").block.median, 100);
    assert!(blocks[1].is_none(), "unknown identity has no cache rows");
    assert_eq!(blocks[2].as_ref().expect("item2 cached").block.median, 200);

    // Spans partition the cache: the same items read under another span
    // come back empty.
    let other = store.get_many(&target, Category::Item, Span::D1, &items).expect("get_many d1");
    assert!(other.iter().all(Option::is_none));

    // Categories partition it too: the same identities under another
    // category have no rows.
    let other =
        store.get_many(&target, Category::Commodity, Span::D7, &items).expect("get_many commodity");
    assert!(other.iter().all(Option::is_none));

    std::fs::remove_dir_all(&dir).ok();
}

/// `backup_to` writes a consistent copy of the live database (readable with
/// plain SQLite), creates missing parent directories, and refuses to
/// overwrite an existing file (callers rotate first).
#[test]
fn backup_to_writes_consistent_copy_and_refuses_existing_target() {
    let dir = temp_dir("backup");
    let store = open_store(&dir, 1_000_000);
    let target = TargetId("us-4".into());
    store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: 1_000_000,
                source_last_modified: Some(999_000),
                entry_count: 1,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &[listing(10, &item(1), 100, 20)],
            b"payload".as_slice(),
        )
        .expect("record");

    // The backup's parent directory does not exist yet: it is created.
    let backup_path = dir.join("backups").join("nightly.sqlite3");
    store.backup_to(&backup_path).expect("backup_to");
    assert!(
        std::fs::metadata(&backup_path).expect("backup file exists").len() > 0,
        "backup file is non-empty"
    );

    // The copy is a real database holding the seeded snapshot row.
    let copy = rusqlite::Connection::open_with_flags(
        &backup_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open backup read-only");
    let (backed_target, entry_count): (String, i64) = copy
        .query_row("SELECT target_id, entry_count FROM snapshots WHERE id = 1", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .expect("seeded snapshot row in backup");
    assert_eq!(backed_target, "us-4");
    assert_eq!(entry_count, 1);

    // `VACUUM INTO` refuses an existing target file.
    let error = store.backup_to(&backup_path).expect_err("second backup must fail");
    assert!(matches!(error, StoreError::Backend(message) if message.contains("backup")));

    std::fs::remove_dir_all(&dir).ok();
}

/// Alert rules and fired events round-trip: conditions survive the JSON
/// mirror, identities rebuild from the `item_ref` join, and events outlive
/// their rule's removal.
#[test]
fn alert_rules_and_events_round_trip() {
    use amtracker_core::{
        domain::{
            AlertCondition,
            FloorPercentile,
            NewAlertEvent,
            NewAlertRule,
        },
        ports::{
            AlertEventRepository,
            AlertRuleRepository,
        },
    };

    let dir = temp_dir("alerts");
    let store = open_store(&dir, 1_000_000);
    let target = TargetId("us-4".into());
    let pet = ItemRef::Pet { species: 264, level: 1, quality: 3, breed: 11 };

    let rule = store
        .add(&NewAlertRule {
            target: target.clone(),
            item: pet.clone(),
            condition: AlertCondition::BelowFloor {
                span: Span::D7,
                percentile: FloorPercentile::P25,
            },
            note: Some("pet floor".into()),
        })
        .expect("add rule");
    assert_eq!(rule.id, 1, "ids are store-assigned and monotonic");
    assert_eq!(
        rule.condition,
        AlertCondition::BelowFloor { span: Span::D7, percentile: FloorPercentile::P25 }
    );
    assert_eq!(rule.last_fired_at, None);

    let event = NewAlertEvent {
        rule_id: rule.id,
        target: target.clone(),
        item: pet.clone(),
        condition: AlertCondition::BelowFloor { span: Span::D7, percentile: FloorPercentile::P25 },
        observed: Some(0.5),
        message: "pet 264: d7 median at/below p25 floor".into(),
        fired_at: 1_000_500,
    };
    let event_id = store.record_firing(&event).expect("record firing");
    let events = store.recent(&target, 10).expect("recent");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, event_id);
    assert_eq!(events[0].observed, Some(0.5));

    // record_firing books the cooldown anchor atomically with the event.
    let listed = store.list(&target).expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].last_fired_at, Some(1_000_500));
    assert_eq!(listed[0].item, pet, "identity rebuilds through the join");

    // Removing the rule keeps the event history (events are the audit log).
    assert!(store.remove(rule.id).expect("remove rule"));
    assert_eq!(store.list(&target).expect("list").len(), 0);
    assert_eq!(store.recent(&target, 10).expect("recent").len(), 1);

    // Rules of other targets stay separate.
    store
        .add(&NewAlertRule {
            target: TargetId("eu-9".into()),
            item: ItemRef::Item { item_id: 2589 },
            condition: AlertCondition::SaleRateAbove { rate: 0.2 },
            note: None,
        })
        .expect("add other rule");
    assert_eq!(store.list(&target).expect("list").len(), 0);
    assert_eq!(store.list(&TargetId("eu-9".into())).expect("list").len(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

/// Databases stamped with a NEWER schema version are refused: running older
/// code against newer tables would misread them in every query, and the
/// stamp is never lowered, so the file stays intact for the matching build.
#[test]
fn future_schema_files_are_refused_with_the_stamp_intact() {
    let dir = temp_dir("future-schema");
    let path = dir.join("db.sqlite3");
    {
        let store = open_store(&dir, 1_000);
        store.latest(&TargetId("us-4".into()), PayloadKind::Commodities).expect("touch");
    }
    {
        let conn = rusqlite::Connection::open(&path).expect("open for stamping");
        conn.pragma_update(None, "user_version", 99).expect("stamp future version");
    }
    let Err(error) = SqliteStore::open(
        &path,
        &dir.join("raw"),
        Arc::new(FixedClock(1_000)) as Arc<dyn ClockPort>,
    ) else {
        panic!("future schema must be refused");
    };
    assert!(error.to_string().contains("v99"), "the error names the schema version: {error}");
    // The refusal never touches the file: the matching build can still open it.
    let conn = rusqlite::Connection::open(&path).expect("reopen");
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).expect("stamp");
    assert_eq!(version, 99, "the marker stays untouched");

    std::fs::remove_dir_all(&dir).ok();
}

/// Sight retention deletes only CLOSED sights closed before the cutoff;
/// open sights and anything newer stay (the reopen-reversal horizon).
#[test]
fn sight_retention_prunes_only_old_closed_sights() {
    use amtracker_core::ports::SalesRepository;

    let dir = temp_dir("sight-retention");
    let day = 86_400;
    let store = open_store(&dir, 111 * day);
    let target = TargetId("us-4".into());

    // Sight A: opened day 100, closed day 100 (OLD closed → pruned).
    let a = [listing(1, &item(1), 100, 1)];
    store.record(&snap(&target, PayloadKind::Commodities, 100 * day, 1), &a, b"s1").expect("s1");
    store.observe(&target, PayloadKind::Commodities, &a, 1, 100 * day).expect("observe s1");
    store
        .record(&snap(&target, PayloadKind::Commodities, 100 * day + 600, 0), &[], b"s2")
        .expect("s2");
    store.close_missing(&target, PayloadKind::Commodities, 2, 100 * day + 600).expect("close A");

    // Sight B: opened day 105, closed day 105 (RECENT closed → kept).
    let b = [listing(2, &item(1), 200, 1)];
    store.record(&snap(&target, PayloadKind::Commodities, 105 * day, 1), &b, b"s3").expect("s3");
    store.observe(&target, PayloadKind::Commodities, &b, 3, 105 * day).expect("observe s3");
    store
        .record(&snap(&target, PayloadKind::Commodities, 105 * day + 600, 0), &[], b"s4")
        .expect("s4");
    store.close_missing(&target, PayloadKind::Commodities, 4, 105 * day + 600).expect("close B");

    // Sight C: opened day 110, never closed (OPEN → kept).
    let c = [listing(3, &item(1), 300, 1)];
    store.record(&snap(&target, PayloadKind::Commodities, 110 * day, 1), &c, b"s5").expect("s5");
    store.observe(&target, PayloadKind::Commodities, &c, 5, 110 * day).expect("observe s5");

    let removed = store.prune_sights_before(&target, 104).expect("prune sights");
    assert_eq!(removed, 1, "only sight A is older than the cutoff");

    {
        let conn = rusqlite::Connection::open(dir.join("db.sqlite3")).expect("inspect db");
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sight WHERE target_id = ?1",
                rusqlite::params![target.as_str()],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(total, 2, "recent-closed + open sights survive");
        let open: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sight WHERE target_id = ?1 AND closed_at IS NULL",
                rusqlite::params![target.as_str()],
                |row| row.get(0),
            )
            .expect("count open");
        assert_eq!(open, 1, "the open sight is never pruned");
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// Drop-reason breakdowns persist with the snapshot metadata and read back
/// through `latest`/`list_recent`; snapshots recorded without a breakdown
/// (or pre-v5 rows) read as empty maps, not errors.
#[test]
fn snapshot_drop_reasons_round_trip_and_default_empty() {
    let dir = temp_dir("drop-reasons");
    let store = open_store(&dir, 1_000_000);
    let target = TargetId("us-4".into());
    let mut reasons = std::collections::BTreeMap::new();
    reasons.insert("missing-price".to_string(), 2);
    reasons.insert("cage-without-species".to_string(), 1);

    store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: 1_000_001,
                source_last_modified: None,
                entry_count: 3,
                dropped_count: 3,
                dropped_reasons: reasons,
                status: SnapshotStatus::Ok,
            },
            &[],
            br"[]",
        )
        .expect("record with breakdown");
    store
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Auctions,
                fetched_at: 1_000_002,
                source_last_modified: None,
                entry_count: 1,
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            &[],
            br"[]",
        )
        .expect("record without breakdown");

    let latest = store.latest(&target, PayloadKind::Commodities).expect("latest").expect("row");
    assert_eq!(latest.dropped_count, 3);
    assert_eq!(latest.dropped_reasons.get("missing-price"), Some(&2));
    assert_eq!(latest.dropped_reasons.get("cage-without-species"), Some(&1));

    let auctions =
        store.latest(&target, PayloadKind::Auctions).expect("latest auctions").expect("row");
    assert!(auctions.dropped_reasons.is_empty(), "no breakdown stored → empty map");

    std::fs::remove_dir_all(&dir).ok();
}

/// Reopening an already-migrated store keeps stored breakdowns readable (the
/// migration branch runs again and must not disturb the data).
#[test]
fn reopening_a_migrated_store_keeps_drop_reasons_readable() {
    let dir = temp_dir("drop-reasons-reopen");
    let target = TargetId("us-4".into());
    {
        let store = open_store(&dir, 1_000_000);
        store
            .record(
                &NewSnapshot {
                    target: target.clone(),
                    kind: PayloadKind::Commodities,
                    fetched_at: 1_000_001,
                    source_last_modified: None,
                    entry_count: 1,
                    dropped_count: 1,
                    dropped_reasons: std::collections::BTreeMap::from([(
                        "missing-price".to_string(),
                        1,
                    )]),
                    status: SnapshotStatus::Ok,
                },
                &[],
                br"[]",
            )
            .expect("record");
    }
    let store = open_store(&dir, 1_000_000);
    let latest = store.latest(&target, PayloadKind::Commodities).expect("latest").expect("row");
    assert_eq!(latest.dropped_reasons.get("missing-price"), Some(&1));

    std::fs::remove_dir_all(&dir).ok();
}

/// v5 → v6: hand-built pre-flavor catalog tables (bare-id primary keys, no
/// enrichment columns) carry over as `retail` rows with NULL enrichment and
/// preserved stamps. This is the schema's one data-destroying migration —
/// pinned against row loss, stamp loss and flavor-key drift.
#[test]
fn v5_catalog_rows_carry_over_as_retail_through_the_v6_rebuild() {
    let dir = temp_dir("catalog-v6-rebuild");
    let db = dir.join("db.sqlite3");
    {
        let connection = rusqlite::Connection::open(&db).expect("open v5 file");
        connection
            .execute_batch(
                "CREATE TABLE item_catalog (
                    item_id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    item_class TEXT,
                    quality TEXT,
                    fetched_at INTEGER NOT NULL
                );
                CREATE TABLE pet_catalog (
                    species_id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    fetched_at INTEGER NOT NULL
                );
                INSERT INTO item_catalog VALUES (2589, 'Linen Cloth', 'Cloth', 'common', 1000000);
                INSERT INTO pet_catalog VALUES (264, 'Crawling Claw', 999000);
                PRAGMA user_version = 5;",
            )
            .expect("seed v5 catalog");
    }
    let store = open_store(&dir, 1_000_001);
    let retail = Flavor("retail".into());

    let item = store.by_id(&retail, 2589).expect("read item").expect("item carried over");
    assert_eq!(item.name, "Linen Cloth");
    assert_eq!(item.item_class.as_deref(), Some("Cloth"), "carried columns survive");
    assert_eq!(item.quality.as_deref(), Some("common"));
    assert_eq!(item.fetched_at, 1_000_000, "the stamp survives for sparse scheduling");
    assert!(item.is_sparse(), "carried rows are NULL-enriched and re-fetched lazily");
    assert!(
        store.by_id(&Flavor("forever".into()), 2589).expect("read").is_none(),
        "carried rows are scoped to retail only"
    );

    let pet = store.pet_by_species(&retail, 264).expect("read pet").expect("pet carried over");
    assert_eq!(pet.name, "Crawling Claw");

    std::fs::remove_dir_all(&dir).ok();
}

/// v6 → v7: the merged aggregate tables are recreated per category and the
/// day folds are rebuilt exactly from the retained raw listings — last
/// observation per auction wins within its snapshot order, commodity and
/// item-AH rows land in separate categories, auctions-payload pets split off
/// into `pet`. The old merged fold row (which cannot be split) is discarded,
/// sale/cache tables start empty, and `sight` is untouched.
#[test]
#[allow(clippy::too_many_lines)]
fn v7_category_migration_rebuilds_folds_from_retained_listings() {
    let dir = temp_dir("category-v7-rebuild");
    let db = dir.join("db.sqlite3");
    {
        let connection = rusqlite::Connection::open(&db).expect("open v6 file");
        connection
            .execute_batch(
                "CREATE TABLE item_ref (
                     id INTEGER PRIMARY KEY,
                     kind TEXT NOT NULL,
                     item_id INTEGER NOT NULL,
                     pet_species INTEGER NOT NULL DEFAULT 0,
                     pet_level INTEGER NOT NULL DEFAULT 0,
                     pet_quality INTEGER NOT NULL DEFAULT 0,
                     pet_breed INTEGER NOT NULL DEFAULT 0,
                     UNIQUE (kind, item_id, pet_species, pet_level, pet_quality, pet_breed)
                 );
                 INSERT INTO item_ref VALUES (1, 'item', 2589, 0, 0, 0, 0);
                 INSERT INTO item_ref VALUES (2, 'pet', 0, 42, 10, 1, 3);
                 CREATE TABLE snapshots (
                     id INTEGER PRIMARY KEY,
                     target_id TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     fetched_at INTEGER NOT NULL,
                     source_last_modified INTEGER,
                     sha256 TEXT NOT NULL,
                     archive_path TEXT,
                     entry_count INTEGER NOT NULL,
                     dropped_count INTEGER NOT NULL,
                     status TEXT NOT NULL
                 );
                 INSERT INTO snapshots VALUES (1, 'us-4', 'commodities', 1_000_000, NULL, 'c', NULL, 3, 0, 'ok');
                 INSERT INTO snapshots VALUES (2, 'us-4', 'auctions', 1_000_600, NULL, 'a', NULL, 2, 0, 'ok');
                 CREATE TABLE listing (
                     snapshot_id INTEGER NOT NULL,
                     target_id TEXT NOT NULL,
                     day INTEGER NOT NULL,
                     auction_id INTEGER NOT NULL,
                     item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
                     quantity INTEGER NOT NULL,
                     unit_price INTEGER NOT NULL,
                     bid INTEGER,
                     time_left TEXT NOT NULL,
                     context INTEGER,
                     bonus_lists TEXT NOT NULL DEFAULT '[]'
                 );
                 -- commodity AH: auction 100 re-sighted with a shrunken qty.
                 INSERT INTO listing VALUES (1, 'us-4', 11, 100, 1, 1, 100, NULL, 'VERY_LONG', NULL, '[]');
                 INSERT INTO listing VALUES (1, 'us-4', 11, 101, 1, 5, 200, NULL, 'VERY_LONG', NULL, '[]');
                 INSERT INTO listing VALUES (1, 'us-4', 11, 100, 1, 2, 100, NULL, 'VERY_LONG', NULL, '[]');
                 -- item AH: same item id in the OTHER market, plus a pet.
                 INSERT INTO listing VALUES (2, 'us-4', 11, 300, 1, 1, 300, 250, 'LONG', NULL, '[]');
                 INSERT INTO listing VALUES (2, 'us-4', 11, 400, 2, 1, 500, NULL, 'VERY_LONG', NULL, '[]');
                 CREATE TABLE item_day (
                     target_id TEXT NOT NULL,
                     item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
                     day INTEGER NOT NULL,
                     fold_json TEXT NOT NULL,
                     PRIMARY KEY (target_id, item_ref_id, day)
                 );
                 -- Stale merged fold: must be dropped, never read back.
                 INSERT INTO item_day VALUES ('us-4', 1, 11,
                     '{\"item\":{\"type\":\"item\",\"item_id\":2589},\"entries\":[[999,1,1]]}');
                 CREATE TABLE sales (
                     target_id TEXT NOT NULL,
                     item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
                     day INTEGER NOT NULL,
                     listings INTEGER NOT NULL DEFAULT 0,
                     closed INTEGER NOT NULL DEFAULT 0,
                     credited INTEGER NOT NULL DEFAULT 0,
                     qty_sold INTEGER NOT NULL DEFAULT 0,
                     PRIMARY KEY (target_id, item_ref_id, day)
                 );
                 INSERT INTO sales VALUES ('us-4', 1, 11, 3, 1, 1, 7);
                 CREATE TABLE metrics_cache (
                     target_id TEXT NOT NULL,
                     item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
                     span TEXT NOT NULL,
                     computed_at INTEGER NOT NULL,
                     block_json TEXT NOT NULL,
                     PRIMARY KEY (target_id, item_ref_id, span)
                 );
                 INSERT INTO metrics_cache VALUES ('us-4', 1, 'd1', 5, '{}');
                 PRAGMA user_version = 6;",
            )
            .expect("seed v6 aggregates");
    }
    let store = open_store(&dir, 1_000_001);
    let target = TargetId("us-4".into());
    let item1 = item(2589);
    let pet = ItemRef::Pet { species: 42, level: 10, quality: 1, breed: 3 };

    // The commodity fold: last observation of auction 100 wins (qty 2), 101
    // intact; the stale merged fold's phantom auction 999 is gone.
    let commodity = store
        .load_fold(&target, &item1, 11, Category::Commodity)
        .expect("load")
        .expect("commodity fold rebuilt from listings");
    assert_eq!(commodity.len(), 2);
    let commodity_entries: std::collections::HashMap<i64, amtracker_core::fold::DayEntry> =
        commodity.entries().collect();
    assert_eq!(
        commodity_entries.get(&100).map(|entry| (entry.price, entry.qty)),
        Some((100, 2)),
        "last observation wins inside the rebuild replay"
    );
    assert_eq!(commodity_entries.get(&101).map(|entry| (entry.price, entry.qty)), Some((200, 5)));

    // The same item id's item-AH rows live in their own category.
    let item_ah = store
        .load_fold(&target, &item1, 11, Category::Item)
        .expect("load")
        .expect("item fold rebuilt");
    assert_eq!(item_ah.len(), 1);

    // The auctions payload's pet rows split off into the pet category.
    let pet_fold =
        store.load_fold(&target, &pet, 11, Category::Pet).expect("load").expect("pet fold rebuilt");
    assert_eq!(pet_fold.len(), 1);

    // ...and nothing crosses categories.
    assert!(store.load_fold(&target, &item1, 11, Category::Pet).expect("load").is_none());
    assert!(store.load_fold(&target, &pet, 11, Category::Item).expect("load").is_none());

    // Sale aggregates and the metric cache start empty (re-accumulate);
    // the version stamp prevents a second rebuild pass.
    {
        let conn = rusqlite::Connection::open(&db).expect("inspect db");
        for table in ["sales", "metrics_cache"] {
            let rows: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
                .expect("count rows");
            assert_eq!(rows, 0, "{table} starts empty after the migration");
        }
        let version: i64 =
            conn.query_row("PRAGMA user_version", [], |row| row.get(0)).expect("version stamp");
        assert_eq!(version, 8, "the file is stamped at the new schema version");
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// Snapshot helper for the sales-ledger tests below.
fn snap(target: &TargetId, kind: PayloadKind, at: i64, entries: u64) -> NewSnapshot {
    NewSnapshot {
        target: target.clone(),
        kind,
        fetched_at: at,
        source_last_modified: None,
        entry_count: entries,
        dropped_count: 0,
        dropped_reasons: std::collections::BTreeMap::new(),
        status: SnapshotStatus::Ok,
    }
}

/// Credits must book on the sight's OPEN day (the same day its `listings`
/// denominator books), not on the close day. Otherwise sights opened just
/// before a trailing window and credited just inside it inflate the rate
/// past 1.0 — with a 12h `VERY_LONG` floor this needs an open late in day N
/// credited early in day N+1, which is an ordinary overnight sale.
#[test]
fn credits_book_on_the_open_day_so_sale_rate_stays_within_one() {
    use amtracker_core::ports::SalesRepository;

    let dir = temp_dir("cohort");
    let day = 86_400;
    // "Now" is day 110 noon: the 14d sale-rate window covers days 97..110.
    let store = open_store(&dir, 110 * day + 12 * 3_600);
    let target = TargetId("us-4".into());

    // Two VERY_LONG sights opened late on day 96 (outside the window). A
    // sub-12h vanish crossing midnight is a credit: booked on day 97 by the
    // old close-day scheme, on day 96 by the cohort scheme.
    let late_evening = 96 * day + 22 * 3_600;
    let batch = [listing(1, &item(1), 100, 1), listing(2, &item(1), 200, 1)];
    store
        .record(&snap(&target, PayloadKind::Commodities, late_evening, 2), &batch, b"s1")
        .expect("s1");
    store.observe(&target, PayloadKind::Commodities, &batch, 1, late_evening).expect("observe s1");

    // Close pass early on day 97: vanish gaps of ~11h/10.5h < 12h floor →
    // both credited. (The second sight was never re-sighted, so its
    // last-seen stays day 96 22:00.)
    let morning = 97 * day + 9 * 3_600;
    store.record(&snap(&target, PayloadKind::Commodities, morning, 0), &[], b"s2").expect("s2");
    let credits =
        store.close_missing(&target, PayloadKind::Commodities, 2, morning).expect("close");
    assert_eq!(credits.len(), 2, "overnight vanishes inside the band floor are sales");

    // One in-window listing for the same item, still open.
    let mid = 100 * day;
    let open_batch = [listing(3, &item(1), 300, 1)];
    store.record(&snap(&target, PayloadKind::Commodities, mid, 1), &open_batch, b"s3").expect("s3");
    store.observe(&target, PayloadKind::Commodities, &open_batch, 3, mid).expect("observe s3");

    // The invariant the UI/ranking rely on: per (item, day) cohort, credits
    // never exceed that day's own listings.
    {
        let conn = rusqlite::Connection::open(dir.join("db.sqlite3")).expect("inspect db");
        let rows: Vec<(i64, i64, i64)> = conn
            .prepare("SELECT day, listings, credited FROM sales WHERE category = 'commodity'")
            .expect("prepare sales scan")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query sales")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(rows.len(), 3, "two open-day cohort rows + the day-97 close-event row");
        for &(day_, listings, credited) in &rows {
            assert!(credited <= listings, "day {day_}: {credited} credits vs {listings} listings");
        }
        let open_day_rows = rows.iter().filter(|(_, listings, _)| *listings > 0).count();
        assert_eq!(open_day_rows, 2, "listings book on their open days (96 and 100) only");
    }

    // Window rate: in-window credits (0 — both booked on day 96) over
    // in-window listings (1) — NOT 2/1 under close-day booking.
    let rate =
        store.sale_rate(&target, &item(1), 14, Category::Commodity).expect("rate").expect("rate");
    assert!((rate - 0.0).abs() < 1e-9, "pre-window opens must not inflate the window rate: {rate}");

    std::fs::remove_dir_all(&dir).ok();
}

/// The close pass re-checks `last_seen_snapshot` under the write: a sight
/// re-sighted under a NEWER snapshot than the one being closed against must
/// stay open. This replays the CLI-fetch-vs-serve race (the candidate scan
/// and the UPDATE are not atomic across processes) sequentially.
#[test]
fn stale_snapshot_close_never_closes_a_re_sighted_auction() {
    use amtracker_core::ports::SalesRepository;

    let dir = temp_dir("stale-close");
    let store = open_store(&dir, 11_000);
    let target = TargetId("us-4".into());

    let batch = [listing(1, &item(1), 100, 1)];
    store.record(&snap(&target, PayloadKind::Commodities, 10_000, 1), &batch, b"s1").expect("s1");
    store.observe(&target, PayloadKind::Commodities, &batch, 1, 10_000).expect("observe s1");

    // The serve process re-sights the same auction under snapshot 2.
    store.record(&snap(&target, PayloadKind::Commodities, 11_000, 1), &batch, b"s2").expect("s2");
    store.observe(&target, PayloadKind::Commodities, &batch, 2, 11_000).expect("observe s2");

    // A concurrent CLI fetch closing against the OLDER snapshot 1 must not
    // close (nor credit) the auction that snapshot 2 just re-sighted.
    let credits =
        store.close_missing(&target, PayloadKind::Commodities, 1, 11_000).expect("stale close");
    assert!(credits.is_empty(), "a re-sighted auction never vanished: no credit");

    {
        let conn = rusqlite::Connection::open(dir.join("db.sqlite3")).expect("inspect db");
        let (closed_at, credited): (Option<i64>, i64) = conn
            .query_row(
                "SELECT closed_at, credited FROM sight
                 WHERE target_id = ?1 AND auction_id = 1 AND kind = 'commodities'",
                rusqlite::params![target.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("sight present");
        assert!(closed_at.is_none(), "re-sighted sight must stay open");
        assert_eq!(credited, 0, "no credit may be stamped");
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM sales WHERE closed + credited > 0", [], |r| r.get(0))
            .expect("sales scan");
        assert_eq!(rows, 0, "no close/credit ledger entries may be booked");
    }

    // The current close pass still works normally: nothing vanished, so it
    // closes nothing.
    let credits =
        store.close_missing(&target, PayloadKind::Commodities, 2, 11_000).expect("fresh close");
    assert!(credits.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

/// A sight reopened under a DIFFERENT identity reverses its prior close on
/// the prior identity's ledger (never on the incoming item's), re-points the
/// sight at the new identity, books a fresh listing event for it, and any
/// later credit books there too.
#[test]
fn id_reuse_reversal_books_on_the_prior_identity_and_reopens_under_the_new_one() {
    use amtracker_core::ports::SalesRepository;

    let dir = temp_dir("id-reuse");
    let store = open_store(&dir, 11_800);
    let target = TargetId("us-4".into());

    // Auction 5 = item 1; vanishes 10 minutes later (VERY_LONG: credit).
    let first = [listing(5, &item(1), 100, 2)];
    store.record(&snap(&target, PayloadKind::Commodities, 10_000, 1), &first, b"s1").expect("s1");
    store.observe(&target, PayloadKind::Commodities, &first, 1, 10_000).expect("observe s1");
    store.record(&snap(&target, PayloadKind::Commodities, 10_600, 0), &[], b"s2").expect("s2");
    let credits = store.close_missing(&target, PayloadKind::Commodities, 2, 10_600).expect("close");
    assert_eq!(credits.len(), 1, "phantom-sale credit for item 1");

    // The same auction id comes back as item 2 (emulator id reuse).
    let reused = [listing(5, &item(2), 500, 1)];
    store.record(&snap(&target, PayloadKind::Commodities, 11_200, 1), &reused, b"s3").expect("s3");
    store.observe(&target, PayloadKind::Commodities, &reused, 3, 11_200).expect("reopen");

    // Item 1's phantom credit is reversed on ITEM 1's ledger; item 2 got a
    // fresh listing event so its later credit has a denominator.
    let rate1 = store.sale_rate(&target, &item(1), 14, Category::Commodity).expect("rate1");
    assert_eq!(rate1, Some(0.0), "item 1: phantom credit fully reversed");
    {
        let conn = rusqlite::Connection::open(dir.join("db.sqlite3")).expect("inspect db");
        let (item1_listings, item1_credited): (i64, i64) = conn
            .query_row(
                "SELECT listings, credited FROM sales
                 WHERE item_ref_id = (SELECT id FROM item_ref WHERE kind = 'item' AND item_id = 1)
                   AND category = 'commodity'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("item 1 ledger row");
        assert_eq!((item1_listings, item1_credited), (1, 0));
        let item2_listings: i64 = conn
            .query_row(
                "SELECT listings FROM sales
                 WHERE item_ref_id = (SELECT id FROM item_ref WHERE kind = 'item' AND item_id = 2)
                   AND category = 'commodity'",
                [],
                |row| row.get(0),
            )
            .expect("item 2 ledger row");
        assert_eq!(item2_listings, 1, "identity-changing reopen books a fresh listing event");
        let sight_ref: i64 = conn
            .query_row(
                "SELECT item_ref_id FROM sight WHERE target_id = ?1 AND auction_id = 5
                   AND kind = 'commodities'",
                rusqlite::params![target.as_str()],
                |row| row.get(0),
            )
            .expect("sight present");
        let item2_ref: i64 = conn
            .query_row("SELECT id FROM item_ref WHERE kind = 'item' AND item_id = 2", [], |row| {
                row.get(0)
            })
            .expect("item 2 ref");
        assert_eq!(sight_ref, item2_ref, "sight re-points at the incoming identity");
    }

    // The reused sight's own vanish credits item 2 (not item 1).
    store.record(&snap(&target, PayloadKind::Commodities, 11_800, 0), &[], b"s4").expect("s4");
    let credits = store.close_missing(&target, PayloadKind::Commodities, 4, 11_800).expect("close");
    assert_eq!(credits.len(), 1);
    assert_eq!(credits.first().map(|c| c.item.clone()), Some(item(2)));
    let rate2 = store.sale_rate(&target, &item(2), 14, Category::Commodity).expect("rate2");
    assert_eq!(rate2, Some(1.0), "item 2: 1 credit against its own 1 listing");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sale_recency_tracks_last_credit_and_closures_after_it() {
    const DAY: i64 = 86_400;
    let dir = temp_dir("sale-recency");
    // Clock pinned to day 15: `sale_recency` measures "days since" against
    // the injected clock; the cycles below pass their own `at` stamps, so
    // ledger rows still book on their true days.
    let t = |d: i64, hour: i64| d * DAY + hour * 3_600;
    let store = open_store(&dir, t(15, 12));
    let target = TargetId("us-4".into());
    let item1 = item(1);

    let snap = |_id: i64, at: i64, count: usize| NewSnapshot {
        target: target.clone(),
        kind: PayloadKind::Commodities,
        fetched_at: at,
        source_last_modified: None,
        entry_count: count as u64,
        dropped_count: 0,
        dropped_reasons: std::collections::BTreeMap::new(),
        status: SnapshotStatus::Ok,
    };

    // Day 12: a Long-band sight opens at 08:00 and is gone by 09:00 —
    // vanished well before the band's 2 h lower bound, so the vanishing
    // credits as a sale (open-day cohort booking) and the closure itself
    // books on the close day.
    let mut open1 = [listing(1, &item1, 100, 1)];
    open1[0].time_left = TimeLeftBand::Long;
    store.record(&snap(1, t(12, 8), open1.len()), &open1, b"p1".as_slice()).expect("record s1");
    store.observe(&target, PayloadKind::Commodities, &open1, 1, t(12, 8)).expect("observe s1");
    store.record(&snap(2, t(12, 9), 0), &[], b"p2".as_slice()).expect("record s2");
    let credits =
        store.close_missing(&target, PayloadKind::Commodities, 2, t(12, 9)).expect("close s2");
    assert_eq!(credits.len(), 1, "the 1-hour vanish credits");

    // Day 14: a fresh Short-band sight opens; day 15 it vanishes after ten
    // minutes — inside the band, so it closes WITHOUT a credit.
    let open2 = [listing(2, &item1, 120, 1)];
    let mut short = open2;
    short[0].time_left = TimeLeftBand::Short;
    store.record(&snap(3, t(14, 10), short.len()), &short, b"p3".as_slice()).expect("record s3");
    store.observe(&target, PayloadKind::Commodities, &short, 3, t(14, 10)).expect("observe s3");
    store.record(&snap(4, t(15, 8), 0), &[], b"p4".as_slice()).expect("record s4");
    store.close_missing(&target, PayloadKind::Commodities, 4, t(15, 8)).expect("close s4");

    // Recency: last credited sale on day 12 (its own close books on the
    // same day, so the strict `day > last_sale_day` excludes it); the only
    // later closure is the quiet Short vanish (day 15). Days since =
    // day 15 − day 12 = 3.
    let recency = store
        .sale_recency(&target, &item1, Category::Commodity)
        .expect("sale_recency")
        .expect("a credited sale exists");
    assert_eq!(recency.last_sale_day, 12);
    assert_eq!(recency.closures_since, 1);

    // Batched path stays positionally aligned; a never-sold identity reads
    // as None (absence, not zero).
    let recencies = store
        .sale_recency_each(&target, &[item1.clone(), item(2)], Category::Commodity)
        .expect("sale_recency_each");
    assert_eq!(recencies[0].as_ref(), Some(&recency));
    assert_eq!(recencies[1], None);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn pooled_sale_recency_takes_the_newest_variant_sale_and_sums_closures_after_it() {
    const DAY: i64 = 86_400;
    let dir = temp_dir("pooled-sale-recency");
    let t = |d: i64, hour: i64| d * DAY + hour * 3_600;
    let store = open_store(&dir, t(15, 12));
    let target = TargetId("us-4".into());
    let variant_a = ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 4 };
    let variant_b = ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 };
    let never_sold = ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 6 };

    let snap = |_id: i64, at: i64, count: usize| NewSnapshot {
        target: target.clone(),
        kind: PayloadKind::Auctions,
        fetched_at: at,
        source_last_modified: None,
        entry_count: count as u64,
        dropped_count: 0,
        dropped_reasons: std::collections::BTreeMap::new(),
        status: SnapshotStatus::Ok,
    };
    let pet_listing = |auction_id: i64, item: &ItemRef, price: i64| {
        let mut row = listing(auction_id, item, price, 1);
        row.time_left = TimeLeftBand::Long;
        row
    };
    let pet_expiry = |auction_id: i64, item: &ItemRef, price: i64| {
        let mut row = listing(auction_id, item, price, 1);
        row.time_left = TimeLeftBand::Short;
        row
    };
    let mut next_id = 0i64;
    let mut record = |store: &SqliteStore, at: i64, listings: &[NormalizedListing]| {
        next_id += 1;
        store
            .record(&snap(next_id, at, listings.len()), listings, format!("p{next_id}").as_bytes())
            .expect("record snapshot")
    };

    // Variant a: a Long-band sight opens day 10 and vanishes after one hour
    // (inside the band's lower bound) — the vanish credits, booking the
    // credit on the OPEN day 10 and the closure on the close day 10.
    let open = [pet_listing(1, &variant_a, 900_000)];
    let s1 = record(&store, t(10, 8), &open);
    store.observe(&target, PayloadKind::Auctions, &open, s1, t(10, 8)).expect("observe a credit");
    let s2 = record(&store, t(10, 9), &[]);
    store.close_missing(&target, PayloadKind::Auctions, s2, t(10, 9)).expect("close a credit");

    // Variant a again: a Short-band sight opens day 11 and vanishes a full
    // day later — natural expiry, a closure WITHOUT credit on day 12.
    let open = [pet_expiry(2, &variant_a, 900_000)];
    let s3 = record(&store, t(11, 8), &open);
    store.observe(&target, PayloadKind::Auctions, &open, s3, t(11, 8)).expect("observe a expiry");
    let s4 = record(&store, t(12, 8), &[]);
    store.close_missing(&target, PayloadKind::Auctions, s4, t(12, 8)).expect("close a expiry");

    // Variant b: a credited sale on day 13 — the POOLED anchor (any variant
    // selling resets the species' clock).
    let open = [pet_listing(3, &variant_b, 950_000)];
    let s5 = record(&store, t(13, 8), &open);
    store.observe(&target, PayloadKind::Auctions, &open, s5, t(13, 8)).expect("observe b credit");
    let s6 = record(&store, t(13, 9), &[]);
    store.close_missing(&target, PayloadKind::Auctions, s6, t(13, 9)).expect("close b credit");

    // Variants b and c expire together on day 15 (Short sights opened day
    // 14): two closures AFTER the pooled day, one from a member that never
    // credited a sale — species churn is pooled evidence.
    let open = [pet_expiry(4, &variant_b, 950_000), pet_expiry(5, &never_sold, 900_000)];
    let s7 = record(&store, t(14, 9), &open);
    store
        .observe(&target, PayloadKind::Auctions, &open, s7, t(14, 9))
        .expect("observe b/c expiries");
    let s8 = record(&store, t(15, 8), &[]);
    store.close_missing(&target, PayloadKind::Auctions, s8, t(15, 8)).expect("close b/c expiries");

    // Pooled: anchor day 13 (MAX — the old earliest-variant MIN would read
    // 10); closures strictly after it are b's and c's day-15 expiries (2).
    // Variant a's day-12 closure falls between the anchors — invisible per
    // member, visible to the pooled query only through the exact pass.
    let pooled = store
        .sale_recency_multi(
            &target,
            &[variant_a.clone(), variant_b.clone(), never_sold.clone()],
            Category::Pet,
        )
        .expect("sale_recency_multi")
        .expect("a credited sale exists in the group");
    assert_eq!(pooled.last_sale_day, 13);
    assert_eq!(pooled.closures_since, 2);

    // A group where no member ever credited a sale reads as None.
    let quiet = store
        .sale_recency_multi(&target, std::slice::from_ref(&never_sold), Category::Pet)
        .expect("sale_recency_multi quiet");
    assert_eq!(quiet, None);

    std::fs::remove_dir_all(&dir).ok();
}

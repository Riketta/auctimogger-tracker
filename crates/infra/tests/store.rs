//! SQLite repository integration tests: atomic snapshot+listing records,
//! fold round-trips through the serialization mirror, retention pruning.

// Test assertions may unwrap/expect/panic and index: fixture-driven,
// deterministic inputs (the SecOutfall precedent).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

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
    fold::DayFold,
    ports::{
        ClockPort,
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
    store.save_fold(&target, &pet, 11, &fold).expect("save");

    let loaded = store.load_fold(&target, &pet, 11).expect("load").expect("fold present");
    assert_eq!(loaded, fold, "fold survives the serialization round-trip");
    assert_eq!(loaded.histogram().listing_count, 2);

    // unknown item/day has no fold
    assert!(store.load_fold(&TargetId("other".into()), &pet, 11).expect("load").is_none());

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
    assert!(store.latest(&target, PayloadKind::Commodities).expect("latest").is_some());

    // old fold rows are permanent too
    let old_item = item(1);
    store.save_fold(&target, &old_item, 10, &DayFold::new(old_item.clone())).expect("save");
    assert!(store.load_fold(&target, &old_item, 10).expect("load").is_some());

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
    let rate = store.sale_rate(&target, &item(1), 14).expect("rate").expect("rate");
    assert!((rate - 1.0).abs() < 1e-9, "all vanishings credited so far");

    // item 2 closed past its band floor: credited stays 0, denominator 1.
    let rate2 = store.sale_rate(&target, &item(2), 14).expect("rate").expect("rate");
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
    let rate = store.sale_rate(&target, &item(1), 14).expect("rate");
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
    store.put(&target, &item, Span::D1, &block, 1234).expect("put");

    let cached = store.get(&target, &item, Span::D1).expect("get").expect("cached");
    assert_eq!(cached.computed_at, 1234);
    assert_eq!(cached.block, block, "block survives the serialization round-trip");

    store.invalidate(&target).expect("invalidate");
    assert!(store.get(&target, &item, Span::D1).expect("get").is_none());

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
    };
    store.put(&target, &item_a, Span::D1, &block, 1234).expect("put a d1");
    store.put(&target, &item_b, Span::D1, &block, 1234).expect("put b d1");
    store.put(&target, &item_a, Span::D7, &block, 1234).expect("put a d7");

    store.invalidate_items(&target, std::slice::from_ref(&item_a)).expect("invalidate a");
    assert!(store.get(&target, &item_a, Span::D1).expect("get a d1").is_none());
    assert!(store.get(&target, &item_a, Span::D7).expect("get a d7").is_none());
    assert!(store.get(&target, &item_b, Span::D1).expect("get b d1").is_some());

    store.invalidate_items(&target, &[item(999_999_999)]).expect("unknown identity");
    assert!(store.get(&target, &item_b, Span::D1).expect("get b d1").is_some());

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
    let rates = store.sale_rate_each(&target, &items, 14).expect("rates");
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
            NameRecord,
            PetNameRecord,
            SearchHit,
        },
        ports::ItemCatalogRepository,
    };

    let dir = temp_dir("pet-catalog");
    let store = open_store(&dir, 1_000_000);

    store
        .upsert(&[NameRecord {
            item_id: 2589,
            name: "Linen Cloth".into(),
            item_class: Some("Cloth".into()),
            quality: Some("Common".into()),
        }])
        .expect("upsert item");
    store
        .upsert_pets(&[
            PetNameRecord { species_id: 264, name: "Mechanical Squirrel Box".into() },
            PetNameRecord { species_id: 42, name: "Linen-labeled Pet".into() },
        ])
        .expect("upsert pets");

    // by-species reads.
    let pet = store.pet_by_species(264).expect("by_species").expect("named");
    assert_eq!(pet.name, "Mechanical Squirrel Box");
    assert!(store.pet_by_species(999_999).expect("by_species").is_none());

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
        domain::NameRecord,
        ports::ItemCatalogRepository,
    };

    let dir = temp_dir("search-offset");
    let store = open_store(&dir, 1_000_000);
    store
        .upsert(&[
            NameRecord { item_id: 1, name: "Linen Cloth".into(), item_class: None, quality: None },
            NameRecord {
                item_id: 2,
                name: "Bolts of Linen Cloth".into(),
                item_class: None,
                quality: None,
            },
            NameRecord {
                item_id: 3,
                name: "Heavy Linen Glove".into(),
                item_class: None,
                quality: None,
            },
        ])
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
    };

    store.put(&target, &item1, Span::D7, &block(100), 999).expect("put item1");
    store.put(&target, &item2, Span::D7, &block(200), 999).expect("put item2");

    let items = vec![item1.clone(), unknown.clone(), item2.clone()];
    let blocks = store.get_many(&target, Span::D7, &items).expect("get_many");
    assert_eq!(blocks.len(), 3, "one slot per requested identity");
    assert_eq!(blocks[0].as_ref().expect("item1 cached").block.median, 100);
    assert!(blocks[1].is_none(), "unknown identity has no cache rows");
    assert_eq!(blocks[2].as_ref().expect("item2 cached").block.median, 200);

    // Spans partition the cache: the same items read under another span
    // come back empty.
    let other = store.get_many(&target, Span::D1, &items).expect("get_many d1");
    assert!(other.iter().all(Option::is_none));

    std::fs::remove_dir_all(&dir).ok();
}

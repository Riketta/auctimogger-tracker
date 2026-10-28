//! Snapshot-viewer integration tests: the browse page read model (filters,
//! sorts, pagination), the lazily-built class summary, the v8 in-place
//! migration, and item-level round-trips through the catalog.

// Test assertions may unwrap/expect/panic and index: fixture-driven,
// deterministic inputs (the SecOutfall precedent).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
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
        ClassSummary,
        Flavor,
        ItemRef,
        ListingKind,
        ListingQuery,
        ListingSortKey,
        NameRecord,
        NewSnapshot,
        NormalizedListing,
        PayloadKind,
        SnapshotStatus,
        TargetId,
        TimeLeftBand,
    },
    ports::{
        ClockPort,
        ItemCatalogRepository,
        PriceHistoryRepository,
        SnapshotRepository,
    },
};
use amtracker_infra::store::SqliteStore;

/// Fixed clock: catalog stamps flow through the injected port so tests are
/// deterministic by construction.
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

fn item(id: i64) -> ItemRef {
    ItemRef::Item { item_id: id }
}

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

/// A listing with full control over every stored field (the fold tests'
/// stock helper pins `time_left`/`bonus_lists`; the viewer cares about bands
/// and bids).
fn view_listing(
    auction_id: i64,
    item: &ItemRef,
    price: i64,
    qty: i64,
    bid: Option<i64>,
    band: TimeLeftBand,
) -> NormalizedListing {
    NormalizedListing {
        auction_id,
        item_ref: item.clone(),
        kind: ListingKind::Commodity,
        quantity: qty,
        unit_price: price,
        bid,
        time_left: band,
        context: None,
        bonus_lists: Vec::new(),
    }
}

fn record(
    name: &str,
    class: Option<&str>,
    quality: Option<&str>,
    level: Option<i64>,
) -> NameRecord {
    NameRecord {
        item_id: 0,
        name: name.to_string(),
        item_class: class.map(str::to_string),
        item_subclass: None,
        inventory_slot: None,
        quality: quality.map(str::to_string),
        icon: None,
        sell_price: None,
        item_level: level,
        fetched_at: 0,
    }
}

fn catalog_record(id: i64, mut record: NameRecord) -> NameRecord {
    record.item_id = id;
    record
}

fn query(
    prefix: Option<&str>,
    item_id: Option<i64>,
    class: Option<&str>,
    quality: Option<&str>,
    sort: ListingSortKey,
    desc: bool,
    page: u32,
) -> ListingQuery {
    ListingQuery {
        name_prefix: prefix.map(str::to_string),
        item_id,
        class: class.map(str::to_string),
        quality: quality.map(str::to_string),
        sort,
        desc,
        page,
    }
}

/// The snapshot viewer's read model: filters (prefix, item id, class,
/// quality), all three sort keys with direction and the auction-id tiebreak,
/// and offset pagination over fixed-size pages.
#[test]
fn listing_pages_sort_filter_and_paginate() {
    let dir = temp_dir("listing-pages");
    let store = open_store(&dir, 1_000_000);
    let flavor = Flavor("retail".into());
    let target = TargetId("us-4".into());

    // Catalog identity: names/classes/levels for a few items; one sparse.
    store
        .upsert(
            &flavor,
            &[
                catalog_record(1, record("Linen Cloth", Some("Cloth"), Some("Common"), None)),
                catalog_record(
                    2,
                    record("Swiftness Potion", Some("Consumable"), Some("Common"), Some(5)),
                ),
                catalog_record(3, record("Arcane Crystal", Some("Gem"), Some("Rare"), None)),
            ],
        )
        .expect("seed catalog");
    let sparse = item(9);

    // Prices order every sort unambiguously except one deliberate 100/100
    // tie (101/103) so the auction-id tiebreak is actually exercised; bands
    // so the semantic rank (Short < Medium < Long) differs from the wire
    // text order (LONG < MEDIUM < SHORT alphabetically).
    let listings = [
        view_listing(100, &item(1), 300, 20, None, TimeLeftBand::Long),
        view_listing(101, &item(2), 100, 1, Some(90), TimeLeftBand::Short),
        view_listing(102, &item(3), 200, 5, None, TimeLeftBand::Medium),
        view_listing(103, &sparse, 100, 2, None, TimeLeftBand::VeryLong),
    ];
    let snapshot_id = store
        .record(&snap(&target, PayloadKind::Commodities, 1_000_000, 4), &listings, b"p")
        .expect("record");

    // Price ascending is the default and the served page index reports back.
    let page = store
        .listings_page(
            &flavor,
            snapshot_id,
            &query(None, None, None, None, ListingSortKey::Price, false, 1),
        )
        .expect("price asc");
    assert_eq!(page.total, 4);
    assert_eq!(page.pages, 1);
    assert_eq!(page.page, 1);
    let prices: Vec<i64> = page.rows.iter().map(|row| row.unit_price).collect();
    assert_eq!(prices, [100, 100, 200, 300]);
    let names: Vec<Option<&str>> = page.rows.iter().map(|row| row.name.as_deref()).collect();
    assert_eq!(
        names,
        [Some("Swiftness Potion"), None, Some("Arcane Crystal"), Some("Linen Cloth")],
        "catalog names resolve; sparse identities read None"
    );
    let levels: Vec<Option<i64>> = page.rows.iter().map(|row| row.item_level).collect();
    assert_eq!(levels, [Some(5), None, None, None], "item level comes from the catalog");

    // Price descending flips; the tied 100s stay deterministic via the
    // ascending auction-id tiebreak (stable in both directions).
    let page = store
        .listings_page(
            &flavor,
            snapshot_id,
            &query(None, None, None, None, ListingSortKey::Price, true, 1),
        )
        .expect("price desc");
    let ids: Vec<i64> = page.rows.iter().map(|row| row.auction_id).collect();
    assert_eq!(ids, [100, 102, 101, 103], "descending price, ties broken by auction id");

    // Time-left sorts by semantic rank, not the wire text.
    let page = store
        .listings_page(
            &flavor,
            snapshot_id,
            &query(None, None, None, None, ListingSortKey::TimeLeft, false, 1),
        )
        .expect("band asc");
    let ids: Vec<i64> = page.rows.iter().map(|row| row.auction_id).collect();
    assert_eq!(ids, [101, 102, 100, 103], "Short < Medium < Long < VeryLong");

    let page = store
        .listings_page(
            &flavor,
            snapshot_id,
            &query(None, None, None, None, ListingSortKey::Quantity, true, 1),
        )
        .expect("qty desc");
    let qtys: Vec<i64> = page.rows.iter().map(|row| row.quantity).collect();
    assert_eq!(qtys, [20, 5, 2, 1]);

    // Filters: exact item id, name prefix (case-insensitive), class, quality.
    let page = store
        .listings_page(
            &flavor,
            snapshot_id,
            &query(None, Some(2), None, None, ListingSortKey::Price, false, 1),
        )
        .expect("item filter");
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].auction_id, 101);

    let page = store
        .listings_page(
            &flavor,
            snapshot_id,
            &query(Some("linen"), None, None, None, ListingSortKey::Price, false, 1),
        )
        .expect("prefix filter");
    assert_eq!(page.total, 1, "prefix search is case-insensitive");
    assert_eq!(page.rows[0].name.as_deref(), Some("Linen Cloth"));

    let page = store
        .listings_page(
            &flavor,
            snapshot_id,
            &query(Some("LINEN CLOTH EXTRA"), None, None, None, ListingSortKey::Price, false, 1),
        )
        .expect("prefix misses");
    assert_eq!(page.total, 0, "a longer prefix matches nothing");

    let page = store
        .listings_page(
            &flavor,
            snapshot_id,
            &query(Some("%"), None, None, None, ListingSortKey::Price, false, 1),
        )
        .expect("wildcard prefix");
    assert_eq!(page.total, 0, "LIKE wildcards in user input are escaped");

    let page = store
        .listings_page(
            &flavor,
            snapshot_id,
            &query(None, None, Some("consumable"), None, ListingSortKey::Price, false, 1),
        )
        .expect("class filter");
    assert_eq!(page.total, 1, "class labels match case-insensitively");
    assert_eq!(page.rows[0].auction_id, 101);

    let page = store
        .listings_page(
            &flavor,
            snapshot_id,
            &query(None, None, None, Some("rare"), ListingSortKey::Price, false, 1),
        )
        .expect("quality filter");
    assert_eq!(page.total, 1, "quality labels match case-insensitively (wowhead vs API spellings)");
    assert_eq!(page.rows[0].auction_id, 102);

    // Pagination: page size 50, page 2 carries the remainder; an
    // out-of-range page clamps to the last page; page 0 is defensive 1.
    let mut many = Vec::new();
    for n in 0..55_i64 {
        many.push(view_listing(1_000 + n, &item(1), 1_000 + n, 1, None, TimeLeftBand::VeryLong));
    }
    let big = store
        .record(&snap(&target, PayloadKind::Auctions, 1_000_100, 55), &many, b"big")
        .expect("record big");
    let page = store
        .listings_page(
            &flavor,
            big,
            &query(None, None, None, None, ListingSortKey::Price, false, 1),
        )
        .expect("page 1");
    assert_eq!(page.total, 55);
    assert_eq!(page.pages, 2);
    assert_eq!(page.rows.len(), 50);
    let page = store
        .listings_page(
            &flavor,
            big,
            &query(None, None, None, None, ListingSortKey::Price, false, 2),
        )
        .expect("page 2");
    assert_eq!(page.rows.len(), 5);
    assert_eq!(page.page, 2);
    let page = store
        .listings_page(
            &flavor,
            big,
            &query(None, None, None, None, ListingSortKey::Price, false, 9),
        )
        .expect("far page");
    assert_eq!(page.page, 2, "an out-of-range page clamps to the last page");
    assert_eq!(page.rows.len(), 5);
    let page = store
        .listings_page(
            &flavor,
            big,
            &query(None, None, None, None, ListingSortKey::Price, false, 0),
        )
        .expect("zero page");
    assert_eq!(page.page, 1, "page 0 is defensively page 1");
    assert_eq!(page.rows.len(), 50);

    // An unknown snapshot id reads as an empty page, not an error.
    let page = store
        .listings_page(
            &flavor,
            999,
            &query(None, None, None, None, ListingSortKey::Price, false, 1),
        )
        .expect("unknown snapshot");
    assert_eq!(page.total, 0);
    assert!(page.rows.is_empty(), "unknown snapshot serves an empty page");

    // Pinned views resolve: snapshot_by_id is the metadata twin of the
    // listing read.
    let meta = store.snapshot_by_id(snapshot_id).expect("by id").expect("present");
    assert_eq!(meta.id, snapshot_id);
    assert_eq!(meta.target, target);
    assert!(store.snapshot_by_id(999).expect("by id").is_none());

    std::fs::remove_dir_all(&dir).ok();
}

/// The class summary fills lazily on first read per (snapshot, flavor),
/// counts only enriched listings, and orders most-listed first.
#[test]
fn snapshot_class_summary_lazily_counts_enriched_listings() {
    let dir = temp_dir("class-summary");
    let store = open_store(&dir, 1_000_000);
    let flavor = Flavor("retail".into());
    let target = TargetId("us-4".into());
    store
        .upsert(
            &flavor,
            &[
                catalog_record(1, record("Linen Cloth", Some("Cloth"), None, None)),
                catalog_record(2, record("Swiftness Potion", Some("Consumable"), None, None)),
            ],
        )
        .expect("seed catalog");
    let sparse = item(9);
    let listings = [
        view_listing(1, &item(1), 100, 1, None, TimeLeftBand::Long),
        view_listing(2, &item(1), 110, 1, None, TimeLeftBand::Long),
        view_listing(3, &item(2), 120, 1, None, TimeLeftBand::Long),
        view_listing(4, &sparse, 130, 1, None, TimeLeftBand::Long),
    ];
    let snapshot_id = store
        .record(&snap(&target, PayloadKind::Commodities, 1_000_000, 4), &listings, b"p")
        .expect("record");

    let summary = store.snapshot_class_summary(&flavor, snapshot_id).expect("summary");
    assert_eq!(
        summary,
        vec![
            ClassSummary { class: "Cloth".into(), listings: 2 },
            ClassSummary { class: "Consumable".into(), listings: 1 },
        ],
        "most-listed class first; sparse identities are not counted"
    );

    // Second read serves the cached table (and stays stable).
    let again = store.snapshot_class_summary(&flavor, snapshot_id).expect("cached");
    assert_eq!(again, summary);

    // Another flavor has no cached rows and no catalog entries -> empty.
    assert!(
        store
            .snapshot_class_summary(&Flavor("classic".into()), snapshot_id)
            .expect("other flavor")
            .is_empty(),
        "summaries are flavor-scoped"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// v8 upgrades a v7 file in place: the catalog gains its (nullable) item
/// level column, the viewer tables/indexes appear, and all prior data
/// survives untouched — including the sale aggregates, since the destructive
/// v7 pass must not re-run on an already-upgraded file.
#[test]
fn v8_migration_adds_viewer_surface_and_preserves_data() {
    let dir = temp_dir("viewer-v8");
    let db = dir.join("db.sqlite3");
    {
        let connection = rusqlite::Connection::open(&db).expect("open v7 file");
        connection
            .execute_batch(
                "CREATE TABLE snapshots (
                     id INTEGER PRIMARY KEY,
                     target_id TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     fetched_at INTEGER NOT NULL,
                     source_last_modified INTEGER,
                     sha256 TEXT NOT NULL,
                     archive_path TEXT,
                     entry_count INTEGER NOT NULL,
                     dropped_count INTEGER NOT NULL,
                     drop_reasons TEXT,
                     status TEXT NOT NULL
                 );
                 CREATE TABLE item_ref (
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
                 INSERT INTO listing VALUES (1, 'us-4', 11, 100, 1, 1, 100, NULL, 'VERY_LONG', NULL, '[]');
                 CREATE TABLE item_day (
                     target_id TEXT NOT NULL,
                     item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
                     day INTEGER NOT NULL,
                     category TEXT NOT NULL,
                     fold_json TEXT NOT NULL,
                     PRIMARY KEY (target_id, item_ref_id, day, category)
                 );
                 INSERT INTO item_day VALUES ('us-4', 1, 11, 'commodity', '{}');
                 CREATE TABLE metrics_cache (
                     target_id TEXT NOT NULL,
                     item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
                     category TEXT NOT NULL,
                     span TEXT NOT NULL,
                     computed_at INTEGER NOT NULL,
                     block_json TEXT NOT NULL,
                     PRIMARY KEY (target_id, item_ref_id, category, span)
                 );
                 CREATE TABLE sales (
                     target_id TEXT NOT NULL,
                     item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
                     day INTEGER NOT NULL,
                     category TEXT NOT NULL,
                     listings INTEGER NOT NULL DEFAULT 0,
                     closed INTEGER NOT NULL DEFAULT 0,
                     credited INTEGER NOT NULL DEFAULT 0,
                     qty_sold INTEGER NOT NULL DEFAULT 0,
                     PRIMARY KEY (target_id, item_ref_id, day, category)
                 );
                 INSERT INTO sales VALUES ('us-4', 1, 11, 'commodity', 3, 2, 1, 5);
                 CREATE TABLE item_catalog (
                     flavor TEXT NOT NULL DEFAULT 'retail',
                     item_id INTEGER NOT NULL,
                     name TEXT NOT NULL,
                     item_class TEXT,
                     item_subclass TEXT,
                     inventory_slot TEXT,
                     quality TEXT,
                     icon TEXT,
                     sell_price INTEGER,
                     fetched_at INTEGER NOT NULL,
                     PRIMARY KEY (flavor, item_id)
                 );
                 INSERT INTO item_catalog VALUES ('retail', 2589, 'Linen Cloth', 'Cloth', NULL, NULL, 'Common', NULL, NULL, 1000000);
                 CREATE TABLE pet_catalog (
                     flavor TEXT NOT NULL DEFAULT 'retail',
                     species_id INTEGER NOT NULL,
                     name TEXT NOT NULL,
                     fetched_at INTEGER NOT NULL,
                     PRIMARY KEY (flavor, species_id)
                 );
                 INSERT INTO pet_catalog VALUES ('retail', 264, 'Crawling Claw', 999000);
                 PRAGMA user_version = 7;",
            )
            .expect("seed v7 file");
    }
    let store = open_store(&dir, 1_000_001);
    let flavor = Flavor("retail".into());

    // The catalog's carried rows survive and read with the new column.
    let carried = store.by_id(&flavor, 2589).expect("read").expect("carried");
    assert_eq!(carried.name, "Linen Cloth");
    assert_eq!(carried.item_class.as_deref(), Some("Cloth"));
    assert_eq!(carried.item_level, None, "carried rows have no level until enriched");

    // The viewer tables exist and the retained listings are readable.
    let page = store
        .listings_page(&flavor, 1, &query(None, None, None, None, ListingSortKey::Price, false, 1))
        .expect("viewer query over migrated file");
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].auction_id, 100);

    {
        let conn = rusqlite::Connection::open(&db).expect("inspect db");
        let has_level: i64 = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info('item_catalog') WHERE name = 'item_level')",
                [],
                |row| row.get(0),
            )
            .expect("pragma");
        assert_eq!(has_level, 1, "item_catalog gained item_level");
        for name in ["idx_listing_snapshot_price", "idx_listing_snapshot_qty"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                    [name],
                    |row| row.get(0),
                )
                .expect("index exists");
            assert_eq!(exists, 1, "{name} exists after migration");
        }
        let has_summary: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'snapshot_class_summary'",
                [],
                |row| row.get(0),
            )
            .expect("table exists");
        assert_eq!(has_summary, 1, "snapshot_class_summary exists after migration");
        let version: i64 =
            conn.query_row("PRAGMA user_version", [], |row| row.get(0)).expect("version stamp");
        assert_eq!(version, 8, "the file is stamped at the new schema version");
        let sale_rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM sales", [], |row| row.get(0)).expect("sales");
        assert_eq!(
            sale_rows, 1,
            "a v7 file keeps its sale aggregates: the destructive v7 pass must not re-run"
        );
        let day_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM item_day", [], |row| row.get(0))
            .expect("item_day rows");
        assert_eq!(day_rows, 1, "the seeded fold survives the upgrade untouched");
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// Item levels round-trip through the catalog and a level-less re-upsert
/// (the Wowhead leg) never erases a level the Blizzard leg stored.
#[test]
fn item_level_round_trips_and_coalesces() {
    let dir = temp_dir("item-level");
    let store = open_store(&dir, 1_000_000);
    let flavor = Flavor("retail".into());
    store
        .upsert(
            &flavor,
            &[catalog_record(
                19019,
                record("Thunderfury", Some("Weapon"), Some("Legendary"), Some(80)),
            )],
        )
        .expect("seed level");
    assert_eq!(store.by_id(&flavor, 19019).expect("read").expect("present").item_level, Some(80));

    store
        .upsert(
            &flavor,
            &[catalog_record(
                19019,
                NameRecord {
                    item_id: 19019,
                    name: "Thunderfury".into(),
                    item_class: None,
                    item_subclass: None,
                    inventory_slot: None,
                    quality: None,
                    icon: Some("inv_sword_39".into()),
                    sell_price: None,
                    item_level: None,
                    fetched_at: 0,
                },
            )],
        )
        .expect("re-upsert without level");
    let record = store.by_id(&flavor, 19019).expect("read").expect("present");
    assert_eq!(record.item_level, Some(80), "a level-less upsert COALESCEs, never erases");
    assert_eq!(record.icon.as_deref(), Some("inv_sword_39"));

    std::fs::remove_dir_all(&dir).ok();
}

/// A genuinely pre-v7 file (merged aggregates, no `category` column) still
/// runs the destructive v7 pass: merged sale rows are dropped and the day
/// folds rebuild from the retained raw listings. The shape guard added with
/// the viewer must not skip files the pass exists for.
#[test]
fn pre_v7_files_still_rebuild_their_aggregates() {
    let dir = temp_dir("viewer-prev7");
    let db = dir.join("db.sqlite3");
    {
        let connection = rusqlite::Connection::open(&db).expect("open pre-v7 file");
        connection
            .execute_batch(
                "CREATE TABLE snapshots (
                     id INTEGER PRIMARY KEY,
                     target_id TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     fetched_at INTEGER NOT NULL,
                     source_last_modified INTEGER,
                     sha256 TEXT NOT NULL,
                     archive_path TEXT,
                     entry_count INTEGER NOT NULL,
                     dropped_count INTEGER NOT NULL,
                     drop_reasons TEXT,
                     status TEXT NOT NULL
                 );
                 CREATE TABLE item_ref (
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
                 INSERT INTO snapshots VALUES (1, 'us-4', 'commodities', 950400, NULL, 'c', NULL, 1, 0, NULL, 'ok');
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
                 INSERT INTO listing VALUES (1, 'us-4', 11, 100, 1, 3, 100, NULL, 'VERY_LONG', NULL, '[]');
                 CREATE TABLE item_day (
                     target_id TEXT NOT NULL,
                     item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
                     day INTEGER NOT NULL,
                     fold_json TEXT NOT NULL,
                     PRIMARY KEY (target_id, item_ref_id, day)
                 );
                 CREATE TABLE metrics_cache (
                     target_id TEXT NOT NULL,
                     item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
                     span TEXT NOT NULL,
                     computed_at INTEGER NOT NULL,
                     block_json TEXT NOT NULL,
                     PRIMARY KEY (target_id, item_ref_id, span)
                 );
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
                 INSERT INTO sales VALUES ('us-4', 1, 11, 7, 3, 1, 9);
                 CREATE TABLE item_catalog (
                     flavor TEXT NOT NULL DEFAULT 'retail',
                     item_id INTEGER NOT NULL,
                     name TEXT NOT NULL,
                     item_class TEXT,
                     item_subclass TEXT,
                     inventory_slot TEXT,
                     quality TEXT,
                     icon TEXT,
                     sell_price INTEGER,
                     fetched_at INTEGER NOT NULL,
                     PRIMARY KEY (flavor, item_id)
                 );
                 CREATE TABLE pet_catalog (
                     flavor TEXT NOT NULL DEFAULT 'retail',
                     species_id INTEGER NOT NULL,
                     name TEXT NOT NULL,
                     fetched_at INTEGER NOT NULL,
                     PRIMARY KEY (flavor, species_id)
                 );
                 PRAGMA user_version = 6;",
            )
            .expect("seed pre-v7 file");
    }
    let _store = open_store(&dir, 1_000_001);

    {
        let conn = rusqlite::Connection::open(&db).expect("inspect db");
        let has_category: i64 = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info('item_day') WHERE name = 'category')",
                [],
                |row| row.get(0),
            )
            .expect("pragma");
        assert_eq!(has_category, 1, "the v7 pass ran for the pre-v7 shape");
        let merged_sales: i64 =
            conn.query_row("SELECT COUNT(*) FROM sales", [], |row| row.get(0)).expect("sales");
        assert_eq!(merged_sales, 0, "merged sale rows are dropped, not split");
        let rebuilt: i64 = conn
            .query_row("SELECT COUNT(*) FROM item_day WHERE category = 'commodity'", [], |row| {
                row.get(0)
            })
            .expect("rebuilt folds");
        assert_eq!(rebuilt, 1, "day folds rebuild from the retained raw listings");
        let version: i64 =
            conn.query_row("PRAGMA user_version", [], |row| row.get(0)).expect("version stamp");
        assert_eq!(version, 8, "the file lands at the current schema version");
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// The class-summary cache invalidates on catalog movement: enrichment after
/// the first read recomputes the counts, and an unenriched snapshot caches
/// its emptiness against the watermark instead of re-aggregating every read.
#[test]
fn snapshot_class_summary_refreshes_when_the_catalog_enriches() {
    let dir = temp_dir("summary-refresh");
    let store = open_store(&dir, 1_000_000);
    let flavor = Flavor("retail".into());
    let target = TargetId("us-4".into());
    let listings = [view_listing(1, &item(9), 100, 1, None, TimeLeftBand::Long)];
    let snapshot_id = store
        .record(&snap(&target, PayloadKind::Commodities, 1_000_000, 1), &listings, b"p")
        .expect("record");

    // Nothing enriched yet: the empty summary is cached under the watermark.
    let summary = store.snapshot_class_summary(&flavor, snapshot_id).expect("empty summary");
    assert!(summary.is_empty(), "unenriched identities are not counted");

    // Enrichment lands (a new catalog row moves the watermark)…
    let mut enriched = record("Minor Wizard Oil", Some("Consumable"), None, None);
    enriched.item_id = 9;
    store.upsert(&flavor, &[enriched]).expect("enrich");

    // …and the same read now sees the class without a fresh snapshot.
    let summary = store.snapshot_class_summary(&flavor, snapshot_id).expect("refreshed");
    assert_eq!(
        summary,
        vec![ClassSummary { class: "Consumable".into(), listings: 1 }],
        "later enrichment recomputes the cached summary"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Pruning a snapshot's raw listings also drops its class-summary cache, so
/// the pinned view of a pruned snapshot no longer offers phantom classes.
#[test]
fn pruning_raw_listings_drops_the_class_summary_cache() {
    let dir = temp_dir("summary-prune");
    let store = open_store(&dir, 1_000_000);
    let flavor = Flavor("retail".into());
    let target = TargetId("us-4".into());
    store
        .upsert(&flavor, &[catalog_record(1, record("Linen Cloth", Some("Cloth"), None, None))])
        .expect("seed catalog");
    let listings = [view_listing(1, &item(1), 100, 1, None, TimeLeftBand::Long)];
    let snapshot_id = store
        .record(&snap(&target, PayloadKind::Commodities, 1_000_000, 1), &listings, b"p")
        .expect("record");
    assert_eq!(
        store.snapshot_class_summary(&flavor, snapshot_id).expect("summary").len(),
        1,
        "the class is cached"
    );

    // Retention prunes the snapshot's raw listings (day 11 < 12)…
    let pruned = store.prune_raw_before(&target, 12).expect("prune");
    assert_eq!(pruned, 1, "the listing row is pruned");

    // …and the cache rows for the emptied snapshot go with them.
    assert!(
        store.snapshot_class_summary(&flavor, snapshot_id).expect("post-prune").is_empty(),
        "the summary cache does not outlive its snapshot's listings"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The price sort plans through the snapshot-leading index, so the viewer's
/// default page stays indexed as snapshots grow (pins the v8 index choice).
#[test]
fn price_sort_plans_through_the_snapshot_index() {
    let dir = temp_dir("sort-plan");
    {
        let store = open_store(&dir, 1_000_000);
        let target = TargetId("us-4".into());
        let listings = [
            view_listing(1, &item(1), 100, 1, None, TimeLeftBand::Long),
            view_listing(2, &item(2), 200, 1, None, TimeLeftBand::Long),
        ];
        store
            .record(&snap(&target, PayloadKind::Commodities, 1_000_000, 2), &listings, b"p")
            .expect("record");
    }
    let conn = rusqlite::Connection::open(dir.join("db.sqlite3")).expect("reopen db");
    let mut statement = conn
        .prepare(
            "EXPLAIN QUERY PLAN SELECT l.auction_id FROM listing l \
             WHERE l.snapshot_id = 1 \
             ORDER BY l.unit_price ASC, l.auction_id ASC LIMIT 50 OFFSET 0",
        )
        .expect("prepare plan");
    let plan = statement
        .query_map([], |row| row.get::<_, String>(3))
        .expect("plan rows")
        .filter_map(Result::ok)
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        plan.contains("idx_listing_snapshot_price"),
        "the default sort rides the snapshot-leading index, got: {plan}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

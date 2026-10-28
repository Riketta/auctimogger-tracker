//! Feed export grain: pet variants of one species must export as ONE
//! species-level entry with merged metrics (the addon collapses every pet
//! entry onto its cage-species key, and its store full-replaces per key —
//! per-variant entries would silently overwrite each other in-game).

// Test assertions may unwrap: fixture-driven, deterministic inputs.
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
        Category,
        FeedFormat,
        ItemRef,
        ListingKind,
        NormalizedListing,
        Span,
        TargetId,
        TimeLeftBand,
    },
    fold::DayFold,
    ports::{
        ClockPort,
        FeedRenderer,
        PriceHistoryRepository,
    },
    services::{
        feed_export::{
            ExportOptions,
            FeedExportService,
        },
        metrics::MetricsService,
    },
};
use amtracker_infra::{
    feed::FeedCodec,
    store::{
        AutoVacuum,
        SqliteStore,
    },
};

struct FixedClock(i64);

impl ClockPort for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "amtracker-fexp-{}-{}-{}",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn listing(auction_id: i64, item: &ItemRef, price: i64) -> NormalizedListing {
    NormalizedListing {
        auction_id,
        item_ref: item.clone(),
        kind: ListingKind::Item,
        quantity: 1,
        unit_price: price,
        bid: None,
        time_left: TimeLeftBand::VeryLong,
        context: None,
        bonus_lists: Vec::new(),
    }
}

fn pet_fold(item: &ItemRef, listings: &[NormalizedListing]) -> DayFold {
    let mut fold = DayFold::new(item.clone());
    fold.apply(listings);
    fold
}

#[test]
fn pet_variants_export_as_one_species_entry_with_merged_metrics() {
    let dir = temp_dir("merge");
    let now = 86_400 * 100; // UTC day 100
    let store = Arc::new(
        SqliteStore::open(
            &dir.join("db.sqlite3"),
            &dir.join("raw"),
            Arc::new(FixedClock(now)),
            AutoVacuum::Off,
        )
        .expect("open"),
    );
    let target = TargetId("us-4".into());

    let low = ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 };
    let high = ItemRef::Pet { species: 2546, level: 25, quality: 2, breed: 4 };
    let solo = ItemRef::Pet { species: 42, level: 10, quality: 1, breed: 3 };

    // species 2546: level-1 variant with 2 listings, level-25 with 3.
    store
        .save_fold(
            &target,
            &low,
            100,
            Category::Pet,
            &pet_fold(&low, &[listing(1, &low, 100), listing(2, &low, 120)]),
        )
        .expect("save low fold");
    store
        .save_fold(
            &target,
            &high,
            100,
            Category::Pet,
            &pet_fold(
                &high,
                &[listing(3, &high, 200), listing(4, &high, 220), listing(5, &high, 240)],
            ),
        )
        .expect("save high fold");
    // species 42: a single variant — still exports under the canonical grain.
    store
        .save_fold(&target, &solo, 100, Category::Pet, &pet_fold(&solo, &[listing(6, &solo, 300)]))
        .expect("save solo fold");

    let clock = Arc::new(FixedClock(now));
    let metrics = MetricsService::new(store.clone(), store.clone(), store.clone(), clock.clone());
    let export = FeedExportService::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(metrics),
        clock,
    );
    let feed = export
        .build_feed(&target, &ExportOptions { spans: vec![Span::D1], watchlist_only: false })
        .expect("build feed");

    assert_eq!(feed.entries.len(), 2, "one entry per SPECIES, not per variant");
    let merged = feed
        .entries
        .iter()
        .find(|entry| matches!(entry.item, ItemRef::Pet { species: 2546, .. }))
        .expect("species 2546 entry");
    assert_eq!(
        merged.item,
        ItemRef::Pet { species: 2546, level: 0, quality: 0, breed: 0 },
        "canonical species-level identity"
    );
    let block = merged.metrics.get(&Span::D1).expect("d1 block");
    assert_eq!(block.n, 5, "merged listing count across variants");
    assert_eq!(block.min, 100, "merged minimum spans both variants");

    // The rendered Lua must carry the canonical (zeroed) pet fields the addon
    // keys on, and only one entry for the species.
    let lua =
        String::from_utf8(FeedCodec.render(&feed, FeedFormat::Lua).expect("render")).expect("utf8");
    assert_eq!(lua.matches("speciesID = 2546").count(), 1, "one entry for species 2546");
    assert!(lua.contains("speciesID = 2546, level = 0"), "canonical grain in the wire format");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[allow(clippy::too_many_lines)]
fn multi_span_export_attributes_every_span_to_its_own_group() {
    // More groups than spans across two spans: span-major matrices consumed
    // as group-major cross-contaminate early entries and drop the tail.
    let dir = temp_dir("multispan");
    let now = 86_400 * 100; // UTC day 100
    let store = Arc::new(
        SqliteStore::open(
            &dir.join("db.sqlite3"),
            &dir.join("raw"),
            Arc::new(FixedClock(now)),
            AutoVacuum::Off,
        )
        .expect("open"),
    );
    let target = TargetId("us-4".into());
    let item1 = ItemRef::Item { item_id: 1 };
    let item2 = ItemRef::Item { item_id: 2 };
    let item3 = ItemRef::Item { item_id: 3 };
    let low = ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 };
    let high = ItemRef::Pet { species: 2546, level: 25, quality: 2, breed: 4 };

    // Plain items: distinct d1 vs d7 evidence per item (day 94 is the oldest
    // d7 day: today 100 - 7 + 1).
    store
        .save_fold(
            &target,
            &item1,
            100,
            Category::Item,
            &pet_fold(&item1, &[listing(1, &item1, 100)]),
        )
        .expect("item1 d1 fold");
    store
        .save_fold(
            &target,
            &item1,
            94,
            Category::Item,
            &pet_fold(&item1, &[listing(2, &item1, 300)]),
        )
        .expect("item1 d7 fold");
    store
        .save_fold(
            &target,
            &item2,
            100,
            Category::Item,
            &pet_fold(&item2, &[listing(3, &item2, 500)]),
        )
        .expect("item2 d1 fold");
    store
        .save_fold(
            &target,
            &item2,
            94,
            Category::Item,
            &pet_fold(&item2, &[listing(4, &item2, 700)]),
        )
        .expect("item2 d7 fold");
    store
        .save_fold(
            &target,
            &item3,
            100,
            Category::Item,
            &pet_fold(&item3, &[listing(5, &item3, 900)]),
        )
        .expect("item3 d1 fold");
    // Species 2546 population across both days: d1 n=5 median 220,
    // d7 n=7 median 220.
    store
        .save_fold(
            &target,
            &low,
            100,
            Category::Pet,
            &pet_fold(&low, &[listing(6, &low, 200), listing(7, &low, 220)]),
        )
        .expect("low d1 fold");
    store
        .save_fold(&target, &low, 94, Category::Pet, &pet_fold(&low, &[listing(8, &low, 300)]))
        .expect("low d7 fold");
    store
        .save_fold(
            &target,
            &high,
            100,
            Category::Pet,
            &pet_fold(
                &high,
                &[listing(9, &high, 200), listing(10, &high, 220), listing(11, &high, 240)],
            ),
        )
        .expect("high d1 fold");
    store
        .save_fold(&target, &high, 94, Category::Pet, &pet_fold(&high, &[listing(12, &high, 400)]))
        .expect("high d7 fold");

    let clock = Arc::new(FixedClock(now));
    let metrics = MetricsService::new(store.clone(), store.clone(), store.clone(), clock.clone());
    let export = FeedExportService::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(metrics),
        clock,
    );
    let feed = export
        .build_feed(
            &target,
            &ExportOptions { spans: vec![Span::D1, Span::D7], watchlist_only: false },
        )
        .expect("build feed");

    assert_eq!(feed.entries.len(), 4, "three items + one species entry, none dropped");
    let expected =
        [(1i64, (1i64, 100i64), (2i64, 200i64)), (2, (1, 500), (2, 600)), (3, (1, 900), (1, 900))];
    for (id, d1, d7) in expected {
        let entry = feed
            .entries
            .iter()
            .find(|entry| entry.item == ItemRef::Item { item_id: id })
            .unwrap_or_else(|| panic!("entry for item {id} missing"));
        let d1_block =
            entry.metrics.get(&Span::D1).unwrap_or_else(|| panic!("item {id} d1 missing"));
        assert_eq!((d1_block.n, d1_block.median), d1, "item {id} d1 block");
        let d7_block =
            entry.metrics.get(&Span::D7).unwrap_or_else(|| panic!("item {id} d7 missing"));
        assert_eq!((d7_block.n, d7_block.median), d7, "item {id} d7 block");
    }
    let pet = feed
        .entries
        .iter()
        .find(|entry| matches!(entry.item, ItemRef::Pet { species: 2546, .. }))
        .expect("species 2546 entry");
    let pet_d1 = pet.metrics.get(&Span::D1).expect("pet d1 block");
    assert_eq!((pet_d1.n, pet_d1.median), (5, 220));
    let pet_d7 = pet.metrics.get(&Span::D7).expect("pet d7 block");
    assert_eq!((pet_d7.n, pet_d7.median), (7, 220));

    std::fs::remove_dir_all(&dir).ok();
}

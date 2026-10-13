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
    store::SqliteStore,
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
        SqliteStore::open(&dir.join("db.sqlite3"), &dir.join("raw"), Arc::new(FixedClock(now)))
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
            &pet_fold(&low, &[listing(1, &low, 100), listing(2, &low, 120)]),
        )
        .expect("save low fold");
    store
        .save_fold(
            &target,
            &high,
            100,
            &pet_fold(
                &high,
                &[listing(3, &high, 200), listing(4, &high, 220), listing(5, &high, 240)],
            ),
        )
        .expect("save high fold");
    // species 42: a single variant — still exports under the canonical grain.
    store
        .save_fold(&target, &solo, 100, &pet_fold(&solo, &[listing(6, &solo, 300)]))
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

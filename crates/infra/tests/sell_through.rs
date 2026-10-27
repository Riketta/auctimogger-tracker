//! Sell-through integration over the REAL sample dump: sightings, snapshot
//! diffing, band-based crediting, and `sale_rate` aggregation.

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
        ListingKind,
        NewSnapshot,
        NormalizedListing,
        PayloadKind,
        SnapshotStatus,
        TargetId,
        TimeLeftBand,
    },
    ports::{
        ClockPort,
        SalesRepository,
        SnapshotRepository,
    },
};
use amtracker_infra::{
    blizzard_source::parse_payload,
    store::SqliteStore,
};

const SAMPLE: &str = include_str!("../../../fixtures/auctions_sample.json");

struct FixedClock(i64);

impl ClockPort for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "amtracker-sell-{}-{}-{}",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn snapshot(target: &TargetId, _id: i64, fetched_at: i64, count: usize) -> NewSnapshot {
    NewSnapshot {
        target: target.clone(),
        kind: PayloadKind::Auctions,
        fetched_at,
        source_last_modified: None,
        entry_count: count as u64,
        dropped_count: 0,
        dropped_reasons: std::collections::BTreeMap::new(),
        status: SnapshotStatus::Ok,
    }
}

#[test]
fn real_sample_sightings_close_and_credit() {
    let (listings, dropped, dropped_reasons) =
        parse_payload(PayloadKind::Auctions, SAMPLE.as_bytes()).expect("sample parses");
    let _ = dropped_reasons;
    assert_eq!(dropped, 0);
    assert!(listings.len() >= 50, "fixture should carry a meaningful batch");

    let dir = temp_dir("sample-sell");
    // Deterministic clock: the band rule only compares sighting gaps, and the
    // sale_rate window is relative to the injected clock.
    let now: i64 = 1_000_000;
    let store = SqliteStore::open(
        &dir.join("db.sqlite3"),
        &dir.join("raw"),
        Arc::new(FixedClock(now + 600)),
    )
    .expect("open");
    let target = TargetId("us-4".into());

    let t0 = now - 600; // cycle 1: 10 minutes ago

    // The real sample's head rows are mostly SHORT (never credits). Give the
    // vanishing half LONG bands (lower bound 2 h) so the 10-minute vanish is
    // deterministically a sale candidate, while keeping real prices/identities.
    let mut listings = listings;
    for listing in listings.iter_mut().take(30) {
        listing.time_left = TimeLeftBand::Long;
    }

    store
        .record(&snapshot(&target, 1, t0, listings.len()), &listings, b"s1".as_slice())
        .expect("record s1");
    store.observe(&target, PayloadKind::Auctions, &listings, 1, t0).expect("observe s1");

    // Cycle 2: the LONG-band first half vanishes 10 minutes after its last
    // sighting; the rest survive.
    let t1 = t0 + 600;
    let survivors: Vec<NormalizedListing> = listings[30..].to_vec();
    let vanished: Vec<NormalizedListing> = listings[..30].to_vec();
    store
        .record(&snapshot(&target, 2, t1, survivors.len()), &survivors, b"s2".as_slice())
        .expect("record s2");
    store.observe(&target, PayloadKind::Auctions, &survivors, 2, t1).expect("observe s2");
    let credits = store.close_missing(&target, PayloadKind::Auctions, 2, t1).expect("close");

    let mut credited_ids: Vec<i64> = credits.iter().map(|c| c.auction_id).collect();
    credited_ids.sort_unstable();
    let mut expected_ids: Vec<i64> = vanished.iter().map(|l| l.auction_id).collect();
    expected_ids.sort_unstable();
    assert_eq!(credited_ids, expected_ids, "all LONG-band vanishings credit");
    assert!(credits.iter().all(|c| c.at == t1));

    // sale_rate for an item whose rows closed: credited / total listings seen.
    // Everything here came from the auctions payload, so the category derives
    // from the credited item's identity shape.
    let first = &credits[0];
    let category = Category::derive(ListingKind::Item, &first.item);
    let rate = store
        .sale_rate(&target, &first.item, 14, category)
        .expect("sale_rate")
        .expect("sales rows exist");
    assert!(rate > 0.0 && rate <= 1.0);

    std::fs::remove_dir_all(&dir).ok();
}

//! Feed publication integration: rendered Lua must load as Lua, pass the
//! contract verifier script, and the publisher must persist atomically.

// Test assertions may unwrap: fixture-driven, deterministic inputs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::Command,
    sync::atomic::{
        AtomicU64,
        Ordering,
    },
};

use amtracker_core::{
    domain::{
        Category,
        FeedEntry,
        FeedFormat,
        FeedSnapshot,
        ItemRef,
        MetricBlock,
        Span,
        TargetId,
    },
    ports::{
        FeedPublisher,
        FeedRenderer,
    },
};
use amtracker_infra::feed::{
    FeedCodec,
    FileFeedPublisher,
};

fn temp_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "amtracker-feed-{}-{}-{}",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn sample_feed() -> FeedSnapshot {
    let mut item_metrics = BTreeMap::new();
    item_metrics.insert(Span::D1, sample_block_holder(4));
    item_metrics.insert(Span::D7, sample_block_holder(9));
    let mut pet_metrics = BTreeMap::new();
    pet_metrics.insert(Span::D1, sample_block_holder(2));
    FeedSnapshot {
        target: TargetId("us-4".into()),
        generated_at: 1_760_000_000,
        entries: vec![
            FeedEntry {
                item: ItemRef::Item { item_id: 194_784 },
                category: Category::Item,
                metrics: item_metrics,
            },
            FeedEntry {
                item: ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 },
                category: Category::Pet,
                metrics: pet_metrics,
            },
        ],
    }
}

fn sample_block_holder(n: i64) -> MetricBlock {
    MetricBlock {
        n,
        min: 100 * n,
        p05: 105 * n,
        p25: 110 * n,
        median: 120 * n,
        p75: 130 * n,
        p95: 140 * n,
        max: 150 * n,
        mad: 8,
        vwap: 121 * n,
        qty_sum: 50 * n,
        listing_count: n,
        sale_rate: Some(0.125),
    }
}

fn find_lua() -> Option<&'static str> {
    ["lua", "lua5.4", "lua5.3", "lua5.1"].into_iter().find(|candidate| {
        Command::new(candidate).arg("-v").output().is_ok_and(|out| out.status.success())
    })
}

#[test]
fn generated_lua_passes_the_contract_verifier() {
    let Some(lua) = find_lua() else {
        eprintln!("lua interpreter not found; skipping the verifier hook");
        return;
    };
    let feed = sample_feed();
    let bytes = FeedCodec.render(&feed, FeedFormat::Lua).expect("render");

    let dir = temp_dir("verify");
    let path = dir.join("feed.lua");
    std::fs::write(&path, &bytes).expect("write feed");

    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/verify_feed.lua");
    let output = Command::new(lua).arg(&script).arg(&path).output().expect("run verify_feed.lua");
    assert!(
        output.status.success(),
        "verifier rejected the feed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("2 items"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn verifier_rejects_a_broken_feed() {
    let Some(lua) = find_lua() else {
        eprintln!("lua interpreter not found; skipping the verifier hook");
        return;
    };
    let dir = temp_dir("verify-bad");
    let path = dir.join("bad.lua");
    // ordering violation: p25 above median
    std::fs::write(
        &path,
        r"AuctiMoggerTrackerFeed = { version = 2, generatedAt = 123, items = {
        { itemID = 1, metrics = { d1 = { n = 5, min = 1, p05 = 1, p25 = 99, median = 10, p75 = 20, p95 = 30, max = 40, mad = 1, vwap = 10, qtySum = 5, listingCount = 5 } } },
} }",
    )
    .expect("write bad feed");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/verify_feed.lua");
    let output = Command::new(lua).arg(&script).arg(&path).output().expect("run verifier");
    assert!(!output.status.success(), "broken feed must be rejected");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn publisher_writes_files_and_tracks_last_publication() {
    let dir = temp_dir("publish");
    let publisher = FileFeedPublisher::new(dir.join("feeds"));
    let target = TargetId("us-4".into());

    assert!(publisher.last_published(&target, FeedFormat::Lua).expect("last").is_none());

    let feed = sample_feed();
    let lua = FeedCodec.render(&feed, FeedFormat::Lua).expect("render");
    publisher.publish(&target, FeedFormat::Lua, &lua).expect("publish lua");
    let json = FeedCodec.render(&feed, FeedFormat::Json).expect("render");
    publisher.publish(&target, FeedFormat::Json, &json).expect("publish json");

    let written = dir.join("feeds").join("us-4").join("feed.lua");
    assert_eq!(std::fs::read(&written).expect("read"), lua);
    let written_json = dir.join("feeds").join("us-4").join("feed.json");
    assert_eq!(std::fs::read(&written_json).expect("read"), json);

    let last = publisher.last_published(&target, FeedFormat::Lua).expect("last").expect("recorded");
    assert_eq!(last.size, lua.len() as u64);
    assert!(last.at > 0);

    std::fs::remove_dir_all(&dir).ok();
}

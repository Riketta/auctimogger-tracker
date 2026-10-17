//! Router-level integration tests: the seam between server-rendered markup
//! and the browser enhancement layer (`app.js`) is exactly where silent
//! drift happened before (chart island shape, completion target attribute,
//! sortable headers) — these tests pin that contract, plus routing/auth
//! behavior end-to-end over a real store.

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
    config::{
        MetricsConfig,
        OpportunitiesConfig,
        PollConfig,
        ScoreWeights,
        ServerConfig,
        StorageConfig,
        TargetConfig,
        TargetKind,
        TrackerConfig,
    },
    domain::{
        utc_day,
        FetchOutcome,
        ItemRef,
        ListingKind,
        NewSnapshot,
        NormalizedListing,
        PayloadKind,
        SnapshotStatus,
        TargetId,
        TimeLeftBand,
    },
    error::SourceError,
    fold::DayFold,
    ports::{
        AuctionSourcePort,
        ClockPort,
        ItemInfoPort,
    },
    services::{
        catalog::CatalogService,
        cycle::IngestCycleService,
        feed_export::FeedExportService,
        health::HealthService,
        metrics::MetricsService,
        opportunity::OpportunityService,
        search::SearchService,
        watchlist::WatchlistService,
    },
};
use amtracker_infra::{
    clock::SystemClock,
    feed::{
        FeedCodec,
        FileFeedPublisher,
    },
    store::SqliteStore,
};
use amtracker_server::web::{
    build_router_arc,
    AppState,
    RecentCycles,
};
use axum::{
    body::Body,
    http::{
        Request,
        StatusCode,
    },
    Router,
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

/// The configured target slug every test state uses.
const TARGET: &str = "t1";

// ------------------------------------------------------------ fixtures

/// Source that always fails: pages must render regardless, and manual-fetch
/// tests get cheap error reports instead of network calls.
struct OfflineSource;

impl AuctionSourcePort for OfflineSource {
    fn fetch(
        &self,
        _target: &TargetConfig,
        _kind: PayloadKind,
        _if_modified_since: Option<i64>,
    ) -> Result<FetchOutcome, SourceError> {
        Err(SourceError::Http("offline in test".into()))
    }
}

/// Catalog lookups resolve nothing — names render as placeholders.
struct NoItemInfo;

impl ItemInfoPort for NoItemInfo {
    fn describe(
        &self,
        _item_id: i64,
    ) -> Result<Option<amtracker_core::domain::NameRecord>, SourceError> {
        Ok(None)
    }

    fn describe_pet(
        &self,
        _species_id: i64,
    ) -> Result<Option<amtracker_core::domain::PetNameRecord>, SourceError> {
        Ok(None)
    }
}

fn temp_root(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "amtracker-web-{}-{}-{}",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn config(token_env: Option<String>) -> TrackerConfig {
    TrackerConfig {
        server: ServerConfig {
            bind: "127.0.0.1:0".into(),
            token_env,
            allow_open: false,
            feed_publish_secs: 300,
        },
        storage: StorageConfig {
            database: "web-test.sqlite3".into(),
            raw_listing_days: 7,
            archive_dir: "raw".into(),
            backup_dir: None,
            backup_secs: 0,
            backup_keep: 7,
        },
        metrics: MetricsConfig {
            spans: vec![amtracker_core::domain::Span::D1, amtracker_core::domain::Span::D7],
        },
        opportunities: OpportunitiesConfig {
            min_samples: 3,
            weights: ScoreWeights { discount: 0.4, sale: 0.3, churn: 0.2, stability: 0.1 },
        },
        targets: vec![TargetConfig {
            id: TargetId(TARGET.into()),
            kind: TargetKind::BlizzardConnectedRealm { region: "us".into(), connected_realm_id: 4 },
            auth: None,
            poll: PollConfig { commodities_secs: 60, auctions_secs: 60 },
        }],
    }
}

/// Full `AppState` over a real (empty) SQLite store in a temp directory.
fn test_state(token_env: Option<String>) -> (Arc<AppState>, PathBuf) {
    let root = temp_root("state");
    let config = config(token_env);
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock);
    let store = Arc::new(
        SqliteStore::open(&root.join("db.sqlite3"), &root.join("raw"), clock.clone())
            .expect("open store"),
    );
    let item_info: Arc<dyn ItemInfoPort> = Arc::new(NoItemInfo);
    let metrics =
        Arc::new(MetricsService::new(store.clone(), store.clone(), store.clone(), clock.clone()));
    let state = AppState {
        config: config.clone(),
        clock: clock.clone(),
        health: Arc::new(HealthService::new(store.clone(), config.clone(), clock.clone())),
        search: Arc::new(SearchService::new(store.clone())),
        catalog: Arc::new(CatalogService::new(store.clone(), item_info.clone())),
        metrics: metrics.clone(),
        opportunities: Arc::new(OpportunityService::new(
            store.clone(),
            store.clone(),
            clock.clone(),
            &config,
        )),
        watchlist: Arc::new(WatchlistService::new(store.clone(), clock.clone())),
        watchlist_report: Arc::new(
            amtracker_core::services::watchlist_report::WatchlistReportService::new(
                store.clone(),
                metrics.clone(),
                store.clone(),
                store.clone(),
                clock.clone(),
            ),
        ),
        feed_export: Arc::new(FeedExportService::new(
            store.clone(),
            store.clone(),
            store.clone(),
            metrics,
            clock.clone(),
        )),
        publisher: Arc::new(FileFeedPublisher::new(root.join("feeds"))),
        renderer: Arc::new(FeedCodec),
        cycle: Arc::new(IngestCycleService::new(
            Arc::new(OfflineSource),
            store.clone(),
            store.clone(),
            store.clone(),
            store.clone(),
            clock.clone(),
        )),
        history: store.clone(),
        snapshots: store.clone(),
        item_info,
        cycle_lock: Arc::new(std::sync::Mutex::new(())),
        recent_cycles: Arc::new(RecentCycles::new(8)),
    };
    (Arc::new(state), root)
}

fn listing(auction_id: i64, item: &ItemRef, price: i64) -> NormalizedListing {
    NormalizedListing {
        auction_id,
        item_ref: item.clone(),
        kind: ListingKind::Commodity,
        quantity: 1,
        unit_price: price,
        bid: None,
        time_left: TimeLeftBand::VeryLong,
        context: None,
        bonus_lists: Vec::new(),
    }
}

/// Seeds two day folds (4 listings each, distinct medians) so the item page
/// has a real chart island AND the item clears the opportunity evidence floor
/// (samples at least `min_samples`, window churn at least 1.0 per day) —
/// ranking and overview tables render real rows.
fn seed_history(state: &AppState) {
    let target = TargetId(TARGET.into());
    let item = ItemRef::Item { item_id: 2589 };
    let today = utc_day(state.clock.now());
    for (offset, price) in [(1_i64, 100_i64), (0, 200)] {
        let mut fold = DayFold::new(item.clone());
        let listings: Vec<NormalizedListing> =
            (0..4).map(|n| listing(1000 + offset * 100 + n, &item, price)).collect();
        fold.apply(&listings);
        state.history.save_fold(&target, &item, today - offset, &fold).expect("save fold");
    }
    // A finished cycle record so the dashboard's recent-cycles table renders.
    state.recent_cycles.push(crate_recent_cycle());
}

fn crate_recent_cycle() -> amtracker_server::web::CycleRecord {
    amtracker_server::web::CycleRecord {
        at: 1_000,
        target: TARGET.into(),
        kind: "commodities".into(),
        outcome: "fresh".into(),
        entries: 4,
        dropped: 0,
        folds: 1,
        credits: 0,
        error: None,
    }
}

async fn get(router: Router, uri: &str) -> (StatusCode, String) {
    let response = router
        .oneshot(Request::builder().uri(uri).body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = response.into_body().collect().await.expect("body").to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn post_json(router: Router, uri: &str, content_type: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().method("POST").uri(uri);
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let response =
        router.oneshot(builder.body(Body::from("{}")).expect("request")).await.expect("response");
    response.status()
}

// --------------------------------------------------------------- tests

/// M2 contract: the search box must carry the target slug — `app.js` builds
/// completion links from it, and an empty target dead-ends on a 400 page.
#[tokio::test]
async fn items_page_carries_the_completion_target() {
    let (state, root) = test_state(None);
    let (status, body) = get(build_router_arc(Arc::clone(&state)), "/items?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"data-search-endpoint="/api/search""#), "endpoint hook missing");
    assert!(
        body.contains(r#"data-search-target="t1""#),
        "completion target hook missing — completions would link to /items//<id>"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// M1 contract: the chart data island must be a `days`-keyed object — the
/// exact shape `app.js` parses. A bare array makes the interactive chart dead
/// code.
#[tokio::test]
async fn chart_island_matches_the_shape_app_js_parses() {
    let (state, root) = test_state(None);
    seed_history(&state);
    let (status, body) = get(build_router_arc(Arc::clone(&state)), "/items/t1/2589").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"id="chart-data""#), "island missing despite seeded folds");

    let island_json = body
        .split(r#"<script type="application/json" id="chart-data">"#)
        .nth(1)
        .and_then(|rest| rest.split("</script>").next())
        .expect("island present");
    let island: Value = serde_json::from_str(island_json).expect("island is valid JSON");
    let days = island.get("days").and_then(Value::as_array).expect("island must be {days: [...]}");
    assert!(days.len() >= 2, "both seeded days are chart rows");
    for day in days {
        assert!(day.get("median").is_some_and(Value::is_i64), "median present");
        assert!(day.get("p05").is_some_and(Value::is_i64), "p05 present");
        assert!(day.get("p75").is_some_and(Value::is_i64), "p75 present");
        assert!(day.get("label").is_some_and(Value::is_string), "day label is a date string");
    }
    std::fs::remove_dir_all(&root).ok();
}

/// M3 contract: advertised sortable tables must actually render sortable
/// headers (`data-sortable` is the hook `app.js` binds to). Seeded history
/// makes the ranking/overview tables render real rows; the seeded cycle
/// record makes the dashboard's cycles table render.
#[tokio::test]
async fn advertised_tables_render_sortable_headers() {
    let (state, root) = test_state(None);
    seed_history(&state);
    let router = build_router_arc(Arc::clone(&state));
    for uri in ["/opportunities?target=t1", "/overview?target=t1", "/"] {
        let (status, body) = get(router.clone(), uri).await;
        assert_eq!(status, StatusCode::OK, "GET {uri}");
        assert!(body.contains("<th data-sortable>"), "GET {uri} renders no sortable headers");
    }
    std::fs::remove_dir_all(&root).ok();
}

/// Routing + validation fallbacks stay intact.
#[tokio::test]
async fn unknown_targets_are_rejected_and_paths_fall_back() {
    let (state, root) = test_state(None);
    let router = build_router_arc(Arc::clone(&state));
    let (status, _) = get(router.clone(), "/items/bogus/2589").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unknown target rejected");
    let (status, _) = get(router.clone(), "/definitely-not-a-page").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "fallback page for unknown paths");
    let (status, body) = get(router.clone(), "/items/t1/pet/264").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("canonical variant"), "pet page explains its default variant");
    std::fs::remove_dir_all(&root).ok();
}

/// CSRF contract: `/api/fetch` must refuse simple cross-site posts — only
/// `application/json` bodies pass (forms cannot send that content type).
#[tokio::test]
async fn fetch_requires_a_json_content_type() {
    let (state, root) = test_state(None);
    let router = build_router_arc(Arc::clone(&state));
    let status = post_json(router.clone(), "/api/fetch", Some("text/plain")).await;
    assert_eq!(
        status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "simple form posts must not trigger ingest cycles"
    );
    let status = post_json(router, "/api/fetch", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK, "the documented empty-JSON body passes");
    std::fs::remove_dir_all(&root).ok();
}

/// Protected mode gates every mutation through the real router, not just the
/// pure authorize helper.
#[tokio::test]
async fn protected_mode_rejects_unauthenticated_mutations() {
    const TOKEN_ENV: &str = "AMT_WEB_TEST_TOKEN";
    // SAFETY-free single-threaded env dance is not available: tests may run in
    // parallel, but no other test reads this variable, and the state under
    // test is the only consumer of the name.
    std::env::set_var(TOKEN_ENV, "s3cret");
    let (state, root) = test_state(Some(TOKEN_ENV.into()));
    let router = build_router_arc(Arc::clone(&state));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/watchlist")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"target":"t1","type":"item","itemID":2589}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "missing bearer rejected");
    std::env::remove_var(TOKEN_ENV);
    std::fs::remove_dir_all(&root).ok();
}

/// The live watchlist report: entries join to metric blocks, sale rates and
/// trend rows, positionally in watchlist order — the JSON contract behind the
/// watchlist page columns.
#[tokio::test]
async fn watchlist_metrics_api_joins_entries_to_market_numbers() {
    let (state, root) = test_state(None);
    seed_history(&state);
    let target = TargetId(TARGET.into());
    state
        .watchlist
        .add(&target, ItemRef::Item { item_id: 2589 }, Some("flip".into()))
        .expect("watch item");
    state
        .watchlist
        .add(&target, ItemRef::Pet { species: 264, level: 1, quality: 3, breed: 11 }, None)
        .expect("watch pet");

    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/api/watchlist/metrics?target=t1&span=d1,d7")
            .await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    let entries = json.get("entries").and_then(Value::as_array).expect("entries array");
    assert_eq!(entries.len(), 2, "one entry per watchlist row, in order");
    let item_entry = &entries[0];
    assert_eq!(item_entry.get("note").and_then(Value::as_str), Some("flip"));
    assert!(
        item_entry.get("metrics").and_then(|m| m.get("d7")).is_some(),
        "seeded folds produce a d7 block"
    );
    assert!(
        item_entry.get("trend").and_then(Value::as_array).is_some_and(|t| t.len() >= 2),
        "both seeded days are trend rows"
    );
    let pet_entry = &entries[1];
    assert!(
        pet_entry.get("metrics").and_then(Value::as_object).is_some_and(serde_json::Map::is_empty),
        "a data-less pet yields an empty metrics map, not an error"
    );

    let (status, _) =
        get(build_router_arc(Arc::clone(&state)), "/api/watchlist/metrics?target=bogus").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unknown target rejected");
    std::fs::remove_dir_all(&root).ok();
}

/// Pet metrics need the full identity tuple: partial queries are rejected,
/// complete ones serve the (possibly empty) span map.
#[tokio::test]
async fn pet_metrics_api_requires_the_full_identity_tuple() {
    let (state, root) = test_state(None);
    let router = build_router_arc(Arc::clone(&state));
    let (status, _) = get(router.clone(), "/api/items/t1/pet/264/metrics").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "missing level/quality/breed rejected");
    let (status, body) =
        get(router, "/api/items/t1/pet/264/metrics?level=1&quality=3&breed=11").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert!(json.as_object().is_some_and(serde_json::Map::is_empty), "no data → no span keys");
    std::fs::remove_dir_all(&root).ok();
}

/// The snapshot browser's JSON twin: metadata rows with the wire field
/// spellings, filterable by payload kind.
#[tokio::test]
async fn snapshots_api_serves_filterable_metadata() {
    let (state, root) = test_state(None);
    let target = TargetId(TARGET.into());
    state
        .snapshots
        .record(
            &NewSnapshot {
                target: target.clone(),
                kind: PayloadKind::Commodities,
                fetched_at: 1_000_001,
                source_last_modified: Some(999),
                entry_count: 3,
                dropped_count: 1,
                status: SnapshotStatus::Ok,
            },
            &[],
            br"[]",
        )
        .expect("record");

    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/api/snapshots?target=t1&limit=10").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    let rows = json.get("snapshots").and_then(Value::as_array).expect("snapshots array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("entryCount").and_then(Value::as_u64), Some(3));
    assert_eq!(rows[0].get("droppedCount").and_then(Value::as_u64), Some(1));
    assert_eq!(rows[0].get("sourceLastModified").and_then(Value::as_i64), Some(999));

    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/api/snapshots?target=t1&kind=auctions").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        json.get("snapshots").and_then(Value::as_array).map(std::vec::Vec::len),
        Some(0),
        "kind filter excludes other kinds"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// Search responses carry the paging envelope (offset/limit/hasMore) so
/// clients can page without guessing.
#[tokio::test]
async fn search_api_reports_the_paging_envelope() {
    let (state, root) = test_state(None);
    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/api/search?q=linen&limit=5&offset=3").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json.get("offset").and_then(Value::as_u64), Some(3));
    assert_eq!(json.get("limit").and_then(Value::as_u64), Some(5));
    assert_eq!(json.get("hasMore").and_then(Value::as_bool), Some(false));
    std::fs::remove_dir_all(&root).ok();
}

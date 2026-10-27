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
        CatalogConfig,
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
        Category,
        FetchOutcome,
        Flavor,
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
    api_routes,
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
        _flavor: &Flavor,
        _item_id: i64,
    ) -> Result<Option<amtracker_core::domain::NameRecord>, SourceError> {
        Ok(None)
    }

    fn describe_pet(
        &self,
        _flavor: &Flavor,
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
            sight_retention_days: 0,
            archive_dir: "raw".into(),
            feed_dir: "feeds".into(),
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
        alerts: amtracker_core::config::AlertsConfig {
            enabled: false,
            cooldown_secs: 21_600,
            delivery: "log".into(),
            webhook_env: None,
            token_env: None,
            chat_id_env: None,
            max_data_age_secs: 0,
        },
        catalog: CatalogConfig::default(),
        targets: vec![TargetConfig {
            id: TargetId(TARGET.into()),
            kind: TargetKind::BlizzardConnectedRealm { region: "us".into(), connected_realm_id: 4 },
            auth: None,
            poll: PollConfig { commodities_secs: 60, auctions_secs: 60 },
            flavor: Flavor::default(),
        }],
    }
}

/// Catalog source that resolves every item/pet to one fixed name — enough
/// to prove the render path substitutes names over the real cache-aside
/// flow (ensure → upsert → label).
struct StaticItemInfo;

impl ItemInfoPort for StaticItemInfo {
    fn describe(
        &self,
        _flavor: &Flavor,
        item_id: i64,
    ) -> Result<Option<amtracker_core::domain::NameRecord>, SourceError> {
        Ok(Some(amtracker_core::domain::NameRecord {
            item_id,
            name: format!("Named Item {item_id}"),
            item_class: None,
            item_subclass: None,
            inventory_slot: None,
            quality: Some("common".into()),
            icon: None,
            sell_price: None,
            fetched_at: 0,
        }))
    }

    fn describe_pet(
        &self,
        _flavor: &Flavor,
        species_id: i64,
    ) -> Result<Option<amtracker_core::domain::PetNameRecord>, SourceError> {
        Ok(Some(amtracker_core::domain::PetNameRecord {
            species_id,
            name: format!("Named Pet {species_id}"),
        }))
    }
}

/// Catalog source that always errors — the render lane must degrade to raw
/// labels (200 pages, no hang) when the name source is down.
struct DownItemInfo;

impl ItemInfoPort for DownItemInfo {
    fn describe(
        &self,
        _flavor: &Flavor,
        _item_id: i64,
    ) -> Result<Option<amtracker_core::domain::NameRecord>, SourceError> {
        Err(SourceError::Http("down in test".into()))
    }

    fn describe_pet(
        &self,
        _flavor: &Flavor,
        _species_id: i64,
    ) -> Result<Option<amtracker_core::domain::PetNameRecord>, SourceError> {
        Err(SourceError::Http("down in test".into()))
    }
}

/// Full `AppState` over a real (empty) SQLite store in a temp directory.
fn test_state(token_env: Option<String>) -> (Arc<AppState>, PathBuf) {
    test_state_with_info(token_env, Arc::new(NoItemInfo))
}

/// Same, with a caller-chosen item-info source (name-resolution tests).
fn test_state_with_info(
    token_env: Option<String>,
    item_info: Arc<dyn ItemInfoPort>,
) -> (Arc<AppState>, PathBuf) {
    test_state_with_config(token_env, item_info, |_| {})
}

/// Same, with a config mutation hook (resolution budget and friends).
fn test_state_with_config(
    token_env: Option<String>,
    item_info: Arc<dyn ItemInfoPort>,
    mutate: impl FnOnce(&mut TrackerConfig),
) -> (Arc<AppState>, PathBuf) {
    let root = temp_root("state");
    let mut config = config(token_env);
    mutate(&mut config);
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock);
    let store = Arc::new(
        SqliteStore::open(&root.join("db.sqlite3"), &root.join("raw"), clock.clone())
            .expect("open store"),
    );
    let metrics =
        Arc::new(MetricsService::new(store.clone(), store.clone(), store.clone(), clock.clone()));
    let state = AppState {
        config: config.clone(),
        clock: clock.clone(),
        health: Arc::new(HealthService::new(store.clone(), config.clone(), clock.clone())),
        search: Arc::new(SearchService::new(store.clone())),
        catalog: Arc::new(CatalogService::new(store.clone(), item_info, clock.clone())),
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
        alerts: Arc::new(amtracker_core::services::alert::AlertEvaluationService::new(
            store.clone(),
            store.clone(),
            metrics.clone(),
            store.clone(),
            store.clone(),
            None,
            clock.clone(),
            0,
        )),
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
        cycle_lock: Arc::new(std::sync::Mutex::new(())),
        recent_cycles: Arc::new(RecentCycles::new(8)),
        timers: Arc::new(std::sync::Mutex::new(
            amtracker_server::scheduler::TargetTimers::default(),
        )),
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
        state
            .history
            .save_fold(&target, &item, today - offset, Category::Commodity, &fold)
            .expect("save fold");
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

/// Contract: the search box must carry the target slug — `app.js` builds
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

/// Contract: the chart data island must be a `days`-keyed object — the
/// exact shape `app.js` parses. A bare array makes the interactive chart dead
/// code.
#[tokio::test]
async fn chart_island_matches_the_shape_app_js_parses() {
    let (state, root) = test_state(None);
    seed_history(&state);
    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/items/t1/2589?category=commodity").await;
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

/// Contract: advertised sortable tables must actually render sortable
/// headers (`data-sortable` is the hook `app.js` binds to). Seeded history
/// makes the ranking/overview tables render real rows; the seeded cycle
/// record makes the dashboard's cycles table render.
#[tokio::test]
async fn advertised_tables_render_sortable_headers() {
    let (state, root) = test_state(None);
    seed_history(&state);
    let router = build_router_arc(Arc::clone(&state));
    for uri in [
        "/opportunities?target=t1&category=commodity",
        "/overview?target=t1&category=commodity",
        "/",
    ] {
        let (status, body) = get(router.clone(), uri).await;
        assert_eq!(status, StatusCode::OK, "GET {uri}");
        assert!(body.contains("<th data-sortable>"), "GET {uri} renders no sortable headers");
    }
    std::fs::remove_dir_all(&root).ok();
}

/// Tables link resolved catalog names (the raw id rides in the data-tip for
/// the floating tooltip layer) instead of the bare `item {id}` label.
#[tokio::test]
async fn tables_render_catalog_names_with_identity_tooltips() {
    let (state, root) = test_state_with_info(None, Arc::new(StaticItemInfo));
    seed_history(&state);
    let router = build_router_arc(Arc::clone(&state));
    for uri in
        ["/opportunities?target=t1&category=commodity", "/overview?target=t1&category=commodity"]
    {
        let (status, body) = get(router.clone(), uri).await;
        assert_eq!(status, StatusCode::OK, "GET {uri}");
        assert!(
            body.contains(">Named Item 2589<"),
            "GET {uri} should link the resolved catalog name"
        );
        assert!(
            body.contains("data-tip=\"item 2589\""),
            "GET {uri} should carry the raw-id tooltip on the named link"
        );
        assert!(!body.contains(">item 2589<"), "GET {uri} should not show the raw label");
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

    let (status, body) = get(
        build_router_arc(Arc::clone(&state)),
        "/api/watchlist/metrics?target=t1&category=commodity&span=d1,d7",
    )
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
                dropped_reasons: std::collections::BTreeMap::new(),
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
    assert_eq!(
        rows[0].get("status").and_then(Value::as_str),
        Some("ok"),
        "lowercase wire spelling, matching the health endpoint and SnapshotStatus::as_str"
    );

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

/// The alert API end to end over the real store: rule add, evaluation firing
/// against seeded folds, event history, rule removal (history retained), and
/// bad conditions rejected with 400.
#[tokio::test]
async fn alert_api_adds_evaluates_and_removes_rules() {
    let (state, root) = test_state(None);
    seed_history(&state);
    let router = build_router_arc(Arc::clone(&state));

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/alerts")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"target":"t1","type":"item","itemID":2589,
                         "condition":{"type":"price-below","copper":300},"note":"flip"}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK, "rule add succeeds in open mode");

    // Unknown condition shapes are rejected before any store write.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/alerts")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"target":"t1","type":"item","itemID":2589,
                         "condition":{"type":"nonsense"}}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "unknown condition type");

    // d1 median of the seeded folds is 200 ≤ 300, so evaluation fires once.
    // POST (the mutation guard) with the documented empty JSON body.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/alerts/evaluate?target=t1")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body();
    let body = String::from_utf8(
        http_body_util::BodyExt::collect(bytes).await.expect("body").to_bytes().to_vec(),
    )
    .expect("utf8");
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json.get("evaluated").and_then(Value::as_u64), Some(1));
    assert_eq!(json.get("fired").and_then(Value::as_u64), Some(1));

    let (status, body) = get(router.clone(), "/api/alerts?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    let rules = json.get("rules").and_then(Value::as_array).expect("rules array");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].get("note").and_then(Value::as_str), Some("flip"));
    let events = json.get("events").and_then(Value::as_array).expect("events array");
    assert_eq!(events.len(), 1, "the fired event is in the history");
    let rule_id = rules[0].get("ruleID").and_then(Value::as_i64).expect("ruleID");

    // Removal works by id and keeps the event history.
    let response = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/alerts")
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"ruleID":{rule_id}}}"#)))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let (_, body) = get(build_router_arc(Arc::clone(&state)), "/api/alerts?target=t1").await;
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json.get("rules").and_then(Value::as_array).map(std::vec::Vec::len), Some(0));
    assert_eq!(
        json.get("events").and_then(Value::as_array).map(std::vec::Vec::len),
        Some(1),
        "event history outlives the rule"
    );
    std::fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------- catalog name lanes

/// Seeds two day folds for pet species 264, so the warm-up and render lanes
/// see a pet identity, not just items.
fn seed_pet_history(state: &AppState) {
    let target = TargetId(TARGET.into());
    let item = ItemRef::Pet { species: 264, level: 1, quality: 3, breed: 11 };
    let today = utc_day(state.clock.now());
    for (offset, price) in [(1_i64, 300_i64), (0, 400)] {
        let mut fold = DayFold::new(item.clone());
        let listings: Vec<NormalizedListing> =
            (0..4).map(|n| listing(5000 + offset * 100 + n, &item, price)).collect();
        fold.apply(&listings);
        state
            .history
            .save_fold(&target, &item, today - offset, Category::Commodity, &fold)
            .expect("save fold");
    }
}

/// Seeds a second item (2592) with high prices so it ranks below 2589 —
/// deterministic second place for the resolution-budget test.
fn seed_second_item(state: &AppState) {
    let target = TargetId(TARGET.into());
    let item = ItemRef::Item { item_id: 2592 };
    let today = utc_day(state.clock.now());
    for (offset, price) in [(1_i64, 1000_i64), (0, 2000)] {
        let mut fold = DayFold::new(item.clone());
        let listings: Vec<NormalizedListing> =
            (0..4).map(|n| listing(6000 + offset * 100 + n, &item, price)).collect();
        fold.apply(&listings);
        state
            .history
            .save_fold(&target, &item, today - offset, Category::Commodity, &fold)
            .expect("save fold");
    }
}

/// Pet identities flow through the same catalog lane: the detail page
/// shows the resolved species name.
#[tokio::test]
async fn pet_pages_render_catalog_names() {
    let (state, root) = test_state_with_info(None, Arc::new(StaticItemInfo));
    seed_history(&state);
    let (status, body) = get(
        build_router_arc(Arc::clone(&state)),
        "/items/t1/pet/264?level=1&quality=3&breed=11&category=commodity",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(">Named Pet 264<"), "the pet detail page shows the resolved name");
    std::fs::remove_dir_all(&root).ok();
}

/// A failing name source degrades to raw labels: pages stay 200 and show
/// the bare id (no hang, no 500, no name substitution).
#[tokio::test]
async fn a_failing_name_source_renders_raw_labels() {
    let (state, root) = test_state_with_info(None, Arc::new(DownItemInfo));
    seed_history(&state);
    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/opportunities?target=t1&category=commodity")
            .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(">item 2589<"), "the raw label stays visible");
    assert!(!body.contains("Named Item"), "no name substitution happened");
    std::fs::remove_dir_all(&root).ok();
}

/// The render lane's budget: only the first `render_resolve_max` distinct
/// identities resolve inline; the rest keep raw labels, and `0` disables
/// inline resolution entirely.
#[tokio::test]
async fn resolve_budget_limits_inline_name_resolution() {
    let (state, root) = test_state_with_config(None, Arc::new(StaticItemInfo), |config| {
        config.catalog.render_resolve_max = 1;
    });
    seed_history(&state);
    seed_second_item(&state);
    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/opportunities?target=t1&category=commodity")
            .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(">Named Item 2589<"), "the first identity resolves inline");
    assert!(!body.contains("Named Item 2592"), "beyond-budget ids keep raw labels");
    assert!(body.contains(">item 2592<"), "the beyond-budget raw label stays visible");
    std::fs::remove_dir_all(&root).ok();

    let (state, root) = test_state_with_config(None, Arc::new(StaticItemInfo), |config| {
        config.catalog.render_resolve_max = 0;
    });
    seed_history(&state);
    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/opportunities?target=t1&category=commodity")
            .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("Named Item"), "budget 0 disables inline resolution");
    std::fs::remove_dir_all(&root).ok();
}

/// The background warm-up consults the PET table for pets: a cataloged item
/// with the same numeric id must not make the pet look known (regression:
/// pets used to be skipped forever on id collisions).
#[tokio::test]
async fn warm_up_names_pets_through_the_pet_table() {
    let (state, root) = test_state_with_info(None, Arc::new(StaticItemInfo));
    let flavor = Flavor::default();
    // The collision: item id 264 is already cataloged...
    state.catalog.ensure(&flavor, 264).expect("catalog item 264");
    assert_eq!(state.catalog.known(&flavor, 264), Ok(true));
    // ...while pet species 264 has fold data but no pet-catalog row.
    seed_pet_history(&state);

    amtracker_server::scheduler::after_cycle_now(&state, TARGET, true);

    assert_eq!(
        state.catalog.known_pet(&flavor, 264),
        Ok(true),
        "the warm-up must consult the pet table for species ids"
    );
    std::fs::remove_dir_all(&root).ok();
}

// --------------------------------------------------------- api discovery

/// Fires one method at a concrete path and returns the status. Registration
/// probe: an existing path answers 405 for methods it does not serve, while a
/// missing path falls through to the fallback's 404.
async fn probe_status(router: &Router, method: &str, path: &str) -> StatusCode {
    let response = router
        .clone()
        .oneshot(Request::builder().method(method).uri(path).body(Body::empty()).expect("request"))
        .await
        .expect("response");
    response.status()
}

/// The API must be agent-discoverable end to end: `GET /api` is a directory
/// whose path set equals the spec's, the spec equals the router's route table
/// in BOTH directions (paths and methods), and the table is what the router
/// actually serves — a new route without a spec entry, a phantom spec path,
/// or a missing method all fail here. `/api/docs` renders the same document
/// as HTML with the units spelled out.
#[tokio::test]
async fn api_is_agent_discoverable_index_openapi_and_docs() {
    let (state, root) = test_state_with_info(None, Arc::new(NoItemInfo));
    let router = build_router_arc(state);

    let (status, body) = get(router.clone(), "/api").await;
    assert_eq!(status, StatusCode::OK);
    let index: Value = serde_json::from_str(&body).expect("index json");
    assert_eq!(index["service"], "auctimogger-tracker");
    assert_eq!(index["openapi"], "/api/openapi.json");

    let (status, body) = get(router.clone(), "/api/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    let spec: Value = serde_json::from_str(&body).expect("spec json");
    assert_eq!(spec["openapi"], "3.1.0");

    // The index directory and the spec agree on the exact path set.
    let mut index_paths: Vec<&str> = index["endpoints"]
        .as_array()
        .expect("endpoints array")
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect();
    index_paths.sort_unstable();
    let mut spec_paths: Vec<&str> =
        spec["paths"].as_object().expect("spec paths").keys().map(String::as_str).collect();
    spec_paths.sort_unstable();
    assert_eq!(index_paths, spec_paths, "/api must list exactly the spec's paths");

    // The spec matches the router's route table in both directions: same
    // path set, same methods per path.
    let table = api_routes();
    let mut table_paths: Vec<&str> = table.iter().map(|(path, _, _)| *path).collect();
    table_paths.sort_unstable();
    assert_eq!(spec_paths, table_paths, "spec paths must equal the router table");
    for (path, methods, _) in &table {
        let ops = &spec["paths"][*path];
        assert!(ops.is_object(), "undocumented route: {path}");
        let spec_methods: Vec<&str> =
            ops.as_object().expect("path operations").keys().map(String::as_str).collect();
        assert_eq!(spec_methods, *methods, "method drift on {path}");
    }

    // And the table is what the router serves: a listed method never answers
    // 405, an unlisted method always does (a missing route would fall through
    // to the fallback's 404 instead).
    for (path, methods, _) in &table {
        let concrete = path
            .replace("{target_id}", TARGET)
            .replace("{item_id}", "2589")
            .replace("{species_id}", "264");
        for method in ["GET", "POST", "DELETE"] {
            let status = probe_status(&router, method, &concrete).await;
            if methods.contains(&method.to_lowercase().as_str()) {
                assert_ne!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
            } else {
                assert_eq!(
                    status,
                    StatusCode::METHOD_NOT_ALLOWED,
                    "{method} {path}: unexpected registration"
                );
            }
        }
    }

    // Mutations declare bearer auth; reads stay open.
    assert!(spec["paths"]["/api/fetch"]["post"]["security"].is_array());
    assert!(spec["paths"]["/api/opportunities"]["get"]["security"].is_null());

    let (status, body) = get(router, "/api/docs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("/api/opportunities"), "docs must list endpoints");
    assert!(body.contains("copper"), "docs must state the units");
    std::fs::remove_dir_all(&root).ok();
}

/// The bulk analysis endpoint: one call returns every item/pet day aggregate
/// in the window in a documented deterministic order, the window clamps on
/// both ends, and the target is mandatory (no silent default market).
#[tokio::test]
async fn api_history_returns_bulk_day_rows() {
    let (state, root) = test_state_with_info(None, Arc::new(NoItemInfo));
    seed_history(&state);
    seed_pet_history(&state);

    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/api/history?target=t1&days=7").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("history json");
    assert_eq!(json["target"], TARGET);
    let rows = json["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 4, "two days times two identities (item + pet)");
    let days: Vec<i64> = rows.iter().filter_map(|row| row["day"].as_i64()).collect();
    let mut sorted_days = days.clone();
    sorted_days.sort_unstable();
    assert_eq!(days, sorted_days, "rows are ordered by day");
    assert_eq!(rows[0]["item"]["type"], "item", "items order before pets within a day");
    assert_eq!(rows[0]["item"]["itemID"], 2589);
    assert!(
        rows.iter().any(|row| row["item"]["type"] == "pet" && row["item"]["speciesID"] == 264),
        "pet identities surface as rows"
    );
    assert!(rows.iter().any(|row| row["p05"].is_i64()), "p05 ships for robust-floor analysis");

    // The target is mandatory: no silent fallback to the first market.
    let (status, _) = get(build_router_arc(Arc::clone(&state)), "/api/history?days=7").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "missing target is a 400");

    // The window clamps on both ends; one day means only the newest folds.
    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/api/history?target=t1&days=0").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("history json");
    assert_eq!(json["days"], 1, "the window clamps up to one day");
    assert_eq!(
        json["rows"].as_array().expect("rows array").len(),
        2,
        "the one-day window keeps only the newest fold per identity"
    );

    let (status, body) = get(build_router_arc(state), "/api/history?target=t1&days=999").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("history json");
    assert_eq!(json["days"], 30, "the window clamps to 30 days");
    std::fs::remove_dir_all(&root).ok();
}

/// Unknown span names must answer 400 like every other enum-ish query
/// parameter: silently dropping a typo reads as empty metrics, which is
/// indistinguishable from "no data".
#[tokio::test]
async fn unknown_span_names_are_rejected_not_silently_dropped() {
    let (state, root) = test_state(None);
    let router = build_router_arc(Arc::clone(&state));
    for uri in [
        "/api/items/t1/2589/metrics?span=bogus",
        "/api/items/t1/2589/metrics?span=d1,,d7",
        "/api/items/t1/2589/metrics?span=",
        "/api/watchlist/metrics?target=t1&span=nope",
    ] {
        let (status, _) = get(router.clone(), uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} must reject unknown spans");
    }
    let (status, _) = get(router, "/api/items/t1/2589/metrics?span=d1,d7").await;
    assert_eq!(status, StatusCode::OK, "valid span lists still parse");
    std::fs::remove_dir_all(&root).ok();
}

/// Watchlist adds validate pet identity fields against their in-game domains
/// (species ≥ 1, level 1..=25, quality 0..=6, breed 3..=11): out-of-range
/// rows could never match a fold and would sit in the watchlist forever.
#[tokio::test]
async fn watchlist_add_rejects_out_of_range_pet_fields() {
    let (state, root) = test_state(None);

    let post = |state: &Arc<AppState>, body: Value| {
        let router = build_router_arc(Arc::clone(state));
        async move {
            router
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/watchlist")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .expect("request"),
                )
                .await
                .expect("response")
                .status()
        }
    };

    let pet = |species: i64, level: i32, quality: i32, breed: i32| {
        serde_json::json!({
            "target": TARGET, "type": "pet",
            "speciesID": species, "level": level, "qualityID": quality, "breedID": breed
        })
    };
    assert_eq!(post(&state, pet(2546, 1, 3, 11)).await, StatusCode::OK, "valid identity accepted");
    for (species, level, quality, breed) in
        [(0, 1, 3, 11), (2546, 0, 3, 11), (2546, 26, 3, 11), (2546, 1, 7, 11), (2546, 1, 3, 12)]
    {
        assert_eq!(
            post(&state, pet(species, level, quality, breed)).await,
            StatusCode::BAD_REQUEST,
            "out-of-range pet identity ({species}, {level}, {quality}, {breed}) rejected"
        );
    }

    std::fs::remove_dir_all(&root).ok();
}

/// A present-but-unparseable `after_id` is a malformed link: it must 400
/// instead of silently skipping every same-name row via the `i64::MAX`
/// tiebreaker.
#[tokio::test]
async fn items_page_rejects_unparseable_after_id() {
    let (state, root) = test_state(None);
    let router = build_router_arc(Arc::clone(&state));
    let (status, _) =
        get(router.clone(), "/items?target=t1&after_name=foo&after_id=not-a-number").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "malformed cursor rejected");
    let (status, _) = get(router, "/items?target=t1").await;
    assert_eq!(status, StatusCode::OK, "plain page renders");
    std::fs::remove_dir_all(&root).ok();
}

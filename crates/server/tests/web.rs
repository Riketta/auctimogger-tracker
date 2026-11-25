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
        DatabaseSnapshot,
        FeedFormat,
        FetchOutcome,
        Flavor,
        ItemRef,
        ListingKind,
        MarketMeta,
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
        FeedRenderer,
        ItemInfoPort,
    },
    services::{
        catalog::CatalogService,
        cycle::IngestCycleService,
        database_export::DatabaseExportService,
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
    store::{
        AutoVacuum,
        SqliteStore,
    },
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
            item_hour_retention_days: 45,
            archive_dir: "raw".into(),
            feed_dir: "feeds".into(),
            backup_dir: None,
            backup_secs: 0,
            backup_keep: 7,
            vacuum: amtracker_core::config::VacuumPolicy::Off,
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
        crafts: amtracker_core::config::CraftsConfig::default(),
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
            item_level: Some(5),
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
    test_state_build(token_env, item_info, None, mutate)
}

/// Same, with an optional clock override: every service sharing `clock`
/// (store stamps, catalog windows, the warmer, the response caches) then
/// runs on time the test moves by hand.
fn test_state_build(
    token_env: Option<String>,
    item_info: Arc<dyn ItemInfoPort>,
    clock_override: Option<Arc<dyn ClockPort>>,
    mutate: impl FnOnce(&mut TrackerConfig),
) -> (Arc<AppState>, PathBuf) {
    test_state_full(token_env, item_info, clock_override, mutate, None)
}

/// Same, with an optional recipe-file watcher attached (the hot-reload
/// tests); every other fixture runs without one.
fn test_state_full(
    token_env: Option<String>,
    item_info: Arc<dyn ItemInfoPort>,
    clock_override: Option<Arc<dyn ClockPort>>,
    mutate: impl FnOnce(&mut TrackerConfig),
    recipe_reloader: Option<Arc<amtracker_server::recipe_reload::RecipeReloader>>,
) -> (Arc<AppState>, PathBuf) {
    let root = temp_root("state");
    let mut config = config(token_env);
    mutate(&mut config);
    let clock: Arc<dyn ClockPort> = clock_override.unwrap_or_else(|| Arc::new(SystemClock));
    let store = Arc::new(
        SqliteStore::open(
            &root.join("db.sqlite3"),
            &root.join("raw"),
            clock.clone(),
            AutoVacuum::Off,
        )
        .expect("open store"),
    );
    let metrics =
        Arc::new(MetricsService::new(store.clone(), store.clone(), store.clone(), clock.clone()));
    let opportunities = Arc::new(OpportunityService::new(
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        clock.clone(),
        &config,
    ));
    let crafts = Arc::new(amtracker_core::services::craft::CraftService::new(
        opportunities.clone(),
        metrics.clone(),
        &config.crafts,
    ));
    let state = AppState {
        config: config.clone(),
        clock: clock.clone(),
        health: Arc::new(HealthService::new(store.clone(), config.clone(), clock.clone())),
        search: Arc::new(SearchService::new(store.clone())),
        browse: Arc::new(amtracker_core::services::snapshot_browse::SnapshotBrowseService::new(
            store.clone(),
        )),
        catalog: Arc::new(CatalogService::new(store.clone(), item_info, clock.clone())),
        metrics: metrics.clone(),
        opportunities: opportunities.clone(),
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
        sell_priority: Arc::new(amtracker_core::services::sell_priority::SellPriorityService::new(
            store.clone(),
            metrics.clone(),
            store.clone(),
            clock.clone(),
        )),
        crafts: Arc::clone(&crafts),
        // No file watcher in the fixtures: tests that exercise the hot
        // reload attach one to a temp file of their own.
        recipe_reloader,
        sales: store.clone(),
        alerts: Arc::new(amtracker_core::services::alert::AlertEvaluationService::new(
            store.clone(),
            store.clone(),
            metrics.clone(),
            store.clone(),
            store.clone(),
            store.clone(),
            None,
            clock.clone(),
            0,
            config.opportunities.min_samples,
        )),
        feed_export: Arc::new(DatabaseExportService::new(
            store.clone(),
            store.clone(),
            metrics,
            opportunities,
            crafts,
            clock.clone(),
            "0.1.0-test",
        )),
        publisher: Arc::new(FileFeedPublisher::new(root.join("feeds"))),
        renderer: Arc::new(FeedCodec),
        response_cache: amtracker_server::web::cache::ResponseCache::new(clock.clone()),
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
        maintenance: store.clone(),
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
    // The newest snapshot lists the item at 150: the ranking is live-only, so
    // without a stored snapshot nothing would rank.
    let now = state.clock.now();
    let live: Vec<NormalizedListing> = (0..4).map(|n| listing(1200 + n, &item, 150)).collect();
    seed_snapshot(state, PayloadKind::Commodities, now, &live);
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

/// Table integrity: every data row carries one cell per header column. The
/// watchlist sparkline and the opportunities watch button were once emitted
/// bare inside `<tr>`; browsers foster-parent such content out of the table,
/// silently shifting the remaining columns one cell to the left.
#[tokio::test]
async fn table_rows_carry_a_cell_for_every_header_column() {
    let (state, root) = test_state(None);
    seed_history(&state);
    let target = TargetId(TARGET.into());
    state.watchlist.add(&target, ItemRef::Item { item_id: 2589 }, None).expect("watch item");
    let router = build_router_arc(Arc::clone(&state));

    let (status, body) = get(router.clone(), "/watchlist?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    let (headers, rows) = table_shape(&body, "watchlist");
    assert_eq!(headers, 11, "watchlist header columns");
    assert!(!rows.is_empty(), "seeded watchlist renders at least one row");
    assert!(
        rows.iter().all(|cells| *cells == headers),
        "every watchlist row is complete: {rows:?}",
    );
    assert!(body.contains("<td><svg"), "the trend sparkline renders inside a cell");

    let (status, body) = get(router, "/opportunities?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    let (headers, rows) = table_shape(&body, "opportunities");
    assert_eq!(headers, 15, "opportunities header columns");
    assert!(!rows.is_empty(), "seeded ranking renders at least one row");
    assert!(rows.iter().all(|cells| *cells == headers), "every ranking row is complete");
    assert!(body.contains("<td><button"), "the watch action renders inside a cell");
    std::fs::remove_dir_all(&root).ok();
}

/// The watchlist opens dearest first by the current day's median: rows sort
/// by their d1 block descending, rows without one keep watchlist order at the
/// end, and the `now (d1)` header alone carries the `aria-sort` state that
/// app.js' click toggle continues from.
#[tokio::test]
async fn watchlist_page_opens_sorted_by_d1_median_descending() {
    let (state, root) = test_state(None);
    seed_history(&state); // item 2589: today's median 200
    let target = TargetId(TARGET.into());
    let today = utc_day(state.clock.now());
    let save_fold_at = |item: &ItemRef, day: i64, price: i64, first_auction: i64| {
        let mut fold = DayFold::new(item.clone());
        let listings: Vec<NormalizedListing> =
            (0..4).map(|n| listing(first_auction + n, item, price)).collect();
        fold.apply(&listings);
        state.history.save_fold(&target, item, day, Category::Commodity, &fold).expect("save fold");
    };
    // Dearest today: 300. And an item with no d1 block at all (yesterday
    // only) — it must sink to the end however much its week was worth.
    let dear = ItemRef::Item { item_id: 301 };
    let cold = ItemRef::Item { item_id: 302 };
    save_fold_at(&dear, today, 300, 2100);
    save_fold_at(&cold, today - 1, 999_999, 2200);
    // Watch order — cold, hot, dear — is none of the expected page order.
    state.watchlist.add(&target, cold.clone(), None).expect("watch cold");
    state.watchlist.add(&target, ItemRef::Item { item_id: 2589 }, None).expect("watch hot");
    state.watchlist.add(&target, dear.clone(), None).expect("watch dear");

    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/watchlist?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    let sorted_at = body
        .find("<th data-sortable aria-sort=\"descending\">")
        .expect("the sorted header carries its state");
    let now_at = body.find("now (d1)").expect("the d1 header");
    assert!(
        sorted_at < now_at && now_at - sorted_at < 400,
        "the d1 header is the one marked sorted"
    );
    assert_eq!(body.matches("aria-sort=").count(), 1, "exactly one column carries the state");
    let dear_at = body.find("/items/t1/301").expect("dear row");
    let hot_at = body.find("/items/t1/2589").expect("hot row");
    let cold_at = body.find("/items/t1/302").expect("cold row");
    assert!(dear_at < hot_at && hot_at < cold_at, "dearest first, no-d1 rows last");
    std::fs::remove_dir_all(&root).ok();
}

/// Counts `<th>` headers and per-row `<td>` cells of one table by id. `<th `
/// (trailing space) so `<thead>` never matches; colspan tables are out of
/// scope — only whole-row tables assert through this helper.
fn table_shape(body: &str, table_id: &str) -> (usize, Vec<usize>) {
    let marker = format!(r#"<table id="{table_id}">"#);
    let table = body.split_once(&marker).expect("table present").1;
    let table = table.split("</table>").next().expect("table closed");
    let thead = table.split_once("<thead>").expect("thead").1;
    let thead = thead.split("</thead>").next().expect("thead closed");
    let headers = thead.match_indices("<th ").count();
    let rows_html = table.split_once("<tbody>").expect("tbody").1;
    let rows = rows_html
        .split("<tr>")
        .skip(1)
        .map(|row| row.match_indices("<td").count())
        .filter(|cells| *cells > 0)
        .collect();
    (headers, rows)
}

// ------------------------------------------------------- snapshot browsing

/// Records one snapshot with the given listings (raw `[]`, no drops).
fn seed_snapshot(
    state: &AppState,
    kind: PayloadKind,
    at: i64,
    listings: &[NormalizedListing],
) -> i64 {
    state
        .snapshots
        .record(
            &NewSnapshot {
                target: TargetId(TARGET.into()),
                kind,
                fetched_at: at,
                source_last_modified: Some(at - 60),
                entry_count: u64::try_from(listings.len()).unwrap_or(u64::MAX),
                dropped_count: 0,
                dropped_reasons: std::collections::BTreeMap::new(),
                status: SnapshotStatus::Ok,
            },
            listings,
            b"[]",
        )
        .expect("record snapshot")
}

/// The snapshot viewer: seeded listings render as AH-style rows with catalog
/// names, item levels and per-unit money, sorted cheapest-first by default.
#[tokio::test]
async fn browse_page_serves_seeded_snapshot_rows() {
    let (state, root) = test_state_with_info(None, Arc::new(StaticItemInfo));
    let listings = [
        listing(100, &ItemRef::Item { item_id: 2589 }, 300),
        listing(101, &ItemRef::Item { item_id: 2589 }, 100),
    ];
    let snapshot_id = seed_snapshot(&state, PayloadKind::Commodities, 1_000_000, &listings);
    // Visiting the item page enriches the catalog, so the viewer's rows
    // carry the resolved name instead of the raw id.
    let router = build_router_arc(Arc::clone(&state));
    let (status, _) = get(router.clone(), "/items/t1/2589").await;
    assert_eq!(status, StatusCode::OK, "item visit enriches the catalog");

    let (status, body) = get(router, "/browse?target=t1&kind=commodities").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("snapshot viewer"), "page title present");
    assert!(body.contains("2 listings match · page 1 of 1"), "count and paging line");
    assert!(body.contains("Named Item 2589"), "catalog name resolves");
    assert!(
        body.contains("data-export=\"item 2589 · Named Item 2589\""),
        "CSV export keeps identity and resolved name"
    );
    let price_asc = body.find("1s 0c").expect("cheapest price row");
    let price_desc = body.find("3s 0c").expect("dearer price row");
    assert!(price_asc < price_desc, "default sort is cheapest first");
    assert!(body.contains(">5<"), "catalog item level renders in the lvl column");
    assert!(body.contains("very long"), "time-left band renders");
    assert!(body.contains("data-tip=\"Current bid"), "bid column present");
    assert!(
        body.contains(&format!("snapshot {snapshot_id}")),
        "meta line names the served snapshot"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// Header links re-query server-side (aria-sort + direction flip), and the
/// toolbar filters narrow the results through the store.
#[tokio::test]
async fn browse_page_sort_and_filters_flow_through() {
    let (state, root) = test_state_with_info(None, Arc::new(StaticItemInfo));
    let listings = [
        listing(100, &ItemRef::Item { item_id: 2589 }, 300),
        listing(101, &ItemRef::Item { item_id: 7909 }, 100),
    ];
    seed_snapshot(&state, PayloadKind::Commodities, 1_000_000, &listings);
    let router = build_router_arc(Arc::clone(&state));
    // Enrich both ids so rows resolve names through the store's join.
    let (_, _) = get(router.clone(), "/items/t1/2589").await;
    let (_, _) = get(router.clone(), "/items/t1/7909").await;

    let (_, body) =
        get(router.clone(), "/browse?target=t1&kind=commodities&sort=price&dir=desc").await;
    assert!(body.contains("aria-sort=\"descending\""), "current sort state on the header");
    let dearer = body.find("3s 0c").expect("300c row");
    let cheaper = body.find("1s 0c").expect("100c row");
    assert!(dearer < cheaper, "dir=desc puts the dearer listing first");

    let (_, body) = get(router.clone(), "/browse?target=t1&kind=commodities&item=7909").await;
    assert!(body.contains("1 listings match"), "exact item-id filter narrows");
    assert!(body.contains("Named Item 7909"));
    assert!(!body.contains("Named Item 2589"), "the other id is filtered out");

    let (_, body) = get(router.clone(), "/browse?target=t1&kind=commodities&q=named").await;
    assert!(body.contains("2 listings match"), "name prefix matches case-insensitively");

    let (_, body) = get(router.clone(), "/browse?target=t1&kind=commodities&q=zzz").await;
    assert!(body.contains("no listings match these filters"), "empty result state");

    let (_, body) =
        get(router.clone(), "/browse?target=t1&kind=commodities&quality=common&class=").await;
    assert!(body.contains("2 listings match"), "quality matches case-insensitively");

    // Both kinds are browsable; the other kind simply has no snapshots.
    let (status, body) = get(router, "/browse?target=t1&kind=auctions").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("no snapshots stored yet"), "per-kind empty state");
    std::fs::remove_dir_all(&root).ok();
}

/// Pinning resolves (latest without an id), and malformed links are client
/// errors: unknown ids are 404, ids from another target/kind are 400.
#[tokio::test]
async fn browse_page_resolves_latest_pins_and_rejects_mismatches() {
    let (state, root) = test_state(None);
    let older = seed_snapshot(&state, PayloadKind::Commodities, 1_000_000, &[]);
    let newer = seed_snapshot(&state, PayloadKind::Commodities, 1_001_000, &[]);
    let auctions = seed_snapshot(&state, PayloadKind::Auctions, 1_002_000, &[]);
    let router = build_router_arc(Arc::clone(&state));

    let (_, body) = get(router.clone(), "/browse?target=t1&kind=commodities").await;
    assert!(
        body.contains(&format!("snapshot {newer}")),
        "without an id the newest snapshot serves"
    );
    assert!(!body.contains(&format!("snapshot {older} —")), "the older snapshot does not");

    let (_, body) =
        get(router.clone(), &format!("/browse?target=t1&kind=commodities&snapshot={older}")).await;
    assert!(body.contains(&format!("snapshot {older}")), "an explicit id pins history");

    let (status, _) =
        get(router.clone(), &format!("/browse?target=t1&kind=commodities&snapshot={auctions}"))
            .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a real id from another kind is 400");

    let (status, _) = get(router.clone(), "/browse?target=t1&kind=commodities&snapshot=999").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown ids are 404");

    let (status, _) = get(router.clone(), "/browse?target=t1&kind=bogus").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unknown kinds are 400");

    let (status, _) = get(router.clone(), "/browse?target=t1&kind=commodities&snapshot=abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "junk snapshot ids are 400");

    let (status, _) = get(router, "/browse?target=t1&kind=commodities&item=abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "junk item ids are 400");
    std::fs::remove_dir_all(&root).ok();
}

/// The JSON twin serves the same resolution and rows as the page, with the
/// wire spellings the `OpenAPI` document promises.
#[tokio::test]
async fn browse_api_mirrors_the_page() {
    let (state, root) = test_state_with_info(None, Arc::new(StaticItemInfo));
    let listings = [
        listing(100, &ItemRef::Item { item_id: 2589 }, 300),
        listing(101, &ItemRef::Item { item_id: 2589 }, 100),
    ];
    let snapshot_id = seed_snapshot(&state, PayloadKind::Commodities, 1_000_000, &listings);
    let router = build_router_arc(Arc::clone(&state));
    let (_, _) = get(router.clone(), "/items/t1/2589").await; // enrich

    let (status, body) =
        get(router.clone(), "/api/browse?target=t1&kind=commodities&sort=price&dir=desc").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json.get("snapshot").and_then(|s| s.get("id")), Some(&Value::from(snapshot_id)));
    let rows = json.get("listings").and_then(Value::as_array).expect("listings array");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("unitPrice"), Some(&Value::from(300)), "desc sort applied");
    assert_eq!(rows[0].get("auctionID"), Some(&Value::from(100)));
    assert_eq!(
        rows[0].pointer("/item/type"),
        Some(&Value::from("item")),
        "identity rides in the alerts/watch wire shape"
    );
    assert_eq!(rows[0].get("name"), Some(&Value::from("Named Item 2589")));
    assert_eq!(rows[0].get("level"), Some(&Value::from(5)));
    let page = json.get("page").expect("page block");
    assert_eq!(page.get("total"), Some(&Value::from(2)));
    assert_eq!(page.get("pages"), Some(&Value::from(1)));
    assert_eq!(page.get("size"), Some(&Value::from(50)));

    let (status, _) = get(router, "/api/browse?target=t1&kind=commodities&snapshot=999").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown pins are 404 on the API too");
    std::fs::remove_dir_all(&root).ok();
}

/// Generated filter links percent-encode user input: a prefix arriving with
/// spaces (form encoding's `+`) must re-appear as `%20` in sort/pagination
/// links, or clicking one would silently search for something else.
#[tokio::test]
async fn browse_links_percent_encode_user_filters() {
    let (state, root) = test_state_with_info(None, Arc::new(StaticItemInfo));
    let listings = [listing(100, &ItemRef::Item { item_id: 2589 }, 100)];
    seed_snapshot(&state, PayloadKind::Commodities, 1_000_000, &listings);
    let router = build_router_arc(Arc::clone(&state));
    let (_, _) = get(router.clone(), "/items/t1/2589").await; // enrich

    // `+` decodes to a space, so the prefix is "named item" and matches.
    let (_, body) = get(router, "/browse?target=t1&kind=commodities&q=named+item").await;
    assert!(body.contains("1 listings match"), "the decoded prefix matches the row");
    assert!(body.contains("value=\"named item\""), "the toolbar echoes the decoded prefix");
    assert!(
        body.contains("q=named%20item"),
        "generated links re-encode the prefix instead of passing it raw"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// CSV export labels keep the resolved name and never collapse pet variants
/// into their bare species id.
#[tokio::test]
async fn browse_export_labels_carry_names_and_pet_variants() {
    let (state, root) = test_state_with_info(None, Arc::new(StaticItemInfo));
    let listings = [
        listing(100, &ItemRef::Item { item_id: 2589 }, 300),
        listing(101, &ItemRef::Pet { species: 42, level: 5, quality: 2, breed: 3 }, 100),
    ];
    seed_snapshot(&state, PayloadKind::Commodities, 1_000_000, &listings);
    let router = build_router_arc(Arc::clone(&state));
    let (_, _) = get(router.clone(), "/items/t1/2589").await; // enrich the item

    let (_, body) = get(router, "/browse?target=t1&kind=commodities").await;
    assert!(
        body.contains("data-export=\"item 2589 · Named Item 2589\""),
        "the item export keeps identity and resolved name"
    );
    assert!(
        body.contains("data-export=\"pet 42 · lvl 5 · quality 2 · breed 3\""),
        "the pet export carries the full variant tuple"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// A pinned snapshot older than the recent-30 dropdown keeps its own
/// selected option, so submitting the toolbar does not silently drop the
/// pin back to the newest snapshot.
#[tokio::test]
async fn browse_toolbar_keeps_a_pin_outside_the_recent_window() {
    let (state, root) = test_state(None);
    let mut last = 0;
    for n in 0..31_i64 {
        last = seed_snapshot(&state, PayloadKind::Commodities, 1_000_000 + n * 1_000, &[]);
    }
    let router = build_router_arc(Arc::clone(&state));
    let oldest = last - 30;
    let (_, body) =
        get(router, &format!("/browse?target=t1&kind=commodities&snapshot={oldest}")).await;
    assert!(
        body.contains(&format!("<option value=\"{oldest}\" selected>")),
        "a pinned snapshot outside the recent window keeps its own option"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// The API contract's error edges: an empty pair is 404 (the page renders a
/// 200 empty state instead, covered by the per-kind test above), and a
/// non-integer page is 400 as the spec documents.
#[tokio::test]
async fn browse_api_rejects_junk_pages_and_empty_pairs() {
    let (state, root) = test_state(None);
    let router = build_router_arc(Arc::clone(&state));
    let (status, _) = get(router.clone(), "/api/browse?target=t1&kind=commodities").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "no snapshots yet is 404 on the API");
    let (status, _) = get(router, "/api/browse?target=t1&kind=commodities&page=abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a non-integer page is 400");
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

/// Protected instances render the mutation controls disabled (not absent):
/// the browser key layer enables them once a token is stored for the tab.
/// The server side stays token-blind — only the `data-auth` marker ships.
#[tokio::test]
async fn protected_pages_render_disabled_mutation_controls() {
    const TOKEN_ENV: &str = "AMT_WEB_TEST_TOKEN_UI";
    std::env::set_var(TOKEN_ENV, "s3cret");
    let (state, root) = test_state(Some(TOKEN_ENV.into()));
    let router = build_router_arc(Arc::clone(&state));

    let (status, dashboard) = get(router.clone(), "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(dashboard.contains("data-fetch-now="), "fetch-now always renders");
    assert!(dashboard.contains("data-auth=\"1\" disabled"), "fetch-now disabled until keyed");

    let (status, alerts) = get(router.clone(), "/alerts?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(alerts.contains("data-alert-form"), "the rule form always renders");
    assert!(alerts.contains("data-auth=\"1\" disabled"), "add-rule submit disabled until keyed");

    let (status, item) = get(router.clone(), "/items/t1/2589").await;
    assert_eq!(status, StatusCode::OK);
    assert!(item.contains("data-watch-add="), "watch button always renders");
    assert!(item.contains("data-auth=\"1\" disabled"), "watch disabled until keyed");
    assert!(!item.contains("amt_api_token"), "the storage key name never ships server-side");

    std::env::remove_var(TOKEN_ENV);
    std::fs::remove_dir_all(&root).ok();

    // Open mode: the same controls render live, with no auth markers at all.
    let (state, root) = test_state(None);
    let router = build_router_arc(Arc::clone(&state));
    let (status, item) = get(router, "/items/t1/2589").await;
    assert_eq!(status, StatusCode::OK);
    assert!(item.contains("data-watch-add="));
    assert!(!item.contains("data-auth"), "open instances render no auth markers");
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

/// The sell-priority API ranks the watchlist best-first; the page twin
/// renders the same order.
#[tokio::test]
async fn priorities_api_and_page_rank_the_watchlist() {
    let (state, root) = test_state(None);
    seed_history(&state);
    let target = TargetId(TARGET.into());
    // A second, flat fish: fold only yesterday at 100 — momentum 0, no
    // sales ledger entries → score 0, ranked behind the hot item.
    let flat = ItemRef::Item { item_id: 300 };
    let mut fold = DayFold::new(flat.clone());
    let listings: Vec<NormalizedListing> = (0..4).map(|n| listing(2000 + n, &flat, 100)).collect();
    fold.apply(&listings);
    state
        .history
        .save_fold(&target, &flat, utc_day(state.clock.now()) - 1, Category::Commodity, &fold)
        .expect("save fold");
    state.watchlist.add(&target, flat.clone(), Some("stale".into())).expect("watch flat");
    state.watchlist.add(&target, ItemRef::Item { item_id: 2589 }, None).expect("watch hot");

    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/api/priorities?target=t1&category=commodity")
            .await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    let entries = json.get("entries").and_then(Value::as_array).expect("entries array");
    assert_eq!(entries.len(), 2, "both watchlist entries rank");
    let first = &entries[0];
    assert_eq!(first.pointer("/item/itemID").and_then(Value::as_i64), Some(2589), "hot first");
    let score = first.get("score").expect("scored entry");
    let value = score.get("value").and_then(Value::as_f64).expect("score value");
    // d7 pools 8 listings (100×4, 200×4 → median 150): momentum +1/3,
    // liquidity/recency 0 → 100 · 0.3 · 1/3 = 10.
    assert!((value - 10.0).abs() < 1e-6, "score {value}");
    assert_eq!(first.get("thinData").and_then(Value::as_bool), Some(true));
    let second = &entries[1];
    assert_eq!(second.pointer("/item/itemID").and_then(Value::as_i64), Some(300));
    let flat_score = second.get("score").and_then(|s| s.get("value")).and_then(Value::as_f64);
    assert_eq!(flat_score, Some(0.0), "flat price, no sales: score 0");

    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/priorities?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("2589"), "the ranked page links the hot item");
    assert!(body.contains("300"), "the ranked page links the flat item");
    assert!(body.contains("score"), "the score column renders");
    // Hot item ranks first: its row must appear before the flat one.
    let hot_at = body.find("2589").expect("hot present");
    let flat_at = body.find("300").expect("flat present");
    assert!(hot_at < flat_at, "page order matches the API order");

    let (status, _) =
        get(build_router_arc(Arc::clone(&state)), "/api/priorities?target=bogus").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unknown target rejected");
    std::fs::remove_dir_all(&root).ok();
}

/// The compare API matrices the union of both targets' watchlists and marks
/// the best-paying market per row; the page twin renders it.
#[tokio::test]
async fn compare_api_and_page_matrix_two_targets() {
    let (mut state, root) = test_state(None);
    let target_a = TargetId("t1".into());
    let target_b = TargetId("t2".into());
    // Second target so the matrix has two columns.
    let mut config = state.config.clone();
    config.targets.push(amtracker_core::config::TargetConfig {
        id: target_b.clone(),
        kind: amtracker_core::config::TargetKind::StaticUrls {
            commodities_url: "http://example.test/c.json".into(),
            auctions_url: "http://example.test/a.json".into(),
        },
        auth: None,
        poll: amtracker_core::config::PollConfig { commodities_secs: 1, auctions_secs: 1 },
        flavor: amtracker_core::domain::Flavor::default(),
    });
    Arc::get_mut(&mut state).expect("exclusive test state").config = config;

    let today = utc_day(state.clock.now());
    let fish = ItemRef::Item { item_id: 2589 };
    // Fish pays double on t2 today.
    for (target, price) in [(&target_a, 100_i64), (&target_b, 200_i64)] {
        let mut fold = DayFold::new(fish.clone());
        let rows: Vec<NormalizedListing> =
            (0..2).map(|n| listing(3000 + n, &fish, price)).collect();
        fold.apply(&rows);
        state
            .history
            .save_fold(target, &fish, today, Category::Commodity, &fold)
            .expect("save fold");
    }
    state.watchlist.add(&target_a, fish.clone(), None).expect("watch on t1");
    state.watchlist.add(&target_b, fish.clone(), None).expect("watch on t2");

    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/api/watchlist/compare?category=commodity")
            .await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    let targets = json.get("targets").and_then(Value::as_array).expect("targets");
    assert_eq!(targets.len(), 2, "both configured targets compare");
    let rows = json.get("rows").and_then(Value::as_array).expect("rows");
    assert_eq!(rows.len(), 1, "the union is one identity");
    let row = &rows[0];
    assert_eq!(row.get("coverage").and_then(Value::as_u64), Some(2));
    assert_eq!(row.get("best").and_then(Value::as_u64), Some(1), "t2 pays double");
    let cells = row.get("cells").and_then(Value::as_array).expect("cells");
    assert_eq!(cells[0].get("now").and_then(Value::as_i64), Some(100));
    assert_eq!(cells[1].get("now").and_then(Value::as_i64), Some(200));

    let (status, body) =
        get(build_router_arc(Arc::clone(&state)), "/compare?category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("2589"), "the compare page links the identity");
    assert!(body.contains("t2"), "both target columns render");
    assert!(body.contains("class=\"best\""), "the best cell carries the highlight");
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

/// The days-since-last-sale condition end to end: created via POST, echoed
/// back by the rules read, and its invalid shapes rejected before any write.
#[tokio::test]
async fn alert_api_accepts_the_days_since_last_sale_condition() {
    let (state, root) = test_state(None);
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
                         "condition":{"type":"days-since-last-sale-above","days":10}}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK, "valid days condition adds");

    let (status, body) = get(router.clone(), "/api/alerts?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    let rules = json.get("rules").and_then(Value::as_array).expect("rules array");
    assert_eq!(rules.len(), 1);
    assert_eq!(
        rules[0].get("condition"),
        Some(&serde_json::json!({"type": "days-since-last-sale-above", "days": 10})),
        "the condition round-trips"
    );

    // Missing days and sub-1 days are rejected before any store write.
    for body in [
        r#"{"target":"t1","type":"item","itemID":2589,
             "condition":{"type":"days-since-last-sale-above"}}"#,
        r#"{"target":"t1","type":"item","itemID":2589,
             "condition":{"type":"days-since-last-sale-above","days":0}}"#,
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/alerts")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "rejected: {body}");
    }
    let (_, body) = get(router, "/api/alerts?target=t1").await;
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json.get("rules").and_then(Value::as_array).map(Vec::len), Some(1));
    std::fs::remove_dir_all(&root).ok();
}

/// The opportunities sort param: `last_sale` is accepted, anything unknown
/// is a 400 naming the allowed values.
#[tokio::test]
async fn opportunities_api_sort_param_validates() {
    let (state, root) = test_state(None);
    let router = build_router_arc(Arc::clone(&state));
    let (status, _) = get(router.clone(), "/api/opportunities?target=t1&sort=last_sale").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = get(router, "/api/opportunities?target=t1&sort=bogus").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("unknown sort"), "{body}");
    std::fs::remove_dir_all(&root).ok();
}

/// app.js is embedded into the binary; the alert form's condition serializer
/// must know every wire type the API accepts, or the UI silently posts
/// conditions the server then rejects (the exact drift this guards).
#[test]
fn alert_form_serializer_knows_every_condition_wire_type() {
    let app_js = include_str!("../assets/app.js");
    for wire in ["price-below", "below-floor", "sale-rate-above", "days-since-last-sale-above"] {
        assert!(
            app_js.contains(&format!("type === \"{wire}\"")),
            "app.js form serializer is missing the `{wire}` branch"
        );
    }
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
    // The ranking is live-only: a newer snapshot listing both items keeps
    // 2589 ranked while 2592 takes its deterministic second place.
    let first = ItemRef::Item { item_id: 2589 };
    let mut live: Vec<NormalizedListing> = (0..4).map(|n| listing(6400 + n, &first, 150)).collect();
    live.extend((0..4).map(|n| listing(6500 + n, &item, 2000)));
    seed_snapshot(state, PayloadKind::Commodities, state.clock.now(), &live);
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

/// The render lane's budget: with a cold catalog only the first
/// `render_resolve_max` distinct identities resolve inline from the source;
/// the rest keep raw labels — and the NEXT view names the backlog (the
/// store pass covers what the earlier render cached, the budget lane takes
/// the next miss). `0` disables the inline source lane only: cataloged
/// names still render through the store pass.
#[tokio::test]
async fn resolve_budget_limits_inline_name_resolution() {
    let (state, root) = test_state_with_config(None, Arc::new(StaticItemInfo), |config| {
        config.catalog.render_resolve_max = 1;
    });
    seed_history(&state);
    seed_second_item(&state);
    let router = build_router_arc(Arc::clone(&state));
    let (status, body) = get(router.clone(), "/opportunities?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(">Named Item 2589<"), "the first identity resolves inline");
    assert!(!body.contains("Named Item 2592"), "beyond-budget ids keep raw labels");
    assert!(body.contains(">item 2592<"), "the beyond-budget raw label stays visible");
    let (_, body) = get(router, "/opportunities?target=t1&category=commodity").await;
    assert!(
        body.contains(">Named Item 2592<"),
        "the next view resolves the previous render's beyond-budget id"
    );
    std::fs::remove_dir_all(&root).ok();

    let (state, root) = test_state_with_config(None, Arc::new(StaticItemInfo), |config| {
        config.catalog.render_resolve_max = 0;
    });
    seed_history(&state);
    let router = build_router_arc(Arc::clone(&state));
    let (status, body) = get(router.clone(), "/opportunities?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("Named Item"), "budget 0 skips the inline source lane");
    // Warm the catalog, then re-render: the store pass still names the row —
    // `0` bounds source lookups, not local reads.
    state.catalog.ensure(&Flavor::default(), 2589).expect("warm the catalog");
    let (_, body) = get(router, "/opportunities?target=t1&category=commodity").await;
    assert!(body.contains(">Named Item 2589<"), "cataloged names render even at budget 0");
    std::fs::remove_dir_all(&root).ok();
}

/// Regression for the watchlist showing raw ids past the render budget with
/// a fully warmed catalog: the store pass is unbounded, so a watchlist
/// longer than `render_resolve_max` renders every row's name (the warmer
/// had already filled the catalog — the page just never read past the
/// budget).
#[tokio::test]
async fn watchlist_names_every_row_from_a_warmed_catalog() {
    let (state, root) = test_state_with_config(None, Arc::new(StaticItemInfo), |config| {
        config.catalog.render_resolve_max = 2; // far below the entry count
    });
    let flavor = Flavor::default();
    let target = TargetId(TARGET.into());
    for item_id in 1..=5_i64 {
        state.watchlist.add(&target, ItemRef::Item { item_id }, None).expect("watch item");
        state.catalog.ensure(&flavor, item_id).expect("warm the catalog row");
    }
    let (status, body) = get(build_router_arc(Arc::clone(&state)), "/watchlist?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    for item_id in 1..=5_i64 {
        assert!(
            body.contains(&format!("Named Item {item_id}")),
            "watchlist row {item_id} renders its catalog name"
        );
    }
    std::fs::remove_dir_all(&root).ok();
}

/// Item links carry the row's market category, and a bare item URL
/// auto-lands on the category that actually holds folds (commodity-only
/// identities used to open on the empty item tab — "no price history").
#[tokio::test]
async fn item_links_carry_category_and_bare_urls_auto_land() {
    let (state, root) = test_state(None);
    seed_history(&state); // commodity folds only, item 2589
    let target = TargetId(TARGET.into());
    let item = ItemRef::Item { item_id: 2589 };

    // Bare URL: only commodity folds exist → redirected to the commodity tab.
    let router = build_router_arc(Arc::clone(&state));
    let response = router
        .clone()
        .oneshot(
            Request::builder().method("GET").uri("/items/t1/2589").body(Body::empty()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "bare URL auto-lands");
    assert_eq!(
        response.headers().get("location").and_then(|value| value.to_str().ok()),
        Some("/items/t1/2589?category=commodity")
    );

    // Other query params survive the landing redirect (re-encoded).
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/items/t1/2589?d=30")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers().get("location").and_then(|value| value.to_str().ok()),
        Some("/items/t1/2589?category=commodity&d=30")
    );

    // A value that needs encoding comes back percent-encoded, in sorted key
    // order — a raw `&` or a line break in the echoed param can neither
    // inject a second parameter nor break the Location header.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/items/t1/2589?q=a%20b%26c%0Ad&d=30")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers().get("location").and_then(|value| value.to_str().ok()),
        Some("/items/t1/2589?category=commodity&d=30&q=a%20b%26c%0Ad")
    );

    // Explicit tabs are respected — no redirect loop.
    let (status, _) = get(router.clone(), "/items/t1/2589?category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get(router.clone(), "/items/t1/2589?category=item").await;
    assert_eq!(status, StatusCode::OK, "an explicit empty tab stays viewable");

    // Item-category folds appear → the bare URL stays on the default tab.
    let mut fold = DayFold::new(item.clone());
    fold.apply(&[listing(2000, &item, 150)]);
    state
        .history
        .save_fold(&target, &item, utc_day(state.clock.now()), Category::Item, &fold)
        .expect("save item fold");
    let (status, _) = get(router.clone(), "/items/t1/2589").await;
    assert_eq!(status, StatusCode::OK, "item-tab data keeps the default landing");

    // Table links land on the row's own market tab.
    state.watchlist.add(&target, item, None).expect("watch");
    let (_, body) = get(router, "/watchlist?target=t1&category=commodity").await;
    assert!(
        body.contains("href=\"/items/t1/2589?category=commodity\""),
        "watchlist rows carry the market category"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// The when-to-buy section: weekday bars from the folds, hourly lines from
/// the permanent intraday hour fold (written by `record`), preset links,
/// mounts + island for the local-time JS re-bucketing — and the
/// price-distribution block still in place below it.
#[tokio::test]
async fn item_page_when_to_buy_section() {
    let (state, root) = test_state(None);
    seed_history(&state);
    let now = state.clock.now();
    let item = ItemRef::Item { item_id: 2589 };
    // Two commodity snapshots today at different hours: cheapest asks 90
    // and 150 (the per-snapshot minima feed the hourly view).
    seed_snapshot(
        &state,
        PayloadKind::Commodities,
        now - 3 * 3_600,
        &[listing(101, &item, 300), listing(102, &item, 90), listing(103, &item, 500)],
    );
    seed_snapshot(
        &state,
        PayloadKind::Commodities,
        now - 3_600,
        &[listing(104, &item, 150), listing(105, &item, 220)],
    );
    let router = build_router_arc(Arc::clone(&state));
    let (status, body) = get(router.clone(), "/items/t1/2589?category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("when to buy"), "section heading present");
    assert!(body.contains("id=\"when-data\""), "JSON island for the JS layer");
    assert!(body.contains("id=\"when-weekday\""), "weekday mount");
    assert!(body.contains("id=\"when-intraday\""), "intraday mount");
    assert!(body.contains("id=\"when-verdict\""), "verdict line mount");
    assert!(body.contains("w=12"), "weekday preset links");
    assert!(body.contains("d=7"), "intraday preset links");
    assert!(body.contains("d=14"), "two-week intraday preset");
    assert!(body.contains("d=30"), "30-day intraday preset");
    assert!(body.contains("category=commodity"), "presets preserve the category");
    assert!(
        !body.contains("no intraday observations"),
        "the seeded snapshots feed the hourly view through the fold"
    );
    assert!(body.contains("cheapest ask by hour"), "the intraday SVG fallback renders");
    // The fallback's y-axis labels: the TOP of the chart is the high end
    // (y grows downward) — top label value must exceed the bottom's.
    let chart = body.split("cheapest ask by hour").nth(1).unwrap_or_default();
    let axis_label = |y: &str| -> i64 {
        let marker = format!("x=\"12\" y=\"{y}\" class=\"muted\">");
        chart
            .find(&marker)
            .and_then(|at| {
                chart.get(at + marker.len()..).and_then(|rest| {
                    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
                })
            })
            .unwrap_or(-1)
    };
    let (top, bottom) = (axis_label("14"), axis_label("116"));
    assert!(top > bottom, "SVG y-axis: high ({top}) above low ({bottom})");
    assert!(
        body.contains("price distribution"),
        "the distribution block stays (kept deliberately)"
    );

    // Out-of-range presets clamp instead of erroring.
    let (status, body) = get(router, "/items/t1/2589?category=commodity&w=99&d=0").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("when to buy"), "clamped render stays healthy");
    std::fs::remove_dir_all(&root).ok();
}

/// Item/pet detail pages link out to the right Wowhead page per flavor:
/// retail ids unprefixed, the era family (`forever`) under `/classic`, pets
/// by species page.
#[tokio::test]
async fn item_pages_link_out_to_wowhead_per_flavor() {
    let (state, root) = test_state(None);
    let router = build_router_arc(Arc::clone(&state));
    let (status, body) = get(router.clone(), "/items/t1/2589").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("href=\"https://www.wowhead.com/item=2589\""),
        "retail items deep-link to the unprefixed wowhead page"
    );

    let (_, body) = get(router, "/items/t1/pet/264?level=1&quality=3&breed=11").await;
    assert!(
        body.contains("href=\"https://www.wowhead.com/pet-species=264\""),
        "pets deep-link to the species page"
    );
    std::fs::remove_dir_all(&root).ok();

    let (state, root) = test_state_with_config(None, Arc::new(NoItemInfo), |config| {
        config.targets[0].flavor = Flavor("forever".into());
    });
    let (status, body) = get(build_router_arc(Arc::clone(&state)), "/items/t1/2589").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("href=\"https://www.wowhead.com/classic/item=2589\""),
        "forever items deep-link to the classic-era section"
    );
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

    let pass = amtracker_server::scheduler::NameWarmer::new().pass(&state);

    assert_eq!(
        state.catalog.known_pet(&flavor, 264),
        Ok(true),
        "the warmer must consult the pet table for species ids"
    );
    assert!(pass.fetched >= 1, "species 264 must have cost a source lookup");
    std::fs::remove_dir_all(&root).ok();
}

/// Clock the tests move by hand (enrichment windows, cache intervals).
struct SettableClock(Arc<std::sync::Mutex<i64>>);

impl ClockPort for SettableClock {
    fn now(&self) -> i64 {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SettableClock {
    fn advance(&self, secs: i64) {
        let mut now = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *now += secs;
    }
}

/// One day fold (four listings) for a plain item on `day`, saved under the
/// commodity category — the warmer/history fixtures.
fn save_item_fold(state: &AppState, item_id: i64, day: i64) {
    let item = ItemRef::Item { item_id };
    let mut fold = DayFold::new(item.clone());
    let listings: Vec<NormalizedListing> =
        (0..4).map(|n| listing(item_id * 1_000 + n, &item, 100)).collect();
    fold.apply(&listings);
    state
        .history
        .save_fold(&TargetId(TARGET.into()), &item, day, Category::Commodity, &fold)
        .expect("save fold");
}

/// The warmer lists all-time ONCE (the first pass after boot) and then only
/// identities with a fold since the previous pass: a backdated fold written
/// later is not re-listed, while anything folding today is picked up on the
/// next pass. (Regression: every pass used to re-list all-time — a full scan
/// of the day folds under the store mutex every 30 seconds.)
#[tokio::test]
async fn warmer_lists_all_time_once_then_only_what_folded_since() {
    let (state, root) = test_state_build(None, Arc::new(StaticItemInfo), None, |config| {
        config.catalog.warm_pace_ms = 0;
    });
    let flavor = Flavor::default();
    let today = utc_day(state.clock.now());
    save_item_fold(&state, 1_001, today - 40);

    let mut warmer = amtracker_server::scheduler::NameWarmer::new();
    let first = warmer.pass(&state);
    assert_eq!(first.fetched, 1, "the boot pass reaches back through all of history");
    assert_eq!(state.catalog.known(&flavor, 1_001), Ok(true));

    // Written after the cursor moved to today: a fold forty days back is
    // invisible to the incremental listing...
    save_item_fold(&state, 1_002, today - 40);
    // ...while a fold landing today is not.
    save_item_fold(&state, 1_003, today);
    let second = warmer.pass(&state);
    assert_eq!(second.fetched, 1, "only today's newcomer costs a lookup");
    assert_eq!(state.catalog.known(&flavor, 1_003), Ok(true));
    assert_eq!(
        state.catalog.known(&flavor, 1_002),
        Ok(false),
        "backdated folds wait for the next boot's all-time pass"
    );

    // Nothing new: an idle pass examines nothing and asks nothing.
    let idle = warmer.pass(&state);
    assert_eq!(
        idle,
        amtracker_server::scheduler::WarmPassStats::default(),
        "steady state costs no catalog reads"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// Unresolved ids are carried from pass to pass and skipped while the
/// negative cache still defers them — neither dropped nor re-asked hot.
#[tokio::test]
async fn warmer_carries_unresolved_ids_and_skips_deferred_ones() {
    // NoItemInfo: the source has no record for anything.
    let (state, root) = test_state_build(None, Arc::new(NoItemInfo), None, |config| {
        config.catalog.warm_pace_ms = 0;
    });
    save_item_fold(&state, 2_001, utc_day(state.clock.now()));

    let mut warmer = amtracker_server::scheduler::NameWarmer::new();
    let first = warmer.pass(&state);
    assert_eq!((first.fetched, first.deferred), (1, 0), "asked once, remembered negatively");
    let second = warmer.pass(&state);
    assert_eq!(
        (second.fetched, second.deferred),
        (0, 1),
        "deferred: no second request inside the negative window"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// A named id is re-checked on the slow ring: once its catalog row is past
/// the enrichment window it is fetched again — without any pass re-listing
/// the whole history.
#[tokio::test]
async fn warmer_rechecks_named_ids_on_the_slow_ring() {
    let settable = Arc::new(SettableClock(Arc::new(std::sync::Mutex::new(1_760_000_000))));
    let clock: Arc<dyn ClockPort> = settable.clone();
    let (state, root) = test_state_build(None, Arc::new(StaticItemInfo), Some(clock), |config| {
        config.catalog.warm_pace_ms = 0;
    });
    let flavor = Flavor::default();
    // Older than any day cursor: only the ring can bring this id back.
    save_item_fold(&state, 3_001, utc_day(settable.now()) - 40);

    let mut warmer = amtracker_server::scheduler::NameWarmer::new();
    let first = warmer.pass(&state);
    assert_eq!(first.fetched, 1, "named by the all-time pass");
    assert_eq!(state.catalog.known(&flavor, 3_001), Ok(true));

    // A day on, the ring visits the id and finds the row still fresh.
    settable.advance(86_400 + 60);
    let rechecked = warmer.pass(&state);
    assert_eq!(
        (rechecked.candidates, rechecked.fetched),
        (1, 0),
        "the ring re-checks the id; a fresh row costs no request"
    );

    // Thirty-one days after its fetch the row is past the enrichment window
    // (the static source never fills the class): the ring heals it.
    settable.advance(31 * 86_400);
    let healed = warmer.pass(&state);
    assert_eq!(healed.fetched, 1, "the stale row is fetched again");
    assert_eq!(state.catalog.known(&flavor, 3_001), Ok(true));
    std::fs::remove_dir_all(&root).ok();
}

/// `/api/feed.json` and `/feed.lua` serve the copies the scheduler publishes
/// — never a per-request whole-market rebuild — and answer 404 until the
/// first publication.
#[tokio::test]
async fn feed_downloads_serve_the_published_copies() {
    let (state, root) = test_state(None);
    let router = build_router_arc(Arc::clone(&state));
    let (status, _) = get(router.clone(), "/api/feed.json?target=t1").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nothing published yet");
    let (status, _) = get(router.clone(), "/feed.lua?target=t1").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nothing published yet");

    let snapshot = DatabaseSnapshot {
        target: TargetId(TARGET.into()),
        market: MarketMeta {
            region: Some("us".into()),
            ruleset_id: None,
            house_id: None,
            connected_realm_id: Some(4),
            flavor: Flavor::default(),
        },
        tracker_version: "0.1.0-test".into(),
        generated_at: 1_760_000_000,
        auctions_at: None,
        commodities_at: None,
        entries: Vec::new(),
    };
    let json = FeedCodec.render(&snapshot, FeedFormat::Json).expect("render");
    let lua = FeedCodec.render(&snapshot, FeedFormat::Lua).expect("render");
    state.publisher.publish(&TargetId(TARGET.into()), FeedFormat::Json, &json).expect("publish");
    state.publisher.publish(&TargetId(TARGET.into()), FeedFormat::Lua, &lua).expect("publish");
    // A rebuild would see this fold; the published copies must win.
    seed_history(&state);

    let (status, body) = get(router.clone(), "/api/feed.json?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), json.as_slice(), "served verbatim");
    let twin: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(twin["version"], 4, "the v4 twin, not the v3 feed");

    let response = router
        .oneshot(
            Request::builder().uri("/feed.lua?target=t1").body(Body::empty()).expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(content_type, "text/x-lua; charset=utf-8");
    let body = response.into_body().collect().await.expect("body").to_bytes();
    assert_eq!(body.as_ref(), lua.as_slice(), "the carrier, served verbatim");
    std::fs::remove_dir_all(&root).ok();
}

/// The watchlist-only feed is gone with the v3 contract: the route answers
/// 404, and a watchlist change no longer feeds anything.
#[tokio::test]
async fn the_slim_feed_route_is_gone() {
    let (state, root) = test_state(None);
    seed_history(&state);
    state
        .watchlist
        .add(&TargetId(TARGET.into()), ItemRef::Item { item_id: 2589 }, None)
        .expect("watch item");
    let router = build_router_arc(Arc::clone(&state));
    let (status, _) = get(router, "/feed-slim.lua?target=t1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    std::fs::remove_dir_all(&root).ok();
}

/// The scheduler's post-cycle pass publishes the v4 feed: after one pass both
/// files exist, the carrier carries the market's records under the addon's
/// loader, and the twin decodes to the same market — with the scan refresh
/// ordered before the publish, the live listing and the rank are the cycle's.
#[tokio::test]
async fn the_scheduler_pass_publishes_the_v4_feed() {
    let (state, root) = test_state(None);
    seed_history(&state);
    amtracker_server::scheduler::after_cycle_now(&state, TARGET);
    let router = build_router_arc(Arc::clone(&state));

    let (status, lua) = get(router.clone(), "/feed.lua?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        lua.starts_with(
            "-- AuctiMogger Database market file - GENERATED by auctimogger-tracker 0.1.0-test."
        ),
        "{lua}"
    );
    assert!(lua.contains("    target = \"t1\",\n"), "{lua}");
    assert!(lua.contains("m.commodities[2589] = \""), "the seeded item's record: {lua}");
    assert!(lua.ends_with("A.EndMarket()\n"), "{lua}");

    let (status, body) = get(router.clone(), "/api/feed.json?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    let twin: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(twin["version"], 4);
    assert_eq!(twin["target"], TARGET);
    assert_eq!(twin["market"]["connectedRealmId"], 4);
    let record = &twin["items"][0];
    assert_eq!(record["itemID"], 2589);
    assert_eq!(record["category"], "commodity");
    assert_eq!(record["live"]["floor"], 150, "the newest snapshot's floor, read directly");
    assert_eq!(record["live"]["listed"], 4);
    assert_eq!(record["value7"], 150);
    assert_eq!(record["deal"]["rank"], 1, "the one rankable listing of the market");
    assert_eq!(record["spans"]["d1"]["median"], 200);
    assert_eq!(record["spans"]["d7"]["median"], 150);

    // Both files are on disk under the feed directory, named for the target.
    let dir = root.join("feeds").join(TARGET);
    assert_eq!(std::fs::read(dir.join("feed.lua")).expect("feed.lua"), lua.as_bytes());
    assert!(std::fs::read(dir.join("feed.json")).expect("feed.json").len() > 2);
    std::fs::remove_dir_all(&root).ok();
}

/// The pass refreshes the prepared scans BEFORE it publishes: a scan that is
/// still served from cache (inside its hard staleness, past the refresh
/// debounce) and disagrees with the store must not leak into the feed — the
/// published record shows the refreshed facts.
#[tokio::test]
async fn the_scheduler_pass_refreshes_scans_before_it_publishes() {
    let settable = Arc::new(SettableClock(Arc::new(std::sync::Mutex::new(1_760_000_000))));
    let clock: Arc<dyn ClockPort> = settable.clone();
    let (state, root) = test_state_build(None, Arc::new(NoItemInfo), Some(clock), |_| {});
    seed_history(&state);
    let target = TargetId(TARGET.into());
    let item = ItemRef::Item { item_id: 2589 };

    // The ranking's scan is prepared, then the market moves underneath it: a
    // stack of 12 at 300 is sighted and sold a minute later.
    state.opportunities.refresh(&target, Category::Commodity).expect("refresh");
    let mut sold = listing(970_001, &item, 300);
    sold.quantity = 12;
    sold.time_left = TimeLeftBand::Medium;
    state
        .sales
        .observe(&target, PayloadKind::Commodities, &[sold], 1, settable.now())
        .expect("observe");
    let credits = state
        .sales
        .close_missing(&target, PayloadKind::Commodities, 2, settable.now() + 60)
        .expect("close");
    assert_eq!(credits.len(), 1, "gone well inside its band: a sale");

    // Past the refresh debounce the cached scan still answers the old facts —
    // which is exactly what a publish-before-refresh pass would ship.
    settable.advance(31);
    let cached = state.opportunities.facts(&target, Category::Commodity).expect("facts");
    assert_eq!(cached.get(&item).expect("a scan row").sold_median14, None);

    amtracker_server::scheduler::after_cycle_now(&state, TARGET);
    let (_, body) = get(build_router_arc(Arc::clone(&state)), "/api/feed.json?target=t1").await;
    let twin: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(twin["items"][0]["soldMedian14"], 300, "published from the refreshed scan");
    std::fs::remove_dir_all(&root).ok();
}

/// `/api/history` reads the whole market once per category per TTL; within
/// the window every request — whatever its `days` — is served from that
/// read, so repeated calls never multiply whole-market scans.
#[tokio::test]
async fn api_history_reads_the_market_once_per_ttl() {
    let (state, root) = test_state(None);
    seed_history(&state);
    let router = build_router_arc(Arc::clone(&state));

    let (status, first) = get(router.clone(), "/api/history?target=t1&days=7").await;
    assert_eq!(status, StatusCode::OK);
    // New folds land; inside the TTL the cached read still answers.
    save_item_fold(&state, 9_001, utc_day(state.clock.now()));
    let (_, second) = get(router.clone(), "/api/history?target=t1&days=7").await;
    assert_eq!(second, first, "within the TTL the whole-market read is not repeated");
    let (_, narrow) = get(router, "/api/history?target=t1&days=1").await;
    let json: Value = serde_json::from_str(&narrow).expect("history json");
    let rows = json["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1, "the one-day window keeps only today's fold");
    assert_eq!(rows[0]["item"]["itemID"], 2589);
    assert_eq!(json["sinceDay"], json["rows"][0]["day"], "sinceDay names the window's first day");
    std::fs::remove_dir_all(&root).ok();
}

/// Sold prices end to end: once enough priced units sold, what buyers paid is
/// the deal's reference — the same listing that was fairly priced against the
/// asks becomes a deep discount against a higher sold median — and the API
/// and the page say which value they used.
#[tokio::test]
async fn sold_prices_become_the_deal_reference() {
    use amtracker_core::ports::SalesRepository;

    let settable = Arc::new(SettableClock(Arc::new(std::sync::Mutex::new(1_760_000_000))));
    let clock: Arc<dyn ClockPort> = settable.clone();
    let (state, root) = test_state_build(None, Arc::new(NoItemInfo), Some(clock), |_| {});
    seed_history(&state);
    let router = build_router_arc(Arc::clone(&state));
    let target = TargetId(TARGET.into());
    let item = ItemRef::Item { item_id: 2589 };

    // Nothing priced has sold: the listing value (150) rules, so the live
    // floor of 150 is no deal.
    let (status, body) =
        get(router.clone(), "/api/opportunities?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    let row = &json["opportunities"][0];
    assert_eq!(row["item"]["itemID"], 2589);
    assert_eq!(row["value7"], 150);
    assert_eq!(row["discount"], 0.0);
    assert_eq!(row["valueSource"], "listed");
    assert!(row["soldMedian14"].is_null(), "no priced sales yet");

    // A stack of 12 at 300 is sighted and gone a minute later: a credited
    // close records what it was last seen asking.
    let seed = SqliteStore::open(
        &root.join("db.sqlite3"),
        &root.join("raw"),
        Arc::new(SystemClock),
        AutoVacuum::Off,
    )
    .expect("seed store");
    let mut sold = listing(970_001, &item, 300);
    sold.quantity = 12;
    sold.time_left = TimeLeftBand::Medium;
    seed.observe(&target, PayloadKind::Commodities, &[sold], 1, settable.now()).expect("observe");
    let credits = seed
        .close_missing(&target, PayloadKind::Commodities, 2, settable.now() + 60)
        .expect("close");
    assert_eq!(credits.len(), 1, "gone well inside its band: a sale");

    // A fresh scan (past the refresh debounce) picks the sold median up: the
    // listing value is still published, but the discount follows the sales.
    settable.advance(31);
    state.opportunities.refresh(&target, Category::Commodity).expect("refresh");
    let (status, body) =
        get(router.clone(), "/api/opportunities?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    let row = &json["opportunities"][0];
    assert_eq!(row["soldMedian14"], 300);
    assert_eq!(row["valueSource"], "sold");
    assert_eq!(row["value7"], 150, "the listing value is still published");
    assert_eq!(row["discount"], 0.5, "1 - 150 / 300");

    let (status, page) = get(router, "/opportunities?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("3s 0c*"), "the page marks the sold-based reference");
    std::fs::remove_dir_all(&root).ok();
}

/// The evidence fields end to end: the ledger counts behind the rate, days of
/// supply, the 30-day low read off the hour fold and the vendor floor all
/// reach the API row, the deals page and the item page's ledger line.
#[tokio::test]
async fn deal_rows_carry_their_evidence_end_to_end() {
    use amtracker_core::{
        domain::{
            Flavor,
            NameRecord,
        },
        ports::{
            ItemCatalogRepository,
            SalesRepository,
        },
    };

    let settable = Arc::new(SettableClock(Arc::new(std::sync::Mutex::new(1_760_000_000))));
    let clock: Arc<dyn ClockPort> = settable.clone();
    let (state, root) = test_state_build(None, Arc::new(NoItemInfo), Some(clock), |_| {});
    let router = build_router_arc(Arc::clone(&state));
    let target = TargetId(TARGET.into());
    let item = ItemRef::Item { item_id: 2589 };
    let now = settable.now();

    // Five prior days of hour cells at 200 (recorded FIRST so the live
    // snapshot below stays the newest by id): the live 150 floor undercuts
    // every one of them — a 30-day low.
    for days_ago in 1..=5 {
        let listings: Vec<NormalizedListing> =
            (0..3).map(|n| listing(80_000 + days_ago * 10 + n, &item, 200)).collect();
        seed_snapshot(&state, PayloadKind::Commodities, now - days_ago * 86_400 + 3_600, &listings);
    }
    seed_live_and_gone(&state);
    // The ledger, in two rounds: a MEDIUM-band stack of 12 gone a minute
    // later (credited), then a LONG-band listing that outlives its band's
    // lower bound (expired unsold).
    let seed = SqliteStore::open(
        &root.join("db.sqlite3"),
        &root.join("raw"),
        Arc::new(SystemClock),
        AutoVacuum::Off,
    )
    .expect("seed store");
    let mut sold = listing(970_001, &item, 300);
    sold.quantity = 12;
    sold.time_left = TimeLeftBand::Medium;
    let mut expired = listing(970_002, &item, 310);
    expired.quantity = 5;
    expired.time_left = TimeLeftBand::Long;
    let t0 = now - 10_000;
    seed.observe(&target, PayloadKind::Commodities, &[sold], 1, t0).expect("observe");
    seed.close_missing(&target, PayloadKind::Commodities, 2, t0 + 60).expect("close sold");
    seed.observe(&target, PayloadKind::Commodities, &[expired], 3, t0 + 120).expect("observe");
    seed.close_missing(&target, PayloadKind::Commodities, 4, t0 + 120 + 3 * 3_600)
        .expect("close expired");
    // A vendor pays 160 for the item: the 150 floor is a zero-risk buy.
    seed.upsert(
        &Flavor::default(),
        &[NameRecord {
            item_id: 2589,
            name: "Linen Cloth".into(),
            item_class: None,
            item_subclass: None,
            inventory_slot: None,
            quality: None,
            icon: None,
            sell_price: Some(160),
            item_level: None,
            fetched_at: 0,
        }],
    )
    .expect("catalog");

    // API: 200 units listed ÷ (12 sold / 14 days) = 233.33 days of supply.
    let (status, body) =
        get(router.clone(), "/api/opportunities?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    let row = &json["opportunities"][0];
    assert_eq!(row["soldCount"], 1);
    assert_eq!(row["expiredCount"], 1);
    assert_eq!(row["supplyDays"], 233.33);
    assert_eq!(row["lowRank30d"], 1.0, "every prior day sat above the live floor");
    assert_eq!(row["vendorPrice"], 160);
    let (_, gone) = get(router.clone(), "/api/missed-deals?target=t1&category=commodity").await;
    let gone: Value = serde_json::from_str(&gone).expect("json");
    assert_eq!(gone["missed"][0]["soldCount"], 1);
    assert_eq!(gone["missed"][0]["expiredCount"], 1);

    // The page: the vendor badge, the low as a percentage, supply in days and
    // the counts in the reliability tooltip.
    let (status, page) = get(router.clone(), "/opportunities?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("30d low"), "the new column: {page}");
    assert!(page.contains("supply"), "the new column: {page}");
    let live_table = page
        .split("<table id=\"opportunities\">")
        .nth(1)
        .expect("live table")
        .split("</table>")
        .next()
        .expect("closed");
    assert!(live_table.contains("≤ vendor"), "the badge: {live_table}");
    assert!(live_table.contains("data-tip=\"Vendors for 1s 60c"), "{live_table}");
    assert!(live_table.contains("data-v=\"1\">100%"), "the 30-day low: {live_table}");
    assert!(live_table.contains(">233d<"), "days of supply: {live_table}");
    assert!(
        live_table.contains("1 sold · 1 expired unsold over 14 days"),
        "the counts behind the rate: {live_table}"
    );

    // The item page carries the same counts under its metric blocks.
    let (status, item_page) = get(router, "/items/t1/2589?category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        item_page.contains("1 sold · 1 expired unsold over the last 14 days — 50% sold"),
        "{item_page}"
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
    // path set, same methods per path. The router spells its one wildcard
    // route `{*path}` (axum's syntax); the spec spells the same route `{path}`.
    let table = api_routes();
    let spelled: Vec<String> =
        table.iter().map(|(path, _, _)| path.replace("{*path}", "{path}")).collect();
    let mut table_paths: Vec<&str> = spelled.iter().map(String::as_str).collect();
    table_paths.sort_unstable();
    assert_eq!(spec_paths, table_paths, "spec paths must equal the router table");
    for (path, methods, _) in &table {
        let ops = &spec["paths"][path.replace("{*path}", "{path}")];
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
            .replace("{species_id}", "264")
            .replace("{*path}", "AuctiMoggerDatabase.toc");
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

// ---------------------------------------------------------- live deal ranking

/// The live-deal scenario: a 7-day value of 150 (yesterday four 50-unit
/// stacks at 100, today four at 150), the newest snapshot still listing the
/// four 150 stacks, and one 60-copper stack of 72 seen today that the newest
/// snapshot no longer lists. Returns the live snapshot's id.
fn seed_live_and_gone(state: &AppState) -> i64 {
    let target = TargetId(TARGET.into());
    let item = ItemRef::Item { item_id: 2589 };
    let today = utc_day(state.clock.now());
    let stack = |auction_id: i64, price: i64, qty: i64| NormalizedListing {
        quantity: qty,
        ..listing(auction_id, &item, price)
    };
    let save = |day: i64, listings: &[NormalizedListing]| {
        let mut fold = DayFold::new(item.clone());
        fold.apply(listings);
        state
            .history
            .save_fold(&target, &item, day, Category::Commodity, &fold)
            .expect("save fold");
    };
    let yesterday: Vec<NormalizedListing> = (0..4).map(|n| stack(1000 + n, 100, 50)).collect();
    save(today - 1, &yesterday);
    let mut seen_today: Vec<NormalizedListing> = (0..4).map(|n| stack(1100 + n, 150, 50)).collect();
    seen_today.push(stack(1150, 60, 72));
    save(today, &seen_today);
    let live: Vec<NormalizedListing> = (0..4).map(|n| stack(1200 + n, 150, 50)).collect();
    seed_snapshot(state, PayloadKind::Commodities, state.clock.now(), &live)
}

/// The opportunities page is live-only: the ranking shows the newest
/// snapshot's price with the snapshot it comes from, and a listing seen today
/// but no longer listed moves to its own table instead of passing as a deal.
#[tokio::test]
async fn opportunities_page_ranks_live_listings_and_lists_gone_deals_apart() {
    let (state, root) = test_state(None);
    seed_history(&state);
    seed_live_and_gone(&state);
    let router = build_router_arc(Arc::clone(&state));

    let (status, body) = get(router, "/opportunities?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("live prices from snapshot #"), "the page names its snapshot: {body}");
    assert!(body.contains("min_discount"), "the gone-today floor is a form control");

    let (headers, rows) = table_shape(&body, "opportunities");
    assert_eq!(headers, 15, "opportunities header columns");
    assert_eq!(rows.len(), 1, "one live deal");
    let live_table = body
        .split("<table id=\"opportunities\">")
        .nth(1)
        .expect("live table")
        .split("</table>")
        .next()
        .expect("closed");
    // Price now 150 against a value of 150: listed, but no discount.
    assert!(live_table.contains("data-v=\"150\""), "the live price: {live_table}");
    assert!(live_table.contains("data-v=\"200\""), "200 units deep: {live_table}");
    assert!(live_table.contains("data-v=\"-1\">—"), "no sale evidence renders as —: {live_table}");

    let (headers, rows) = table_shape(&body, "missed-deals");
    assert_eq!(headers, 8, "gone-today header columns");
    assert_eq!(rows.len(), 1, "the gone 60-copper stack is reported, not ranked");
    let gone_table = body
        .split("<table id=\"missed-deals\">")
        .nth(1)
        .expect("gone table")
        .split("</table>")
        .next()
        .expect("closed");
    assert!(gone_table.contains("data-v=\"0.600\""), "60 under the 150 value: {gone_table}");
    assert!(gone_table.contains("data-v=\"60\""), "{gone_table}");
    assert!(gone_table.contains("data-v=\"72\""), "the gone listing's quantity: {gone_table}");

    std::fs::remove_dir_all(&root).ok();
}

/// `/api/opportunities` reports live prices plus the snapshot they come
/// from; the all-categories shape stays keyed arrays; gone deals live in
/// `/api/missed-deals` behind their own discount floor.
#[tokio::test]
async fn opportunities_api_reports_live_prices_and_missed_deals_separately() {
    let (state, root) = test_state(None);
    let live_id = seed_live_and_gone(&state);
    let router = build_router_arc(Arc::clone(&state));

    let (status, body) =
        get(router.clone(), "/api/opportunities?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["snapshot"]["id"], live_id, "the newest snapshot is named");
    let rows = json["opportunities"].as_array().expect("opportunities array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["floorNow"], 150);
    assert_eq!(rows[0]["floorQty"], 200);
    assert_eq!(rows[0]["listedNow"], 4);
    assert_eq!(rows[0]["value7"], 150);
    assert_eq!(rows[0]["valueSource"], "listed");
    assert!(
        rows[0]["soldMedian14"].is_null(),
        "no priced sales: the listing value is the reference"
    );
    assert_eq!(rows[0]["discount"], 0.0);
    assert!(rows[0]["reliability"].is_null(), "no sale evidence is null, never a zero");

    let (status, body) = get(router.clone(), "/api/opportunities?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert!(json["commodity"].is_array(), "the all-categories shape stays keyed arrays");
    assert!(json.get("snapshot").is_none());

    let (status, body) =
        get(router.clone(), "/api/missed-deals?target=t1&category=commodity").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["minDiscount"], 25.0);
    let missed = json["missed"].as_array().expect("missed array");
    assert_eq!(missed.len(), 1);
    assert_eq!(missed[0]["seenFloor"], 60);
    assert_eq!(missed[0]["seenFloorQty"], 72);
    assert_eq!(missed[0]["liveFloor"], 150);
    assert_eq!(missed[0]["discount"], 0.6);
    assert!(missed[0]["reliability"].is_null());

    let (status, body) =
        get(router.clone(), "/api/missed-deals?target=t1&category=commodity&min_discount=90").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        json["missed"].as_array().map(std::vec::Vec::len),
        Some(0),
        "a 60% gap stays under a 90% floor"
    );
    let (status, _) = get(router, "/api/missed-deals?target=t1&category=nonsense").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "an unknown category is rejected");

    std::fs::remove_dir_all(&root).ok();
}

async fn post_alert(router: Router, condition: &str) -> StatusCode {
    let body =
        format!(r#"{{"target":"{TARGET}","type":"item","itemID":2589,"condition":{condition}}}"#);
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/alerts")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .expect("request"),
        )
        .await
        .expect("response");
    response.status()
}

/// The live-deal alert conditions end to end over the real store: accepted
/// by the API, echoed in rule JSON, fired against the newest snapshot's
/// floor, labelled on the page, and rejected with 400 when malformed.
#[tokio::test]
async fn alert_api_handles_the_live_deal_conditions() {
    let (state, root) = test_state(None);
    seed_live_and_gone(&state);
    let router = build_router_arc(Arc::clone(&state));

    assert_eq!(
        post_alert(router.clone(), r#"{"type":"floor-below","copper":200}"#).await,
        StatusCode::OK
    );
    assert_eq!(
        post_alert(router.clone(), r#"{"type":"floor-below-value","fraction":1.0}"#).await,
        StatusCode::OK
    );
    assert_eq!(
        post_alert(router.clone(), r#"{"type":"new-listing-below","fraction":0.5}"#).await,
        StatusCode::OK
    );
    // Malformed shapes never reach the store.
    assert_eq!(
        post_alert(router.clone(), r#"{"type":"floor-below-value","fraction":1.5}"#).await,
        StatusCode::BAD_REQUEST,
        "a fraction above 1 is rejected"
    );
    assert_eq!(
        post_alert(router.clone(), r#"{"type":"new-listing-below"}"#).await,
        StatusCode::BAD_REQUEST,
        "a missing fraction is rejected"
    );
    assert_eq!(
        post_alert(router.clone(), r#"{"type":"floor-below"}"#).await,
        StatusCode::BAD_REQUEST,
        "a missing copper threshold is rejected"
    );

    // The live floor is 150: at/below 200 fires, within 100% of the 150 value
    // fires; the 150 stacks are not within 50% of it.
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
    assert_eq!(json.get("evaluated").and_then(Value::as_u64), Some(3));
    assert_eq!(json.get("fired").and_then(Value::as_u64), Some(2));

    let (status, body) = get(router.clone(), "/api/alerts?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    let rules = json["rules"].as_array().expect("rules array");
    assert_eq!(rules.len(), 3);
    assert_eq!(rules[1]["condition"]["type"], "floor-below-value");
    assert_eq!(rules[1]["condition"]["fraction"], 1.0);
    let events = json["events"].as_array().expect("events array");
    assert_eq!(events.len(), 2);
    assert!(
        events.iter().any(|event| event["message"]
            .as_str()
            .is_some_and(|m| m.contains("live floor 150 copper"))),
        "{events:?}"
    );

    let (status, body) = get(router, "/alerts?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("live floor ≤ 100% of 7d value"), "{body}");
    assert!(body.contains("new listing ≤ 50% of 7d value"), "{body}");

    std::fs::remove_dir_all(&root).ok();
}

// -------------------------------------------------------------------- crafts

/// One recipe for the crafting tests: `output` from `(reagent, qty)` pairs.
fn recipe(output: i64, reagents: &[(i64, i64)]) -> amtracker_core::domain::Recipe {
    amtracker_core::domain::Recipe {
        item: ItemRef::Item { item_id: output },
        makes: 1,
        reagents: reagents
            .iter()
            .map(|(id, qty)| amtracker_core::domain::Reagent {
                item: ItemRef::Item { item_id: *id },
                qty: *qty,
            })
            .collect(),
    }
}

/// The craftable market behind the crafting tests: output 2589 sells at a
/// 7-day value of 1 000 with two credited sales behind it, reagent 2590 at
/// 200 and reagent 2591 at 100 (both listed in the newest snapshot), 2594
/// has a price but never sold, 2592 and 2593 were never seen. The state's
/// recipe book holds five recipes covering every outcome.
fn craft_state() -> (Arc<AppState>, PathBuf) {
    craft_state_with(vec![
        recipe(2589, &[(2590, 1), (2591, 2)]),
        recipe(2589, &[(2590, 3)]),
        recipe(2592, &[(2590, 1)]),
        recipe(2594, &[(2590, 1)]),
        recipe(2594, &[(2593, 1), (2590, 1)]),
    ])
}

/// The craftable market behind the crafting tests with a caller-supplied
/// recipe book: output 2589 sells at a 7-day value of 1 000 with two credited
/// sales behind it, reagent 2590 at 200 and reagent 2591 at 100 (both listed in
/// the newest snapshot), 2594 has a price but never sold, 2592 and 2593 were
/// never seen.
fn craft_state_with(recipes: Vec<amtracker_core::domain::Recipe>) -> (Arc<AppState>, PathBuf) {
    use amtracker_core::ports::{
        PriceHistoryRepository,
        SalesRepository,
        SnapshotRepository,
    };

    let (state, root) = test_state_with_config(None, Arc::new(NoItemInfo), |config| {
        config.crafts.recipes = recipes;
        config.crafts.recipes_file = Some("recipes.toml".into());
    });
    let store = SqliteStore::open(
        &root.join("db.sqlite3"),
        &root.join("raw"),
        Arc::new(SystemClock),
        AutoVacuum::Off,
    )
    .expect("open the seed store");
    let target = TargetId(TARGET.into());
    let at = state.clock.now();
    let today = utc_day(at);
    for (item_id, price, count) in
        [(2589, 1_000, 20), (2590, 200, 10), (2591, 100, 10), (2594, 500, 5)]
    {
        let item = ItemRef::Item { item_id };
        let mut fold = DayFold::new(item.clone());
        for n in 0..count {
            fold.apply(&[listing(900_000 + item_id * 10 + n, &item, price)]);
        }
        store.save_fold(&target, &item, today, Category::Commodity, &fold).expect("save fold");
    }

    // Sale evidence for the output: two listings sighted an hour ago are gone
    // by the newest snapshot (credited), which lists the two reagents instead.
    let snap = |fetched_at: i64, count: usize| NewSnapshot {
        target: target.clone(),
        kind: PayloadKind::Commodities,
        fetched_at,
        source_last_modified: None,
        entry_count: count as u64,
        dropped_count: 0,
        dropped_reasons: std::collections::BTreeMap::new(),
        status: SnapshotStatus::Ok,
    };
    let sellers = [
        listing(950_001, &ItemRef::Item { item_id: 2589 }, 1_100),
        listing(950_002, &ItemRef::Item { item_id: 2589 }, 1_200),
    ];
    let first = store
        .record(&snap(at - 1_200, sellers.len()), &sellers, b"sellers")
        .expect("record sellers");
    store
        .observe(&target, PayloadKind::Commodities, &sellers, first, at - 1_200)
        .expect("observe sellers");
    let mut listed = Vec::new();
    for n in 0..3 {
        listed.push(listing(960_000 + n, &ItemRef::Item { item_id: 2590 }, 200));
        listed.push(listing(960_100 + n, &ItemRef::Item { item_id: 2591 }, 100));
    }
    let newest = store.record(&snap(at - 600, listed.len()), &listed, b"listed").expect("record");
    store
        .observe(&target, PayloadKind::Commodities, &listed, newest, at - 600)
        .expect("observe listed");
    let credits = store
        .close_missing(&target, PayloadKind::Commodities, newest, at - 600)
        .expect("close missing");
    assert_eq!(credits.len(), 2, "both output listings sold: a 100% sale rate");
    (state, root)
}

/// The crafting page ranks efficient crafts best-first with their arithmetic,
/// explains the rest, and is reachable from the navigation.
#[tokio::test]
async fn crafts_page_ranks_efficient_crafts_and_explains_the_rest() {
    let (state, root) = craft_state();
    let router = build_router_arc(Arc::clone(&state));

    let (status, body) = get(router, "/crafts?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"href="/crafts""#), "the navigation links the page");
    assert!(body.contains("recipes from <code>recipes.toml</code> (5 loaded"), "{body}");

    let (headers, rows) = table_shape(&body, "crafts");
    assert_eq!(headers, 11, "efficient-craft header columns");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|cells| *cells == headers), "every row is complete: {rows:?}");
    let ranked = body
        .split("<table id=\"crafts\">")
        .nth(1)
        .expect("ranked table")
        .split("</table>")
        .next()
        .expect("closed");
    // Best craft first: cost 400 for a 1 000 output, 50 cut, 550 profit,
    // expected 550 at a 100% sale rate, ROI 1.375 (shown as 138%).
    assert!(
        ranked.contains("data-v=\"400\"")
            && ranked.contains("data-v=\"1000\"")
            && ranked.contains("data-v=\"550.000\"")
            && ranked.contains("data-v=\"1.375\">138%"),
        "{ranked}"
    );
    assert!(ranked.contains("data-v=\"600\""), "the second recipe for 2589 costs 600");

    let (headers, rows) = table_shape(&body, "crafts-unranked");
    assert_eq!(headers, 6, "not-ranked header columns");
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|cells| *cells == headers), "{rows:?}");
    assert!(body.contains("the crafted item has no 7-day price"), "{body}");
    assert!(body.contains("no sale evidence for the output yet"), "{body}");
    assert!(body.contains("reagent without a 7-day price: item 2593"), "{body}");

    std::fs::remove_dir_all(&root).ok();
}

/// `/api/crafts` carries every recipe, ranked first, with the priced
/// reagents and the per-make money fields.
#[tokio::test]
async fn crafts_api_reports_priced_recipes_ranked_first() {
    let (state, root) = craft_state();
    let router = build_router_arc(Arc::clone(&state));

    let (status, body) = get(router.clone(), "/api/crafts?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["recipesFile"], "recipes.toml");
    assert_eq!(json["ahCutPercent"], 5.0);
    assert_eq!(json["minSaleRate"], 0.2);
    let crafts = json["crafts"].as_array().expect("crafts array");
    assert_eq!(crafts.len(), 5);

    let best = &crafts[0];
    assert_eq!(best["ranked"], true);
    assert_eq!(best["item"]["itemID"], 2589);
    assert_eq!(best["outputCategory"], "commodity");
    assert_eq!(best["outputValue"], 1000);
    assert_eq!(best["saleRate"], 1.0);
    assert_eq!(best["cost"], 400);
    assert_eq!(best["costNow"], 400, "both reagents are listed at their values");
    assert_eq!(best["gross"], 1000);
    assert_eq!(best["profit"], 550);
    assert_eq!(best["expected"], 550.0);
    assert_eq!(best["roi"], 1.375);
    let reagents = best["reagents"].as_array().expect("reagents");
    assert_eq!(reagents.len(), 2);
    assert_eq!(reagents[0]["item"]["itemID"], 2590);
    assert_eq!(reagents[0]["unitValue"], 200);
    assert_eq!(reagents[0]["liveUnitPrice"], 200);
    assert_eq!(reagents[0]["category"], "commodity");
    let sold_per_day = best["unitsSoldPerDay"].as_f64().expect("units sold per day");
    assert!((sold_per_day - 2.0 / 14.0).abs() < 1e-9, "{sold_per_day}");

    assert_eq!(crafts[1]["ranked"], true);
    assert_eq!(crafts[1]["cost"], 600);
    assert_eq!(crafts[1]["expected"], 350.0);

    let issues: Vec<&str> = crafts[2..]
        .iter()
        .map(|craft| {
            assert_eq!(craft["ranked"], false);
            craft["issue"].as_str().expect("an issue")
        })
        .collect();
    assert_eq!(
        issues,
        ["output-without-price", "no-sale-evidence", "reagent-without-price"],
        "unranked rows keep recipe order"
    );
    assert_eq!(crafts[3]["profit"], 275, "priced but unproven outputs still show the arithmetic");
    assert_eq!(crafts[3]["expected"], serde_json::Value::Null);

    let (status, _) = get(router, "/api/crafts?target=nobody").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unknown targets answer 400");

    std::fs::remove_dir_all(&root).ok();
}

/// A reagent the book itself crafts is sourced at the cheaper of its market
/// price and its craft cost: the row's cost drops to the craft route, the API
/// names both numbers and the reagent tooltip says which route was taken.
#[tokio::test]
async fn a_crafted_reagent_is_sourced_at_its_craft_cost() {
    // 2590 (market 200) crafts from one 2591 (100): cheaper to make than to
    // buy, so the 2589 recipe costs 100 + 2 × 100 instead of 200 + 200.
    let (state, root) =
        craft_state_with(vec![recipe(2589, &[(2590, 1), (2591, 2)]), recipe(2590, &[(2591, 1)])]);
    let router = build_router_arc(Arc::clone(&state));

    let (status, body) = get(router.clone(), "/api/crafts?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    let best = &json["crafts"][0];
    assert_eq!(best["cost"], 300, "the crafted route prices the reagent");
    assert_eq!(best["costNow"], 400, "buying right now is still the market price");
    assert_eq!(best["profit"], 650);
    let reagents = best["reagents"].as_array().expect("reagents");
    assert_eq!(reagents[0]["item"]["itemID"], 2590);
    assert_eq!(reagents[0]["craftCost"], 100);
    assert_eq!(reagents[0]["unitValue"], 100, "sourced at the craft route");
    assert_eq!(reagents[0]["liveUnitPrice"], 200);
    assert_eq!(reagents[1]["item"]["itemID"], 2591);
    assert_eq!(reagents[1]["craftCost"], serde_json::Value::Null, "a raw material");
    assert_eq!(reagents[1]["unitValue"], 100);
    assert_eq!(json["recipeCount"], 2);
    assert_eq!(json["reloadError"], serde_json::Value::Null);

    let (status, page) = get(router, "/crafts?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains(
            "data-tip=\"item 2590 ×1 at crafted at 1s 0c each; item 2591 ×2 at 1s 0c each\""
        ),
        "the tooltip names the craft route: {page}"
    );
    assert!(page.contains("data-v=\"300\">3s 0c"), "the cost column: {page}");
    assert!(page.contains("edit the file and reload this page to apply"), "{page}");
    std::fs::remove_dir_all(&root).ok();
}

/// The recipe file is live: a changed file is applied on the next `/crafts`
/// view, a broken save keeps the previous book and says why, and a fixed file
/// clears the error.
#[tokio::test]
async fn the_recipe_file_reloads_on_the_next_crafts_view() {
    use amtracker_server::recipe_reload::RecipeReloader;

    let file_root = temp_root("recipes");
    let path = file_root.join("recipes.toml");
    std::fs::write(
        &path,
        "[[recipe]]\nitem = 1\nreagents = { 2 = 1 }\n[[recipe]]\nitem = 3\nreagents = { 2 = 2 }\n",
    )
    .expect("write recipes");
    let (state, root) = test_state_full(
        None,
        Arc::new(NoItemInfo),
        None,
        |config| config.crafts.recipes_file = Some(path.display().to_string()),
        Some(Arc::new(RecipeReloader::new(&path))),
    );
    let router = build_router_arc(Arc::clone(&state));

    // The first view reads the file: two recipes are live.
    let (status, body) = get(router.clone(), "/crafts?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("(2 loaded"), "{body}");
    assert!(!body.contains("recipe file not reloaded"), "{body}");

    // A broken save (a non-numeric reagent key) is refused; the book stays.
    std::fs::write(&path, "[[recipe]]\nitem = 1\nreagents = { 2 = 1 }\n[[recipe]]\nitem = 3\nreagents = { oops = 2 }\n")
        .expect("write broken recipes");
    let (status, body) = get(router.clone(), "/crafts?target=t1").await;
    assert_eq!(status, StatusCode::OK, "a refused reload is not a server error");
    assert!(body.contains("recipe file not reloaded"), "{body}");
    assert!(body.contains("(2 loaded"), "the previous book stays live: {body}");
    assert!(body.contains("not an item id"), "{body}");
    let (_, api) = get(router.clone(), "/api/crafts?target=t1").await;
    let api: Value = serde_json::from_str(&api).expect("json");
    assert_eq!(api["recipeCount"], 2);
    assert!(api["reloadError"].as_str().is_some_and(|error| error.contains("not an item id")));

    // The fix lands on the very next view and clears the error.
    std::fs::write(&path, "[[recipe]]\nitem = 5\nreagents = { 2 = 3 }\n")
        .expect("write fixed recipes");
    let (status, body) = get(router.clone(), "/crafts?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("(1 loaded"), "{body}");
    assert!(!body.contains("recipe file not reloaded"), "{body}");
    let (_, api) = get(router, "/api/crafts?target=t1").await;
    let api: Value = serde_json::from_str(&api).expect("json");
    assert_eq!(api["crafts"][0]["item"]["itemID"], 5, "the new book is what is evaluated");
    assert_eq!(api["reloadError"], serde_json::Value::Null);
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&file_root).ok();
}

/// Without a recipe book the page says how to configure one and the API
/// answers with an empty list.
#[tokio::test]
async fn crafts_page_without_recipes_says_so() {
    let (state, root) = test_state(None);
    let router = build_router_arc(Arc::clone(&state));

    let (status, body) = get(router.clone(), "/crafts?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("no recipes configured"), "{body}");
    assert!(body.contains("no recipe file configured"), "{body}");
    assert!(!body.contains("<table"), "nothing to tabulate: {body}");

    let (status, body) = get(router, "/api/crafts?target=t1").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["crafts"].as_array().map(std::vec::Vec::len), Some(0));
    assert!(json["recipesFile"].is_null());

    std::fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------- addon bundle

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    use sha2::{
        Digest,
        Sha256,
    };
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // `String`'s `fmt::Write` impl is infallible.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The addon routes serve the bundle the binary was built with: the manifest
/// is what the sync's own client parses, every listed file answers with its
/// manifest hash and size, and nothing else does — the lookup reads the
/// embedded table, so no path reaches outside it.
#[tokio::test]
async fn addon_routes_serve_the_embedded_bundle() {
    let (state, root) = test_state(None);
    let router = build_router_arc(Arc::clone(&state));

    let response = router
        .clone()
        .oneshot(
            Request::builder().uri("/api/addon/manifest").body(Body::empty()).expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let body = response.into_body().collect().await.expect("body").to_bytes();
    let manifest = amtracker_infra::addon_sync::parse_manifest(&body)
        .expect("the sync client parses the served manifest");
    assert_eq!(manifest.addon, "AuctiMoggerDatabase");
    assert_eq!(manifest.feed_version, 4);
    assert_eq!(manifest.version, amtracker_server::addon_bundle::toc_version());
    assert!(manifest.files.iter().any(|file| file.path == "Core/pack.lua"), "nested paths");
    assert!(manifest.files.iter().any(|file| file.path == "Market1.lua"), "slot placeholders");

    for file in &manifest.files {
        let (status, body) = get(router.clone(), &format!("/api/addon/file/{}", file.path)).await;
        assert_eq!(status, StatusCode::OK, "{}", file.path);
        assert_eq!(body.len(), usize::try_from(file.size).expect("size"), "{}", file.path);
        assert_eq!(sha256_hex(body.as_bytes()), file.sha256, "{}", file.path);
    }

    // Verbatim, the way the tree holds them, under the right content type.
    let (status, body) = get(router.clone(), "/api/addon/file/AuctiMoggerDatabase.toc").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, include_str!("../../../addon/AuctiMoggerDatabase/AuctiMoggerDatabase.toc"));
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/addon/file/Core/pack.lua")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").and_then(|value| value.to_str().ok()),
        Some("text/x-lua; charset=utf-8")
    );
    let body = response.into_body().collect().await.expect("body").to_bytes();
    assert_eq!(
        body.as_ref(),
        include_str!("../../../addon/AuctiMoggerDatabase/Core/pack.lua").as_bytes()
    );

    // A percent-encoded slash names the same file.
    let (status, _) = get(router.clone(), "/api/addon/file/Core%2Fpack.lua").await;
    assert_eq!(status, StatusCode::OK);

    for path in ["nope.lua", "../Cargo.toml", "Core/../../README.md", "Market0.lua", ""] {
        let (status, _) = get(router.clone(), &format!("/api/addon/file/{path}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    std::fs::remove_dir_all(&root).ok();
}

/// `addon-sync` end to end against a live socket: the real HTTP client and
/// the real on-disk folder talk to this very router. One run installs the
/// addon and every market; a second has nothing left to write; a slot that
/// holds a market is never replaced by its placeholder.
#[tokio::test]
async fn addon_sync_installs_the_bundle_and_the_markets_from_a_live_server() {
    use amtracker_core::services::addon_sync::{
        AddonSyncService,
        ChangeKind,
        SyncOptions,
    };
    use amtracker_infra::addon_sync::{
        AddonFolder,
        HttpTrackerAddon,
    };

    let (state, root) = test_state(None);
    let snapshot = DatabaseSnapshot {
        target: TargetId(TARGET.into()),
        market: MarketMeta {
            region: Some("us".into()),
            ruleset_id: None,
            house_id: None,
            connected_realm_id: Some(4),
            flavor: Flavor::default(),
        },
        tracker_version: "0.1.0-test".into(),
        generated_at: state.clock.now() - 1_500,
        auctions_at: None,
        commodities_at: None,
        entries: Vec::new(),
    };
    let lua = FeedCodec.render(&snapshot, FeedFormat::Lua).expect("render");
    state.publisher.publish(&TargetId(TARGET.into()), FeedFormat::Lua, &lua).expect("publish");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, build_router_arc(state)).await;
    });

    // The sync is blocking (ureq); it runs off the runtime that serves.
    let run = |base: String, addons_dir: PathBuf, options: SyncOptions| {
        tokio::task::spawn_blocking(move || {
            AddonSyncService::new(
                Arc::new(
                    HttpTrackerAddon::new(&base, std::time::Duration::from_secs(30))
                        .expect("client"),
                ),
                Arc::new(AddonFolder::new(addons_dir.join("AuctiMoggerDatabase"))),
                Arc::new(SystemClock),
            )
            .run(&options)
        })
    };
    let addons_dir = root.join("AddOns");
    let first = run(base.clone(), addons_dir.clone(), SyncOptions::default())
        .await
        .expect("join")
        .expect("run");
    assert!(first.is_clean(), "{:#?}", first.failures());
    assert_eq!(
        first.addon_count(ChangeKind::Written),
        first.manifest.as_ref().expect("manifest").files.len()
    );
    assert_eq!(first.markets.len(), 1);
    assert_eq!(first.markets[0].slot, 1);
    assert_eq!(first.markets[0].target, TARGET);

    let addon = addons_dir.join("AuctiMoggerDatabase");
    assert_eq!(
        std::fs::read(addon.join("AuctiMoggerDatabase.toc")).expect("toc"),
        include_str!("../../../addon/AuctiMoggerDatabase/AuctiMoggerDatabase.toc").as_bytes()
    );
    assert_eq!(
        std::fs::read(addon.join("Core/pack.lua")).expect("nested file"),
        include_str!("../../../addon/AuctiMoggerDatabase/Core/pack.lua").as_bytes()
    );
    assert_eq!(std::fs::read(addon.join("Market1.lua")).expect("the market"), lua.as_slice());
    assert_eq!(
        std::fs::read(addon.join("Market2.lua")).expect("placeholder 2"),
        include_str!("../../../addon/AuctiMoggerDatabase/Market2.lua").as_bytes()
    );

    // The second run: nothing left to write, and the filled slot still holds
    // its market rather than the placeholder.
    let second =
        run(base, addons_dir.clone(), SyncOptions::default()).await.expect("join").expect("run");
    assert!(second.is_clean(), "{:#?}", second.failures());
    assert_eq!(
        second.addon_count(ChangeKind::Unchanged),
        second.manifest.as_ref().expect("manifest").files.len()
    );
    assert_eq!(second.market_count(ChangeKind::Unchanged), 1);
    assert_eq!(std::fs::read(addon.join("Market1.lua")).expect("the market"), lua.as_slice());

    server.abort();
    std::fs::remove_dir_all(&root).ok();
}

/// The dashboard's feed section points at the addon manifest and names the
/// command that installs it.
#[tokio::test]
async fn dashboard_links_the_addon_manifest() {
    let (state, root) = test_state(None);
    let (status, body) = get(build_router_arc(Arc::clone(&state)), "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("href=\"/api/addon/manifest\""), "manifest link missing");
    assert!(body.contains("addon-sync --url"), "the install command is named");
    std::fs::remove_dir_all(&root).ok();
}

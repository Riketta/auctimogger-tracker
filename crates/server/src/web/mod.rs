//! Driving HTTP adapter: pages (dashboard, search/browse, item/pet detail,
//! opportunities, overview, watchlist, help), the JSON API surface, feed
//! downloads, and token-gated mutation endpoints. Rendering helpers live in
//! [`render`], the embedded assets in [`assets`], handlers in [`pages`] and
//! [`api`].

// Handlers stay `async` even when they never await so every axum route
// signature is uniform and stays forward-compatible with async extraction.
// The attribute applies module-wide; each submodule redeclares it for its
// own handlers.
#![allow(clippy::unused_async)]

pub mod api;
pub mod assets;
pub mod openapi;
pub mod pages;
pub mod render;

use std::{
    collections::{
        HashMap,
        VecDeque,
    },
    sync::{
        Arc,
        Mutex,
    },
};

use amtracker_core::{
    config::TrackerConfig,
    domain::{
        Category,
        Flavor,
        TargetId,
    },
    error::StoreError,
    ports::{
        ClockPort,
        FeedPublisher,
        FeedRenderer,
        PriceHistoryRepository,
        SnapshotRepository,
    },
    services::{
        cycle::IngestCycleService,
        feed_export::FeedExportService,
        health::{
            HealthService,
            TargetHealth,
        },
        metrics::MetricsService,
        opportunity::OpportunityService,
        watchlist::WatchlistService,
    },
};
use axum::{
    http::StatusCode,
    response::{
        IntoResponse,
        Response,
    },
    routing::{
        get,
        post,
        MethodRouter,
    },
    Router,
};

/// Shared handlers state. Everything is core-typed (port traits + services);
/// concrete adapters are wired in `main.rs`.
pub struct AppState {
    /// Fully parsed runtime configuration: targets, spans and thresholds.
    pub config: TrackerConfig,
    /// Time injection shared with the scheduler (prune/publish day math).
    pub clock: Arc<dyn ClockPort>,
    /// Snapshot freshness/age reporting for the dashboard and health API.
    pub health: Arc<HealthService>,
    /// Case-insensitive item/pet-name search over the catalog tables.
    pub search: Arc<amtracker_core::services::search::SearchService>,
    /// Snapshot viewer (browse) reads: pinned/latest resolution and paged
    /// listing rows behind `/browse` and `/api/browse`.
    pub browse: Arc<amtracker_core::services::snapshot_browse::SnapshotBrowseService>,
    /// Item/pet-name catalog backed by the store plus the Blizzard APIs.
    pub catalog: Arc<amtracker_core::services::catalog::CatalogService>,
    /// Precomputed per-span metric blocks (medians, VWAP, sale rate).
    pub metrics: Arc<MetricsService>,
    /// Discount/opportunity ranking behind the opportunities pages, served
    /// from scans the scheduler prepares in the background.
    pub opportunities: Arc<OpportunityService>,
    /// User-curated watch list that drives the slim feed export.
    pub watchlist: Arc<WatchlistService>,
    /// Whole-watchlist market reports (blocks, sale rates, trends) behind
    /// the live watchlist page and `/api/watchlist/metrics`.
    pub watchlist_report: Arc<amtracker_core::services::watchlist_report::WatchlistReportService>,
    /// Alert rule evaluation + CRUD behind the scheduler's post-cycle pass
    /// and the `/alerts` page/API.
    pub alerts: Arc<amtracker_core::services::alert::AlertEvaluationService>,
    /// Feed assembly shared by the JSON endpoint and the Lua downloads.
    pub feed_export: Arc<FeedExportService>,
    /// Published feed bytes backing the cached `/feed.lua` download.
    pub publisher: Arc<dyn FeedPublisher>,
    /// Feed serialization into Lua/JSON byte payloads.
    pub renderer: Arc<dyn FeedRenderer>,
    /// Runs one ingestion cycle (fetch, fold, credit) against a source.
    pub cycle: Arc<IngestCycleService>,
    /// Raw per-day price rows behind the item detail sparkline.
    pub history: Arc<dyn PriceHistoryRepository>,
    /// Snapshot metadata behind the snapshot browser's history view.
    pub snapshots: Arc<dyn SnapshotRepository>,
    /// Serializes ingestion cycles (scheduled + manual `/api/fetch`) so the
    /// `observe`/`close_missing` sequences of two cycles never interleave.
    pub cycle_lock: Arc<std::sync::Mutex<()>>,
    /// Recent ingest-cycle outcomes (fed by the scheduler), shown on the
    /// dashboard so `serve` activity is visible without reading logs.
    pub recent_cycles: Arc<RecentCycles>,
    /// Live fetch-schedule board: per-(target, kind) cycle timers written by
    /// the scheduler (and manual `/api/fetch`), rendered by the dashboard's
    /// "next fetch" rows and `/api/schedule`.
    pub timers: Arc<std::sync::Mutex<crate::scheduler::TargetTimers>>,
}

/// Builds the full HTTP router (pages, JSON API, feed downloads, token-gated
/// mutations) over `state`.
pub fn build_router(state: AppState) -> Router {
    build_router_arc(Arc::new(state))
}

/// Serves the router until the process is interrupted (call from a runtime).
/// Graceful shutdown lets in-flight requests finish: `Ctrl+C` anywhere, or
/// `SIGTERM` on Unix (systemd stop); the binary then flushes queued
/// logs/Sentry events before exiting.
///
/// `bind` is the resolved socket address ([`startup::resolve_bind`]) — the
/// listener and the startup loopback guard share one parse, so they can
/// never disagree about what is actually exposed.
///
/// # Errors
/// Returns the underlying [`std::io::Error`] when binding `bind` fails or
/// when the accept loop surfaces a fatal IO error.
pub async fn serve(state: Arc<AppState>, bind: std::net::SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, build_router_arc(state)).with_graceful_shutdown(shutdown_signal()).await
}

/// Resolves on `Ctrl+C` (all platforms) or `SIGTERM` (Unix).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{
            signal,
            SignalKind,
        };
        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => tracing::warn!(error = %error, "SIGTERM handler unavailable"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

/// One mounted API route: the spec/router path, its methods, its handlers.
type ApiRoute = (&'static str, &'static [&'static str], MethodRouter<Arc<AppState>>);

/// The canonical API surface: every JSON/feed route with its methods, in one
/// table. `build_router_arc` mounts exactly this table, and the drift test
/// (`tests/web.rs`) asserts the `OpenAPI` document matches it in both
/// directions — so a route cannot exist without a spec entry, and a spec
/// entry cannot exist without a route. Paths use axum 0.8 `{param}` syntax,
/// which is also the `OpenAPI` path syntax, so the strings are shared
/// verbatim.
pub fn api_routes() -> Vec<ApiRoute> {
    vec![
        ("/api", &["get"], get(api::api_index)),
        ("/api/health", &["get"], get(api::api_health)),
        ("/api/openapi.json", &["get"], get(api::api_openapi)),
        ("/api/docs", &["get"], get(openapi::docs_page)),
        ("/api/browse", &["get"], get(api::api_browse)),
        ("/api/history", &["get"], get(api::api_history)),
        ("/api/search", &["get"], get(api::api_search)),
        ("/api/opportunities", &["get"], get(api::api_opportunities)),
        ("/api/overview", &["get"], get(api::api_overview)),
        ("/api/items/{target_id}/{item_id}/metrics", &["get"], get(api::api_item_metrics)),
        (
            "/api/items/{target_id}/pet/{species_id}/metrics",
            &["get"],
            get(api::api_item_pet_metrics),
        ),
        ("/api/feed.json", &["get"], get(api::api_feed_json)),
        (
            "/api/watchlist",
            // Alphabetical: serde_json maps serialize keys sorted, and the
            // drift test compares the table against the spec verbatim.
            &["delete", "get", "post"],
            get(api::api_watchlist_get)
                .post(api::api_watchlist_post)
                .delete(api::api_watchlist_delete),
        ),
        ("/api/watchlist/metrics", &["get"], get(api::api_watchlist_metrics)),
        ("/api/snapshots", &["get"], get(api::api_snapshots)),
        ("/api/schedule", &["get"], get(api::api_schedule)),
        (
            "/api/alerts",
            &["delete", "get", "post"],
            get(api::api_alerts_get).post(api::api_alerts_post).delete(api::api_alerts_delete),
        ),
        ("/api/alerts/evaluate", &["post"], post(api::api_alerts_evaluate)),
        ("/api/fetch", &["post"], post(api::api_fetch)),
        ("/feed.lua", &["get"], get(api::feed_lua)),
        ("/feed-slim.lua", &["get"], get(api::feed_slim_lua)),
    ]
}

/// Builds the full HTTP router (pages, JSON API, feed downloads, token-gated
/// mutations) over shared state — the `Arc<AppState>` form `serve` uses, and
/// the form integration tests drive. The API surface is mounted from
/// [`api_routes`], the one table the drift test pins the spec to.
pub fn build_router_arc(state: Arc<AppState>) -> Router {
    let mut router = Router::new()
        .route("/", get(pages::dashboard))
        .route("/browse", get(pages::browse_page))
        .route("/snapshots", get(pages::snapshots_page))
        .route("/items", get(pages::items_page))
        .route("/items/{target_id}/{item_id}", get(pages::item_page))
        .route("/items/{target_id}/pet/{species_id}", get(pages::pet_page))
        .route("/opportunities", get(pages::opportunities_page))
        .route("/overview", get(pages::overview_page))
        .route("/watchlist", get(pages::watchlist_page))
        .route("/alerts", get(pages::alerts_page))
        .route("/help", get(pages::help_page))
        .route("/assets/app.css", get(assets::asset_css))
        .route("/assets/app.js", get(assets::asset_js))
        .route("/favicon.svg", get(assets::favicon));
    for (path, _, method_router) in api_routes() {
        router = router.route(path, method_router);
    }
    router.fallback(not_found).with_state(state)
}

async fn not_found() -> Response {
    let mut response = render::page(
        "not found",
        "",
        "<p class=\"muted\">The requested path does not exist. Try the <a href=\"/\">dashboard</a>.</p>",
        None,
    );
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

// ------------------------------------------------------- recent cycles log

/// One finished ingest cycle as surfaced on the dashboard.
#[derive(Clone, Debug)]
pub struct CycleRecord {
    /// When the cycle finished (unix seconds).
    pub at: i64,
    /// Target the cycle ran for.
    pub target: String,
    /// Payload kind (`commodities` / `auctions`).
    pub kind: String,
    /// Outcome label: `fresh`, `not-modified`, `empty`, `failed`, `panicked`.
    pub outcome: String,
    /// Listings accepted by the ACL pass (0 unless fresh).
    pub entries: u64,
    /// Listings dropped by the ACL pass.
    pub dropped: u64,
    /// Items whose day fold changed.
    pub folds: usize,
    /// Sale credits granted.
    pub credits: usize,
    /// Error/panic message for failed or panicked cycles.
    pub error: Option<String>,
}

/// Bounded in-memory ring of recent cycle outcomes (dashboard only; the file
/// log remains the durable record).
pub struct RecentCycles {
    inner: Mutex<VecDeque<CycleRecord>>,
    capacity: usize,
}

impl RecentCycles {
    /// Creates the ring with the given retention.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self { inner: Mutex::new(VecDeque::new()), capacity }
    }

    /// Records one finished cycle, dropping the oldest beyond capacity.
    pub fn push(&self, record: CycleRecord) {
        let mut ring = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if ring.len() >= self.capacity {
            ring.pop_front();
        }
        ring.push_back(record);
    }

    /// Newest-last copy of the ring (dashboard rendering).
    #[must_use]
    pub fn snapshot(&self) -> Vec<CycleRecord> {
        let ring = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        ring.iter().cloned().collect()
    }
}

// ---------------------------------------------------------------- helpers

/// Runs a blocking store-backed health read off the async workers and maps
/// failures into the shared 500 page (`Box`ed: `Response` is large).
pub(crate) async fn health_blocking(
    state: &Arc<AppState>,
) -> Result<Vec<TargetHealth>, Box<Response>> {
    let task = tokio::task::spawn_blocking({
        let state = Arc::clone(state);
        move || state.health.health()
    });
    match task.await {
        Ok(Ok(health)) => Ok(health),
        Ok(Err(error)) => Err(Box::new(render::store_error_page(error).await)),
        Err(error) => {
            Err(Box::new(render::store_error_page(StoreError::Backend(error.to_string())).await))
        }
    }
}

/// Resolves the `?target=` query parameter against the configured targets.
/// Unknown ids are rejected: they must never reach the store, the feed
/// publisher's filesystem reads (path traversal), or the rendered HTML
/// (reflected content).
pub(crate) fn target_param(
    params: &HashMap<String, String>,
    config: &TrackerConfig,
) -> Result<TargetId, Box<Response>> {
    match params.get("target") {
        None => {
            // `TrackerConfig::validate` requires at least one target, so the
            // index is provably in bounds; a fallback would silently query
            // the wrong market.
            #[allow(clippy::indexing_slicing)]
            {
                Ok(config.targets[0].id.clone())
            }
        }
        Some(t) => known_target(config, t),
    }
}

/// Like [`target_param`], but the parameter is mandatory (JSON API shape).
pub(crate) fn required_target(
    params: &HashMap<String, String>,
    config: &TrackerConfig,
) -> Result<TargetId, Box<Response>> {
    match params.get("target") {
        Some(t) => known_target(config, t),
        None => Err(Box::new((StatusCode::BAD_REQUEST, "missing ?target=").into_response())),
    }
}

/// Parses the optional `?category=` query parameter. Absent → the given
/// default (pages default to `item`); unknown values are rejected with a 400
/// that names the valid options.
pub(crate) fn category_param(
    params: &HashMap<String, String>,
    default: Category,
) -> Result<Category, Box<Response>> {
    match params.get("category") {
        None => Ok(default),
        Some(value) => Category::parse(value).ok_or_else(|| {
            Box::new(
                (
                    StatusCode::BAD_REQUEST,
                    format!("unknown category '{value}' (expected item, commodity or pet)"),
                )
                    .into_response(),
            )
        }),
    }
}

/// Like [`category_param`], but absent → `None` (the JSON API's
/// all-categories keyed response shape).
pub(crate) fn category_option(
    params: &HashMap<String, String>,
) -> Result<Option<Category>, Box<Response>> {
    match params.get("category") {
        None => Ok(None),
        Some(value) => Ok(Some(Category::parse(value).ok_or_else(|| {
            Box::new(
                (
                    StatusCode::BAD_REQUEST,
                    format!("unknown category '{value}' (expected item, commodity or pet)"),
                )
                    .into_response(),
            )
        })?)),
    }
}

/// The catalog flavor configured for one target; unknown targets default to
/// retail (they are rejected by `known_target` on the render paths anyway).
pub(crate) fn target_flavor(config: &TrackerConfig, target: &TargetId) -> Flavor {
    config.find_target(target.as_str()).map_or_else(Flavor::default, |target| target.flavor.clone())
}

/// Maps a raw target slug to a configured [`TargetId`], rejecting unknown
/// ids with a 400 that names the valid options.
pub(crate) fn known_target(config: &TrackerConfig, id: &str) -> Result<TargetId, Box<Response>> {
    config.find_target(id).map(|target| target.id.clone()).ok_or_else(|| {
        Box::new(
            (
                StatusCode::BAD_REQUEST,
                format!(
                    "unknown target '{}' (configured: {})",
                    render::html_escape(id),
                    config
                        .targets
                        .iter()
                        .map(|t| t.id.as_str().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
                .into_response(),
        )
    })
}

/// The configured API token value; `None` = the mutating API is open (no
/// `token_env` configured, or the named env var is unset, non-unicode, or
/// EMPTY — the documented developer-mode behavior: an empty compose default
/// must not turn into a zero-length password).
pub(crate) fn expected_api_token(config: &TrackerConfig) -> Option<String> {
    let env = config.server.token_env.as_ref()?;
    std::env::var(env).ok().filter(|value| !value.trim().is_empty())
}

/// Whether mutation controls may be rendered into pages: only when the
/// mutating API is actually open. With a token configured the browser cannot
/// hold it safely, so the UI stays read-only.
pub(crate) fn mutations_open(config: &TrackerConfig) -> bool {
    expected_api_token(config).is_none()
}

/// Adapter-side logging for a failed core call (the other half of the "core
/// never logs" rule): the client sees the 500, the file log and Sentry see
/// the cause.
pub(crate) fn log_failure(op: &'static str, error: impl std::fmt::Display) {
    tracing::error!(op, error = %error, "request failed");
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect/panic: fixture-driven, deterministic
    // inputs (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use amtracker_core::domain::Span;

    use super::*;

    /// Minimal-but-valid tracker config for authorization/target tests.
    pub(crate) fn test_config(allow_open: bool) -> TrackerConfig {
        use amtracker_core::config::PollConfig;
        TrackerConfig {
            server: amtracker_core::config::ServerConfig {
                bind: "127.0.0.1:0".into(),
                token_env: Some("AMT_API_TOKEN_TEST".into()),
                allow_open,
                feed_publish_secs: 300,
            },
            storage: amtracker_core::config::StorageConfig {
                database: "mem.sqlite3".into(),
                raw_listing_days: 7,
                sight_retention_days: 0,
                archive_dir: "raw".into(),
                feed_dir: "feeds".into(),
                backup_dir: None,
                backup_secs: 0,
                backup_keep: 7,
            },
            metrics: amtracker_core::config::MetricsConfig { spans: vec![Span::D1] },
            opportunities: amtracker_core::config::OpportunitiesConfig {
                min_samples: 1,
                weights: amtracker_core::config::ScoreWeights {
                    discount: 0.4,
                    sale: 0.3,
                    churn: 0.2,
                    stability: 0.1,
                },
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
            catalog: amtracker_core::config::CatalogConfig::default(),
            targets: vec![amtracker_core::config::TargetConfig {
                id: TargetId("us-4".into()),
                kind: amtracker_core::config::TargetKind::BlizzardConnectedRealm {
                    region: "us".into(),
                    connected_realm_id: 4,
                },
                auth: None,
                poll: PollConfig { commodities_secs: 60, auctions_secs: 60 },
                flavor: amtracker_core::domain::Flavor::default(),
            }],
        }
    }

    /// authorize matrix: unset/empty token (open mode), valid, and wrong
    /// token, crossed with the open-mode expectation.
    #[test]
    fn authorize_matrix_open_mode_allows_anything() {
        // No configured token: open, with or without a presented bearer.
        assert!(api::authorize_result(None, None).is_ok());
        assert!(api::authorize_result(None, Some("anything")).is_ok());
        // Empty/blank configured values normalize to open (the env filter
        // mirrors `expected_api_token`).
        assert!(api::authorize_result(None, Some("")).is_ok());
    }

    #[test]
    fn authorize_matrix_protected_mode_rejects_wrong_or_missing_tokens() {
        let expected = Some("s3cret");
        assert!(api::authorize_result(expected, Some("s3cret")).is_ok());
        for provided in [None, Some(""), Some("wrong"), Some("s3cret "), Some("s3cretx")] {
            let response = api::authorize_result(expected, provided).expect_err("must reject");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "provided {provided:?}");
        }
    }

    #[test]
    fn unknown_targets_are_rejected_with_a_bad_request() {
        let config = test_config(false);
        let response = known_target(&config, "nope").expect_err("unknown target");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(known_target(&config, "us-4").is_ok(), "configured target resolves");
    }

    #[test]
    fn constant_time_compare_matches_exact_tokens_only() {
        assert!(api::tokens_match("abc", "abc"));
        assert!(!api::tokens_match("abd", "abc"));
        assert!(!api::tokens_match("abcd", "abc"));
        assert!(!api::tokens_match("", "abc"));
        assert!(api::tokens_match("", ""));
    }

    #[test]
    fn recent_cycles_ring_bounds_and_orders() {
        let ring = RecentCycles::new(3);
        for i in 0..5 {
            ring.push(CycleRecord {
                at: i,
                target: "t".into(),
                kind: "commodities".into(),
                outcome: "fresh".into(),
                entries: u64::try_from(i).unwrap_or(0),
                dropped: 0,
                folds: 0,
                credits: 0,
                error: None,
            });
        }
        let snapshot = ring.snapshot();
        assert_eq!(snapshot.len(), 3, "capacity is respected");
        assert_eq!(snapshot.first().map(|r| r.at), Some(2), "oldest entries are dropped");
        assert_eq!(snapshot.last().map(|r| r.at), Some(4), "newest stays last");
    }
}

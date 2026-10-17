//! Driving HTTP adapter: pages (dashboard, search, item detail,
//! opportunities, overview, snapshots), the JSON API surface, feed downloads,
//! and token-gated mutation endpoints.

// Handlers stay `async` even when they never await so every axum route
// signature is uniform and stays forward-compatible with async extraction.
#![allow(clippy::unused_async)]

use std::{
    collections::HashMap,
    fmt::Write as _,
    sync::Arc,
};

use amtracker_core::{
    config::TrackerConfig,
    domain::{
        FeedFormat,
        ItemRef,
        PayloadKind,
        Span,
        TargetId,
    },
    error::StoreError,
    ports::{
        FeedPublisher,
        FeedRenderer,
        ItemInfoPort,
        PriceHistoryRepository,
    },
    services::{
        cycle::IngestCycleService,
        feed_export::{
            ExportOptions,
            FeedExportService,
        },
        health::HealthService,
        metrics::MetricsService,
        opportunity::{
            MoverRow,
            OpportunityService,
        },
        watchlist::WatchlistService,
    },
};
use axum::{
    extract::{
        Path,
        Query,
        State,
    },
    http::{
        header,
        HeaderMap,
        StatusCode,
    },
    response::{
        Html,
        IntoResponse,
        Response,
    },
    routing::{
        get,
        post,
    },
    Json,
    Router,
};
use serde::Deserialize;

/// Shared handlers state. Everything is core-typed (port traits + services);
/// concrete adapters are wired in `main.rs`.
pub struct AppState {
    /// Fully parsed runtime configuration: targets, spans and thresholds.
    pub config: TrackerConfig,
    /// Time injection shared with the scheduler (prune/publish day math).
    pub clock: Arc<dyn amtracker_core::ports::ClockPort>,
    /// Snapshot freshness/age reporting for the dashboard and health API.
    pub health: Arc<HealthService>,
    /// Case-insensitive item-name search over the catalog table.
    pub search: Arc<amtracker_core::services::search::SearchService>,
    /// Item-name catalog backed by the store plus the Blizzard item API.
    pub catalog: Arc<amtracker_core::services::catalog::CatalogService>,
    /// Precomputed per-span metric blocks (medians, VWAP, sale rate).
    pub metrics: Arc<MetricsService>,
    /// Discount/opportunity ranking behind the opportunities pages.
    pub opportunities: Arc<OpportunityService>,
    /// User-curated watch list that drives the slim feed export.
    pub watchlist: Arc<WatchlistService>,
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
    /// Late item-name resolution for IDs missing from the catalog.
    pub item_info: Arc<dyn ItemInfoPort>,
    /// Serializes ingestion cycles (scheduled + manual `/api/fetch`) so the
    /// `observe`/`close_missing` sequences of two cycles never interleave.
    pub cycle_lock: Arc<std::sync::Mutex<()>>,
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
/// # Errors
/// Returns the underlying [`std::io::Error`] when binding `bind` fails or
/// when the accept loop surfaces a fatal IO error.
pub async fn serve(state: Arc<AppState>, bind: String) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(&bind).await?;
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

fn build_router_arc(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/snapshots", get(snapshots_page))
        .route("/items", get(items_search_page))
        .route("/items/{target_id}/{item_id}", get(item_page))
        .route("/opportunities", get(opportunities_page))
        .route("/overview", get(overview_page))
        .route("/feed.lua", get(feed_lua))
        .route("/feed-slim.lua", get(feed_slim_lua))
        .route("/api/health", get(api_health))
        .route("/api/search", get(api_search))
        .route("/api/opportunities", get(api_opportunities))
        .route("/api/overview", get(api_overview))
        .route("/api/items/{target_id}/{item_id}/metrics", get(api_item_metrics))
        .route("/api/feed.json", get(api_feed_json))
        .route(
            "/api/watchlist",
            get(api_watchlist_get).post(api_watchlist_post).delete(api_watchlist_delete),
        )
        .route("/api/fetch", post(api_fetch))
        .with_state(state)
}

// ------------------------------------------------------------------ helpers

const STYLES: &str = r"<style>
body { font-family: system-ui, sans-serif; margin: 1rem 2rem; color: #ddd; background: #16181d; }
a { color: #7ab8ff; text-decoration: none; } a:hover { text-decoration: underline; }
h1 { font-size: 1.3rem; } h2 { font-size: 1.1rem; margin-top: 1.4rem; }
table { border-collapse: collapse; margin: .6rem 0; }
th, td { border: 1px solid #333; padding: .25rem .6rem; text-align: right; }
th:first-child, td:first-child { text-align: left; }
nav a { margin-right: 1rem; }
.bar { background: #2f81f7; height: .55rem; min-width: 1px; display: inline-block; vertical-align: middle; }
input[type=text] { background: #22262d; color: #ddd; border: 1px solid #444; padding: .25rem .5rem; width: 24rem; }
.ok { color: #7ee787; } .bad { color: #ff7b72; } .muted { color: #888; }
code { color: #ffa657; }
</style>";

fn layout(title: &str, body: &str) -> Response {
    Html(format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>{title} — auctimogger-tracker</title>{STYLES}</head>\
         <body><nav><a href=\"/\">dashboard</a><a href=\"/opportunities\">opportunities</a>\
         <a href=\"/overview\">overview</a><a href=\"/snapshots\">snapshots</a></nav>\
         <h1>{title}</h1>{body}<p class=\"muted\">auctimogger-tracker</p></body></html>"
    ))
    .into_response()
}

fn money(copper: i64) -> String {
    let gold = copper / 10_000;
    let silver = (copper % 10_000) / 100;
    let rem = copper % 100;
    if gold > 0 {
        format!("{gold}g {silver}s {rem}c")
    } else if silver > 0 {
        format!("{silver}s {rem}c")
    } else {
        format!("{rem}c")
    }
}

fn item_label(item: &ItemRef) -> String {
    match item {
        ItemRef::Item { item_id } => format!("item {item_id}"),
        ItemRef::Pet { species, level, .. } => format!("pet {species} (lvl {level})"),
    }
}

fn item_link(target: &TargetId, item: &ItemRef) -> String {
    let item_id = match item {
        ItemRef::Item { item_id } => *item_id,
        ItemRef::Pet { species, .. } => *species,
    };
    format!("<a href=\"/items/{}/{}\">{}</a>", target.as_str(), item_id, item_label(item))
}

// SVG coordinates: the integer→float rounding loss is invisible at 120×30 px.
#[allow(clippy::cast_precision_loss)]
fn sparkline(values: &[i64]) -> String {
    if values.len() < 2 {
        return String::new();
    }
    let min = *values.iter().min().unwrap_or(&0);
    let max = *values.iter().max().unwrap_or(&1);
    let span = (max - min).max(1) as f64;
    let step = 120.0 / (values.len() - 1) as f64;
    let points: Vec<String> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let y = 30.0 - ((*v - min) as f64 / span * 28.0);
            format!("{:.1},{:.1}", i as f64 * step, y)
        })
        .collect();
    format!(
        r##"<svg width="120" height="30" aria-hidden="true"><polyline fill="none" stroke="#7ab8ff" stroke-width="1.5" points="{}"/></svg>"##,
        points.join(" ")
    )
}

/// Resolves the `?target=` query parameter against the configured targets.
/// Unknown ids are rejected: they must never reach the store, the feed
/// publisher's filesystem reads (path traversal), or the rendered HTML
/// (reflected content).
fn target_param(
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
fn required_target(
    params: &HashMap<String, String>,
    config: &TrackerConfig,
) -> Result<TargetId, Box<Response>> {
    match params.get("target") {
        Some(t) => known_target(config, t),
        None => Err(Box::new((StatusCode::BAD_REQUEST, "missing ?target=").into_response())),
    }
}

fn known_target(config: &TrackerConfig, id: &str) -> Result<TargetId, Box<Response>> {
    config.find_target(id).map(|target| target.id.clone()).ok_or_else(|| {
        Box::new(
            (
                StatusCode::BAD_REQUEST,
                format!(
                    "unknown target '{}' (configured: {})",
                    html_escape(id),
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

/// Escapes a string for safe interpolation into HTML text/attribute
/// positions. External data (item names, query echoes) never reaches markup
/// raw.
fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

async fn error_page(message: String) -> Response {
    let mut response = layout("error", &format!("<p class=\"bad\">{message}</p>")).into_response();
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response
}

/// Adapter-side logging for a failed core call (the other half of the "core
/// never logs" rule): the client sees the 500, the file log and Sentry see
/// the cause.
fn log_failure(op: &'static str, error: impl std::fmt::Display) {
    tracing::error!(op, error = %error, "request failed");
}

async fn store_error_page(error: StoreError) -> Response {
    log_failure("page", &error);
    let mut response = error_page(error.to_string()).await;
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response
}

// ------------------------------------------------------------------ pages

async fn dashboard(State(state): State<Arc<AppState>>) -> Response {
    // SQLite reads stay off the async workers (see item_page/feed_file).
    let health = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        move || state.health.health()
    });
    let health = match health.await {
        Ok(Ok(health)) => health,
        Ok(Err(error)) => return store_error_page(error).await,
        Err(error) => return store_error_page(StoreError::Backend(error.to_string())).await,
    };
    let mut body = String::from(
        "<h2>targets</h2><table><tr><th>target</th><th>payload</th><th>latest fetch</th><th>age</th><th>entries</th><th>dropped</th></tr>",
    );
    for target in &health {
        for kind in &target.kinds {
            match &kind.latest {
                Some(meta) => {
                    let _ = write!(
                        body,
                        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}s</td><td>{}</td><td>{}</td></tr>",
                        target.target,
                        kind.kind.as_str(),
                        meta.fetched_at,
                        kind.age_seconds.unwrap_or(0),
                        meta.entry_count,
                        meta.dropped_count,
                    );
                }
                None => {
                    let _ = write!(
                        body,
                        "<tr><td>{}</td><td>{}</td><td colspan=\"4\" class=\"muted\">no snapshot yet</td></tr>",
                        target.target,
                        kind.kind.as_str(),
                    );
                }
            }
        }
    }
    body.push_str("</table>");
    body.push_str(
        "<h2>feeds</h2><p><a href=\"/feed.lua\">feed.lua</a> · <a href=\"/feed-slim.lua\">feed-slim.lua</a> · \
         <a href=\"/api/feed.json\">feed.json</a></p><p class=\"muted\">download into \
         <code>AuctiMogger/AuctiMoggerTracker/</code> and <code>/reload</code></p>",
    );
    layout("dashboard", &body)
}

async fn snapshots_page(State(state): State<Arc<AppState>>) -> Response {
    let health = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        move || state.health.health()
    });
    let health = match health.await {
        Ok(Ok(health)) => health,
        Ok(Err(error)) => return store_error_page(error).await,
        Err(error) => return store_error_page(StoreError::Backend(error.to_string())).await,
    };
    let mut body = String::from(
        "<h2>latest snapshots</h2><table><tr><th>target</th><th>payload</th><th>fetched at</th><th>source modified</th><th>entries</th><th>dropped</th><th>status</th></tr>",
    );
    for target in &health {
        for kind in &target.kinds {
            match &kind.latest {
                Some(meta) => {
                    let _ = write!(
                        body,
                        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                        target.target,
                        kind.kind.as_str(),
                        meta.fetched_at,
                        meta.source_last_modified.unwrap_or(0),
                        meta.entry_count,
                        meta.dropped_count,
                        meta.status.as_str(),
                    );
                }
                None => {
                    let _ = write!(
                        body,
                        "<tr><td>{}</td><td>{}</td><td colspan=\"5\" class=\"muted\">none yet</td></tr>",
                        target.target,
                        kind.kind.as_str(),
                    );
                }
            }
        }
    }
    body.push_str("</table>");
    layout("snapshots", &body)
}

async fn items_search_page(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match target_param(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let query = params.get("q").cloned().unwrap_or_default();
    // The completions script builds detail-page URLs; the target id is a
    // configured slug, but it is escaped like every other interpolation so a
    // pathological id cannot break out of the JS string or the script tag.
    let completions = format!(
        "<input type=\"text\" id=\"q\" placeholder=\"search item name\u{2026}\" autocomplete=\"off\">\
         <div id=\"completions\"></div>\
         <script>const q=document.getElementById('q'),c=document.getElementById('completions');\
         let t=null;q.addEventListener('input',()=>{{clearTimeout(t);const v=q.value.trim();\
         if(v.length<2){{c.textContent='';return;}}\
         t=setTimeout(async()=>{{const r=await fetch('/api/search?q='+encodeURIComponent(v));\
         const j=await r.json();c.textContent='';\
         for(const h of j.hits){{const d=document.createElement('div');\
         const a=document.createElement('a');a.href='/items/{target_js}/'+h.itemID;a.textContent=h.name;\
         const s=document.createElement('span');s.className='muted';\
         s.textContent=h.itemID+(h.itemClass?' \u{2014} '+h.itemClass:'');\
         d.appendChild(a);d.appendChild(s);c.appendChild(d);}},250);}});</script>",
        target_js = html_escape(target.as_str())
    );
    let mut body = completions;
    if !query.trim().is_empty() {
        // SQLite reads stay off the async workers.
        let hits = tokio::task::spawn_blocking({
            let state = Arc::clone(&state);
            let query = query.clone();
            move || state.search.search(&query, 25)
        });
        match hits.await {
            Ok(Ok(hits)) => {
                body.push_str("<h2>results</h2><ul>");
                for hit in hits {
                    let _ = write!(
                        body,
                        "<li><a href=\"/items/{}/{}\">{}</a> <span class=\"muted\">{}</span></li>",
                        html_escape(target.as_str()),
                        hit.item_id,
                        html_escape(&hit.name),
                        html_escape(hit.item_class.as_deref().unwrap_or(""))
                    );
                }
                body.push_str("</ul>");
            }
            Ok(Err(error)) => return store_error_page(error).await,
            Err(error) => return store_error_page(StoreError::Backend(error.to_string())).await,
        }
    }
    layout("items", &body)
}

/// What the item page renders from the store: the item-scoped day rows plus
/// one optional metric block per span.
type ItemPageData = (
    Vec<amtracker_core::domain::ItemDayRow>,
    Vec<(Span, Option<amtracker_core::domain::MetricBlock>)>,
);

async fn item_page(
    State(state): State<Arc<AppState>>,
    Path((target_id, item_id)): Path<(String, i64)>,
) -> Response {
    let target = match known_target(&state.config, &target_id) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    // name lookup: catalog first, then the item API (cached permanently);
    // blocking sync core work stays off the async workers.
    let state2 = Arc::clone(&state);
    let lookup = tokio::task::spawn_blocking(move || state2.catalog.ensure(item_id));
    let name = match lookup.await {
        Ok(Ok(record)) => record.map(|r| r.name),
        Ok(Err(error)) => {
            // The name is auxiliary: a failed item-API lookup (expired
            // token, upstream outage) must not take the whole page down
            // while the metrics below are still renderable.
            tracing::warn!(
                item_id,
                error = %error,
                "item name lookup failed; rendering with placeholder"
            );
            None
        }
        Err(error) => return store_error_page(StoreError::Backend(error.to_string())).await,
    };
    let title = html_escape(&name.unwrap_or_else(|| format!("item {item_id}")));

    // Window + analytics all read SQLite through the injected clock; keep the
    // blocking body off the async workers.
    let today = amtracker_core::domain::utc_day(state.clock.now());
    let item = ItemRef::Item { item_id };
    let since = today - 29;
    let page_data = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        let item = item.clone();
        move || -> Result<ItemPageData, StoreError> {
            // Item-scoped query: the SQL filters on the exact identity tuple
            // instead of loading the whole market's rows (the opportunity
            // scan keeps the unfiltered variant).
            let rows = state.history.item_day_rows_for_item(&target, &item, since)?;
            let mut blocks = Vec::with_capacity(4);
            for span in [Span::D1, Span::D7, Span::D14, Span::D30] {
                blocks.push((span, state.metrics.block(&target, &item, span)?));
            }
            Ok((rows, blocks))
        }
    });
    let (rows, blocks) = match page_data.await {
        Ok(Ok(data)) => data,
        Ok(Err(error)) => return store_error_page(error).await,
        Err(error) => return store_error_page(StoreError::Backend(error.to_string())).await,
    };
    let mut days: Vec<&amtracker_core::domain::ItemDayRow> = rows.iter().collect();
    days.sort_by_key(|row| row.day);
    let medians: Vec<i64> = days.iter().map(|row| row.median).collect();
    let chart = sparkline(&medians);

    let mut body = format!(
        "<h2>{title}</h2><p class=\"muted\">target {} \u{b7} item id {item_id}</p>",
        html_escape(target.as_str())
    );
    let _ = write!(body, "<h2>30-day median</h2><p>{chart}</p>");
    body.push_str("<h2>metric blocks</h2><table><tr><th>span</th><th>n</th><th>median</th><th>min</th><th>p05</th><th>p25</th><th>p75</th><th>p95</th><th>max</th><th>mad</th><th>vwap</th><th>qty</th><th>sale rate</th></tr>");
    for (span, block) in &blocks {
        if let Some(block) = block {
            let _ = write!(
                body,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                span.name(),
                block.n,
                money(block.median),
                money(block.min),
                money(block.p05),
                money(block.p25),
                money(block.p75),
                money(block.p95),
                money(block.max),
                block.mad,
                money(block.vwap),
                block.qty_sum,
                block.sale_rate.map_or_else(|| "—".into(), |r| format!("{r:.2}"))
            );
        }
    }
    body.push_str("</table>");
    layout(&title, &body)
}

fn component_bar(label: &str, value: f64) -> String {
    format!(
        "<span class=\"muted\">{label}</span> <span class=\"bar\" style=\"width:{:.0}px\"></span> {:.2}",
        value * 60.0,
        value
    )
}

async fn opportunities_page(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match target_param(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let min_churn: f64 = params.get("min_churn").and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let data = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || state.opportunities.opportunities(&target, min_churn, 50)
    });
    let data = match data.await {
        Ok(Ok(data)) => data,
        Ok(Err(error)) => return store_error_page(error).await,
        Err(error) => return store_error_page(StoreError::Backend(error.to_string())).await,
    };
    let mut body = format!(
        "<p>ranked by score = w\u{b7}(discount + reliability + churn + stability); \
         confidence = evidence vs min samples. target <code>{}</code></p>\
         <table><tr><th>item</th><th>score</th><th>confidence</th><th>discount</th><th>reliability</th>\
         <th>churn/day</th><th>stability</th><th>floor</th><th>median7</th></tr>",
        html_escape(target.as_str())
    );
    for o in &data {
        let _ = write!(
            body,
            "<tr><td>{}</td><td>{:.3}</td><td>{:.0}%</td><td>{}</td><td>{}</td><td>{:.1}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            item_link(&target, &o.item),
            o.score,
            o.confidence * 100.0,
            component_bar("disc", o.discount),
            component_bar("rel", o.reliability),
            o.churn_per_day,
            component_bar("stab", o.stability),
            money(o.floor_now),
            money(o.median7),
        );
    }
    body.push_str("</table>");
    layout("opportunities", &body)
}

async fn overview_page(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // Display-only conversion: the f64 median feeds the i64 `money` and
    // sparkline helpers, matching the previous `as i64` rendering.
    #[allow(clippy::cast_possible_truncation)]
    fn table(rows: &[MoverRow], target: &TargetId) -> String {
        let mut out = String::from("<table><tr><th>item</th><th>median now</th><th>median 7d</th><th>change</th><th>churn/day</th><th>30d</th></tr>");
        for m in rows {
            let _ = write!(
                out,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:+.1}%</td><td>{:.1}</td><td>{}</td></tr>",
                item_link(target, &m.item),
                money(m.median_now),
                money(m.median_prev as i64),
                m.change * 100.0,
                m.churn_per_day,
                sparkline(&[m.median_now, m.median_prev as i64]),
            );
        }
        out.push_str("</table>");
        out
    }
    let target = match target_param(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let data = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || state.opportunities.overview(&target, 15)
    });
    let data = match data.await {
        Ok(Ok(data)) => data,
        Ok(Err(error)) => return store_error_page(error).await,
        Err(error) => return store_error_page(StoreError::Backend(error.to_string())).await,
    };
    let body = format!(
        "<h2>rising</h2>{}<h2>falling</h2>{}<h2>most active</h2>{}",
        table(&data.movers_up, &target),
        table(&data.movers_down, &target),
        table(&data.most_active, &target)
    );
    layout("market overview", &body)
}

// ------------------------------------------------------------------ feeds

async fn feed_file(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    slim: bool,
) -> Response {
    let target = match target_param(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    // Feed assembly/render touches SQLite and possibly lots of folds; keep it
    // off the async workers.
    let build = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || {
            if slim {
                // slim is built on demand from the watchlist (no cached copy)
                let options = ExportOptions {
                    spans: state.config.metrics.spans.clone(),
                    watchlist_only: true,
                };
                state.feed_export.build_feed(&target, &options).and_then(|feed| {
                    state
                        .renderer
                        .render(&feed, FeedFormat::Lua)
                        .map(Some)
                        .map_err(|e| StoreError::Backend(e.to_string()))
                })
            } else {
                state.publisher.read(&target, FeedFormat::Lua)
            }
        }
    });
    let bytes = match build.await {
        Ok(result) => result,
        Err(error) => return store_error_page(StoreError::Backend(error.to_string())).await,
    };
    match bytes {
        Ok(Some(bytes)) => (
            [
                (header::CONTENT_TYPE, "text/x-lua; charset=utf-8"),
                (
                    header::CONTENT_DISPOSITION,
                    if slim {
                        "attachment; filename=\"feed-slim.lua\""
                    } else {
                        "attachment; filename=\"feed.lua\""
                    },
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            format!("no feed published yet for target '{}'", target.as_str()),
        )
            .into_response(),
        Err(error) => store_error_page(error).await,
    }
}

async fn feed_lua(state: State<Arc<AppState>>, params: Query<HashMap<String, String>>) -> Response {
    feed_file(state, params, false).await
}

async fn feed_slim_lua(
    state: State<Arc<AppState>>,
    params: Query<HashMap<String, String>>,
) -> Response {
    feed_file(state, params, true).await
}

// ------------------------------------------------------------------ api

async fn api_health(State(state): State<Arc<AppState>>) -> Response {
    let health = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        move || state.health.health()
    });
    match health.await {
        Ok(Ok(health)) => Json(serde_json::json!({
            "targets": health.iter().map(|t| serde_json::json!({
                "target": t.target.as_str(),
                "kinds": t.kinds.iter().map(|k| serde_json::json!({
                    "kind": k.kind.as_str(),
                    "latest": k.latest.as_ref().map(|m| serde_json::json!({
                        "fetchedAt": m.fetched_at,
                        "sourceLastModified": m.source_last_modified,
                        "entryCount": m.entry_count,
                        "droppedCount": m.dropped_count,
                        "status": m.status.as_str(),
                    })),
                    "ageSeconds": k.age_seconds,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(Err(error)) => {
            log_failure("health", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
        Err(error) => {
            log_failure("health-join", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    }
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    20
}

fn item_json(item: &ItemRef) -> serde_json::Value {
    match item {
        ItemRef::Item { item_id } => serde_json::json!({ "type": "item", "itemID": item_id }),
        ItemRef::Pet { species, level, quality, breed } => serde_json::json!({
            "type": "pet",
            "speciesID": species,
            "level": level,
            "qualityID": quality,
            "breedID": breed,
        }),
    }
}

async fn api_search(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> Response {
    let hits = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let q = params.q.clone();
        let limit = params.limit;
        move || state.search.search(&q, limit)
    });
    match hits.await {
        Ok(Ok(hits)) => Json(serde_json::json!({
            "hits": hits.iter().map(|hit| serde_json::json!({
                "itemID": hit.item_id,
                "name": hit.name,
                "itemClass": hit.item_class,
                "quality": hit.quality,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(Err(error)) => {
            log_failure("search", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
        Err(error) => {
            log_failure("search-join", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    }
}

async fn api_opportunities(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match required_target(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let min_churn = params.get("min_churn").and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let limit = params.get("limit").and_then(|v| v.parse().ok()).unwrap_or(50);
    let list = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || state.opportunities.opportunities(&target, min_churn, limit)
    });
    match list.await {
        Ok(Ok(list)) => Json(serde_json::json!({
            "opportunities": list.iter().map(|o| serde_json::json!({
                "item": item_json(&o.item),
                "median7": o.median7,
                "floorNow": o.floor_now,
                "discount": o.discount,
                "reliability": o.reliability,
                "churnPerDay": o.churn_per_day,
                "stability": o.stability,
                "samples7": o.samples7,
                "confidence": o.confidence,
                "score": o.score,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(Err(error)) => {
            log_failure("opportunities", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
        Err(error) => {
            log_failure("opportunities-join", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    }
}

async fn api_overview(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match required_target(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let data = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || state.opportunities.overview(&target, 15)
    });
    match data.await {
        Ok(Ok(data)) => {
            let mover = |m: &MoverRow| {
                serde_json::json!({
                    "item": item_json(&m.item),
                    "medianNow": m.median_now,
                    "medianPrev": m.median_prev,
                    "change": m.change,
                    "churnPerDay": m.churn_per_day,
                })
            };
            Json(serde_json::json!({
                "moversUp": data.movers_up.iter().map(&mover).collect::<Vec<_>>(),
                "moversDown": data.movers_down.iter().map(&mover).collect::<Vec<_>>(),
                "mostActive": data.most_active.iter().map(&mover).collect::<Vec<_>>(),
            }))
            .into_response()
        }
        Ok(Err(error)) => {
            log_failure("overview", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
        Err(error) => {
            log_failure("overview-join", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    }
}

async fn api_item_metrics(
    State(state): State<Arc<AppState>>,
    Path((target_id, item_id)): Path<(String, i64)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match known_target(&state.config, &target_id) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let spans: Vec<Span> = params
        .get("span")
        .map_or_else(|| vec![Span::D1], |s| s.split(',').filter_map(Span::parse).collect());
    let item = ItemRef::Item { item_id };
    let computed = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || -> Result<Vec<(Span, Option<amtracker_core::domain::MetricBlock>)>, StoreError> {
            let mut out = Vec::with_capacity(spans.len());
            for span in spans {
                out.push((span, state.metrics.block(&target, &item, span)?));
            }
            Ok(out)
        }
    });
    let computed = match computed.await {
        Ok(Ok(computed)) => computed,
        Ok(Err(error)) => {
            log_failure("item-metrics", &error);
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
        Err(error) => {
            log_failure("item-metrics-join", &error);
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let mut out = serde_json::Map::new();
    for (span, block) in computed {
        if let Some(block) = block {
            out.insert(
                span.name().to_string(),
                serde_json::json!({
                    "n": block.n, "min": block.min, "p05": block.p05, "p25": block.p25,
                    "median": block.median, "p75": block.p75, "p95": block.p95, "max": block.max,
                    "mad": block.mad, "vwap": block.vwap, "qtySum": block.qty_sum,
                    "listingCount": block.listing_count, "saleRate": block.sale_rate,
                }),
            );
        }
    }
    Json(serde_json::Value::Object(out)).into_response()
}

async fn api_feed_json(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match target_param(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let options =
        ExportOptions { spans: state.config.metrics.spans.clone(), watchlist_only: false };
    // Feed assembly touches SQLite and lots of folds; keep it off the async
    // workers (render errors map into the same store-error shape as build).
    let payload = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || {
            state.feed_export.build_feed(&target, &options).and_then(|feed| {
                state
                    .renderer
                    .render(&feed, FeedFormat::Json)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            })
        }
    });
    match payload.await {
        Ok(Ok(bytes)) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Ok(Err(error)) => {
            log_failure("feed-json", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
        Err(error) => {
            log_failure("feed-json-join", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    }
}

#[derive(Deserialize)]
struct WatchlistPost {
    target: String,
    #[serde(flatten)]
    item: ApiItemRef,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", rename_all_fields = "camelCase")]
enum ApiItemRef {
    Item {
        #[serde(rename = "itemID")]
        item_id: i64,
    },
    Pet {
        #[serde(rename = "speciesID")]
        species: i64,
        level: i32,
        #[serde(rename = "qualityID")]
        quality: i32,
        #[serde(rename = "breedID")]
        breed: i32,
    },
}

impl ApiItemRef {
    fn to_ref(&self) -> ItemRef {
        match self {
            ApiItemRef::Item { item_id } => ItemRef::Item { item_id: *item_id },
            ApiItemRef::Pet { species, level, quality, breed } => {
                ItemRef::Pet { species: *species, level: *level, quality: *quality, breed: *breed }
            }
        }
    }
}

async fn api_watchlist_get(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(name) = params.get("target") else {
        return (StatusCode::BAD_REQUEST, "missing ?target=").into_response();
    };
    // Unknown ids are rejected before they reach the store, like every read
    // path (they would silently query an empty watchlist).
    let target = match known_target(&state.config, name) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let entries = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || state.watchlist.list(&target)
    });
    match entries.await {
        Ok(Ok(entries)) => Json(serde_json::json!({
            "items": entries.iter().map(|e| serde_json::json!({
                "item": item_json(&e.item),
                "note": e.note,
                "addedAt": e.added_at,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(Err(error)) => {
            log_failure("watchlist-get", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
        Err(error) => {
            log_failure("watchlist-get-join", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    }
}

/// The configured API token value; `None` = the mutating API is open (no
/// `token_env` configured, or the named env var is unset, non-unicode, or
/// EMPTY — the documented developer-mode behavior: an empty compose default
/// must not turn into a zero-length password).
fn expected_api_token(config: &TrackerConfig) -> Option<String> {
    let env = config.server.token_env.as_ref()?;
    std::env::var(env).ok().filter(|value| !value.trim().is_empty())
}

/// The bearer value presented by the request, if well-formed.
fn provided_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// Length-checked, constant-time-ish comparison: the XOR fold visits every
/// byte pair regardless of where a difference sits, so token bytes do not
/// leak through early-exit timing (length still does; tokens are not secret
/// from the operator).
fn tokens_match(provided: &str, expected: &str) -> bool {
    if provided.len() != expected.len() {
        return false;
    }
    let diff = provided.bytes().zip(expected.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b));
    diff == 0
}

/// The authorization verdict for one request given the expected token
/// (`None` = open mode) and the presented bearer value.
fn authorize_result(expected: Option<&str>, provided: Option<&str>) -> Result<(), Box<Response>> {
    let Some(expected) = expected else {
        return Ok(());
    };
    match provided {
        Some(token) if tokens_match(token, expected) => Ok(()),
        _ => Err(Box::new(unauthorized_response())),
    }
}

fn unauthorized_response() -> Response {
    let mut response =
        (StatusCode::UNAUTHORIZED, "invalid or missing bearer token\n").into_response();
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, header::HeaderValue::from_static("Bearer"));
    response
}

fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    authorize_result(expected_api_token(&state.config).as_deref(), provided_token(headers))
}

async fn api_watchlist_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<WatchlistPost>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    let target = match known_target(&state.config, &body.target) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    match state.watchlist.add(&target, body.item.to_ref(), body.note) {
        Ok(entry) => {
            Json(serde_json::json!({ "ok": true, "addedAt": entry.added_at })).into_response()
        }
        Err(StoreError::Conflict(message)) => (StatusCode::CONFLICT, message).into_response(),
        Err(error) => {
            log_failure("watchlist-add", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    }
}

/// DELETE /api/watchlist body: same identity shape as POST, so pets can be
/// removed too (the old path-parameter form only ever matched plain items).
#[derive(Deserialize)]
struct WatchlistDelete {
    target: String,
    #[serde(flatten)]
    item: ApiItemRef,
}

async fn api_watchlist_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<WatchlistDelete>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    let target = match known_target(&state.config, &body.target) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    match state.watchlist.remove(&target, &body.item.to_ref()) {
        Ok(removed) => Json(serde_json::json!({ "ok": true, "removed": removed })).into_response(),
        Err(error) => {
            log_failure("watchlist-remove", &error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    }
}

#[derive(Deserialize)]
struct FetchParams {
    #[serde(default)]
    target: Option<String>,
}

async fn api_fetch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<FetchParams>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    // An unknown target would silently no-op through the filter below;
    // validate it like every other target-bearing endpoint.
    let wanted = match params.target.as_deref() {
        Some(name) => match known_target(&state.config, name) {
            Ok(target) => Some(target),
            Err(response) => return *response,
        },
        None => None,
    };
    // A cycle can block on the wire for minutes; keep it off the async
    // workers. The cycle lock serializes against the scheduler's cycles so
    // observe/close_missing sequences never interleave, and a successful
    // fetch republishes the target's feed right here (the scheduler only
    // does that on its own cadence — without this, `/api/fetch` never
    // refreshed what `/feed.lua` serves).
    let reports = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        move || {
            let _guard = state.cycle_lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut reports = Vec::new();
            for target in &state.config.targets {
                if let Some(wanted) = &wanted {
                    if target.id != *wanted {
                        continue;
                    }
                }
                let mut fetched = false;
                for kind in [PayloadKind::Commodities, PayloadKind::Auctions] {
                    match state.cycle.run_cycle(target, kind) {
                        Ok(report) => {
                            fetched = true;
                            reports.push(serde_json::json!({
                                "target": report.target.as_str(),
                                "kind": report.kind.as_str(),
                                "outcome": format!("{:?}", report.outcome),
                                "entries": report.entries,
                                "dropped": report.dropped,
                                "saleCredits": report.sale_credits,
                            }));
                        }
                        Err(error) => reports.push(serde_json::json!({
                            "target": target.id.as_str(),
                            "kind": kind.as_str(),
                            "error": error.to_string(),
                        })),
                    }
                }
                if fetched {
                    // Same operational side effects the scheduler runs after
                    // its own successful cycles (retention + feed publish).
                    crate::scheduler::after_cycle_now(&state, target.id.as_str());
                }
            }
            reports
        }
    });
    let reports = match reports.await {
        Ok(reports) => reports,
        Err(error) => {
            log_failure("fetch", &error);
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    Json(serde_json::json!({ "reports": reports })).into_response()
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect/panic: fixture-driven, deterministic
    // inputs (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// Minimal-but-valid tracker config for authorization/target tests.
    fn test_config(allow_open: bool) -> TrackerConfig {
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
                archive_dir: "raw".into(),
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
            targets: vec![amtracker_core::config::TargetConfig {
                id: TargetId("us-4".into()),
                kind: amtracker_core::config::TargetKind::BlizzardConnectedRealm {
                    region: "us".into(),
                    connected_realm_id: 4,
                },
                auth: None,
                poll: PollConfig { commodities_secs: 60, auctions_secs: 60 },
            }],
        }
    }

    /// authorize matrix: unset/empty token (open mode), valid, and wrong
    /// token, crossed with the open-mode expectation.
    #[test]
    fn authorize_matrix_open_mode_allows_anything() {
        // No configured token: open, with or without a presented bearer.
        assert!(authorize_result(None, None).is_ok());
        assert!(authorize_result(None, Some("anything")).is_ok());
        // Empty/blank configured values normalize to open (the env filter
        // mirrors `expected_api_token`).
        assert!(authorize_result(None, Some("")).is_ok());
    }

    #[test]
    fn authorize_matrix_protected_mode_rejects_wrong_or_missing_tokens() {
        let expected = Some("s3cret");
        assert!(authorize_result(expected, Some("s3cret")).is_ok());
        for provided in [None, Some(""), Some("wrong"), Some("s3cret "), Some("s3cretx")] {
            let response = authorize_result(expected, provided).expect_err("must reject");
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
        assert!(tokens_match("abc", "abc"));
        assert!(!tokens_match("abd", "abc"));
        assert!(!tokens_match("abcd", "abc"));
        assert!(!tokens_match("", "abc"));
        assert!(tokens_match("", ""));
    }
}

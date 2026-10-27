//! JSON API surface, feed downloads, and the token-gated mutation endpoints
//! (watchlist add/remove, manual fetch). Page handlers live in [`pages`].

#![allow(clippy::unused_async)]

use std::{
    collections::HashMap,
    sync::Arc,
    time::Instant,
};

use amtracker_core::{
    domain::{
        Category,
        FeedFormat,
        PayloadKind,
        Span,
    },
    error::StoreError,
    services::{
        cycle::CycleOutcome,
        feed_export::ExportOptions,
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
        IntoResponse,
        Response,
    },
    Json,
};
use serde::Deserialize;

use super::{
    category_option,
    expected_api_token,
    known_target,
    log_failure,
    required_target,
    target_param,
    AppState,
};

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
pub(crate) fn tokens_match(provided: &str, expected: &str) -> bool {
    if provided.len() != expected.len() {
        return false;
    }
    let diff = provided.bytes().zip(expected.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b));
    diff == 0
}

/// The authorization verdict for one request given the expected token
/// (`None` = open mode) and the presented bearer value.
pub(crate) fn authorize_result(
    expected: Option<&str>,
    provided: Option<&str>,
) -> Result<(), Box<Response>> {
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

/// Logs the failure at full fidelity and returns a generic 500 body:
/// internals (SQL text, paths) stay in the server log, never on the wire.
/// Any error flavor funnels through here — store failures and
/// blocking-task join failures alike.
fn internal_error(scope: &'static str, error: &impl std::fmt::Display) -> Response {
    log_failure(scope, error);
    (StatusCode::INTERNAL_SERVER_ERROR, "internal server error\n").into_response()
}

// ------------------------------------------------------------------- feeds

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
        Err(error) => {
            return super::render::store_error_page(StoreError::Backend(error.to_string())).await
        }
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
        Err(error) => super::render::store_error_page(error).await,
    }
}

/// `GET /feed.lua?target=` — the cached full Lua feed.
pub(crate) async fn feed_lua(
    state: State<Arc<AppState>>,
    params: Query<HashMap<String, String>>,
) -> Response {
    feed_file(state, params, false).await
}

/// `GET /feed-slim.lua?target=` — watchlist-only feed, built on demand.
pub(crate) async fn feed_slim_lua(
    state: State<Arc<AppState>>,
    params: Query<HashMap<String, String>>,
) -> Response {
    feed_file(state, params, true).await
}

// -------------------------------------------------------------------- api

/// `GET /api/health` — per-target snapshot freshness for scripts/monitors.
pub(crate) async fn api_health(State(state): State<Arc<AppState>>) -> Response {
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
        Ok(Err(error)) => internal_error("health", &error),
        Err(error) => internal_error("health-join", &error),
    }
}

#[derive(Deserialize)]
pub(crate) struct SearchParams {
    q: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

fn default_limit() -> usize {
    20
}

/// JSON shape for one item reference (API contract, camelCase on the wire).
fn item_json(item: &amtracker_core::domain::ItemRef) -> serde_json::Value {
    match item {
        amtracker_core::domain::ItemRef::Item { item_id } => {
            serde_json::json!({ "type": "item", "itemID": item_id })
        }
        amtracker_core::domain::ItemRef::Pet { species, level, quality, breed } => {
            serde_json::json!({
                "type": "pet",
                "speciesID": species,
                "level": level,
                "qualityID": quality,
                "breedID": breed,
            })
        }
    }
}

/// `GET /api/search?q=&limit=&offset=` — kind-aware catalog search
/// (completions and browse pages; page with `offset` until `hasMore` is
/// false).
pub(crate) async fn api_search(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> Response {
    let limit = params.limit.clamp(1, 200);
    let offset = params.offset;
    let hits = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let q = params.q.clone();
        move || state.search.search(&q, limit, offset)
    });
    match hits.await {
        Ok(Ok(hits)) => {
            let has_more = hits.len() == limit;
            Json(serde_json::json!({
                "offset": offset,
                "limit": limit,
                "hasMore": has_more,
                "hits": hits.iter().map(|hit| match hit {
                    amtracker_core::domain::SearchHit::Item(record) => serde_json::json!({
                        "type": "item",
                        "itemID": record.item_id,
                        "name": record.name,
                        "itemClass": record.item_class,
                        "quality": record.quality,
                    }),
                    amtracker_core::domain::SearchHit::Pet(record) => serde_json::json!({
                        "type": "pet",
                        "speciesID": record.species_id,
                        "name": record.name,
                    }),
                })
                .collect::<Vec<_>>(),
            }))
            .into_response()
        }
        Ok(Err(error)) => internal_error("search", &error),
        Err(error) => internal_error("search-join", &error),
    }
}

/// `GET /api` — machine-readable directory: every endpoint with its summary,
/// plus links to the `OpenAPI` document and the rendered docs page. An agent
/// (or curl) can discover the whole JSON API + feed surface from here.
pub(crate) async fn api_index() -> Response {
    let spec = super::openapi::spec();
    let mut endpoints = Vec::new();
    if let Some(paths) = spec["paths"].as_object() {
        for (path, operations) in paths {
            // Iterate the operations themselves (not a fixed method list) so
            // a future put/patch cannot be silently dropped; the JSON map is
            // alphabetically ordered, so the sweep is deterministic.
            let mut methods = Vec::new();
            if let Some(ops) = operations.as_object() {
                for (method, op) in ops {
                    methods.push(serde_json::json!({
                        "method": method.to_uppercase(),
                        "summary": op["summary"].as_str().unwrap_or_default(),
                    }));
                }
            }
            endpoints.push(serde_json::json!({ "path": path, "operations": methods }));
        }
    }
    Json(serde_json::json!({
        "service": "auctimogger-tracker",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Local market-memory API over WoW auction data. Prices are integers in copper (10,000 copper = 1 gold); day values are UTC day numbers.",
        "openapi": "/api/openapi.json",
        "docs": "/api/docs",
        "feeds": ["/feed.lua", "/feed-slim.lua", "/api/feed.json"],
        "endpoints": endpoints,
    }))
    .into_response()
}

/// `GET /api/openapi.json` — the `OpenAPI` 3.1 document describing the whole
/// JSON API + feed surface; the same value the docs page renders. The spec is
/// an immutable `&'static Value`, so it is serialized in place (no clone).
pub(crate) async fn api_openapi() -> Response {
    Json(super::openapi::spec()).into_response()
}

/// `GET /api/history?target=&category=&days=` — bulk per-item-per-day
/// aggregates for every item/pet with fold data in the window: the
/// analysis-grade payload for scripts (one call covers the whole market;
/// every price field is integer copper, days are UTC day numbers). Without
/// `category`, rows of all three categories are concatenated (each row names
/// its `category`); with it, only that category's rows.
pub(crate) async fn api_history(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match required_target(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let category = match category_option(&params) {
        Ok(category) => category,
        Err(response) => return *response,
    };
    let days = params.get("days").and_then(|v| v.parse::<i64>().ok()).map_or(7, |v| v.clamp(1, 30));
    let computed = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || -> Result<(i64, Vec<amtracker_core::domain::ItemDayRow>), StoreError> {
            let today = amtracker_core::domain::utc_day(state.clock.now());
            let since_day = today.saturating_sub(days - 1);
            let rows = match category {
                Some(category) => state.history.item_day_rows(&target, since_day, category)?,
                None => Category::ALL
                    .iter()
                    .map(|category| state.history.item_day_rows(&target, since_day, *category))
                    .collect::<Result<Vec<Vec<_>>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect(),
            };
            Ok((since_day, rows))
        }
    });
    match computed.await {
        Ok(Ok((since_day, rows))) => Json(serde_json::json!({
            "target": target.as_str(),
            "category": category.map_or_else(|| "all".to_string(), |c| c.as_str().to_string()),
            "days": days,
            "sinceDay": since_day,
            "rows": rows.iter().map(|row| serde_json::json!({
                "item": item_json(&row.item),
                "category": row.category.as_str(),
                "day": row.day,
                "listingCount": row.listing_count,
                "qtySum": row.qty_sum,
                "min": row.min,
                "p05": row.p05,
                "median": row.median,
                "mad": row.mad,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(Err(error)) => internal_error("history", &error),
        Err(error) => internal_error("history-join", &error),
    }
}

/// `GET /api/opportunities?target=&category=&min_churn=&limit=&sort=` —
/// without `category`, one response keyed by all three categories; with it,
/// the bare single-category shape. Every row names its category. `sort` is
/// `score` (default) or `last_sale` (stalest first).
pub(crate) async fn api_opportunities(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match required_target(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let category = match category_option(&params) {
        Ok(category) => category,
        Err(response) => return *response,
    };
    let min_churn = params
        .get("min_churn")
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(1.0);
    let limit = params.get("limit").and_then(|v| v.parse().ok()).unwrap_or(50);
    let sort = match params.get("sort").map_or("score", String::as_str) {
        "score" => amtracker_core::services::opportunity::OpportunitySort::Score,
        "last_sale" => amtracker_core::services::opportunity::OpportunitySort::LastSale,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown sort '{other}' (expected score|last_sale)"),
            )
                .into_response();
        }
    };
    let list = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || -> Result<Vec<(Category, Vec<_>)>, StoreError> {
            let categories: Vec<Category> = match category {
                Some(category) => vec![category],
                None => Category::ALL.to_vec(),
            };
            categories
                .iter()
                .map(|category| {
                    state
                        .opportunities
                        .opportunities(&target, *category, min_churn, limit, sort)
                        .map(|rows| (*category, rows))
                })
                .collect()
        }
    });
    match list.await {
        Ok(Ok(list)) => {
            let ranked = |rows: &[amtracker_core::services::opportunity::Opportunity]| {
                rows.iter()
                    .map(|o| {
                        serde_json::json!({
                            "item": item_json(&o.item),
                            "category": o.category.as_str(),
                            "median7": o.median7,
                            "floorNow": o.floor_now,
                            "discount": o.discount,
                            "reliability": o.reliability,
                            "churnPerDay": o.churn_per_day,
                            "stability": o.stability,
                            "samples7": o.samples7,
                            "confidence": o.confidence,
                            "score": o.score,
                            "daysSinceLastSale": o.days_since_last_sale,
                            "closuresSinceLastSale": o.closures_since_last_sale,
                        })
                    })
                    .collect::<Vec<_>>()
            };
            match category {
                Some(category) => Json(serde_json::json!({
                    "category": category.as_str(),
                    "opportunities": list.into_iter().next().map(|(_, rows)| ranked(&rows)).unwrap_or_default(),
                }))
                .into_response(),
                None => Json(serde_json::json!(
                    list.into_iter()
                        .map(|(category, rows)| (category.as_str().to_string(), serde_json::json!(ranked(&rows))))
                        .collect::<serde_json::Map<String, serde_json::Value>>()
                ))
                .into_response(),
            }
        }
        Ok(Err(error)) => internal_error("opportunities", &error),
        Err(error) => internal_error("opportunities-join", &error),
    }
}

/// `GET /api/overview?target=&category=&limit=` — without `category`, one
/// response keyed by all three categories; with it, the bare shape.
pub(crate) async fn api_overview(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match required_target(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let category = match category_option(&params) {
        Ok(category) => category,
        Err(response) => return *response,
    };
    let limit =
        params.get("limit").and_then(|v| v.parse::<usize>().ok()).map_or(15, |v| v.clamp(1, 50));
    let data = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || -> Result<Vec<(Category, _)>, StoreError> {
            let categories: Vec<Category> = match category {
                Some(category) => vec![category],
                None => Category::ALL.to_vec(),
            };
            categories
                .iter()
                .map(|category| {
                    state
                        .opportunities
                        .overview(&target, *category, limit)
                        .map(|data| (*category, data))
                })
                .collect()
        }
    });
    match data.await {
        Ok(Ok(list)) => {
            let mover = |m: &amtracker_core::services::opportunity::MoverRow| {
                serde_json::json!({
                    "item": item_json(&m.item),
                    "category": m.category.as_str(),
                    "medianNow": m.median_now,
                    "medianPrev": m.median_prev,
                    "change": m.change,
                    "churnPerDay": m.churn_per_day,
                })
            };
            let overview_json = |data: &amtracker_core::services::opportunity::OverviewData| {
                serde_json::json!({
                    "moversUp": data.movers_up.iter().map(&mover).collect::<Vec<_>>(),
                    "moversDown": data.movers_down.iter().map(&mover).collect::<Vec<_>>(),
                    "mostActive": data.most_active.iter().map(&mover).collect::<Vec<_>>(),
                })
            };
            match category {
                Some(category) => {
                    let data = list.into_iter().next().map(|(_, data)| data);
                    let mut base = match data {
                        Some(data) => overview_json(&data),
                        None => serde_json::json!({}),
                    };
                    if let Some(object) = base.as_object_mut() {
                        object.insert("category".into(), serde_json::json!(category.as_str()));
                    }
                    Json(base).into_response()
                }
                None => Json(serde_json::json!(list
                    .iter()
                    .map(|(category, data)| (category.as_str().to_string(), overview_json(data)))
                    .collect::<serde_json::Map<String, serde_json::Value>>()))
                .into_response(),
            }
        }
        Ok(Err(error)) => internal_error("overview", &error),
        Err(error) => internal_error("overview-join", &error),
    }
}

/// JSON shape for one metric block (camelCase on the wire).
fn metric_block_json(block: &amtracker_core::domain::MetricBlock) -> serde_json::Value {
    serde_json::json!({
        "n": block.n, "min": block.min, "p05": block.p05, "p25": block.p25,
        "median": block.median, "p75": block.p75, "p95": block.p95, "max": block.max,
        "mad": block.mad, "vwap": block.vwap, "qtySum": block.qty_sum,
        "listingCount": block.listing_count, "saleRate": block.sale_rate,
        "saleRate180d": block.sale_rate_180d,
        "daysSinceLastSale": block.days_since_last_sale,
        "closuresSinceLastSale": block.closures_since_last_sale,
    })
}

/// Parses the `?span=` list (comma-separated); `default` when absent.
/// Unknown span names are rejected with a 400, matching the API's own
/// `category`/`kind` conventions — silently dropping a typo would read as
/// empty metrics, which is indistinguishable from "no data".
fn spans_param(
    params: &HashMap<String, String>,
    default: Span,
) -> Result<Vec<Span>, Box<Response>> {
    match params.get("span") {
        None => Ok(vec![default]),
        Some(raw) => {
            let mut spans = Vec::new();
            for token in raw.split(',') {
                let Some(span) = Span::parse(token) else {
                    return Err(Box::new(
                        (
                            StatusCode::BAD_REQUEST,
                            format!("unknown span '{token}' (expected d1, d7, d14 or d30)"),
                        )
                            .into_response(),
                    ));
                };
                spans.push(span);
            }
            Ok(spans)
        }
    }
}

/// Blocking read of one item identity's category's blocks across `spans`;
/// the shared body of the item and pet metrics handlers.
type SpanBlocks = Vec<(Span, Option<amtracker_core::domain::MetricBlock>)>;
fn fetch_span_blocks(
    state: &Arc<AppState>,
    target: &amtracker_core::domain::TargetId,
    item: &amtracker_core::domain::ItemRef,
    category: Category,
    spans: Vec<Span>,
) -> Result<SpanBlocks, StoreError> {
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        out.push((span, state.metrics.block(target, item, category, span)?));
    }
    Ok(out)
}

/// `GET /api/items/{target_id}/{item_id}/metrics?category=&span=d7,d30` —
/// the same blocks the item page shows, JSON-shaped. Without `category`, one
/// object keyed by category (each holding the span map); with it, the bare
/// span map.
pub(crate) async fn api_item_metrics(
    State(state): State<Arc<AppState>>,
    Path((target_id, item_id)): Path<(String, i64)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match known_target(&state.config, &target_id) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let category = match category_option(&params) {
        Ok(category) => category,
        Err(response) => return *response,
    };
    let spans = match spans_param(&params, Span::D1) {
        Ok(spans) => spans,
        Err(response) => return *response,
    };
    let item = amtracker_core::domain::ItemRef::Item { item_id };
    let computed = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || -> Result<serde_json::Value, StoreError> {
            let categories: Vec<Category> = match category {
                Some(category) => vec![category],
                None => Category::ALL.to_vec(),
            };
            let mut out = serde_json::Map::new();
            for category in &categories {
                let mut spans_json = serde_json::Map::new();
                for (span, block) in
                    fetch_span_blocks(&state, &target, &item, *category, spans.clone())?
                {
                    if let Some(block) = block {
                        spans_json.insert(span.name().to_string(), metric_block_json(&block));
                    }
                }
                if categories.len() == 1 {
                    return Ok(serde_json::Value::Object(spans_json));
                }
                out.insert(category.as_str().to_string(), serde_json::Value::Object(spans_json));
            }
            Ok(serde_json::Value::Object(out))
        }
    });
    match computed.await {
        Ok(Ok(json)) => Json(json).into_response(),
        Ok(Err(error)) => internal_error("item-metrics", &error),
        Err(error) => internal_error("item-metrics-join", &error),
    }
}

/// `GET /api/items/{target_id}/pet/{species_id}/metrics?level=&quality=&breed=&span=…`
/// — the pet-variant twin of [`api_item_metrics`]: the identity is the full
/// species/level/quality/breed tuple, matching the HTML pet links and the
/// watchlist item shape.
pub(crate) async fn api_item_pet_metrics(
    State(state): State<Arc<AppState>>,
    Path((target_id, species_id)): Path<(String, i64)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match known_target(&state.config, &target_id) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let item = match pet_ref_from_params(&params, species_id) {
        Ok(item) => item,
        Err(response) => return *response,
    };
    let spans = match spans_param(&params, Span::D1) {
        Ok(spans) => spans,
        Err(response) => return *response,
    };
    // Pets only ever trade in the item AH: the pet identity's category is
    // always `pet` (the commodity payload never carries pets).
    let computed = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || fetch_span_blocks(&state, &target, &item, Category::Pet, spans)
    });
    blocks_response(computed.await)
}

/// Builds the pet [`ItemRef`] from the required `level`, `quality` and
/// `breed` query parameters (all three, or the identity is ambiguous).
fn pet_ref_from_params(
    params: &HashMap<String, String>,
    species_id: i64,
) -> Result<amtracker_core::domain::ItemRef, Box<Response>> {
    let missing = ["level", "quality", "breed"]
        .into_iter()
        .filter(|key| params.get(*key).is_none_or(|v| v.parse::<i32>().is_err()))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(Box::new(
            (
                StatusCode::BAD_REQUEST,
                format!("missing or non-numeric query parameter(s): {}", missing.join(", ")),
            )
                .into_response(),
        ));
    }
    // Unwrap-free parsing: the `missing` filter proved every value parses.
    let parse =
        |key: &str| -> i32 { params.get(key).and_then(|v| v.parse().ok()).unwrap_or_default() };
    Ok(amtracker_core::domain::ItemRef::Pet {
        species: species_id,
        level: parse("level"),
        quality: parse("quality"),
        breed: parse("breed"),
    })
}

/// Shared response assembly for the metrics handlers: span-keyed JSON object
/// holding each block; spans without data are omitted (matching the item
/// page, which shows "no data").
fn blocks_response(
    computed: Result<Result<SpanBlocks, StoreError>, tokio::task::JoinError>,
) -> Response {
    match computed {
        Ok(Ok(computed)) => {
            let mut out = serde_json::Map::new();
            for (span, block) in computed {
                if let Some(block) = block {
                    out.insert(span.name().to_string(), metric_block_json(&block));
                }
            }
            Json(serde_json::Value::Object(out)).into_response()
        }
        Ok(Err(error)) => internal_error("item-metrics", &error),
        Err(error) => internal_error("item-metrics-join", &error),
    }
}

/// `GET /api/feed.json?target=` — the full feed, JSON rendering.
pub(crate) async fn api_feed_json(
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
        Ok(Err(error)) => internal_error("feed-json", &error),
        Err(error) => internal_error("feed-json-join", &error),
    }
}

/// `GET /api/snapshots?target=&kind=&limit=` — the snapshot browser's rows,
/// JSON-shaped: newest first per payload kind, filtered to one kind when
/// `kind=commodities|auctions` is given.
pub(crate) async fn api_snapshots(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match required_target(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let kinds: Vec<PayloadKind> = match params.get("kind") {
        Some(raw) => match PayloadKind::parse(raw) {
            Some(kind) => vec![kind],
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("unknown kind '{raw}' (expected commodities|auctions)"),
                )
                    .into_response();
            }
        },
        None => vec![PayloadKind::Commodities, PayloadKind::Auctions],
    };
    let limit =
        params.get("limit").and_then(|v| v.parse::<usize>().ok()).map_or(50, |v| v.clamp(1, 500));
    let rows = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || -> Result<Vec<amtracker_core::domain::SnapshotMeta>, StoreError> {
            let mut out = Vec::new();
            for kind in kinds {
                out.extend(state.snapshots.list_recent(&target, kind, limit)?);
            }
            Ok(out)
        }
    });
    match rows.await {
        Ok(Ok(rows)) => Json(serde_json::json!({
            "target": target.as_str(),
            "snapshots": rows.iter().map(|meta| serde_json::json!({
                "id": meta.id,
                "target": meta.target.as_str(),
                "kind": meta.kind.as_str(),
                "fetchedAt": meta.fetched_at,
                "sourceLastModified": meta.source_last_modified,
                "entryCount": meta.entry_count,
                "droppedCount": meta.dropped_count,
                "droppedReasons": meta.dropped_reasons,
                "status": meta.status.as_str(),
            }))
            .collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(Err(error)) => internal_error("snapshots", &error),
        Err(error) => internal_error("snapshots-join", &error),
    }
}

/// `GET /api/watchlist/metrics?target=&category=&span=&sale_days=&trend_days=`
/// — every watchlist entry joined to its current market numbers in one
/// batched read: metric blocks per requested span, trailing sale rate and
/// per-day trend rows. Without `category`, one response keyed by all three
/// categories; with it, the bare entry-list shape.
pub(crate) async fn api_watchlist_metrics(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match required_target(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let category = match category_option(&params) {
        Ok(category) => category,
        Err(response) => return *response,
    };
    let spans = match spans_param(&params, Span::D7) {
        Ok(spans) => spans,
        Err(response) => return *response,
    };
    let sale_days = days_param(&params, "sale_days", 14);
    let trend_days = days_param(&params, "trend_days", 30);
    let report = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        let spans = spans.clone();
        move || -> Result<Vec<(Category, Vec<_>)>, StoreError> {
            let categories: Vec<Category> = match category {
                Some(category) => vec![category],
                None => Category::ALL.to_vec(),
            };
            categories
                .iter()
                .map(|category| {
                    state
                        .watchlist_report
                        .report(&target, *category, &spans, sale_days, trend_days)
                        .map(|entries| (*category, entries))
                })
                .collect()
        }
    });
    let span_names: Vec<&'static str> = spans.iter().map(Span::name).collect();
    let today = amtracker_core::domain::utc_day(state.clock.now());
    match report.await {
        Ok(Ok(list)) => {
            let entries_json =
                |entries: &[amtracker_core::services::watchlist_report::WatchlistReportEntry]| {
                    entries.iter().map(|entry| serde_json::json!({
                    "item": item_json(&entry.entry.item),
                    "note": entry.entry.note,
                    "addedAt": entry.entry.added_at,
                    "metrics": entry
                        .blocks
                        .iter()
                        .zip(spans.iter())
                        .filter_map(|(block, span)| block.as_ref().map(|b| (span.name(), b)))
                        .map(|(name, block)| (name.to_string(), metric_block_json(block)))
                        .collect::<serde_json::Map<String, serde_json::Value>>(),
                    "saleRate": entry.sale_rate,
                    "daysSinceLastSale": entry
                        .recency
                        .as_ref()
                        .map(|recency| today - recency.last_sale_day),
                    "closuresSinceLastSale": entry
                        .recency
                        .as_ref()
                        .map(|recency| recency.closures_since),
                    "trend": entry.trend.iter().map(|row| serde_json::json!({
                        "day": row.day,
                        "listingCount": row.listing_count,
                        "qtySum": row.qty_sum,
                        "min": row.min,
                        "median": row.median,
                        "mad": row.mad,
                    }))
                    .collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>()
                };
            match category {
                Some(category) => Json(serde_json::json!({
                    "target": target.as_str(),
                    "category": category.as_str(),
                    "spans": span_names,
                    "saleDays": sale_days,
                    "trendDays": trend_days,
                    "entries": list.into_iter().next().map(|(_, entries)| entries_json(&entries)).unwrap_or_default(),
                }))
                .into_response(),
                None => Json(serde_json::json!(
                    list.iter()
                        .map(|(category, entries)| (
                            category.as_str().to_string(),
                            serde_json::json!(entries_json(entries))
                        ))
                        .collect::<serde_json::Map<String, serde_json::Value>>()
                ))
                .into_response(),
            }
        }
        Ok(Err(error)) => internal_error("watchlist-metrics", &error),
        Err(error) => internal_error("watchlist-metrics-join", &error),
    }
}

/// Parses an optional day-window query parameter, clamped to 1..=90.
fn days_param(params: &HashMap<String, String>, key: &str, default: i64) -> i64 {
    params.get(key).and_then(|v| v.parse::<i64>().ok()).map_or(default, |v| v.clamp(1, 90))
}

#[derive(Deserialize)]
pub(crate) struct WatchlistPost {
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
    fn to_ref(&self) -> amtracker_core::domain::ItemRef {
        match self {
            ApiItemRef::Item { item_id } => {
                amtracker_core::domain::ItemRef::Item { item_id: *item_id }
            }
            ApiItemRef::Pet { species, level, quality, breed } => {
                amtracker_core::domain::ItemRef::Pet {
                    species: *species,
                    level: *level,
                    quality: *quality,
                    breed: *breed,
                }
            }
        }
    }

    /// Rejects pet identity fields outside their in-game domains before they
    /// become unusable watchlist rows that can never match a fold. Ranges
    /// follow the domain's own documentation: species ids are positive,
    /// levels run 1..=25, quality 0 (poor) ..= 6 (artifact), breeds 3..=11.
    /// Deletion paths stay permissive — anything already stored can be
    /// removed.
    fn validate_for_add(&self) -> Result<(), &'static str> {
        match self {
            ApiItemRef::Item { .. } => Ok(()),
            ApiItemRef::Pet { species, level, quality, breed } => {
                if *species <= 0 {
                    Err("pet speciesID must be positive")
                } else if !(1..=25).contains(level) {
                    Err("pet level must be within 1..=25")
                } else if !(0..=6).contains(quality) {
                    Err("pet qualityID must be within 0..=6")
                } else if !(3..=11).contains(breed) {
                    Err("pet breedID must be within 3..=11")
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// `GET /api/watchlist?target=`
pub(crate) async fn api_watchlist_get(
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
        Ok(Err(error)) => internal_error("watchlist-get", &error),
        Err(error) => internal_error("watchlist-get-join", &error),
    }
}

/// `POST /api/watchlist` — add one item/pet (bearer-gated when configured).
pub(crate) async fn api_watchlist_post(
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
    if let Err(message) = body.item.validate_for_add() {
        return (StatusCode::BAD_REQUEST, message).into_response();
    }
    // SQLite access stays off the async workers, like every other store path:
    // the single connection mutex can be held for a while by ingest
    // transactions, and a blocked worker stalls the whole runtime.
    let written = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let item = body.item.to_ref();
        move || state.watchlist.add(&target, item, body.note)
    });
    match written.await {
        Ok(Ok(entry)) => {
            Json(serde_json::json!({ "ok": true, "addedAt": entry.added_at })).into_response()
        }
        Ok(Err(StoreError::Conflict(message))) => (StatusCode::CONFLICT, message).into_response(),
        Ok(Err(error)) => internal_error("watchlist-add", &error),
        Err(error) => internal_error("watchlist-add-join", &error),
    }
}

/// DELETE /api/watchlist body: same identity shape as POST, so pets can be
/// removed too (the old path-parameter form only ever matched plain items).
#[derive(Deserialize)]
pub(crate) struct WatchlistDelete {
    target: String,
    #[serde(flatten)]
    item: ApiItemRef,
}

/// `DELETE /api/watchlist` — remove one item/pet (bearer-gated).
pub(crate) async fn api_watchlist_delete(
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
    // Same spawn_blocking discipline as the POST path above.
    let removed = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let item = body.item.to_ref();
        move || state.watchlist.remove(&target, &item)
    });
    match removed.await {
        Ok(Ok(removed)) => {
            Json(serde_json::json!({ "ok": true, "removed": removed })).into_response()
        }
        Ok(Err(error)) => internal_error("watchlist-remove", &error),
        Err(error) => internal_error("watchlist-remove-join", &error),
    }
}

// ------------------------------------------------------------------- alerts

/// JSON shape of one alert condition (the tagged union of rule triggers).
#[derive(Deserialize)]
pub(crate) struct AlertConditionPost {
    /// `price-below` | `below-floor` | `sale-rate-above` |
    /// `days-since-last-sale-above`.
    #[serde(rename = "type")]
    kind: String,
    copper: Option<i64>,
    span: Option<String>,
    percentile: Option<String>,
    rate: Option<f64>,
    days: Option<i64>,
}

impl AlertConditionPost {
    /// Maps the wire union into the domain enum, rejecting incomplete or
    /// out-of-range payloads with a human message.
    fn to_condition(&self) -> Result<amtracker_core::domain::AlertCondition, String> {
        use amtracker_core::domain::AlertCondition;
        match self.kind.as_str() {
            "price-below" => {
                let copper = self.copper.ok_or("price-below requires copper")?;
                if copper <= 0 {
                    return Err("copper must be positive".into());
                }
                Ok(AlertCondition::PriceBelow { copper })
            }
            "below-floor" => {
                let span = self
                    .span
                    .as_deref()
                    .and_then(Span::parse)
                    .ok_or("below-floor requires span d1|d7|d14|d30")?;
                let percentile = self
                    .percentile
                    .as_deref()
                    .and_then(amtracker_core::domain::FloorPercentile::parse)
                    .ok_or("below-floor requires percentile p05|p25")?;
                Ok(AlertCondition::BelowFloor { span, percentile })
            }
            "sale-rate-above" => {
                let rate = self.rate.ok_or("sale-rate-above requires rate")?;
                if !(0.0..=1.0).contains(&rate) {
                    return Err("rate must be within 0.0..=1.0".into());
                }
                Ok(AlertCondition::SaleRateAbove { rate })
            }
            "days-since-last-sale-above" => {
                let days = self.days.ok_or("days-since-last-sale-above requires days")?;
                if days < 1 {
                    return Err("days must be at least 1".into());
                }
                Ok(AlertCondition::DaysSinceLastSaleAbove { days })
            }
            other => Err(format!(
                "unknown condition type '{other}' (expected \
                 price-below|below-floor|sale-rate-above|days-since-last-sale-above)"
            )),
        }
    }
}

/// JSON shape of one condition, for rule/event responses.
fn condition_json(condition: &amtracker_core::domain::AlertCondition) -> serde_json::Value {
    use amtracker_core::domain::AlertCondition;
    match *condition {
        AlertCondition::PriceBelow { copper } => {
            serde_json::json!({ "type": "price-below", "copper": copper })
        }
        AlertCondition::BelowFloor { span, percentile } => serde_json::json!({
            "type": "below-floor",
            "span": span.name(),
            "percentile": percentile.as_str(),
        }),
        AlertCondition::SaleRateAbove { rate } => {
            serde_json::json!({ "type": "sale-rate-above", "rate": rate })
        }
        AlertCondition::DaysSinceLastSaleAbove { days } => {
            serde_json::json!({ "type": "days-since-last-sale-above", "days": days })
        }
    }
}

/// JSON shape of one stored rule.
fn alert_rule_json(rule: &amtracker_core::domain::AlertRule) -> serde_json::Value {
    serde_json::json!({
        "ruleID": rule.id,
        "target": rule.target.as_str(),
        "item": item_json(&rule.item),
        "condition": condition_json(&rule.condition),
        "note": rule.note,
        "createdAt": rule.created_at,
        "lastFiredAt": rule.last_fired_at,
    })
}

/// JSON shape of one fired event.
fn alert_event_json(event: &amtracker_core::domain::AlertEvent) -> serde_json::Value {
    serde_json::json!({
        "eventID": event.id,
        "ruleID": event.rule_id,
        "target": event.target.as_str(),
        "item": item_json(&event.item),
        "condition": condition_json(&event.condition),
        "observed": event.observed,
        "message": event.message,
        "firedAt": event.fired_at,
    })
}

/// `GET /api/alerts?target=&events=` — the target's rules plus (default 50)
/// newest fired events; the alerts page's data source.
pub(crate) async fn api_alerts_get(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match required_target(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let event_limit =
        params.get("events").and_then(|v| v.parse::<usize>().ok()).map_or(50, |v| v.clamp(1, 500));
    let loaded = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || -> Result<_, StoreError> {
            let rules = state.alerts.list_rules(&target)?;
            let events = state.alerts.recent_events(&target, event_limit)?;
            Ok((rules, events))
        }
    });
    match loaded.await {
        Ok(Ok((rules, events))) => Json(serde_json::json!({
            "target": target.as_str(),
            "rules": rules.iter().map(alert_rule_json).collect::<Vec<_>>(),
            "events": events.iter().map(alert_event_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(Err(error)) => internal_error("alerts-get", &error),
        Err(error) => internal_error("alerts-get-join", &error),
    }
}

/// Body of `POST /api/alerts` (token-gated): target + item union + condition
/// union + optional note.
#[derive(Deserialize)]
pub(crate) struct AlertPost {
    target: String,
    #[serde(flatten)]
    item: ApiItemRef,
    condition: AlertConditionPost,
    #[serde(default)]
    note: Option<String>,
}

/// `POST /api/alerts` — adds one rule (token-gated).
pub(crate) async fn api_alerts_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<AlertPost>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    let target = match known_target(&state.config, &body.target) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let condition = match body.condition.to_condition() {
        Ok(condition) => condition,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };
    let added = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let item = body.item.to_ref();
        move || {
            state.alerts.add_rule(&amtracker_core::domain::NewAlertRule {
                target,
                item,
                condition,
                note: body.note,
            })
        }
    });
    match added.await {
        Ok(Ok(rule)) => Json(alert_rule_json(&rule)).into_response(),
        Ok(Err(error)) => internal_error("alerts-add", &error),
        Err(error) => internal_error("alerts-add-join", &error),
    }
}

/// Body of `DELETE /api/alerts` (token-gated).
#[derive(Deserialize)]
pub(crate) struct AlertDelete {
    #[serde(rename = "ruleID")]
    rule_id: i64,
}

/// `DELETE /api/alerts` — removes one rule by id (token-gated). Its event
/// history stays.
pub(crate) async fn api_alerts_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<AlertDelete>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    let removed = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        move || state.alerts.remove_rule(body.rule_id)
    });
    match removed.await {
        Ok(Ok(removed)) => {
            Json(serde_json::json!({ "ok": true, "removed": removed })).into_response()
        }
        Ok(Err(error)) => internal_error("alerts-remove", &error),
        Err(error) => internal_error("alerts-remove-join", &error),
    }
}

/// `POST /api/alerts/evaluate?target=` — runs one evaluation pass now
/// (token-gated); the same pass the scheduler runs after every cycle. The
/// `Json` extractor doubles as the CSRF guard: simple cross-site forms cannot
/// send `application/json`.
pub(crate) async fn api_alerts_evaluate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    Json(_body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    let target = match required_target(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let cooldown = i64::try_from(state.config.alerts.cooldown_secs).unwrap_or(i64::MAX);
    let run = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || state.alerts.evaluate(&target, cooldown)
    });
    match run.await {
        Ok(Ok(run)) => Json(serde_json::json!({
            "evaluated": run.evaluated,
            "fired": run.fired,
            "dispatched": run.dispatched,
            "dispatchFailures": run.dispatch_failures,
        }))
        .into_response(),
        Ok(Err(error)) => internal_error("alerts-evaluate", &error),
        Err(error) => internal_error("alerts-evaluate-join", &error),
    }
}

#[derive(Deserialize)]
pub(crate) struct FetchParams {
    #[serde(default)]
    target: Option<String>,
}

/// `POST /api/fetch?target=` — run one ingest cycle now (bearer-gated).
///
/// The JSON body requirement (`{}` is fine) is deliberate CSRF hygiene: a
/// cross-site form can fire simple `POST`s with arbitrary query strings but
/// cannot send `application/json`, so requiring the content type keeps the
/// open-mode (token-less) mutating endpoint unreachable from drive-by web
/// pages. Bearer-gated deployments are unaffected either way.
pub(crate) async fn api_fetch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<FetchParams>,
    Json(_body): Json<serde_json::Value>,
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
    // observe/close_missing sequences never interleave — scoped PER CYCLE
    // (like the scheduler), not across the whole loop: one slow manual fetch
    // must not stall scheduled ingestion for every other target for minutes.
    // A successful fetch republishes the target's feed right here (the
    // scheduler only does that on its own cadence).
    let reports = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        move || {
            let mut reports = Vec::new();
            for target in &state.config.targets {
                if let Some(wanted) = &wanted {
                    if target.id != *wanted {
                        continue;
                    }
                }
                let mut fetched = false;
                for kind in [PayloadKind::Commodities, PayloadKind::Auctions] {
                    let key = (target.id.0.clone(), kind);
                    {
                        let mut timers =
                            state.timers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        timers.running.insert(key.clone());
                    }
                    let outcome = {
                        let _guard = state
                            .cycle_lock
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        state.cycle.run_cycle(target, kind)
                    };
                    {
                        let mut timers =
                            state.timers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        timers.running.remove(&key);
                        // A manual cycle counts as a run for the schedule,
                        // like the scheduler's own post-cycle stamping: the
                        // next poll lands a full interval later instead of
                        // re-fetching the same payload right away.
                        timers.cycle_run.insert(key, std::time::Instant::now());
                    }
                    match outcome {
                        Ok(report) => {
                            fetched = true;
                            reports.push(serde_json::json!({
                                "target": report.target.as_str(),
                                "kind": report.kind.as_str(),
                                "outcome": cycle_outcome_label(&report.outcome),
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
                    // its own successful cycles (retention + feed publish);
                    // warm_names = false leaves the name warm-up on the
                    // maintenance worker so this response never waits out
                    // its pacing sleeps.
                    crate::scheduler::after_cycle_now(&state, target.id.as_str(), false);
                }
            }
            reports
        }
    });
    let reports = match reports.await {
        Ok(reports) => reports,
        Err(error) => {
            return internal_error("fetch", &error);
        }
    };
    Json(serde_json::json!({ "reports": reports })).into_response()
}

/// `GET /api/schedule` — the scheduler's live fetch queue: when each
/// (target, kind) becomes due, whether it is parked by a rate-limit backoff,
/// and whether a cycle is in flight right now. Same data the dashboard's
/// "next fetch" rows render, as JSON for scripts and monitors.
pub(crate) async fn api_schedule(State(state): State<Arc<AppState>>) -> Response {
    let now_unix = state.clock.now();
    let entries = {
        let timers = state.timers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::scheduler::schedule_view(&state.config.targets, &timers, Instant::now())
    };
    let entries: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "target": entry.target,
                "kind": entry.kind.as_str(),
                "intervalSecs": entry.interval_secs,
                "nextDueInSecs": entry.next_due_in_secs,
                "backoffInSecs": entry.backoff_in_secs,
                "running": entry.running,
            })
        })
        .collect();
    Json(serde_json::json!({ "now": now_unix, "entries": entries })).into_response()
}

/// Stable wire label for a cycle outcome (`Debug` would drift silently).
fn cycle_outcome_label(outcome: &CycleOutcome) -> &'static str {
    match outcome {
        CycleOutcome::Fetched => "fresh",
        CycleOutcome::NotModified => "not-modified",
        CycleOutcome::Empty => "empty",
    }
}

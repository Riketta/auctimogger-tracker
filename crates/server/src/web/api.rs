//! JSON API surface, feed downloads, and the token-gated mutation endpoints
//! (watchlist add/remove, manual fetch). Page handlers live in [`pages`].

#![allow(clippy::unused_async)]

use std::{
    collections::HashMap,
    sync::Arc,
};

use amtracker_core::{
    domain::{
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

/// `GET /api/search?q=&limit=` — kind-aware catalog search (completions).
pub(crate) async fn api_search(
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
            }).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(Err(error)) => internal_error("search", &error),
        Err(error) => internal_error("search-join", &error),
    }
}

/// `GET /api/opportunities?target=&min_churn=&limit=`
pub(crate) async fn api_opportunities(
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
        Ok(Err(error)) => internal_error("opportunities", &error),
        Err(error) => internal_error("opportunities-join", &error),
    }
}

/// `GET /api/overview?target=`
pub(crate) async fn api_overview(
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
            let mover = |m: &amtracker_core::services::opportunity::MoverRow| {
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
        Ok(Err(error)) => internal_error("overview", &error),
        Err(error) => internal_error("overview-join", &error),
    }
}

/// `GET /api/items/{target_id}/{item_id}/metrics?span=d7,d30` — the same
/// blocks the item page shows, JSON-shaped. Plain items only (the URL cannot
/// carry the pet variant tuple).
pub(crate) async fn api_item_metrics(
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
    let item = amtracker_core::domain::ItemRef::Item { item_id };
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
            return internal_error("item-metrics", &error);
        }
        Err(error) => {
            return internal_error("item-metrics-join", &error);
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
                    let outcome = {
                        let _guard = state
                            .cycle_lock
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        state.cycle.run_cycle(target, kind)
                    };
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
            return internal_error("fetch", &error);
        }
    };
    Json(serde_json::json!({ "reports": reports })).into_response()
}

/// Stable wire label for a cycle outcome (`Debug` would drift silently).
fn cycle_outcome_label(outcome: &CycleOutcome) -> &'static str {
    match outcome {
        CycleOutcome::Fetched => "fresh",
        CycleOutcome::NotModified => "not-modified",
        CycleOutcome::Empty => "empty",
    }
}

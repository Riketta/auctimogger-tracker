//! Machine-readable API contract: an `OpenAPI` 3.1 document ([`spec`]) and the
//! server-rendered docs page built from it. Hand-authored next to the
//! handlers; the drift test (`tests/web.rs`) asserts the document matches the
//! router's route table in both directions — paths and methods — and the
//! router itself is mounted from that same table, so a route cannot exist
//! without a spec entry or the other way around. No codegen or extra
//! dependencies.
//!
//! Agents are first-class consumers: `GET /api` is a discoverable directory,
//! every price field is integer copper, and reads are unauthenticated.

#![allow(clippy::unused_async)]
use std::{
    fmt::Write as _,
    sync::OnceLock,
};

use axum::response::Response;
use serde_json::{
    json,
    Value,
};

use super::render;

/// The `OpenAPI` document describing the whole JSON API + feed surface. Built
/// once and served verbatim; the docs page renders from the same value.
pub fn spec() -> &'static Value {
    static SPEC: OnceLock<Value> = OnceLock::new();
    SPEC.get_or_init(build_spec)
}

/// Required `?target=` for every JSON read: the handlers answer 400 rather
/// than silently querying a default market, so the contract says required.
fn target_param() -> Value {
    json!({
        "name": "target",
        "in": "query",
        "required": true,
        "description": "Target slug (see /api/health).",
        "schema": { "type": "string" }
    })
}

/// Optional `?target=` on the feed downloads: an omitted target falls back to
/// the first configured target.
fn feed_target_param() -> Value {
    json!({
        "name": "target",
        "in": "query",
        "required": false,
        "description": "Target slug (see /api/health). Optional; defaults to the first configured target.",
        "schema": { "type": "string" }
    })
}

fn days_param(name: &str, default: i64, max: i64, description: &str) -> Value {
    json!({
        "name": name,
        "in": "query",
        "required": false,
        "description": description,
        "schema": { "type": "integer", "default": default, "minimum": 1, "maximum": max }
    })
}

/// The optional `?category=` query parameter shared by every market view.
fn category_param() -> Value {
    json!({
        "name": "category",
        "in": "query",
        "required": false,
        "description": "item|commodity|pet. Narrows the view to one market category; absent, the response covers all three, keyed by category. Each category is a separate evidence pool: an item trading in both auction houses appears independently in each.",
        "schema": { "type": "string", "enum": ["item", "commodity", "pet"] }
    })
}

// JSON document builders: `json!` only borrows its interpolations, so the
// linter's pass-by-value preference would force `&&&` noise at every call
// site. By-value reads keep the call sites readable.
#[allow(clippy::needless_pass_by_value)]
fn ok_json(schema: Value) -> Value {
    json!({
        "description": "OK",
        "content": { "application/json": { "schema": schema } }
    })
}

#[allow(clippy::needless_pass_by_value)]
fn get_op(summary: &str, description: &str, params: Vec<Value>, schema: Value) -> Value {
    json!({
        "tags": ["api"],
        "summary": summary,
        "description": description,
        "parameters": params,
        "responses": { "200": ok_json(schema), "400": { "description": "Bad request (unknown/missing target, bad parameter)." }, "500": { "description": "Store or internal failure." } }
    })
}

#[allow(clippy::needless_pass_by_value)]
fn post_op(summary: &str, description: &str, params: Vec<Value>, body: Value) -> Value {
    json!({
        "tags": ["api"],
        "summary": summary,
        "description": description,
        "parameters": params,
        "security": [ { "bearerAuth": [] } ],
        "requestBody": { "required": true, "content": { "application/json": { "schema": body } } },
        "responses": { "200": ok_json(json!({ "type": "object" })), "400": { "description": "Bad request (unknown/missing target)." }, "401": { "description": "Invalid or missing bearer token." } }
    })
}

#[allow(clippy::needless_pass_by_value)]
fn delete_op(summary: &str, description: &str, params: Vec<Value>, body: Value) -> Value {
    json!({
        "tags": ["api"],
        "summary": summary,
        "description": description,
        "parameters": params,
        "security": [ { "bearerAuth": [] } ],
        "requestBody": { "required": true, "content": { "application/json": { "schema": body } } },
        "responses": { "200": ok_json(json!({ "type": "object" })), "401": { "description": "Invalid or missing bearer token." } }
    })
}

// One declarative data table per group: splitting the document into chunks
// keeps each `json!` literal's macro expansion shallow enough for the default
// recursion limit, at zero runtime cost (the chunks merge once).
#[allow(clippy::too_many_lines)]
fn build_spec() -> Value {
    let target = target_param();
    let mut spec = json!({
        "openapi": "3.1.0",
        "info": {
            "title": "auctimogger-tracker API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": concat!(
                "Local market-memory HTTP API over WoW auction-house data: ranked deals, ",
                "per-item day-level price history, metric blocks, watchlists, alerts and feed exports. ",
                "All prices are integers in copper (10,000 copper = 1 gold). ",
                "Reads are unauthenticated; mutations require `Authorization: Bearer <token>` when ",
                "token authentication is active (open mode when no usable token is configured). ",
                "There is no rate limiting."
            ),
        },
        "servers": [ { "url": "/" } ],
        "tags": [
            { "name": "api", "description": "JSON API" },
            { "name": "feeds", "description": "Addon feed downloads" }
        ],
    });
    let mut paths = serde_json::Map::new();
    for chunk in [
        spec_paths_discovery(),
        spec_paths_market(&target),
        spec_paths_items_and_feeds(),
        spec_paths_watchlist_alerts(&target),
    ] {
        if let Value::Object(map) = chunk {
            paths.extend(map);
        }
    }
    let mut schemas = serde_json::Map::new();
    for chunk in [spec_schemas_market(), spec_schemas_secondary()] {
        if let Value::Object(map) = chunk {
            schemas.extend(map);
        }
    }
    if let Some(root) = spec.as_object_mut() {
        root.insert(String::from("paths"), Value::Object(paths));
        root.insert(
            String::from("components"),
            json!({
                "securitySchemes": {
                    "bearerAuth": { "type": "http", "scheme": "bearer" }
                },
                "schemas": Value::Object(schemas),
            }),
        );
    }
    spec
}

/// Directory, freshness probe, and the contract itself. These can only answer
/// 200, so they hand-roll their responses instead of using [`get_op`]'s
/// 400/500 stamps.
fn spec_paths_discovery() -> Value {
    json!({
        "/api": {
            "get": {
                "tags": ["api"],
                "summary": "Endpoint directory",
                "description": "Machine-readable index of every endpoint with its summary, plus links to this document and the docs page. Start here.",
                "parameters": [],
                "responses": { "200": { "description": "OK", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/EndpointIndex" } } } } }
            },
        },
        "/api/health": {
            "get": get_op(
                "Snapshot freshness",
                "Per-target, per-payload-kind freshness: latest snapshot stamp, entry/drop counts and age in seconds.",
                vec![],
                ok_json(json!({
                    "type": "object",
                    "properties": {
                        "targets": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "target": { "type": "string" },
                                    "kinds": { "type": "array", "items": { "type": "object" } }
                                }
                            }
                        }
                    }
                })),
            ),
        },
        "/api/openapi.json": {
            "get": {
                "tags": ["api"],
                "summary": "This document",
                "description": "The OpenAPI document itself.",
                "parameters": [],
                "responses": { "200": { "description": "OK", "content": { "application/json": { "schema": { "type": "object" } } } } }
            },
        },
        "/api/docs": {
            "get": {
                "tags": ["api"],
                "summary": "Docs page",
                "description": "Server-rendered HTML reference built from this document (no JS).",
                "parameters": [],
                "responses": { "200": { "description": "OK", "content": { "text/html": { "schema": { "type": "string" } } } } }
            },
        },
    })
}

/// Read-only market data. Every endpoint here requires `?target=`.
// One declarative spec table per group: the path list reads best as a single
// ordered block.
#[allow(clippy::too_many_lines)]
fn spec_paths_market(target: &Value) -> Value {
    json!({
        "/api/search": {
            "get": get_op(
                "Catalog search",
                "Case-insensitive item/pet name search over the resolved catalog (SQLite NOCASE: case-folding is ASCII-only — non-ASCII names match exact case). Page with offset until hasMore is false.",
                vec![
                    json!({ "name": "q", "in": "query", "required": true, "description": "Name fragment.", "schema": { "type": "string" } }),
                    json!({ "name": "limit", "in": "query", "required": false, "description": "Page size, clamped to 1..=200.", "schema": { "type": "integer", "default": 20, "minimum": 1, "maximum": 200 } }),
                    json!({ "name": "offset", "in": "query", "required": false, "description": "Page offset.", "schema": { "type": "integer", "default": 0, "minimum": 0 } }),
                ],
                ok_json_ref("SearchResponse"),
            ),
        },
        "/api/opportunities": {
            "get": get_op(
                "Ranked live deals",
                "Snipe candidates sorted by score descending — only items the newest snapshot still lists. discount = 1 - floorNow/reference, the reference being soldMedian14 (the 14-day quantity-weighted median of what actually sold) once at least 3 priced units sold in the window, else value7 (the quantity-weighted 7-day listing value); the score is a weighted geometric mean of discount, 14-day sale-rate reliability, churn and stability (any component at zero scores zero). limit is uncapped enough for whole-ranking pulls (scores are ranking-only; deal gates use exact metric blocks). Deals seen today but already gone: /api/missed-deals.",
                vec![
                    target.clone(),
                    category_param(),
                    json!({ "name": "min_churn", "in": "query", "required": false, "description": "Only items with at least this many unique listings churning per day rank (per category). Non-finite values fall back to the default.", "schema": { "type": "number", "default": 1.0 } }),
                    json!({ "name": "limit", "in": "query", "required": false, "description": "Maximum rows per category; 0 is treated as 1.", "schema": { "type": "integer", "default": 50 } }),
                    json!({ "name": "sort", "in": "query", "required": false, "description": "Row ordering: score (weighted score, default) or last_sale (stalest days-since-last-sale first; never-sold rows last). Unknown values are rejected with 400.", "schema": { "type": "string", "enum": ["score", "last_sale"], "default": "score" } }),
                ],
                ok_json_ref("OpportunitiesResponse"),
            ),
        },
        "/api/missed-deals": {
            "get": get_op(
                "Deals seen today, already gone",
                "Listings seen today (UTC) at least min_discount percent under the item's quantity-weighted 7-day value that the newest snapshot no longer lists — bought or cancelled after the tracker saw them. Today's day fold never forgets a listing, so its floor outlives the listing; these rows report that honestly instead of ranking it as buyable. Deepest discount first. Same evidence floor as the ranking.",
                vec![
                    target.clone(),
                    category_param(),
                    json!({ "name": "min_discount", "in": "query", "required": false, "description": "Minimum discount in whole percent (25 = seen at 75% of the value or cheaper). Clamped to 0..=100; non-finite values fall back to the default.", "schema": { "type": "number", "default": 25, "minimum": 0, "maximum": 100 } }),
                    json!({ "name": "limit", "in": "query", "required": false, "description": "Maximum rows per category; 0 is treated as 1.", "schema": { "type": "integer", "default": 50 } }),
                ],
                ok_json_ref("MissedDealsResponse"),
            ),
        },
        "/api/crafts": {
            "get": get_op(
                "Efficient crafts",
                "The manual recipe book ([crafts].recipes) evaluated against the target: every recipe with its reagents and output priced at their reference value (the 14-day sold median once at least 3 priced units sold, else the quantity-weighted 7-day listing value; reagents also at the newest snapshot's floors), the output's 14-day sale rate, and the money fields per make in copper. Efficient crafts come first (ranked = true, highest expected profit first); the rest carry the first issue that kept them out.",
                vec![target.clone()],
                ok_json_ref("CraftsResponse"),
            ),
        },
        "/api/history": {
            "get": get_op(
                "Bulk day-level market history",
                concat!(
                    "One call, whole market: per-item-per-day aggregate rows for every item/pet with fold data in the window. ",
                    "This is the analysis-grade bulk payload — feed it straight into pandas/a script. ",
                    "Days are UTC day numbers (days since 1970-01-01). min/p05/median/mad are listing-level nearest-rank stats in copper. ",
                    "Rows arrive ordered by day, then item kind, then item id. ",
                    "Rows are read once per category per 300 seconds (the prepared scans' cadence); any days window is served from that cache."
                ),
                vec![
                    target.clone(),
                    category_param(),
                    days_param("days", 7, 30, "Window length in days ending today (UTC). Out-of-range values are clamped to 1..=30; unparseable values default to 7."),
                ],
                ok_json_ref("HistoryResponse"),
            ),
        },
        "/api/overview": {
            "get": get_op(
                "Market overview",
                "Movers up/down by median change and the most active items by churn, computed from the prepared per-category scans.",
                vec![
                    target.clone(),
                    category_param(),
                    json!({ "name": "limit", "in": "query", "required": false, "description": "Rows per list per category, clamped to 1..=50.", "schema": { "type": "integer", "default": 15, "minimum": 1, "maximum": 50 } }),
                ],
                ok_json_ref("OverviewResponse"),
            ),
        },
        "/api/browse": {
            "get": get_op(
                "Snapshot viewer",
                "One page of a stored snapshot's listings joined to their catalog identity. Without snapshot, the newest snapshot of the (target, kind) pair serves. sort is price|qty|time (default price, cheapest first; unknown values fall back to price); dir=desc flips it. q is a case-insensitive (ASCII) name prefix, item an exact item id; class/quality match catalog labels case-insensitively (ASCII). Pages are 50 rows; out-of-range pages clamp to the last. Errors: no snapshots for the pair is 404, an unknown snapshot id is 404, a snapshot id from another target/kind and non-integer item or page values are 400.",
                vec![
                    target.clone(),
                    json!({ "name": "kind", "in": "query", "required": false, "description": "commodities|auctions; commodities when absent.", "schema": { "type": "string", "enum": ["commodities", "auctions"] } }),
                    json!({ "name": "snapshot", "in": "query", "required": false, "description": "Pin one stored snapshot id; absent = newest for the pair.", "schema": { "type": "integer" } }),
                    json!({ "name": "q", "in": "query", "required": false, "description": "Case-insensitive (ASCII) catalog name prefix.", "schema": { "type": "string" } }),
                    json!({ "name": "item", "in": "query", "required": false, "description": "Exact item id.", "schema": { "type": "integer" } }),
                    json!({ "name": "class", "in": "query", "required": false, "description": "Catalog item-class label (ASCII case-insensitive).", "schema": { "type": "string" } }),
                    json!({ "name": "quality", "in": "query", "required": false, "description": "Catalog quality label (ASCII case-insensitive).", "schema": { "type": "string" } }),
                    json!({ "name": "sort", "in": "query", "required": false, "description": "price|qty|time; unknown values fall back to price (cheapest first).", "schema": { "type": "string", "enum": ["price", "qty", "time"] } }),
                    json!({ "name": "dir", "in": "query", "required": false, "description": "desc flips the sort column; any other value sorts ascending.", "schema": { "type": "string", "enum": ["desc"] } }),
                    json!({ "name": "page", "in": "query", "required": false, "description": "1-based page index; out-of-range pages clamp to the last page; non-integer values are rejected with 400.", "schema": { "type": "integer", "default": 1, "minimum": 1 } }),
                ],
                ok_json(json!({ "type": "object" })),
            ),
        },
        "/api/snapshots": {
            "get": get_op(
                "Snapshot metadata",
                "Newest-first raw snapshot metadata per payload kind, with entry/drop counts.",
                vec![
                    target.clone(),
                    json!({ "name": "kind", "in": "query", "required": false, "description": "commodities|auctions; both when absent.", "schema": { "type": "string", "enum": ["commodities", "auctions"] } }),
                    json!({ "name": "limit", "in": "query", "required": false, "description": "Per kind, clamped to 1..=500.", "schema": { "type": "integer", "default": 50, "minimum": 1, "maximum": 500 } }),
                ],
                ok_json(json!({ "type": "object" })),
            ),
        },
        "/api/schedule": {
            "get": get_op(
                "Fetch schedule queue",
                "Live scheduler board: when each (target, kind) cycle is next due, whether a rate-limit backoff parks it, and whether one is running right now.",
                vec![],
                ok_json(json!({ "type": "object" })),
            ),
        },
    })
}

/// Item/pet metric blocks and the feed downloads.
fn spec_paths_items_and_feeds() -> Value {
    json!({
        "/api/items/{target_id}/{item_id}/metrics": {
            "get": get_op(
                "Item metric blocks",
                "The exact per-span metric blocks behind the item page (discount gates and alerts read these). Spans without data are omitted. span is a comma list of d1,d7,d14,d30.",
                vec![
                    json!({ "name": "target_id", "in": "path", "required": true, "description": "Target slug.", "schema": { "type": "string" } }),
                    json!({ "name": "item_id", "in": "path", "required": true, "description": "Item id.", "schema": { "type": "integer" } }),
                    category_param(),
                    json!({ "name": "span", "in": "query", "required": false, "description": "Comma-separated spans; default d1. Unknown span names are rejected with 400." }),
                ],
                ok_json(json!({
                    "type": "object",
                    "description": "Span-keyed object: {\"d1\": MetricBlock, \"d7\": MetricBlock, ...}",
                    "additionalProperties": { "$ref": "#/components/schemas/MetricBlock" }
                })),
            ),
        },
        "/api/items/{target_id}/pet/{species_id}/metrics": {
            "get": get_op(
                "Pet-variant metric blocks",
                "The pet twin of item metrics: the identity is the full species/level/quality/breed tuple; all three query parameters are required.",
                vec![
                    json!({ "name": "target_id", "in": "path", "required": true, "description": "Target slug.", "schema": { "type": "string" } }),
                    json!({ "name": "species_id", "in": "path", "required": true, "description": "Pet species id.", "schema": { "type": "integer" } }),
                    json!({ "name": "level", "in": "query", "required": true, "description": "Pet level.", "schema": { "type": "integer" } }),
                    json!({ "name": "quality", "in": "query", "required": true, "description": "Pet quality id.", "schema": { "type": "integer" } }),
                    json!({ "name": "breed", "in": "query", "required": true, "description": "Pet breed id.", "schema": { "type": "integer" } }),
                    json!({ "name": "span", "in": "query", "required": false, "description": "Comma-separated spans; default d1. Unknown span names are rejected with 400.", "schema": { "type": "string" } }),
                ],
                ok_json(json!({
                    "type": "object",
                    "description": "Span-keyed object of MetricBlock.",
                    "additionalProperties": { "$ref": "#/components/schemas/MetricBlock" }
                })),
            ),
        },
        "/api/feed.json": {
            "get": json!({
                "tags": ["feeds"],
                "summary": "Full market feed (JSON)",
                "description": "Per-item metric blocks and identities for the whole market — the addon feed in JSON form. Served from the copy the scheduler publishes after each successful cycle (throttled by server.feed_publish_secs), exactly like /feed.lua; a request never rebuilds the market.",
                "parameters": vec![feed_target_param()],
                "responses": {
                    "200": { "description": "OK", "content": { "application/json": { "schema": { "type": "object" } } } },
                    "400": { "description": "Unknown target." },
                    "404": { "description": "No feed published yet for the target." },
                    "500": { "description": "Store or internal failure." }
                }
            }),
        },
        "/feed.lua": {
            "get": json!({
                "tags": ["feeds"],
                "summary": "Full addon feed (Lua)",
                "description": "The complete market feed the addon consumes, Lua-table serialization. Same data as /api/feed.json.",
                "parameters": vec![feed_target_param()],
                "responses": {
                    "200": { "description": "OK", "content": { "text/x-lua": { "schema": { "type": "string" } } } },
                    "400": { "description": "Unknown target." },
                    "404": { "description": "No feed published yet for the target." }
                }
            }),
        },
        "/feed-slim.lua": {
            "get": json!({
                "tags": ["feeds"],
                "summary": "Watchlist-only addon feed (Lua)",
                "description": "Slim feed covering exactly the target's watchlist entries. Built on demand, then cached for the feed publish interval; a watchlist change shows on the next request.",
                "parameters": vec![feed_target_param()],
                "responses": {
                    "200": { "description": "OK", "content": { "text/x-lua": { "schema": { "type": "string" } } } },
                    "400": { "description": "Unknown target." }
                }
            }),
        },
    })
}

/// Watchlist and alert management, plus the token-gated triggers. Query
/// parameters live on the POSTs too: `evaluate` requires one, `fetch` takes
/// an optional one.
// One declarative spec table per group: the path list reads best as a single
// ordered block.
#[allow(clippy::too_many_lines)]
fn spec_paths_watchlist_alerts(target: &Value) -> Value {
    json!({
        "/api/watchlist": {
            "get": get_op(
                "List watchlist",
                "The target's curated watch list (item refs + notes).",
                vec![target.clone()],
                ok_json(json!({ "type": "object" })),
            ),
            "post": post_op(
                "Add watchlist entry",
                concat!("Body: ", "{\"target\", \"type\": \"item\", \"itemID\", \"note\"?} or ",
                        "{\"target\", \"type\": \"pet\", \"speciesID\", \"level\", \"qualityID\", \"breedID\", \"note\"?}.",
                        " Pet fields are validated (species ≥ 1, level 1..=25, quality 0..=6, breed 3..=11); violations answer 400."),
                vec![],
                json!({ "type": "object" }),
            ),
            "delete": delete_op(
                "Remove watchlist entry",
                concat!("Body: ", "{\"target\"} plus the item identity (same shape as POST, without note)."),
                vec![],
                json!({ "type": "object" }),
            ),
        },
        "/api/watchlist/metrics": {
            "get": get_op(
                "Watchlist market report",
                "Every watchlist entry joined to its current market numbers: metric blocks per span, trailing sale rate and per-day trend rows.",
                vec![
                    target.clone(),
                    category_param(),
                    json!({ "name": "span", "in": "query", "required": false, "description": "Comma-separated spans; default d7. Unknown span names are rejected with 400.", "schema": { "type": "string" } }),
                    days_param("sale_days", 14, 90, "Trailing sale-rate window (sale_days)."),
                    json!({ "name": "trend_days", "in": "query", "required": false, "description": "Trend window, clamped to 1..=90.", "schema": { "type": "integer", "default": 30, "minimum": 1, "maximum": 90 } }),
                ],
                ok_json(json!({ "type": "object" })),
            ),
        },
        "/api/priorities": {
            "get": get_op(
                "Sell priorities",
                "The target's watchlist ranked best sell first: score (50% liquidity + 30% momentum + 20% recency) with its components, current-day median, week block and trend rows. Best sell first; entries without fold data last with a null score.",
                vec![
                    target.clone(),
                    json!({ "name": "category", "in": "query", "required": false, "description": "One market category; default item. Unknown names are rejected with 400.", "schema": { "type": "string" } }),
                ],
                ok_json(json!({ "type": "object" })),
            ),
        },
        "/api/watchlist/compare": {
            "get": get_op(
                "Cross-target watchlist comparison",
                "The union of all configured targets' watchlists as a matrix: one row per identity, one cell per target (watched flag, current-day median, 7-day median), best = index of the highest-paying market per row. No target parameter: compares every configured target.",
                vec![
                    json!({ "name": "category", "in": "query", "required": false, "description": "One market category; default item. Unknown names are rejected with 400.", "schema": { "type": "string" } }),
                ],
                ok_json(json!({ "type": "object" })),
            ),
        },
        "/api/alerts": {
            "get": get_op(
                "Alert rules and events",
                "The target's alert rules plus recent fired events (events caps the event list at 1..=500).",
                vec![
                    target.clone(),
                    json!({ "name": "events", "in": "query", "required": false, "description": "Maximum recent events.", "schema": { "type": "integer", "default": 50, "minimum": 1, "maximum": 500 } }),
                ],
                ok_json(json!({ "type": "object" })),
            ),
            "post": post_op(
                "Create alert rule",
                concat!("Body: ", "{\"target\", item identity, \"condition\": {...}, \"note\"?} — conditions mirror the /help glossary ",
                        "(price-below {copper}, below-floor {span, percentile}, floor-below {copper}, floor-below-value {fraction}, ",
                        "new-listing-below {fraction}, sale-rate-above {rate}, days-since-last-sale-above {days}; fraction is a share of the item's ",
                        "quantity-weighted 7-day value within (0, 1]). Each market category only observes ",
                        "while its own feed snapshot is younger than [alerts].max_data_age_secs."),
                vec![],
                json!({ "type": "object" }),
            ),
            "delete": delete_op(
                "Delete alert rule",
                "Body: {\"ruleID\": <id>}.",
                vec![],
                json!({ "type": "object" }),
            ),
        },
        "/api/alerts/evaluate": {
            "post": post_op(
                "Evaluate alerts now",
                "Runs one alert evaluation pass for the target outside the scheduler cadence. Body is ignored (send {}).",
                vec![target.clone()],
                json!({ "type": "object" }),
            ),
        },
        "/api/fetch": {
            "post": post_op(
                "Trigger a fetch cycle",
                "Runs one ingest cycle now (all configured targets when ?target= is absent) and reports per-target outcomes. Bypasses the poll interval; the scheduler continues on its own cadence.",
                vec![json!({ "name": "target", "in": "query", "required": false, "description": "Fetch only this target; all configured targets when absent.", "schema": { "type": "string" } })],
                json!({ "type": "object" }),
            ),
        },
    })
}

/// Price/statistics schemas shared by the ranking and history payloads.
#[allow(clippy::too_many_lines)] // one flat JSON literal per schema
fn spec_schemas_market() -> Value {
    json!({
        "ItemRef": {
            "description": "One tradeable identity: an item id, or the full pet species/level/quality/breed tuple.",
            "oneOf": [
                {
                    "type": "object",
                    "required": ["type", "itemID"],
                    "properties": {
                        "type": { "const": "item" },
                        "itemID": { "type": "integer", "description": "WoW item id." }
                    }
                },
                {
                    "type": "object",
                    "required": ["type", "speciesID", "level", "qualityID", "breedID"],
                    "properties": {
                        "type": { "const": "pet" },
                        "speciesID": { "type": "integer", "description": "Positive species id.", "minimum": 1 },
                        "level": { "type": "integer", "description": "1..=25.", "minimum": 1, "maximum": 25 },
                        "qualityID": { "type": "integer", "description": "0 (poor) ..= 6 (artifact).", "minimum": 0, "maximum": 6 },
                        "breedID": { "type": "integer", "description": "3..=11.", "minimum": 3, "maximum": 11 }
                    }
                }
            ]
        },
        "MetricBlock": {
            "description": concat!("Nearest-rank price statistics over one span window; all price fields are integer copper. ",
                                   "n is unique auctions folded (dedup'd per day), vwap is the quantity-weighted mean ",
                                   "(median fallback when qty is 0), saleRate is the trailing-14d sell-through fraction ",
                                   "over credit-eligible listings and saleRate180d the same estimator over 180 days ",
                                   "(the slow demand window). daysSinceLastSale/closuresSinceLastSale are ledger recency ",
                                   "(null = no credited sale ever); qtySold is the estimated units sold in the span ",
                                   "window (null = no sales rows at all)."),
            "type": "object",
            "properties": {
                "n": { "type": "integer" },
                "min": { "type": "integer" },
                "p05": { "type": "integer" },
                "p25": { "type": "integer" },
                "median": { "type": "integer" },
                "p75": { "type": "integer" },
                "p95": { "type": "integer" },
                "max": { "type": "integer" },
                "mad": { "type": "integer", "description": "Median absolute deviation of unit prices." },
                "vwap": { "type": "integer" },
                "qtySum": { "type": "integer" },
                "listingCount": { "type": "integer" },
                "saleRate": { "type": ["number", "null"], "description": "0..1; null when the window holds no credit-eligible close (no evidence — a listing not yet closed is not a zero)." },
                "saleRate180d": { "type": ["number", "null"], "description": "0..1 over 180 days; null when the window has no sighting evidence." },
                "daysSinceLastSale": { "type": ["integer", "null"], "description": "Whole days since the ledger last credited a sale; null = never sold." },
                "closuresSinceLastSale": { "type": ["integer", "null"], "description": "Net listings closed after that last credited sale; null alongside daysSinceLastSale." },
                "qtySold": { "type": ["integer", "null"], "description": "Estimated units sold in the span window (credited vanishes + commodity partial fills); null = no sales rows at all." }
            }
        },
        "Opportunity": {
            "type": "object",
            "properties": {
                "item": { "$ref": "#/components/schemas/ItemRef" },
                "category": { "type": "string", "enum": ["item", "commodity", "pet"], "description": "The market this row ranks in." },
                "median7": { "type": "integer", "description": "Count-weighted 7-day median unit price of the median LISTING (copper) — the listing-level reference; stacks weigh like singles." },
                "floorNow": { "type": "integer", "description": "Cheapest unit price listed in the newest snapshot (copper) — buyable at the time of that fetch." },
                "floorQty": { "type": "integer", "description": "Units listed at exactly floorNow: how deep the deal is." },
                "listedNow": { "type": "integer", "description": "Listings of the item in the newest snapshot." },
                "value7": { "type": "integer", "description": "Quantity-weighted 7-day median LISTING unit price (copper): where half the listed quantity trades — the bulk price for stackables. The discount reference whenever soldMedian14 is null." },
                "soldMedian14": { "type": ["integer", "null"], "description": "14-day quantity-weighted median unit price of SOLD listings (copper) — what buyers actually paid; null when fewer than 3 priced units sold in the window or the ledger predates sold-price recording." },
                "valueSource": { "type": "string", "enum": ["sold", "listed"], "description": "Which value the discount is measured against: soldMedian14 when present, else value7." },
                "discount": { "type": "number", "description": "1 - floorNow/reference (soldMedian14 when present, else value7), clamped to [0,1]." },
                "reliability": { "type": ["number", "null"], "description": "14-day sale-through fraction 0..1; null = no credit-eligible evidence (the score then uses the scan's median known rate)." },
                "churnPerDay": { "type": "number", "description": "Average unique listings per day across the 7-day window." },
                "stability": { "type": "number", "description": "1 - min(1, mad7/median7)." },
                "samples7": { "type": "integer", "description": "Auction-days folded over the window (listings x days seen)." },
                "confidence": { "type": "number", "description": "min(1, samples7/[opportunities].min_samples) — always 1 on ranked rows, which the evidence floor already admits." },
                "score": { "type": "number", "description": "Weighted geometric mean of discount, reliability, churn and stability — for RANKING only; any component at zero scores zero." },
                "daysSinceLastSale": { "type": ["integer", "null"], "description": "Whole days since the ledger last credited a sale; null = no credited sale ever (sorts last under sort=last_sale)." },
                "closuresSinceLastSale": { "type": ["integer", "null"], "description": "Net listings closed after that last credited sale." }
            }
        },
        "OpportunitiesResponse": {
            "description": concat!("With ?category=, the bare shape below (plus a category echo and the snapshot its prices come from). ",
                                   "Without it, an object keyed by category: {\"item\": […], \"commodity\": […], \"pet\": […]} (arrays, no snapshot)."),
            "type": "object",
            "properties": {
                "category": { "type": "string", "enum": ["item", "commodity", "pet"] },
                "snapshot": {
                    "type": ["object", "null"],
                    "description": "The newest stored snapshot floorNow comes from; null when the target has none for this market.",
                    "properties": {
                        "id": { "type": "integer" },
                        "fetchedAt": { "type": "integer", "description": "Unix seconds of the fetch." },
                        "sourceLastModified": { "type": ["integer", "null"] }
                    }
                },
                "opportunities": { "type": "array", "items": { "$ref": "#/components/schemas/Opportunity" } }
            }
        },
        "MissedDeal": {
            "description": "One listing seen today that the newest snapshot no longer lists.",
            "type": "object",
            "properties": {
                "item": { "$ref": "#/components/schemas/ItemRef" },
                "category": { "type": "string", "enum": ["item", "commodity", "pet"] },
                "seenFloor": { "type": "integer", "description": "Today's cumulative fold minimum (copper) — what the gone listing asked." },
                "seenFloorQty": { "type": "integer", "description": "Units that listing offered." },
                "liveFloor": { "type": ["integer", "null"], "description": "Cheapest listing of the item in the newest snapshot (copper); null = nothing of it is listed." },
                "value7": { "type": "integer", "description": "Quantity-weighted 7-day median listing unit price (copper), as on opportunities." },
                "soldMedian14": { "type": ["integer", "null"], "description": "14-day sold-price median (copper), as on opportunities." },
                "valueSource": { "type": "string", "enum": ["sold", "listed"], "description": "Which value the discount is measured against, as on opportunities." },
                "discount": { "type": "number", "description": "1 - seenFloor/reference (soldMedian14 when present, else value7), clamped to [0,1]." },
                "reliability": { "type": ["number", "null"], "description": "14-day sale-through fraction; null = no credit-eligible evidence." },
                "samples7": { "type": "integer", "description": "Auction-days folded over the window — the evidence behind value7." }
            }
        },
        "MissedDealsResponse": {
            "description": "With ?category=, the bare shape below. Without it, an object keyed by category: {\"item\": […], \"commodity\": […], \"pet\": […]}.",
            "type": "object",
            "properties": {
                "category": { "type": "string", "enum": ["item", "commodity", "pet"] },
                "minDiscount": { "type": "number", "description": "The applied minimum discount in whole percent." },
                "missed": { "type": "array", "items": { "$ref": "#/components/schemas/MissedDeal" } }
            }
        },
        "HistoryRow": {
            "description": "One item's aggregate over one UTC day in one category: listing-level nearest-rank stats in copper.",
            "type": "object",
            "properties": {
                "item": { "$ref": "#/components/schemas/ItemRef" },
                "category": { "type": "string", "enum": ["item", "commodity", "pet"] },
                "day": { "type": "integer", "description": "UTC day number (days since epoch)." },
                "listingCount": { "type": "integer", "description": "Unique auctions that day." },
                "qtySum": { "type": "integer" },
                "min": { "type": "integer" },
                "p05": { "type": "integer" },
                "median": { "type": "integer" },
                "mad": { "type": "integer" }
            }
        },
        "HistoryResponse": {
            "type": "object",
            "properties": {
                "target": { "type": "string" },
                "days": { "type": "integer" },
                "sinceDay": { "type": "integer", "description": "First UTC day number included." },
                "rows": { "type": "array", "items": { "$ref": "#/components/schemas/HistoryRow" } }
            }
        },
    })
}

/// Overview, search and index schemas.
// One declarative schema per named response: the table reads best as a single
// ordered block.
#[allow(clippy::too_many_lines)]
fn spec_schemas_secondary() -> Value {
    json!({
        "CraftsResponse": {
            "description": "The recipe book evaluated against one target; ranked rows first, then the unranked ones in recipe order.",
            "type": "object",
            "properties": {
                "target": { "type": "string" },
                "recipesFile": { "type": ["string", "null"], "description": "Configured recipe file path; null when none is configured." },
                "ahCutPercent": { "type": "number", "description": "House cut applied to the output value, in percent." },
                "minSaleRate": { "type": "number", "description": "Outputs below this 14-day sale rate are not ranked." },
                "crafts": { "type": "array", "items": { "$ref": "#/components/schemas/CraftRow" } }
            }
        },
        "CraftRow": {
            "description": "One recipe priced on the target. Money fields are per make, in copper; null whenever an input they need is missing.",
            "type": "object",
            "properties": {
                "item": { "$ref": "#/components/schemas/ItemRef" },
                "makes": { "type": "integer", "description": "Units one make yields." },
                "ranked": { "type": "boolean", "description": "True for an efficient craft (priced, selling fast enough, profitable)." },
                "issue": { "type": ["string", "null"], "enum": ["reagent-without-price", "output-without-price", "no-sale-evidence", "slow-seller", "not-profitable", null], "description": "The first reason the row did not rank; null when ranked." },
                "outputCategory": { "type": ["string", "null"], "enum": ["item", "commodity", null], "description": "The market the output was priced in (the one with more 7-day evidence)." },
                "outputValue": { "type": ["integer", "null"], "description": "Output's reference unit price: the 14-day sold median once at least 3 priced units sold, else the quantity-weighted 7-day listing value. Reagents are priced by the same rule." },
                "outputValueSource": { "type": ["string", "null"], "enum": ["sold", "listed", null], "description": "Which value outputValue resolved to; null when the output has no price." },
                "outputSoldMedian14": { "type": ["integer", "null"], "description": "The 14-day sold median behind outputValue, when outputValueSource is sold." },
                "saleRate": { "type": ["number", "null"], "description": "Output's 14-day sale-through fraction 0..1; null = no credit-eligible evidence." },
                "unitsSoldPerDay": { "type": ["number", "null"], "description": "Units the market absorbed per day over the trailing 14 days." },
                "reagents": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "item": { "$ref": "#/components/schemas/ItemRef" },
                            "qty": { "type": "integer" },
                            "category": { "type": ["string", "null"], "enum": ["item", "commodity", null] },
                            "unitValue": { "type": "integer", "description": "Quantity-weighted 7-day unit value; 0 without a price." },
                            "liveUnitPrice": { "type": ["integer", "null"], "description": "Cheapest listing in the newest snapshot, when listed." }
                        }
                    }
                },
                "cost": { "type": ["integer", "null"], "description": "Reagents at 7-day values; null when a reagent is unpriced." },
                "costNow": { "type": ["integer", "null"], "description": "Reagents at the newest snapshot's floors; null unless every reagent is listed." },
                "gross": { "type": ["integer", "null"], "description": "Output value x makes, before the house cut." },
                "profit": { "type": ["integer", "null"], "description": "Gross - cut - cost." },
                "expected": { "type": ["number", "null"], "description": "Profit x sale rate — the ranking key." },
                "roi": { "type": ["number", "null"], "description": "Profit / cost." }
            }
        },
        "OverviewResponse": {
            "description": concat!("With ?category=, the bare shape below (plus a category echo). ",
                                   "Without it, an object keyed by category: {\"item\": {…}, \"commodity\": {…}, \"pet\": {…}}."),
            "type": "object",
            "properties": {
                "category": { "type": "string", "enum": ["item", "commodity", "pet"] },
                "moversUp": { "type": "array", "items": { "$ref": "#/components/schemas/MoverRow" } },
                "moversDown": { "type": "array", "items": { "$ref": "#/components/schemas/MoverRow" } },
                "mostActive": { "type": "array", "items": { "$ref": "#/components/schemas/MoverRow" } }
            }
        },
        "MoverRow": {
            "type": "object",
            "properties": {
                "item": { "$ref": "#/components/schemas/ItemRef" },
                "category": { "type": "string", "enum": ["item", "commodity", "pet"] },
                "medianNow": { "type": "integer" },
                "medianPrev": { "type": "number", "description": "7-day weighted median (copper)." },
                "change": { "type": "number", "description": "medianNow/medianPrev - 1." },
                "churnPerDay": { "type": "number" }
            }
        },
        "SearchResponse": {
            "type": "object",
            "properties": {
                "offset": { "type": "integer" },
                "limit": { "type": "integer" },
                "hasMore": { "type": "boolean" },
                "hits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "description": "Item hits carry itemID/name/itemClass/quality; pet hits speciesID/name.",
                        "properties": {
                            "type": { "type": "string", "enum": ["item", "pet"] },
                            "itemID": { "type": "integer" },
                            "speciesID": { "type": "integer" },
                            "name": { "type": "string" },
                            "itemClass": { "type": ["string", "null"] },
                            "quality": { "type": ["string", "null"] }
                        }
                    }
                }
            }
        },
        "EndpointIndex": {
            "type": "object",
            "properties": {
                "service": { "type": "string" },
                "version": { "type": "string" },
                "description": { "type": "string", "description": "Units and conventions (copper, UTC day numbers)." },
                "openapi": { "type": "string", "description": "Link to /api/openapi.json." },
                "docs": { "type": "string" },
                "feeds": { "type": "array", "items": { "type": "string" } },
                "endpoints": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "operations": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "method": { "type": "string" },
                                        "summary": { "type": "string" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

/// `$ref` variant of [`ok_json`] for named component schemas.
fn ok_json_ref(name: &str) -> Value {
    ok_json(json!({ "$ref": format!("#/components/schemas/{name}") }))
}

/// `GET /api/docs` — the human/agent-readable reference, server-rendered from
/// [`spec`]: no JavaScript, no external assets, and it can never disagree
/// with the served document.
pub(crate) async fn docs_page() -> Response {
    render::page("api docs", "", docs_body(), None)
}

/// The rendered body is static — build it once and hand out the cached text.
fn docs_body() -> &'static String {
    static BODY: OnceLock<String> = OnceLock::new();
    BODY.get_or_init(build_docs_body)
}

#[allow(clippy::too_many_lines)]
fn build_docs_body() -> String {
    let spec = spec();
    let mut body = String::from(
        "<p class=\"muted\">Machine-readable: <a href=\"/api/openapi.json\">/api/openapi.json</a> \
         (OpenAPI 3.1) · <a href=\"/api\">/api</a> (endpoint directory). \
         Reads are unauthenticated; mutations need <code>Authorization: Bearer &lt;token&gt;</code> 
         when token authentication is active (the web UI collects the token via its key 
         button — held in sessionStorage for the browser tab, never persisted). No rate limiting. 
         All prices are integers in <strong>copper</strong> (10,000 copper = 1 gold); \
         <code>day</code> values are UTC day numbers (days since 1970-01-01).</p>\
         <div class=\"card\"><h3>python quickstart</h3>\
         <pre class=\"mono\">",
    );
    let _ = write!(
        body,
        "{}",
        render::html_escape(concat!(
            "import requests\n",
            "\n",
            "BASE = \"http://127.0.0.1:8080\"   # [server].bind\n",
            "TARGET = \"your-target\"           # see /api/health\n",
            "\n",
            "# what can I ask?\n",
            "print(requests.get(f\"{BASE}/api\").json())\n",
            "\n",
            "# live deals: only what the newest snapshot still lists, whole\n",
            "# ranking for one market category (omit category to get all three)\n",
            "resp = requests.get(f\"{BASE}/api/opportunities\",\n",
            "                     params={\"target\": TARGET, \"category\": \"commodity\",\n",
            "                             \"limit\": 1000}).json()\n",
            "deals, snapshot = resp[\"opportunities\"], resp[\"snapshot\"]\n",
            "\n",
            "# deals seen today that are already gone (bought or cancelled)\n",
            "missed = requests.get(f\"{BASE}/api/missed-deals\",\n",
            "                      params={\"target\": TARGET, \"category\": \"commodity\"\",\n",
            "                              \"min_discount\": 25}).json()[\"missed\"]\n",
            "\n",
            "# which manual recipes are worth crafting here (see recipes.example.toml)\n",
            "crafts = requests.get(f\"{BASE}/api/crafts\",\n",
            "                    params={\"target\": TARGET}).json()[\"crafts\"]\n",
            "\n",
            "# bulk day-level history for the whole market (all categories)\n",
            "rows = requests.get(f\"{BASE}/api/history\",\n",
            "                    params={\"target\": TARGET, \"days\": 7}).json()[\"rows\"]\n",
            "\n",
            "# exact metric blocks for the most interesting deal (pets use the pet twin)\n",
            "top = deals[0][\"item\"]\n",
            "top_category = deals[0][\"category\"]\n",
            "if top[\"type\"] == \"pet\":\n",
            "    blocks = requests.get(\n",
            "        f\"{BASE}/api/items/{TARGET}/pet/{top['speciesID']}/metrics\",\n",
            "        params={\"level\": top[\"level\"], \"quality\": top[\"qualityID\"],\n",
            "                \"breed\": top[\"breedID\"], \"span\": \"d1,d7,d30\"}).json()\n",
            "else:\n",
            "    blocks = requests.get(f\"{BASE}/api/items/{TARGET}/{top['itemID']}/metrics\",\n",
            "                          params={\"category\": top_category,\n",
            "                                  \"span\": \"d1,d7,d30\"}).json()"
        ))
    );
    body.push_str(
        "</pre></div>\
         <div class=\"scroll-x\"><table id=\"api-docs\"><thead><tr>\
         <th>method</th><th>endpoint</th><th>summary</th><th>details</th></tr></thead><tbody>",
    );
    // Deterministic order regardless of the JSON map's internal ordering.
    let mut paths: Vec<(&String, &Value)> =
        spec["paths"].as_object().map(|map| map.iter().collect()).unwrap_or_default();
    paths.sort_by_key(|(path, _)| (*path).clone());
    for (path, operations) in paths {
        let Some(ops) = operations.as_object() else { continue };
        for (method, op) in ops {
            let summary = op["summary"].as_str().unwrap_or_default();
            let description = op["description"].as_str().unwrap_or_default();
            let mut details = String::new();
            if let Some(params) = op["parameters"].as_array() {
                for param in params {
                    let name = param["name"].as_str().unwrap_or_default();
                    let location = param["in"].as_str().unwrap_or_default();
                    let required = param["required"].as_bool().unwrap_or(false);
                    let description = param["description"].as_str().unwrap_or_default();
                    let _ = write!(
                        details,
                        "<code>{}</code> ({}{}): {}<br>",
                        render::html_escape(name),
                        render::html_escape(location),
                        if required { ", required" } else { "" },
                        render::html_escape(description),
                    );
                }
            }
            if op["security"].is_array() {
                details.push_str("requires bearer token<br>");
            }
            if !description.is_empty() {
                let _ = write!(details, "{}", render::html_escape(description));
            }
            let _ = write!(
                body,
                "<tr><td>{}</td><td class=\"mono\">{}</td><td>{}</td><td>{}</td></tr>",
                render::html_escape(&method.to_uppercase()),
                render::html_escape(path),
                render::html_escape(summary),
                details,
            );
        }
    }
    body.push_str("</tbody></table></div>");
    body
}

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
                "Ranked deals",
                "Snipe candidates sorted by score descending: discount (robust p05 floor vs 7-day median), 14-day sale-rate reliability, churn and stability. limit is uncapped enough for whole-ranking pulls (scores are ranking-only; deal gates use exact metric blocks).",
                vec![
                    target.clone(),
                    category_param(),
                    json!({ "name": "min_churn", "in": "query", "required": false, "description": "Only items with at least this many unique listings churning per day rank (per category). Non-finite values fall back to the default.", "schema": { "type": "number", "default": 1.0 } }),
                    json!({ "name": "limit", "in": "query", "required": false, "description": "Maximum rows per category; 0 is treated as 1.", "schema": { "type": "integer", "default": 50 } }),
                ],
                ok_json_ref("OpportunitiesResponse"),
            ),
        },
        "/api/history": {
            "get": get_op(
                "Bulk day-level market history",
                concat!(
                    "One call, whole market: per-item-per-day aggregate rows for every item/pet with fold data in the window. ",
                    "This is the analysis-grade bulk payload — feed it straight into pandas/a script. ",
                    "Days are UTC day numbers (days since 1970-01-01). min/p05/median/mad are listing-level nearest-rank stats in copper. ",
                    "Rows arrive ordered by day, then item kind, then item id."
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
                "description": "Per-item metric blocks and identities for the whole market — the addon feed in JSON form.",
                "parameters": vec![feed_target_param()],
                "responses": {
                    "200": { "description": "OK", "content": { "application/json": { "schema": { "type": "object" } } } },
                    "400": { "description": "Unknown target." },
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
                "description": "Slim feed covering exactly the target's watchlist entries.",
                "parameters": vec![feed_target_param()],
                "responses": {
                    "200": { "description": "OK", "content": { "text/x-lua": { "schema": { "type": "string" } } } },
                    "400": { "description": "Unknown target." },
                    "404": { "description": "No feed published yet for the target." }
                }
            }),
        },
    })
}

/// Watchlist and alert management, plus the token-gated triggers. Query
/// parameters live on the POSTs too: `evaluate` requires one, `fetch` takes
/// an optional one.
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
                        "(price-below, below-floor, sale-rate-above)."),
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
                                   "(median fallback when qty is 0), saleRate is the trailing-14d sell-through fraction."),
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
                "saleRate": { "type": ["number", "null"], "description": "0..1; null when no sightings exist." }
            }
        },
        "Opportunity": {
            "type": "object",
            "properties": {
                "item": { "$ref": "#/components/schemas/ItemRef" },
                "category": { "type": "string", "enum": ["item", "commodity", "pet"], "description": "The market this row ranks in." },
                "median7": { "type": "integer", "description": "Count-weighted 7-day median unit price (copper)." },
                "floorNow": { "type": "integer", "description": "Cheapest listing of the newest day (copper)." },
                "discount": { "type": "number", "description": "1 - p05(newest day)/median7, clamped to [0,1] — robust against single outlier listings." },
                "reliability": { "type": "number", "description": "14-day sale-through fraction 0..1." },
                "churnPerDay": { "type": "number", "description": "Average unique listings per day across the 7-day window." },
                "stability": { "type": "number", "description": "1 - min(1, mad7/median7)." },
                "samples7": { "type": "integer", "description": "Unique listings folded over the window." },
                "confidence": { "type": "number", "description": "min(1, samples7/[opportunities].min_samples)." },
                "score": { "type": "number", "description": "Weighted ranking score — for RANKING only." }
            }
        },
        "OpportunitiesResponse": {
            "description": concat!("With ?category=, the bare shape below (plus a category echo). ",
                                   "Without it, an object keyed by category: {\"item\": {…}, \"commodity\": {…}, \"pet\": {…}}."),
            "type": "object",
            "properties": {
                "category": { "type": "string", "enum": ["item", "commodity", "pet"] },
                "opportunities": { "type": "array", "items": { "$ref": "#/components/schemas/Opportunity" } }
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
fn spec_schemas_secondary() -> Value {
    json!({
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
         Reads are unauthenticated; mutations need <code>Authorization: Bearer &lt;token&gt;</code> \
         when token authentication is active. No rate limiting. \
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
            "# ranked deals, whole ranking for one market category\n",
            "# (omit category to get all three, keyed by category)\n",
            "deals = requests.get(f\"{BASE}/api/opportunities\",\n",
            "                     params={\"target\": TARGET, \"category\": \"commodity\",\n",
            "                             \"limit\": 1000}).json()[\"opportunities\"]\n",
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

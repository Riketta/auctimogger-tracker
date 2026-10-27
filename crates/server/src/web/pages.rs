//! HTML pages: dashboard, snapshots, item search/browse, item & pet detail,
//! opportunities, market overview, watchlist and the terminology page.
//! Every page is complete server-rendered HTML; `app.js` only enhances it.

// Page renderers are linear HTML builders: each handler assembles one page
// top-to-bottom. Splitting them into artificial sub-functions just to satisfy
// a line counter would hurt readability, so the pedantic length lint is
// relaxed module-wide (the SecOutfall-style scoped-allow precedent).
#![allow(clippy::too_many_lines)]
#![allow(clippy::unused_async)]

use std::{
    collections::{
        HashMap,
        HashSet,
    },
    fmt::Write as _,
    sync::Arc,
};

use amtracker_core::{
    domain::{
        utc_day,
        Category,
        Flavor,
        ItemRef,
        MetricBlock,
        PayloadKind,
        SearchHit,
        Span,
        TargetId,
    },
    error::StoreError,
    metrics::block_from_histograms,
};
use axum::{
    extract::{
        Path,
        Query,
        State,
    },
    http::StatusCode,
    response::{
        IntoResponse,
        Response,
    },
};

use super::{
    category_param,
    health_blocking,
    mutations_open,
    render::{
        self,
        component_bar,
        distribution_svg,
        freshness_badge,
        html_escape,
        human_age,
        human_in,
        human_time,
        item_cell,
        money,
        outcome_badge,
        page,
        quality_chip,
        sparkline,
        target_switcher,
        tip,
    },
    target_param,
    AppState,
};

/// What the item detail page renders from the store, in one store pass: the
/// item-scoped day rows, the per-span metric blocks, the per-day chart rows
/// (via the golden single-day engine) and today's price-distribution
/// buckets.
type ItemPageData = (
    Vec<amtracker_core::domain::ItemDayRow>,
    Vec<(Span, Option<MetricBlock>)>,
    Vec<serde_json::Value>,
    std::collections::BTreeMap<i64, amtracker_core::domain::PriceBucket>,
    // Same identity on the other configured targets: (target, d7 block).
    Vec<(TargetId, Option<MetricBlock>)>,
);

/// Watchlist membership key: kind-prefixed identity string, so items and
/// pets (and different pet variants) can never collide.
fn item_key(item: &ItemRef) -> String {
    match item {
        ItemRef::Item { item_id } => format!("i:{item_id}"),
        ItemRef::Pet { species, level, quality, breed } => {
            format!("p:{species}:{level}:{quality}:{breed}")
        }
    }
}

/// JSON body for the watchlist mutation endpoints, ready for the
/// `data-watch-*` button attributes.
fn watch_payload(target: &TargetId, item: &ItemRef) -> String {
    let value = match item {
        ItemRef::Item { item_id } => serde_json::json!({
            "target": target.as_str(), "type": "item", "itemID": item_id
        }),
        ItemRef::Pet { species, level, quality, breed } => serde_json::json!({
            "target": target.as_str(), "type": "pet", "speciesID": species,
            "level": level, "qualityID": quality, "breedID": breed
        }),
    };
    value.to_string()
}

/// Watch/unwatch button (open-mutation mode only — protected instances never
/// render mutation controls).
fn watch_button(open: bool, watched: bool, target: &TargetId, item: &ItemRef) -> String {
    if !open {
        return String::new();
    }
    let payload = html_escape(&watch_payload(target, item));
    if watched {
        format!(
            "<button class=\"btn btn-sm btn-danger\" data-watch-remove='{payload}' \
             title=\"remove from watchlist (drives feed-slim.lua)\">unwatch</button>"
        )
    } else {
        format!(
            "<button class=\"btn btn-sm\" data-watch-add='{payload}' \
             title=\"add to watchlist (drives feed-slim.lua)\">watch</button>"
        )
    }
}

/// UTC calendar date label for a day number (chart axes).
fn day_label(day: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(day * 86_400)
        .ok()
        .and_then(|ts| ts.format(&time::macros::format_description!("[year]-[month]-[day]")).ok())
        .unwrap_or_else(|| day.to_string())
}

// ---------------------------------------------------------------- dashboard

/// `GET /` — fleet overview: KPIs, per-target health cards, feed links,
/// recent ingest cycles. Auto-reloads every 30s (JS-enhanced).
pub(crate) async fn dashboard(State(state): State<Arc<AppState>>) -> Response {
    let health = match health_blocking(&state).await {
        Ok(health) => health,
        Err(response) => return *response,
    };
    let counts = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        move || state.catalog.counts()
    });
    let counts = match counts.await {
        Ok(Ok(counts)) => counts,
        Ok(Err(error)) => return render::store_error_page(error).await,
        Err(error) => {
            return render::store_error_page(StoreError::Backend(error.to_string())).await
        }
    };
    let cycles = state.recent_cycles.snapshot();
    let open = mutations_open(&state.config);
    // The scheduler's live fetch schedule, keyed per (target, kind) for the
    // target cards below.
    let schedule: HashMap<(String, PayloadKind), crate::scheduler::ScheduleEntry> = {
        let timers = state.timers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::scheduler::schedule_view(&state.config.targets, &timers, std::time::Instant::now())
            .into_iter()
            .map(|entry| ((entry.target.clone(), entry.kind), entry))
            .collect()
    };

    let mut body = String::from("<div class=\"cards\">");
    let _ = write!(
        body,
        "<div class=\"card\"><h3>{}</h3><div class=\"kpi\">{} <small>configured</small></div></div>",
        tip("targets", "Configured market sources (Blizzard connected realms or conforming static endpoints)."),
        state.config.targets.len()
    );
    let _ = write!(
        body,
        "<div class=\"card\"><h3>{}</h3><div class=\"kpi\">{} <small>items</small> · {} <small>pets</small></div>\
         <p class=\"muted\" style=\"margin:0\">{} {}</p></div>",
        tip("named catalog", "How many item and pet-species names have been resolved into the search catalog (via the Blizzard item/pet APIs)."),
        counts.items,
        counts.pets,
        tip("items", "Named item records in the catalog."),
        tip("pets", "Named battle-pet species records in the catalog.")
    );
    let fresh = health
        .iter()
        .flat_map(|t| t.kinds.iter())
        .filter(|k| k.age_seconds.is_some_and(|age| age < 3600))
        .count();
    let _ = write!(
        body,
        "<div class=\"card\"><h3>{}</h3><div class=\"kpi\">{} <small>/ {} snapshots fresh</small></div>\
         <p class=\"muted\" style=\"margin:0\">{} {}</p></div>",
        tip("freshness", "A snapshot counts as fresh when it was fetched within the last hour."),
        fresh,
        health.iter().map(|t| t.kinds.len()).sum::<usize>(),
        tip("snapshots", "One row per configured target and payload kind (commodities, item auctions)."),
        tip("fresh", "Fetched within the last hour.")
    );
    let _ = write!(
        body,
        "<div class=\"card\"><h3>{}</h3><div class=\"kpi\">{} <small>recent cycles</small></div></div>",
        tip("recent cycles", "The last ingest cycles run by the scheduler or via “fetch now” — success, 304, failures and panics alike."),
        cycles.len()
    );
    body.push_str("</div>");

    body.push_str("<h2>targets</h2><div class=\"cards\">");
    for target in &state.config.targets {
        let _ = write!(
            body,
            "<div class=\"card\"><h3><a href=\"/opportunities?target={}\">{}</a></h3>",
            html_escape(target.id.as_str()),
            html_escape(target.id.as_str())
        );
        let _ = write!(
            body,
            "<p class=\"muted\" style=\"margin:.1rem 0 .5rem\">{}</p>",
            html_escape(&target.describe())
        );
        let target_health = health.iter().find(|h| h.target == target.id);
        if let Some(target_health) = target_health {
            for kind in &target_health.kinds {
                let interval = match kind.kind {
                    PayloadKind::Commodities => target.poll.commodities_secs,
                    PayloadKind::Auctions => target.poll.auctions_secs,
                };
                let _ = write!(
                    body,
                    "<div class=\"kv\"><span class=\"k\">{}</span><span class=\"v\">{}</span></div>",
                    tip(kind.kind.as_str(), "Payload kind: the commodity auction house dump, or the per-item auction house dump."),
                    freshness_badge(kind.age_seconds, interval)
                );
                let schedule_row = schedule.get(&(target.id.0.clone(), kind.kind));
                let _ = write!(
                    body,
                    "<div class=\"kv\"><span class=\"k\">{}</span><span class=\"v\">{}</span></div>",
                    tip(
                        "next fetch",
                        "When the scheduler polls this payload next. The first fetch after a \
                         restart is jittered across the poll interval; a rate-limited poll \
                         parks until the source's Retry-After. The page reloads every 30 s."
                    ),
                    next_fetch_cell(schedule_row)
                );
                if let Some(meta) = &kind.latest {
                    let _ = write!(
                        body,
                        "<div class=\"kv\"><span class=\"k\">{}</span><span class=\"v\">{} <span class=\"muted\">({})</span></span></div>",
                        tip("latest fetch", "When this snapshot was stored, and its age."),
                        human_time(meta.fetched_at),
                        human_age(kind.age_seconds.unwrap_or(0))
                    );
                    let dropped_note = if meta.dropped_count > 0 {
                        format!(
                            " <span class=\"bad\">{}</span>",
                            tip("dropped!", "Some rows failed the strict ACL parse and were counted, never ingested. A persistent non-zero value means the source format drifted.")
                        )
                    } else {
                        String::new()
                    };
                    let _ = write!(
                        body,
                        "<div class=\"kv\"><span class=\"k\">{}</span><span class=\"v num\">{} rows · {}{}</span></div>",
                        tip("entries", "Listings that survived the strict parse into this snapshot."),
                        meta.entry_count,
                        meta.dropped_count,
                        dropped_note
                    );
                }
            }
        }
        let fetch_button = if open {
            format!(
                "<button class=\"btn btn-sm\" data-fetch-now=\"{}\" \
                 title=\"run one ingest cycle for this target right now\">fetch now</button>",
                html_escape(target.id.as_str())
            )
        } else {
            String::new()
        };
        let _ = write!(
            body,
            "<div style=\"margin-top:.6rem;display:flex;gap:.4rem;flex-wrap:wrap\">{fetch_button}</div>"
        );
        body.push_str("</div>");
    }
    body.push_str("</div>");

    body.push_str("<h2>feeds</h2><div class=\"cards\">");
    for target in &state.config.targets {
        let slug = html_escape(target.id.as_str());
        let _ = write!(
            body,
            "<div class=\"card\"><h3>{slug}</h3><p class=\"feed-links\" style=\"margin:.2rem 0\">\
             <a href=\"/feed.lua?target={slug}\">feed.lua</a> · \
             <a href=\"/feed-slim.lua?target={slug}\">feed-slim.lua</a> · \
             <a href=\"/api/feed.json?target={slug}\">feed.json</a></p>\
             <p class=\"muted\" style=\"margin:.2rem 0 0\">{} {}</p></div>",
            tip("feed.lua", "The full tracker feed consumed by the AuctiMogger addon; download into AuctiMogger/AuctiMoggerTracker/ and /reload."),
            tip("feed-slim.lua", "Watchlist-only feed — the same contract, restricted to the items you watch on this target.")
        );
    }
    body.push_str("</div>");

    body.push_str("<h2>recent cycles</h2>");
    if cycles.is_empty() {
        body.push_str(
            "<div class=\"empty\">No ingest cycles have run since this process started. \
             They appear here as the scheduler ticks (or use “fetch now”).</div>",
        );
    } else {
        body.push_str("<div class=\"scroll-x\"><table id=\"cycles\"><thead><tr>");
        for (label, tip_text) in [
            ("when", "When the cycle finished (hover for the absolute UTC time)."),
            ("target", "Market source the cycle ran for."),
            ("kind", "Payload kind polled by this cycle."),
            ("outcome", "fresh = new data stored · not-modified = source answered 304 · empty = valid payload with zero listings · failed/panicked = error (see message)."),
            ("entries", "Listings accepted into the snapshot."),
            ("dropped", "Listings rejected by the strict parse (counted, never ingested)."),
            ("folds", "Distinct items whose per-day fold changed."),
            ("credits", "Sell-through credits granted by closing vanished listings."),
            ("message", "Error detail for failed or panicked cycles."),
        ] {
            let _ = write!(body, "<th data-sortable>{}</th>", tip(label, tip_text));
        }
        body.push_str("</tr></thead><tbody>");
        for cycle in cycles.iter().rev() {
            let _ = write!(
                body,
                "<tr><td title=\"{}\">{}</td><td>{}</td><td>{}</td><td>{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 <td class=\"muted\">{}</td></tr>",
                html_escape(&human_time(cycle.at)),
                html_escape(&human_age(state.clock.now().saturating_sub(cycle.at))),
                html_escape(&cycle.target),
                html_escape(&cycle.kind),
                outcome_badge(&cycle.outcome),
                cycle.entries,
                cycle.entries,
                cycle.dropped,
                cycle.dropped,
                cycle.folds,
                cycle.folds,
                cycle.credits,
                cycle.credits,
                html_escape(cycle.error.as_deref().unwrap_or(""))
            );
        }
        body.push_str("</tbody></table></div>");
        let _ = write!(
            body,
            "<p><button class=\"btn btn-sm\" data-csv=\"cycles\">export CSV</button> \
             <span class=\"muted\" data-autoreload-note>page auto-reloads every 30s</span></p>"
        );
    }

    page("dashboard", "dashboard", &body, None)
}

/// The dashboard's "next fetch" cell: a countdown, an in-flight badge, a
/// rate-limit park notice, or a dash when the scheduler has not seeded the
/// timer yet (the board fills during startup).
fn next_fetch_cell(entry: Option<&crate::scheduler::ScheduleEntry>) -> String {
    let Some(entry) = entry else {
        return "<span class=\"muted\">—</span>".into();
    };
    if entry.running {
        return format!(
            "<span class=\"badge ok\" tabindex=\"0\" data-tip=\"{}\">running now</span>",
            html_escape(
                "A cycle for this payload is in flight right now (scheduler or manual fetch).",
            )
        );
    }
    if let Some(parked) = entry.backoff_in_secs {
        return format!(
            "<span class=\"badge warn\" tabindex=\"0\" data-tip=\"{}\">parked · retry in {}</span>",
            html_escape(
                "The source answered rate-limited (429) with a Retry-After; the poll is \
                 parked until then, then resumes on its normal cadence.",
            ),
            human_in(parked)
        );
    }
    if entry.next_due_in_secs == 0 {
        return format!(
            "<span class=\"badge warn\" tabindex=\"0\" data-tip=\"{}\">due now</span>",
            html_escape(
                "The poll interval has elapsed; the scheduler runs it on its next 30 s tick.",
            )
        );
    }
    format!(
        "<span tabindex=\"0\" data-tip=\"{}\">in {}</span>",
        html_escape(
            "Countdown to the next scheduled poll. The page auto-reloads every 30 s, so \
             this stays current without a manual refresh."
        ),
        human_in(entry.next_due_in_secs)
    )
}

// ---------------------------------------------------------------- snapshots

/// `GET /snapshots` — recent snapshot history per target and payload kind.
pub(crate) async fn snapshots_page(State(state): State<Arc<AppState>>) -> Response {
    let health = match health_blocking(&state).await {
        Ok(health) => health,
        Err(response) => return *response,
    };
    // One blocking pass per (target, kind) pair; the history ring is small
    // (8 per kind).
    let histories = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let targets: Vec<TargetId> = state.config.targets.iter().map(|t| t.id.clone()).collect();
        move || -> Result<Vec<(TargetId, PayloadKind, Vec<amtracker_core::domain::SnapshotMeta>)>, StoreError> {
            let mut out = Vec::new();
            for target in &targets {
                for kind in [PayloadKind::Commodities, PayloadKind::Auctions] {
                    let rows = state.snapshots.list_recent(target, kind, 8)?;
                    out.push((target.clone(), kind, rows));
                }
            }
            Ok(out)
        }
    });
    let histories = match histories.await {
        Ok(Ok(histories)) => histories,
        Ok(Err(error)) => return render::store_error_page(error).await,
        Err(error) => {
            return render::store_error_page(StoreError::Backend(error.to_string())).await
        }
    };

    let mut body = String::new();
    for (target, kind, rows) in &histories {
        let interval_target = state.config.find_target(target.as_str());
        let interval = interval_target.map_or(300, |t| match kind {
            PayloadKind::Commodities => t.poll.commodities_secs,
            PayloadKind::Auctions => t.poll.auctions_secs,
        });
        let health_row = health
            .iter()
            .find(|h| &h.target == target)
            .and_then(|h| h.kinds.iter().find(|k| k.kind == *kind));
        let _ = write!(
            body,
            "<h2>{} · {} {}</h2>",
            html_escape(target.as_str()),
            tip(
                kind.as_str(),
                "Payload kind: commodities (commodity AH dump) or auctions (per-item AH dump)."
            ),
            freshness_badge(health_row.and_then(|k| k.age_seconds), interval)
        );
        if rows.is_empty() {
            body.push_str("<div class=\"empty\">no snapshots stored yet — run a fetch cycle</div>");
            continue;
        }
        body.push_str("<div class=\"scroll-x\"><table><thead><tr>");
        for (label, tip_text) in [
            ("id", "Storage-assigned monotonic snapshot id."),
            ("fetched at", "When the tracker stored this snapshot (UTC)."),
            ("source modified", "The source's Last-Modified header — the addon's freshness anchor. “—” when the source does not send it."),
            ("entries", "Listings that survived the strict parse."),
            ("dropped", "Listings rejected by the strict parse (counted, never ingested). Hover for the per-reason breakdown when the source classifies them."),
            ("status", "Storage status of the snapshot."),
        ] {
            let _ = write!(body, "<th data-sortable>{}</th>", tip(label, tip_text));
        }
        body.push_str("</tr></thead><tbody>");
        for meta in rows {
            // Per-reason breakdown as the hover title: "reason ×count",
            // most frequent first; empty when the source drops nothing or
            // does not classify (pre-v5 rows read empty too).
            let mut reasons: Vec<(&String, &u64)> = meta.dropped_reasons.iter().collect();
            reasons.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            let breakdown = reasons
                .iter()
                .map(|(reason, count)| format!("{reason} ×{count}"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = write!(
                body,
                "<tr><td class=\"num\" data-v=\"{}\">{}</td><td>{}</td><td>{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 <td class=\"num\" data-v=\"{}\" title=\"{}\">{}</td><td>{}</td></tr>",
                meta.id,
                meta.id,
                human_time(meta.fetched_at),
                meta.source_last_modified.map_or_else(|| "—".into(), human_time),
                meta.entry_count,
                meta.entry_count,
                meta.dropped_count,
                html_escape(&breakdown),
                meta.dropped_count,
                html_escape(meta.status.as_str())
            );
        }
        body.push_str("</tbody></table></div>");
    }
    page("snapshots", "snapshots", &body, None)
}

// ------------------------------------------------------------ items search

/// `GET /items?q=&after_name=&after_kind=&after_id=` — name search with
/// completions plus an alphabetical catalog browser (keyset-paginated).
pub(crate) async fn items_page(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match target_param(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let query = params.get("q").cloned().unwrap_or_default();
    let switcher = target_switcher(&state.config, "/items", &target, &[("q", &query)]);

    let mut body = format!(
        "<div class=\"searchbox\"><input type=\"search\" id=\"q\" \
         placeholder=\"search item or pet name…\" autocomplete=\"off\" \
         data-search-endpoint=\"/api/search\" data-search-target=\"{}\" /></div>\
         <p class=\"muted\">type at least 2 characters — completions cover named items and pet species \
         (names resolve through the Wowhead-first source chain as items are seen or visited)</p>",
        html_escape(target.as_str())
    );

    if !query.trim().is_empty() {
        let hits = tokio::task::spawn_blocking({
            let state = Arc::clone(&state);
            let query = query.clone();
            move || state.search.search(&query, 25, 0)
        });
        match hits.await {
            Ok(Ok(hits)) => {
                if hits.is_empty() {
                    body.push_str(
                        "<div class=\"empty\">nothing named like that is in the catalog yet — \
                         names appear as the tracker sees items in dumps or as pages are visited</div>",
                    );
                } else {
                    body.push_str(
                        "<h2>results</h2><div class=\"scroll-x\"><table id=\"results\"><thead><tr>",
                    );
                    for (label, tip_text) in [
                        ("name", "Item or pet-species display name."),
                        ("kind", "item = tradable item · pet = battle-pet species (detail pages pick the canonical level-1 variant)."),
                        ("class", "Item class label from the API, when known."),
                        ("quality", "Quality label, color-coded with the loot convention."),
                        ("id", "Item id or pet species id."),
                    ] {
                        let _ = write!(body, "<th data-sortable>{}</th>", tip(label, tip_text));
                    }
                    body.push_str("</tr></thead><tbody>");
                    for hit in &hits {
                        match hit {
                            SearchHit::Item(record) => {
                                let _ = write!(
                                    body,
                                    "<tr><td><a href=\"/items/{}/{}\">{}</a></td><td>item</td>\
                                     <td>{}</td><td>{}</td><td class=\"num\" data-v=\"{}\">{}</td></tr>",
                                    html_escape(target.as_str()),
                                    record.item_id,
                                    html_escape(&record.name),
                                    html_escape(record.item_class.as_deref().unwrap_or("—")),
                                    quality_chip(record.quality.as_deref()),
                                    record.item_id,
                                    record.item_id
                                );
                            }
                            SearchHit::Pet(record) => {
                                let _ = write!(
                                    body,
                                    "<tr><td><a href=\"/items/{}/pet/{}\">{}</a></td>\
                                     <td><span class=\"badge pet\">pet</span></td><td>—</td><td>—</td>\
                                     <td class=\"num\" data-v=\"{}\">{}</td></tr>",
                                    html_escape(target.as_str()),
                                    record.species_id,
                                    html_escape(&record.name),
                                    record.species_id,
                                    record.species_id
                                );
                            }
                        }
                    }
                    body.push_str("</tbody></table></div>");
                }
            }
            Ok(Err(error)) => return render::store_error_page(error).await,
            Err(error) => {
                return render::store_error_page(StoreError::Backend(error.to_string())).await
            }
        }
    }

    // Alphabetical browse below the search (keyset pagination).
    // A present-but-unparseable `after_id` is a malformed link: rejecting it
    // beats silently skipping every same-name row below the i64::MAX
    // tiebreaker (an absent id keeps the legacy tiebreak for name-only
    // navigation).
    let after_id = match params.get("after_id") {
        None => None,
        Some(raw) => match raw.parse::<i64>() {
            Ok(id) => Some(id),
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "after_id must be an integer").into_response();
            }
        },
    };
    let cursor = params.get("after_name").map(|after_name| amtracker_core::domain::BrowseCursor {
        name: after_name.clone(),
        kind: params.get("after_kind").cloned().unwrap_or_else(|| "item".into()),
        id: after_id.unwrap_or(i64::MAX),
    });
    let browse = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        move || state.catalog.browse(cursor.as_ref(), 30)
    });
    match browse.await {
        Ok(Ok((hits, has_more))) => {
            body.push_str("<h2>browse catalog</h2>");
            if hits.is_empty() {
                body.push_str("<div class=\"empty\">the catalog is empty so far — visit an item page or wait for the tracker to see named items</div>");
            } else {
                body.push_str("<div class=\"scroll-x\"><table id=\"browse\"><thead><tr>");
                for (label, tip_text) in [
                    ("name", "Display name."),
                    ("kind", "item or pet species."),
                    ("id", "Item id or species id."),
                ] {
                    let _ = write!(body, "<th data-sortable>{}</th>", tip(label, tip_text));
                }
                body.push_str("</tr></thead><tbody>");
                for hit in &hits {
                    let (kind_badge, href, name, id) = match hit {
                        SearchHit::Item(record) => (
                            String::from("item"),
                            format!("/items/{}/{}", html_escape(target.as_str()), record.item_id),
                            record.name.clone(),
                            record.item_id,
                        ),
                        SearchHit::Pet(record) => (
                            String::from("<span class=\"badge pet\">pet</span>"),
                            format!(
                                "/items/{}/pet/{}",
                                html_escape(target.as_str()),
                                record.species_id
                            ),
                            record.name.clone(),
                            record.species_id,
                        ),
                    };
                    let _ = write!(
                        body,
                        "<tr><td><a href=\"{href}\">{}</a></td><td>{kind_badge}</td>\
                         <td class=\"num\" data-v=\"{id}\">{id}</td></tr>",
                        html_escape(&name)
                    );
                }
                body.push_str("</tbody></table></div>");
                if has_more {
                    if let Some(last) = hits.last() {
                        let (last_name, last_kind, last_id) = match last {
                            SearchHit::Item(record) => {
                                (record.name.clone(), "item", record.item_id)
                            }
                            SearchHit::Pet(record) => {
                                (record.name.clone(), "pet", record.species_id)
                            }
                        };
                        let _ = write!(
                            body,
                            "<div class=\"pagenav\"><a class=\"btn btn-sm\" href=\"/items?target={}&amp;after_name={}&amp;after_kind={}&amp;after_id={}\">next page →</a> <span class=\"muted\">alphabetical, 30 per page</span></div>",
                            html_escape(target.as_str()),
                            html_escape(&last_name),
                            last_kind,
                            last_id
                        );
                    }
                } else {
                    body.push_str("<p class=\"muted\">end of catalog</p>");
                }
                if params.contains_key("after_name") {
                    let _ = write!(
                        body,
                        "<div class=\"pagenav\"><a class=\"btn btn-sm\" href=\"/items?target={}\">↑ back to top</a></div>",
                        html_escape(target.as_str())
                    );
                }
            }
        }
        Ok(Err(error)) => return render::store_error_page(error).await,
        Err(error) => {
            return render::store_error_page(StoreError::Backend(error.to_string())).await
        }
    }

    page("items", "items", &body, Some(switcher))
}

// ------------------------------------------------------------- item detail

/// `GET /items/{target_id}/{item_id}` — one item's 30-day history.
pub(crate) async fn item_page(
    State(state): State<Arc<AppState>>,
    Path((target_id, item_id)): Path<(String, i64)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match super::known_target(&state.config, &target_id) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let category = match category_param(&params, Category::Item) {
        Ok(category) => category,
        Err(response) => return *response,
    };
    let item = ItemRef::Item { item_id };
    let name =
        resolve_item_name(&state, &super::target_flavor(&state.config, &target), item_id).await;
    item_detail_page(state, target, item, name, item_id.to_string(), String::new(), category).await
}

/// `GET /items/{target_id}/pet/{species_id}?level=&quality=&breed=` — one
/// battle-pet variant's history. Missing variant fields fall back to the
/// canonical level-1/quality-1/breed-0 tuple and say so on the page.
pub(crate) async fn pet_page(
    State(state): State<Arc<AppState>>,
    Path((target_id, species_id)): Path<(String, i64)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match super::known_target(&state.config, &target_id) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let level = params.get("level").and_then(|v| v.parse().ok()).unwrap_or(1);
    let quality = params.get("quality").and_then(|v| v.parse().ok()).unwrap_or(1);
    let breed = params.get("breed").and_then(|v| v.parse().ok()).unwrap_or(0);
    let defaulted = !params.contains_key("level")
        && !params.contains_key("quality")
        && !params.contains_key("breed");
    let item = ItemRef::Pet { species: species_id, level, quality, breed };
    let category = match category_param(&params, Category::Pet) {
        Ok(category) => category,
        Err(response) => return *response,
    };
    let name =
        resolve_pet_name(&state, &super::target_flavor(&state.config, &target), species_id).await;
    let note = if defaulted {
        format!(
            "<p class=\"muted\">showing the canonical variant ({}); links from watchlists or rankings carry the exact variant</p>",
            tip("level 1 · quality 1 · breed 0", "A search hit only knows the species, so the detail page defaults to the level-1, quality-1, breed-0 identity tuple. Data may live under other variants."),
        )
    } else {
        String::new()
    };
    item_detail_page(state, target, item, name, format!("pet {species_id}"), note, category).await
}

/// Resolves an item's display name (catalog first, then the item API) for
/// the target's game flavor. The name is auxiliary: failures degrade to a
/// placeholder.
async fn resolve_item_name(state: &Arc<AppState>, flavor: &Flavor, item_id: i64) -> Option<String> {
    let state2 = Arc::clone(state);
    let flavor2 = flavor.clone();
    let lookup = tokio::task::spawn_blocking(move || state2.catalog.ensure(&flavor2, item_id));
    match lookup.await {
        Ok(Ok(record)) => record.map(|r| r.name),
        Ok(Err(error)) => {
            tracing::warn!(item_id, error = %error, "item name lookup failed; rendering with placeholder");
            None
        }
        Err(error) => {
            tracing::warn!(item_id, error = %error, "item name lookup join failed");
            None
        }
    }
}

/// Resolves a pet species' display name (same degradation policy as items).
async fn resolve_pet_name(
    state: &Arc<AppState>,
    flavor: &Flavor,
    species_id: i64,
) -> Option<String> {
    let state2 = Arc::clone(state);
    let flavor2 = flavor.clone();
    let lookup =
        tokio::task::spawn_blocking(move || state2.catalog.ensure_pet(&flavor2, species_id));
    match lookup.await {
        Ok(Ok(record)) => record.map(|r| r.name),
        Ok(Err(error)) => {
            tracing::warn!(species_id, error = %error, "pet name lookup failed; rendering with placeholder");
            None
        }
        Err(error) => {
            tracing::warn!(species_id, error = %error, "pet name lookup join failed");
            None
        }
    }
}

/// Wall-clock cap for the render lane's inline lookups: page latency stays
/// bounded even during a source outage. Timed-out lookups keep running on
/// their blocking threads and land in the catalog whenever they finish, so
/// the next view (or the background warm-up) finds them cached.
const RENDER_RESOLVE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// The render lane's bounded name resolution for one table page: the first
/// `[catalog].render_resolve_max` distinct ids/species of `items` resolve
/// concurrently off the async runtime — cached ids cost one local read,
/// uncached ones hit the source once ever (the catalog caches). Ids beyond
/// the budget keep their raw labels until a later view; `0` disables inline
/// resolution (the background warm-up still fills the catalog over time).
async fn resolve_names(
    state: &Arc<AppState>,
    flavor: &Flavor,
    items: &[ItemRef],
) -> render::ItemNames {
    let mut names = render::ItemNames::default();
    let budget = usize::try_from(state.config.catalog.render_resolve_max).unwrap_or(0);
    if budget == 0 {
        return names;
    }
    // Distinct identities in first-appearance order (the table's top rows
    // are the ones worth the inline wait), capped at the budget.
    let mut seen_items = HashSet::new();
    let mut seen_pets = HashSet::new();
    let mut pending: Vec<ItemRef> = Vec::new();
    for item in items {
        let fresh = match item {
            ItemRef::Item { item_id } => seen_items.insert(*item_id),
            ItemRef::Pet { species, .. } => seen_pets.insert(*species),
        };
        if fresh {
            pending.push(item.clone());
            if pending.len() >= budget {
                break;
            }
        }
    }
    let lookups = pending
        .into_iter()
        .map(|item| {
            let state = Arc::clone(state);
            let flavor = flavor.clone();
            tokio::task::spawn_blocking(move || match &item {
                ItemRef::Item { item_id } => state
                    .catalog
                    .ensure(&flavor, *item_id)
                    .map(|record| record.map(|r| (item, r.name))),
                ItemRef::Pet { species, .. } => state
                    .catalog
                    .ensure_pet(&flavor, *species)
                    .map(|record| record.map(|r| (item, r.name))),
            })
        })
        .collect::<Vec<_>>();
    let mut failures = 0usize;
    for lookup in lookups {
        match tokio::time::timeout(RENDER_RESOLVE_DEADLINE, lookup).await {
            Ok(Ok(Ok(Some((item, name))))) => match &item {
                ItemRef::Item { item_id } => {
                    names.items.insert(*item_id, name);
                }
                ItemRef::Pet { species, .. } => {
                    names.pets.insert(*species, name);
                }
            },
            Ok(Ok(Ok(None))) => {}
            Ok(Ok(Err(_)) | Err(_)) | Err(_) => failures += 1,
        }
    }
    if failures > 0 {
        tracing::warn!(failures, "some item names failed to resolve; rendering placeholders");
    }
    names
}

/// The shared item/pet detail renderer: name header, 30-day band chart
/// (JSON island + server SVG fallback), today's price distribution, the
/// per-span metric blocks and the watch action — all within one market
/// category (the tabs switch between them; merged views would mix two
/// markets' evidence).
async fn item_detail_page(
    state: Arc<AppState>,
    target: TargetId,
    item: ItemRef,
    name: Option<String>,
    id_label: String,
    note: String,
    category: Category,
) -> Response {
    let title_raw = name.unwrap_or_else(|| format!("{} (name unknown)", render::item_label(&item)));
    let title = html_escape(&title_raw);

    // The whole page's store reads happen in ONE blocking pass: day rows,
    // per-span blocks, per-day chart rows and today's distribution. The
    // chart/distribution folds come out of the same loop; `metrics.block`
    // re-reads folds per span, but its block cache makes repeat views cheap,
    // and duplicating its cache + sale-rate plumbing here would risk engine
    // drift. Day labels stay numeric and become UTC dates after the await.
    let today = utc_day(state.clock.now());
    let since = today - 29;
    let page_data = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        let item = item.clone();
        move || -> Result<ItemPageData, StoreError> {
            let rows = state.history.item_day_rows_for_item(&target, &item, since, category)?;
            let mut days: Vec<i64> = rows.iter().map(|row| row.day).collect();
            days.sort_unstable();
            days.dedup();
            let mut chart_rows = Vec::with_capacity(days.len());
            let mut distribution = std::collections::BTreeMap::new();
            for day in days {
                let Some(fold) = state.history.load_fold(&target, &item, day, category)? else {
                    continue;
                };
                let histogram = fold.histogram();
                if day == today {
                    distribution = histogram.buckets.clone();
                }
                // Golden engine on the single-day fold: the chart's
                // percentiles are exactly what the addon would compute.
                if let Some(block) = block_from_histograms(std::slice::from_ref(&histogram), None) {
                    chart_rows.push(serde_json::json!({
                        "label": day,
                        "median": block.median,
                        "p05": block.p05,
                        "p75": block.p75,
                        "p95": block.p95,
                        "vwap": block.vwap,
                        "qty": block.qty_sum,
                        "n": block.n,
                    }));
                }
            }
            let mut blocks = Vec::with_capacity(4);
            for span in [Span::D1, Span::D7, Span::D14, Span::D30] {
                blocks.push((span, state.metrics.block(&target, &item, category, span)?));
            }
            // Cross-target comparison: the same identity + category on every
            // other configured target (cached d7 blocks — cheap by design).
            let mut peers = Vec::new();
            for other in state.config.targets.iter().filter(|t| t.id != target) {
                let block = state.metrics.block(&other.id, &item, category, Span::D7)?;
                peers.push((other.id.clone(), block));
            }
            Ok((rows, blocks, chart_rows, distribution, peers))
        }
    });
    let (rows, blocks, mut chart_rows, distribution, peers) = match page_data.await {
        Ok(Ok(data)) => data,
        Ok(Err(error)) => return render::store_error_page(error).await,
        Err(error) => {
            return render::store_error_page(StoreError::Backend(error.to_string())).await
        }
    };
    for row in &mut chart_rows {
        if let Some(day) = row.get("label").and_then(serde_json::Value::as_i64) {
            if let Some(obj) = row.as_object_mut() {
                obj.insert("label".into(), serde_json::Value::String(day_label(day)));
            }
        }
    }

    // Server-rendered fallback line (medians) + distribution chart.
    let mut days: Vec<&amtracker_core::domain::ItemDayRow> = rows.iter().collect();
    days.sort_by_key(|row| row.day);
    let medians: Vec<i64> = days.iter().map(|row| row.median).collect();
    let fallback = sparkline(&medians);

    let mut body = format!(
        "<h2>{title}</h2><p class=\"muted\">target <a href=\"/opportunities?target={}\">{}</a> · {} · {}</p>{note}",
        html_escape(target.as_str()),
        html_escape(target.as_str()),
        tip("identity", "The exact identity tuple the metrics engine keys on: items by id, pets by species + level + quality + breed."),
        html_escape(&id_label),
    );

    body.push_str("<h2>30-day price history</h2><div class=\"chart-card\">");
    if chart_rows.is_empty() {
        body.push_str(
            "<div class=\"empty\">no day folds in the last 30 days for this identity — \
                       data appears after the tracker sees it in a snapshot</div>",
        );
    } else {
        let _ = write!(
            body,
            "<div id=\"chart\"><div class=\"chart-wrap\">{fallback}</div></div>\
             <script type=\"application/json\" id=\"chart-data\">{}</script>\
             <div class=\"chart-legend\">\
             <span class=\"key\"><span class=\"swatch\" style=\"background:rgba(90,162,255,0.35)\"></span> {}</span>\
             <span class=\"key\"><span class=\"swatch\" style=\"background:#5aa2ff\"></span> {}</span>\
             <span class=\"key\"><span class=\"swatch vwap\"></span> {}</span>\
             </div>",
            json_island(&serde_json::json!({ "days": chart_rows.clone() })),
            tip("typical range", "p05–p95 band: 90% of observed prices fell inside it (the golden engine computes it per day)."),
            tip("median", "Day median of unique auctions, each counted once at its last observed price."),
            tip("vwap", "Volume-weighted average price: Σ(price × qty) ÷ Σqty over the day's auctions.")
        );
    }
    body.push_str("</div>");

    // Today's price distribution (server-rendered; no JS required); the
    // buckets arrived with the same store pass as everything else above.
    if !distribution.is_empty() {
        let _ = write!(
            body,
            "<h2>{} today</h2><div class=\"chart-card\">{}</div>",
            tip("price distribution", "Unique auctions folded so far today, bucketed by unit price. Bar height = number of listings at that price; hover a bar for details."),
            distribution_svg(&distribution)
        );
    }

    // Cross-target comparison: the same identity on every other configured
    // target, from cached d7 blocks. Commodities rows are region-global, so
    // same-region spreads there are expected to be ~zero — the note says so.
    if !peers.is_empty() {
        body.push_str(
            "<h2>across targets</h2><div class=\"scroll-x\"><table id=\"peers\"><thead><tr>",
        );
        for (label, tip_text) in [
            ("target", "Another configured market serving this identity."),
            ("median d7", "7-day median unit price on that target."),
            ("floor d7", "7-day p05 (deal floor) on that target."),
            (
                "Δ vs this",
                "How that target's median compares to this one: negative = cheaper there.",
            ),
            ("sale rate 14d", "Sell-through on that target."),
        ] {
            let _ = write!(body, "<th data-sortable>{}</th>", tip(label, tip_text));
        }
        body.push_str("</tr></thead><tbody>");
        // Display-only percentage math in f64 (rendering, not decisions).
        #[allow(clippy::cast_precision_loss)]
        let delta_pct =
            |other: i64, mine: i64| -> f64 { (other - mine) as f64 / mine as f64 * 100.0 };
        let here = blocks.iter().find(|(span, _)| *span == Span::D7).and_then(|(_, b)| b.as_ref());
        for (peer_target, block) in &peers {
            let Some(block) = block else {
                let _ = write!(
                    body,
                    "<tr><td>{}</td><td class=\"muted\" colspan=\"4\">no data in the last 7 days</td></tr>",
                    html_escape(peer_target.as_str())
                );
                continue;
            };
            let delta = match here {
                Some(here) if here.median > 0 => format!(
                    "<span class=\"num\" data-v=\"{:.1}\">{:+.1}%</span>",
                    delta_pct(block.median, here.median),
                    delta_pct(block.median, here.median)
                ),
                _ => "—".to_string(),
            };
            let _ = write!(
                body,
                "<tr><td><a href=\"/items/{}/{}\">{}</a></td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 <td>{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td></tr>",
                html_escape(peer_target.as_str()),
                peer_item_path(&item),
                html_escape(peer_target.as_str()),
                block.median,
                money(block.median),
                block.p05,
                money(block.p05),
                delta,
                block.sale_rate.map_or(String::new(), |r| format!("{r:.4}")),
                block.sale_rate.map_or("—".to_string(), |r| format!("{:.0}%", r * 100.0)),
            );
        }
        body.push_str(
            "</tbody></table></div><p class=\"muted\">commodity rows are region-global — \
             same-region spreads there are expected to be ~zero; the signal lives in \
             pets and gear (the auctions payload).</p>",
        );
    }

    body.push_str("<h2>metric blocks</h2><div class=\"scroll-x\"><table id=\"blocks\"><thead><tr>");
    for (label, tip_text) in [
        ("span", "Rolling window behind the block: d1 = current UTC day-to-date (thin right after midnight), d7/d14/d30 = full days."),
        ("n", "Unique auctions folded across the window (dedup'd per day, so hourly snapshots do not inflate it)."),
        ("median", "Middle price of the window's samples."),
        ("min", "Cheapest observed unit price."),
        ("p05", "5th percentile — the near-floor band."),
        ("p25", "25th percentile."),
        ("p75", "75th percentile."),
        ("p95", "95th percentile — the near-ceiling band."),
        ("max", "Most expensive observed unit price."),
        ("mad", "Median absolute deviation — how spread prices are around the median (low = stable market)."),
        ("vwap", "Volume-weighted average price: Σ(price × qty) ÷ Σqty over the window."),
        ("qty", "Summed quantity across the window's auctions."),
        ("sale rate", "Estimated sell-through over 14 days: fraction of daily sightings that vanish and stay gone long enough to count as sold. “—” = no sell-through evidence yet."),
    ] {
        let _ = write!(body, "<th data-sortable>{}</th>", tip(label, tip_text));
    }
    body.push_str("</tr></thead><tbody>");
    for (span, block) in &blocks {
        let Some(block) = block else { continue };
        let _ = write!(
            body,
            "<tr><td>{}</td><td class=\"num\" data-v=\"{}\">{}</td><td class=\"num\" data-v=\"{}\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\" data-v=\"{}\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\" data-v=\"{}\">{}</td>\
             <td class=\"num\" data-v=\"{}\">{}</td></tr>",
            span.name(),
            block.n, block.n,
            block.median, money(block.median),
            money(block.min),
            money(block.p05),
            money(block.p25),
            money(block.p75),
            money(block.p95),
            money(block.max),
            block.mad, block.mad,
            money(block.vwap),
            block.qty_sum, block.qty_sum,
            block.sale_rate.unwrap_or(-1.0),
            block.sale_rate.map_or_else(|| "—".into(), |rate| format!("{rate:.2}"))
        );
    }
    body.push_str("</tbody></table></div>");

    // Watch action (open mode only).
    let open = mutations_open(&state.config);
    if open {
        let watched = tokio::task::spawn_blocking({
            let state = Arc::clone(&state);
            let target = target.clone();
            let item = item.clone();
            move || {
                state
                    .watchlist
                    .list(&target)
                    .map(|entries| entries.iter().any(|entry| entry.item == item))
            }
        });
        match watched.await {
            Ok(Ok(watched)) => {
                let _ = write!(
                    body,
                    "<p style=\"margin-top:1rem\">{}</p>",
                    watch_button(true, watched, &target, &item)
                );
            }
            Ok(Err(error)) => return render::store_error_page(error).await,
            Err(error) => {
                return render::store_error_page(StoreError::Backend(error.to_string())).await
            }
        }
    }

    // `page` escapes its title itself; passing the escaped copy would
    // double-escape ("&" rendering as "&amp;"). `title` stays the
    // single-escaped copy for the body <h2> above. The category tabs ride
    // next to the target switcher; a pet's variant tuple is preserved as
    // hidden fields (a GET form replaces the action's query string).
    let base_path = match &item {
        ItemRef::Item { item_id } => format!("/items/{}/{}", target.as_str(), item_id),
        ItemRef::Pet { species, .. } => {
            format!("/items/{}/pet/{}", target.as_str(), species)
        }
    };
    let preserve: Vec<(&str, String)> = match &item {
        ItemRef::Item { .. } => Vec::new(),
        ItemRef::Pet { level, quality, breed, .. } => {
            vec![
                ("level", level.to_string()),
                ("quality", quality.to_string()),
                ("breed", breed.to_string()),
            ]
        }
    };
    let preserve_refs: Vec<(&str, &str)> =
        preserve.iter().map(|(key, value)| (*key, value.as_str())).collect();
    let switcher = format!(
        "{}{}",
        target_switcher(&state.config, "/items", &target, &[]),
        render::category_switcher(&base_path, &preserve_refs, category)
    );
    page(&title_raw, "items", &body, Some(switcher))
}

/// Serializes a JSON value for embedding in a `<script type="application/json">`
/// island. `html_escape` is WRONG here: script content is raw text (entities
/// are not decoded, so `&quot;` would break JSON.parse). Only `<` needs
/// neutralizing (script-tag breakout), and JSON's own `\u003c` escape does
/// that while staying valid JSON.
fn json_island(value: &serde_json::Value) -> String {
    value.to_string().replace('<', "\\u003c")
}

// ----------------------------------------------------------- opportunities

/// `<option>` fragment for an inline category select with `current`
/// preselected (the opportunities toolbar; standalone pages use
/// `render::category_switcher`'s full form).
fn category_options(current: Category) -> String {
    let mut out = String::new();
    for category in Category::ALL {
        let selected = if category == current { " selected" } else { "" };
        let _ = write!(
            out,
            "<option value=\"{}\"{selected}>{}</option>",
            category.as_str(),
            category.as_str()
        );
    }
    out
}

/// `GET /opportunities?target=&category=&min_churn=&limit=` — ranked deals
/// for one market category with filters, sortable columns and per-row watch
/// actions.
pub(crate) async fn opportunities_page(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match target_param(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let category = match category_param(&params, Category::Item) {
        Ok(category) => category,
        Err(response) => return *response,
    };
    let min_churn: f64 = params
        .get("min_churn")
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(1.0);
    let limit: usize = params.get("limit").and_then(|v| v.parse().ok()).unwrap_or(50);
    let data = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || state.opportunities.opportunities(&target, category, min_churn, limit)
    });
    let data = match data.await {
        Ok(Ok(data)) => data,
        Ok(Err(error)) => return render::store_error_page(error).await,
        Err(error) => {
            return render::store_error_page(StoreError::Backend(error.to_string())).await
        }
    };
    let watched = watched_keys(&state, &target).await;
    let names = resolve_names(
        &state,
        &super::target_flavor(&state.config, &target),
        &data.iter().map(|o| o.item.clone()).collect::<Vec<_>>(),
    )
    .await;

    let min_churn_text = format!("{min_churn}");
    let limit_text = format!("{limit}");
    let mut limit_options = String::new();
    for n in [25usize, 50, 100, 200] {
        let selected = if n == limit { " selected" } else { "" };
        let _ = write!(limit_options, "<option value=\"{n}\"{selected}>{n}</option>");
    }
    let switcher = target_switcher(
        &state.config,
        "/opportunities",
        &target,
        &[
            ("category", category.as_str()),
            ("min_churn", &min_churn_text),
            ("limit", limit_text.as_str()),
        ],
    );

    let mut body = format!(
        "<p class=\"muted\">{} target <code>{}</code> · category <code>{}</code></p>\
         <form class=\"toolbar\" method=\"get\" action=\"/opportunities\">\
         <input type=\"hidden\" name=\"target\" value=\"{}\">\
         <label for=\"category\">{}</label>\
         <select id=\"category\" name=\"category\" data-autosubmit>{}</select>\
         <label for=\"min-churn\">{} ≥</label>\
         <input type=\"number\" id=\"min-churn\" name=\"min_churn\" value=\"{min_churn}\" min=\"0\" step=\"0.1\" style=\"width:6rem\">\
         <label for=\"limit\">{}</label>\
         <select id=\"limit\" name=\"limit\">{limit_options}</select>\
         <button class=\"btn btn-sm btn-primary\" type=\"submit\">apply</button>\
         </form>",
        tip("score", "score = Σ wᵢ·componentᵢ over discount, reliability, churn and stability (weights from [opportunities] in the config; only ratios matter)."),
        html_escape(target.as_str()),
        html_escape(category.as_str()),
        html_escape(target.as_str()),
        tip("category", "Which market the ranking covers: items (item AH), commodities (commodity AH) or pets. Each category ranks on its own evidence — an item trading in both AHs appears independently in each."),
        category_options(category),
        tip("min churn", "Only items with at least this many listings churning per day rank — filters out thin, un-tradeable markets."),
        tip("rows", "Maximum number of ranked items to show."),
    );

    if data.is_empty() {
        body.push_str(
            "<div class=\"empty\">nothing ranks yet — the engine needs day folds plus sell-through \
             evidence. Let cycles accumulate (or raise/lower min churn).</div>",
        );
    } else {
        body.push_str("<div class=\"scroll-x\"><table id=\"opportunities\"><thead><tr>");
        for (label, tip_text) in [
            ("item", "Item or pet variant the ranking row describes — click for its full history."),
            ("score", "Weighted sum of the four components below. Sorted descending by default."),
            ("confidence", "Evidence vs. [opportunities].min_samples: 100% = the window holds at least the configured number of unique listings."),
            ("discount", "1 − p05/median7: how far today's robust floor (5th percentile) sits under the 7-day median — one outlier listing cannot fake it."),
            ("reliability", "Sell-through strength over the 14-day window — how confidently listings actually sell here."),
            ("churn/day", "Average unique listings appearing per day: trading volume."),
            ("stability", "Low MAD relative to the median — tight, predictable pricing."),
            ("floor now", "Cheapest listing folded today."),
            ("median 7d", "7-day median price."),
            ("samples 7d", "Unique listings folded over the last 7 days (the evidence behind confidence)."),
            ("", "Watchlist action (open mode only)."),
        ] {
            let _ = write!(body, "<th data-sortable>{}</th>", tip(label, tip_text));
        }
        body.push_str("</tr></thead><tbody>");
        let open = mutations_open(&state.config);
        for o in &data {
            let _ = write!(
                body,
                "<tr><td>{}</td>\
                 <td data-v=\"{:.4}\"><strong>{:.3}</strong></td>\
                 <td data-v=\"{:.3}\">{:.0}%</td>\
                 <td data-v=\"{:.4}\">{}</td>\
                 <td data-v=\"{:.4}\">{}</td>\
                 <td class=\"num\" data-v=\"{:.2}\">{:.1}</td>\
                 <td data-v=\"{:.4}\">{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 {}</tr>",
                item_cell(&target, &o.item, &names),
                o.score,
                o.score,
                o.confidence,
                o.confidence * 100.0,
                o.discount,
                component_bar("disc", o.discount),
                o.reliability,
                component_bar("rel", o.reliability),
                o.churn_per_day,
                o.churn_per_day,
                o.stability,
                component_bar("stab", o.stability),
                o.floor_now,
                money(o.floor_now),
                o.median7,
                money(o.median7),
                o.samples7,
                o.samples7,
                watch_button(open, watched.contains(&item_key(&o.item)), &target, &o.item)
            );
        }
        body.push_str("</tbody></table></div>");
        let _ = write!(
            body,
            "<p><button class=\"btn btn-sm\" data-csv=\"opportunities\">export CSV</button></p>"
        );
    }
    page("opportunities", "opportunities", &body, Some(switcher))
}

// ---------------------------------------------------------------- overview

/// One movers table (rising / falling / most active) with honest 30-day
/// sparklines from the pre-fetched per-item series.
fn mover_table(
    rows: &[amtracker_core::services::opportunity::MoverRow],
    target: &TargetId,
    series: &HashMap<String, Vec<i64>>,
    names: &render::ItemNames,
    table_id: &str,
) -> String {
    // Display-only conversion: the f64 median feeds the i64 `money` +
    // sparkline helpers (the original rendering did the same cast).
    #[allow(clippy::cast_possible_truncation)]
    fn body_rows(
        rows: &[amtracker_core::services::opportunity::MoverRow],
        target: &TargetId,
        series: &HashMap<String, Vec<i64>>,
        names: &render::ItemNames,
    ) -> String {
        let mut out = String::new();
        for m in rows {
            let trend =
                series.get(&item_key(&m.item)).map(|values| sparkline(values)).unwrap_or_default();
            let change_class = if m.change >= 0.0 { "up" } else { "down" };
            let _ = write!(
                out,
                "<tr>{}<td class=\"num\" data-v=\"{}\">{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 <td class=\"{change_class}\" data-v=\"{:.4}\">{:+.1}%</td>\
                 <td class=\"num\" data-v=\"{:.2}\">{:.1}</td><td>{trend}</td></tr>",
                item_cell(target, &m.item, names),
                m.median_now,
                money(m.median_now),
                m.median_prev,
                money(m.median_prev as i64),
                m.change,
                m.change,
                m.churn_per_day,
                m.churn_per_day
            );
        }
        out
    }
    let mut out = format!(
        "<div class=\"scroll-x\"><table id=\"{table_id}\"><thead><tr>\
         <th data-sortable>{}</th><th data-sortable>{}</th><th data-sortable>{}</th><th data-sortable>{}</th><th data-sortable>{}</th><th data-sortable>{}</th></tr></thead><tbody>",
        tip("item", "Click through for the full 30-day history."),
        tip("median now", "Today's folded median."),
        tip("median 7d", "Median over the previous 7-day window."),
        tip("change", "Relative movement between the two medians."),
        tip("churn/day", "Average unique listings per day."),
        tip("30d", "Actual 30-day median series (real data, not a two-point sketch).")
    );
    out.push_str(&body_rows(rows, target, series, names));
    out.push_str("</tbody></table></div>");
    out
}

/// `GET /overview?target=&category=` — rising / falling / most-active movers
/// for one market category, with honest 30-day sparklines.
pub(crate) async fn overview_page(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match target_param(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let category = match category_param(&params, Category::Item) {
        Ok(category) => category,
        Err(response) => return *response,
    };
    let data = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || state.opportunities.overview(&target, category, 15)
    });
    let data = match data.await {
        Ok(Ok(data)) => data,
        Ok(Err(error)) => return render::store_error_page(error).await,
        Err(error) => {
            return render::store_error_page(StoreError::Backend(error.to_string())).await
        }
    };

    // Honest sparklines: one item-scoped 30-day query per distinct mover.
    let mut refs: Vec<ItemRef> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for mover in data.movers_up.iter().chain(&data.movers_down).chain(&data.most_active) {
        let key = item_key(&mover.item);
        if seen.insert(key) {
            refs.push(mover.item.clone());
        }
    }
    let series = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        let refs = refs.clone();
        move || -> Result<HashMap<String, Vec<i64>>, StoreError> {
            let since = utc_day(state.clock.now()) - 29;
            let mut out = HashMap::new();
            for item in &refs {
                let mut rows =
                    state.history.item_day_rows_for_item(&target, item, since, category)?;
                rows.sort_by_key(|row| row.day);
                out.insert(item_key(item), rows.iter().map(|row| row.median).collect());
            }
            Ok(out)
        }
    });
    let series: HashMap<String, Vec<i64>> = match series.await {
        Ok(Ok(series)) => series,
        Ok(Err(error)) => return render::store_error_page(error).await,
        Err(error) => {
            return render::store_error_page(StoreError::Backend(error.to_string())).await
        }
    };
    let names = resolve_names(&state, &super::target_flavor(&state.config, &target), &refs).await;

    let switcher = format!(
        "{}{}",
        target_switcher(&state.config, "/overview", &target, &[("category", category.as_str())]),
        render::category_switcher("/overview", &[("target", target.as_str())], category)
    );

    let mut body = String::new();
    body.push_str("<h2>rising</h2>");
    if data.movers_up.is_empty() {
        body.push_str("<div class=\"empty\">no rising movers yet</div>");
    } else {
        body.push_str(&mover_table(&data.movers_up, &target, &series, &names, "rising"));
    }
    body.push_str("<h2>falling</h2>");
    if data.movers_down.is_empty() {
        body.push_str("<div class=\"empty\">no falling movers yet</div>");
    } else {
        body.push_str(&mover_table(&data.movers_down, &target, &series, &names, "falling"));
    }
    body.push_str("<h2>most active</h2>");
    if data.most_active.is_empty() {
        body.push_str("<div class=\"empty\">no churn data yet</div>");
    } else {
        body.push_str(&mover_table(&data.most_active, &target, &series, &names, "active"));
    }
    page("market overview", "overview", &body, Some(switcher))
}

// --------------------------------------------------------------- watchlist

/// `GET /watchlist?target=&category=` — the curated list driving
/// `feed-slim.lua`, joined to one market category's numbers.
pub(crate) async fn watchlist_page(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match target_param(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let category = match category_param(&params, Category::Item) {
        Ok(category) => category,
        Err(response) => return *response,
    };
    let report = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || state.watchlist_report.report(&target, category, &[Span::D1, Span::D7], 14, 30)
    });
    let report = match report.await {
        Ok(Ok(report)) => report,
        Ok(Err(error)) => return render::store_error_page(error).await,
        Err(error) => {
            return render::store_error_page(StoreError::Backend(error.to_string())).await
        }
    };
    let names = resolve_names(
        &state,
        &super::target_flavor(&state.config, &target),
        &report.iter().map(|row| row.entry.item.clone()).collect::<Vec<_>>(),
    )
    .await;
    let switcher = format!(
        "{}{}",
        target_switcher(&state.config, "/watchlist", &target, &[("category", category.as_str())]),
        render::category_switcher("/watchlist", &[("target", target.as_str())], category)
    );
    let open = mutations_open(&state.config);

    let mut body = format!(
        "<p class=\"muted\">{} <code>{}</code> — {} <code>feed-slim.lua</code> {}</p>",
        tip("watchlist", "Your curated list for this target."),
        html_escape(target.as_str()),
        tip("drives", "The slim feed is built on demand from exactly these entries."),
        tip("feed-slim.lua", "Download it from the dashboard or /feed-slim.lua?target=…")
    );
    if report.is_empty() {
        let _ = write!(
            body,
            "<div class=\"empty\">nothing watched on this target yet — use the watch buttons on \
             {} or {} {}</div>",
            html_escape("item pages"),
            html_escape("the"),
            tip(
                "opportunities ranking",
                "Each ranked row carries a watch action when the mutating API is open."
            )
        );
    } else {
        // Display-only percentage math in f64 (rendering, not decisions).
        #[allow(clippy::cast_precision_loss)]
        let delta_pct =
            |now: i64, baseline: i64| -> f64 { (now - baseline) as f64 / baseline as f64 * 100.0 };
        body.push_str("<div class=\"scroll-x\"><table id=\"watchlist\"><thead><tr>");
        for (label, tip_text) in [
            ("item", "Watched item or pet variant."),
            (
                "now (d1)",
                "Median unit price of the current UTC day-to-date — thin right after midnight.",
            ),
            (
                "median d7",
                "Median unit price over the trailing 7 days: what this item normally sells for.",
            ),
            (
                "floor d7",
                "5th-percentile price over the trailing 7 days — the deal floor. Listings near \
                 it are cheap relative to the week, not necessarily rare.",
            ),
            (
                "Δ vs d7",
                "How today's median compares to the 7-day median: negative means trading below \
                 its weekly norm.",
            ),
            (
                "sale rate 14d",
                "Share of this item's listings credited as sold within the last 14 days.",
            ),
            ("trend 30d", "Daily medians over the last 30 days."),
            ("note", "Your free-form note."),
            ("added", "When the entry was added (UTC)."),
            ("", "Removal action (open mode only)."),
        ] {
            let _ = write!(body, "<th data-sortable>{}</th>", tip(label, tip_text));
        }
        body.push_str("</tr></thead><tbody>");
        for row in &report {
            let now = row.blocks.first().and_then(|block| block.as_ref()).map(|b| b.median);
            let week = row.blocks.get(1).and_then(|block| block.as_ref());
            let delta = match (now, week) {
                (Some(now), Some(week)) if week.median > 0 => {
                    format!(
                        "<span class=\"num\" data-v=\"{:.1}\">{:+.1}%</span>",
                        delta_pct(now, week.median),
                        delta_pct(now, week.median)
                    )
                }
                _ => "—".to_string(),
            };
            let mut days: Vec<&amtracker_core::domain::ItemDayRow> = row.trend.iter().collect();
            days.sort_unstable_by_key(|day| day.day);
            let medians: Vec<i64> = days.iter().map(|day| day.median).collect();
            let _ = write!(
                body,
                "<tr><td>{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 <td>{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td>\
                 {}\
                 <td class=\"muted\">{}</td>\
                 <td title=\"{}\">{}</td><td>{}</td></tr>",
                item_cell(&target, &row.entry.item, &names),
                now.map_or(String::new(), |v| v.to_string()),
                now.map_or("—".to_string(), money),
                week.map_or(String::new(), |w| w.median.to_string()),
                week.map_or("—".to_string(), |w| money(w.median)),
                week.map_or(String::new(), |w| w.p05.to_string()),
                week.map_or("—".to_string(), |w| money(w.p05)),
                delta,
                row.sale_rate.map_or(String::new(), |r| format!("{r:.2}")),
                row.sale_rate.map_or("—".to_string(), |r| format!("{:.0}%", r * 100.0)),
                sparkline(&medians),
                html_escape(row.entry.note.as_deref().unwrap_or("")),
                html_escape(&human_time(row.entry.added_at)),
                html_escape(&human_time(row.entry.added_at)),
                watch_button(open, true, &target, &row.entry.item)
            );
        }
        body.push_str("</tbody></table></div>");
    }
    if !open {
        body.push_str(
            "<p class=\"muted\">this instance requires an API token, so watchlist edits stay \
             API-only: POST/DELETE /api/watchlist with the bearer token</p>",
        );
    }
    page("watchlist", "watchlist", &body, Some(switcher))
}

// -------------------------------------------------------------------- alerts

/// Human one-liner for a stored alert condition.
fn condition_label(condition: &amtracker_core::domain::AlertCondition) -> String {
    use amtracker_core::domain::AlertCondition;
    match *condition {
        AlertCondition::PriceBelow { copper } => {
            format!("d1 median ≤ {} copper", money(copper))
        }
        AlertCondition::BelowFloor { span, percentile } => {
            format!("{} median ≤ {} floor", span.name(), percentile.as_str())
        }
        AlertCondition::SaleRateAbove { rate } => format!("14d sale rate ≥ {rate:.2}"),
    }
}

/// `GET /alerts` — a target's alert rules, their fired history and (in open
/// mutation mode) a rule form. Data-less until rules exist; evaluation runs
/// after every ingest cycle when `[alerts].enabled`.
pub(crate) async fn alerts_page(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let target = match target_param(&params, &state.config) {
        Ok(target) => target,
        Err(response) => return *response,
    };
    let loaded = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let target = target.clone();
        move || -> Result<_, StoreError> {
            let rules = state.alerts.list_rules(&target)?;
            let events = state.alerts.recent_events(&target, 50)?;
            Ok((rules, events))
        }
    });
    let (rules, events) = match loaded.await {
        Ok(Ok(loaded)) => loaded,
        Ok(Err(error)) => return render::store_error_page(error).await,
        Err(error) => {
            return render::store_error_page(StoreError::Backend(error.to_string())).await
        }
    };
    let names = resolve_names(
        &state,
        &super::target_flavor(&state.config, &target),
        &rules.iter().map(|rule| rule.item.clone()).collect::<Vec<_>>(),
    )
    .await;
    let switcher = target_switcher(&state.config, "/alerts", &target, &[]);
    let open = mutations_open(&state.config);

    let delivery = state.config.alerts.delivery_kind();
    let delivery_label = match delivery {
        amtracker_core::config::AlertDelivery::Log => "log (events below + JSON API only)",
        amtracker_core::config::AlertDelivery::Discord => "discord webhook",
        amtracker_core::config::AlertDelivery::Telegram => "telegram bot",
    };
    let cooldown_label = format!("cooldown {}s", state.config.alerts.cooldown_secs);
    let mut body = format!(
        "<p class=\"muted\">{} <code>{}</code> — {} <code>{}</code>, {} {}</p>",
        tip("alerts", "Rules evaluated after every ingest cycle; a fired rule re-fires at most once per cooldown window."),
        html_escape(target.as_str()),
        tip("delivery", "Where fired alerts are sent. \"log\" keeps them in this page and the JSON API only."),
        html_escape(delivery_label),
        tip("cooldown", "The minimum seconds between two fires of the same rule."),
        cooldown_label,
    );
    if !state.config.alerts.enabled {
        body.push_str(
            "<div class=\"empty\">alert evaluation is disabled ([alerts].enabled = false) — \
             rules and history stay browsable, nothing fires</div>",
        );
    }
    body.push_str("<h2>rules</h2>");
    if rules.is_empty() {
        body.push_str("<div class=\"empty\">no alert rules on this target yet</div>");
    } else {
        body.push_str("<div class=\"scroll-x\"><table id=\"alert-rules\"><thead><tr>");
        for (label, tip_text) in [
            ("item", "The watched item or pet variant."),
            ("condition", "What fires the rule (evaluated against the same cached blocks the item page shows)."),
            ("note", "Your free-form label."),
            ("created", "When the rule was added (UTC)."),
            ("last fired", "The rule's cooldown anchor: null until it has fired once."),
            ("", "Removal action (open mode only)."),
        ] {
            let _ = write!(body, "<th data-sortable>{}</th>", tip(label, tip_text));
        }
        body.push_str("</tr></thead><tbody>");
        for rule in &rules {
            let _ = write!(
                body,
                "<tr>{}<td>{}</td><td class=\"muted\">{}</td>\
                 <td title=\"{}\">{}</td>\
                 <td>{}</td><td>{}</td></tr>",
                item_cell(&target, &rule.item, &names),
                condition_label(&rule.condition),
                html_escape(rule.note.as_deref().unwrap_or("")),
                html_escape(&human_time(rule.created_at)),
                html_escape(&human_time(rule.created_at)),
                rule.last_fired_at.map_or("—".to_string(), |at| html_escape(&human_time(at))),
                alert_remove_button(open, rule.id),
            );
        }
        body.push_str("</tbody></table></div>");
    }
    if open {
        let _ = write!(
            body,
            "<h2>add rule</h2>\
             <form data-alert-form data-target=\"{}\" class=\"alert-form\">\
             <input name=\"itemID\" type=\"number\" min=\"1\" placeholder=\"item id\" aria-label=\"item id\">\
             <input name=\"speciesID\" type=\"number\" min=\"1\" placeholder=\"pet species (optional)\" aria-label=\"pet species id\">\
             <input name=\"level\" type=\"number\" placeholder=\"pet level\" aria-label=\"pet level\">\
             <input name=\"qualityID\" type=\"number\" placeholder=\"pet quality\" aria-label=\"pet quality id\">\
             <input name=\"breedID\" type=\"number\" placeholder=\"pet breed\" aria-label=\"pet breed id\">\
             <select name=\"type\" aria-label=\"condition\">\
             <option value=\"price-below\">d1 median ≤ copper</option>\
             <option value=\"below-floor\">median ≤ floor percentile</option>\
             <option value=\"sale-rate-above\">14d sale rate ≥ rate</option>\
             </select>\
             <input name=\"copper\" type=\"number\" min=\"1\" placeholder=\"copper\" aria-label=\"copper threshold\">\
             <select name=\"span\" aria-label=\"span\">\
             <option value=\"d7\">d7</option><option value=\"d14\">d14</option>\
             <option value=\"d30\">d30</option><option value=\"d1\">d1</option>\
             </select>\
             <select name=\"percentile\" aria-label=\"floor percentile\">\
             <option value=\"p05\">p05</option><option value=\"p25\">p25</option>\
             </select>\
             <input name=\"rate\" type=\"number\" min=\"0\" max=\"1\" step=\"0.01\" placeholder=\"rate\" aria-label=\"sale rate threshold\">\
             <input name=\"note\" placeholder=\"note (optional)\" aria-label=\"rule note\">\
             <button class=\"btn btn-sm\" type=\"submit\">add rule</button>\
             </form>\
             <p class=\"muted\">pets: fill species + level + quality + breed (they share one market per species).</p>",
            html_escape(target.as_str()),
        );
    } else {
        body.push_str(
            "<p class=\"muted\">this instance requires an API token, so rules are \
             API-only: POST/DELETE /api/alerts with the bearer token</p>",
        );
    }

    body.push_str("<h2>recent fires</h2>");
    if events.is_empty() {
        body.push_str("<div class=\"empty\">nothing has fired on this target yet</div>");
    } else {
        body.push_str("<div class=\"scroll-x\"><table id=\"alert-events\"><thead><tr>");
        for (label, tip_text) in [
            ("fired at", "When the condition held (UTC)."),
            ("rule", "The rule that fired."),
            ("observed", "The number that fired it: median copper or sale-rate fraction."),
            ("detail", "The recorded one-liner; delivery adapters send this text."),
        ] {
            let _ = write!(body, "<th data-sortable>{}</th>", tip(label, tip_text));
        }
        body.push_str("</tr></thead><tbody>");
        for event in &events {
            let _ = write!(
                body,
                "<tr><td title=\"{}\">{}</td><td>#{}</td>\
                 <td class=\"num\" data-v=\"{}\">{}</td><td>{}</td></tr>",
                html_escape(&human_time(event.fired_at)),
                html_escape(&human_time(event.fired_at)),
                event.rule_id,
                event.observed.map_or(String::new(), |v| format!("{v:.4}")),
                event.observed.map_or("—".to_string(), |v| format!("{v:.2}")),
                html_escape(&event.message),
            );
        }
        body.push_str("</tbody></table></div>");
    }
    page("alerts", "alerts", &body, Some(switcher))
}

/// Removal button for one alert rule (open mutation mode only).
fn alert_remove_button(open: bool, rule_id: i64) -> String {
    if !open {
        return String::new();
    }
    format!(
        "<button class=\"btn btn-sm btn-danger\" data-alert-remove='{rule_id}' \
         title=\"remove this rule (its history stays)\">remove</button>"
    )
}

/// Path segment under `/items/{target}` for an identity, mirroring
/// `render::item_link`'s scheme (items by id, pets by query-carried tuple).
fn peer_item_path(item: &ItemRef) -> String {
    match item {
        ItemRef::Item { item_id } => format!("/{item_id}"),
        ItemRef::Pet { species, level, quality, breed } => {
            format!("/pet/{species}?level={level}&quality={quality}&breed={breed}")
        }
    }
}

// -------------------------------------------------------------------- help

/// `GET /help` — every term the UI uses, explained once.
pub(crate) async fn help_page() -> Response {
    const TERMS: &[(&str, &str)] = &[
        ("target", "A configured market source: a Blizzard connected realm (region + realm id) or any conforming static endpoint. Each target has its own history, feed and watchlist."),
        ("payload kind", "commodities = the commodity AH dump (stackables, per-unit prices). auctions = the per-item auction dump (whole listings). Both are polled per target."),
        ("snapshot", "One stored fetch of a payload: metadata, the strict-parsed listings and the raw archive. Failures to parse are counted as dropped, never ingested."),
        ("fresh / aging / stale", "Freshness of the newest snapshot vs. the target's poll interval: fresh < 2 intervals, aging < 6, stale beyond. “no data” = nothing stored yet."),
        ("day fold", "Per UTC day, each auction is counted once at its last observed price/quantity. Hourly snapshots therefore cannot inflate sample counts — the engine stays bit-identical to the addon's."),
        ("span (d1/d7/d14/d30)", "Rolling windows behind a metric block. d1 is the current UTC day-to-date, so it is thin right after midnight."),
        ("n", "Unique auctions folded across the window (dedup'd per day)."),
        ("median / p05 / p25 / p75 / p95", "Order statistics of the window's prices: half the samples sit around the median; p05 approximates the floor, p95 the ceiling."),
        ("mad", "Median absolute deviation — spread around the median. Low MAD = stable, predictable market."),
        ("vwap", "Volume-weighted average price: Σ(price × qty) ÷ Σqty. Weights big stacks more than single listings."),
        ("qty", "Summed quantity across the window's auctions."),
        ("sale rate", "Estimated sell-through over 14 days: share of daily sightings that vanish and stay gone past their time-left band's lower bound (the credit rule), so replaced listings do not fake sales."),
        ("floor now", "Cheapest listing folded for the current UTC day."),
        ("discount", "1 − p05 ÷ median7: how far today's robust floor (5th-percentile price) sits under the weekly median; a single outlier listing cannot fake a deep discount."),
        ("reliability", "Sell-through strength feeding the ranking — how confidently listings actually sell on this item."),
        ("churn/day", "Average unique listings per day across the whole 7-day window — days without data count as zero, so a single-day spike reads as a spike, not sustained volume."),
        ("stability", "Low MAD relative to the median — tight, predictable pricing."),
        ("confidence", "Evidence vs. [opportunities].min_samples: 100% means the window holds at least that many unique listings."),
        ("score", "Weighted sum of discount, reliability, churn and stability (weights from the config; only ratios matter for ranking)."),
        ("watchlist", "Your curated per-target list. Drives feed-slim.lua — the addon consumes exactly those items."),
        ("alert rule", "A per-target watch condition (price-below, below-floor, sale-rate-above) evaluated against the same cached blocks the item pages show — an alert can never contradict its page. Rules live in the store; evaluation runs after every ingest cycle."),
        ("cooldown", "A fired rule re-fires at most once per [alerts].cooldown_secs, so a persistently cheap item does not spam delivery. The anchor is the rule's last-fired stamp."),
        ("delivery", "Where fired alerts go: log (this page + JSON API only), discord (webhook message) or telegram (bot message). Webhook/token values are read from the named environment variables on every send, so rotating them needs no restart."),
        ("generatedAt / source modified", "The feed's freshness anchor: taken from the source's Last-Modified header so the addon can trust how current its data is."),
    ];
    let mut body = String::from(
        "<p class=\"muted\">The same explanations appear as hover/focus \
         tooltips (dotted underlines) throughout the UI.</p><dl class=\"glossary\">",
    );
    for (term, definition) in TERMS {
        let _ = write!(body, "<dt>{}</dt><dd>{}</dd>", html_escape(term), html_escape(definition));
    }
    body.push_str("</dl><h2>mutating API</h2><p class=\"muted\">When no API token is configured \
         (open mode, localhost), the UI offers watchlist buttons and “fetch now”. With a token set, \
         those actions stay API-only: <code>POST/DELETE /api/watchlist</code> and \
         <code>POST /api/fetch?target=…</code> with the bearer token.</p>");
    page("help", "", &body, None)
}

/// Watchlist membership keys for one target (dedup'd button state).
async fn watched_keys(state: &Arc<AppState>, target: &TargetId) -> HashSet<String> {
    let task = tokio::task::spawn_blocking({
        let state = Arc::clone(state);
        let target = target.clone();
        move || state.watchlist.list(&target)
    });
    match task.await {
        Ok(Ok(entries)) => entries.iter().map(|entry| item_key(&entry.item)).collect(),
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "watchlist read failed; rendering as unwatched");
            HashSet::new()
        }
        Err(error) => {
            tracing::warn!(error = %error, "watchlist read join failed");
            HashSet::new()
        }
    }
}

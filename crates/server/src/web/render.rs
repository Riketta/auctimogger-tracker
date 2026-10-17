//! Page rendering: HTML shell, design-system helpers, tooltips, humanized
//! timestamps, charts and target switcher. All external data (item names,
//! query echoes, target slugs) goes through [`html_escape`] before it may
//! touch markup.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
};

use amtracker_core::{
    config::TrackerConfig,
    domain::{
        ItemRef,
        PriceBucket,
        TargetId,
    },
    error::StoreError,
};
use axum::{
    http::StatusCode,
    response::{
        Html,
        IntoResponse,
        Response,
    },
};

/// UTC timestamp shape used across pages ("2026-10-17 14:03:07 UTC").
const TS_FORMAT: &[time::format_description::BorrowedFormatItem<'static>] =
    time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second] UTC");

/// Escapes a string for safe interpolation into HTML text/attribute
/// positions. External data (item names, query echoes) never reaches markup
/// raw.
#[must_use]
pub fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Formats a copper amount as gold/silver/copper.
#[must_use]
pub fn money(copper: i64) -> String {
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

/// A term with an explanation tooltip (hover/focus, CSS-only, plain text —
/// `attr()` content is never parsed as markup).
#[must_use]
pub fn tip(term: &str, explanation: &str) -> String {
    format!(
        "<span class=\"tip\" tabindex=\"0\" data-tip=\"{}\">{}</span>",
        html_escape(explanation),
        html_escape(term)
    )
}

/// Human-readable absolute UTC time; unparseable values degrade to the raw
/// number instead of failing the page.
#[must_use]
pub fn human_time(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|ts| ts.format(TS_FORMAT).ok())
        .unwrap_or_else(|| unix.to_string())
}

/// Compact relative age ("just now", "4m", "3h 12m", "6d 3h").
#[must_use]
pub fn human_age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 90 {
        return "just now".into();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 48 {
        return format!("{}h {}m", hours, minutes % 60);
    }
    let days = hours / 24;
    format!("{days}d {}h", hours % 24)
}

/// Freshness badge for one payload kind: green within 2 poll intervals,
/// amber within 6, red beyond, red "no data" when nothing was ever fetched.
#[must_use]
pub fn freshness_badge(age_seconds: Option<i64>, interval_secs: u64) -> String {
    let interval = i64::try_from(interval_secs).unwrap_or(i64::MAX);
    let (class, label, tip_text) = match age_seconds {
        None => ("badge bad", "no data", "No snapshot has been stored yet for this payload kind."),
        Some(age) if age < 2 * interval => (
            "badge ok",
            "fresh",
            "Newer than two poll intervals — the source is answering normally.",
        ),
        Some(age) if age < 6 * interval => (
            "badge warn",
            "aging",
            "Older than two poll intervals — the source may be rate-limiting or down.",
        ),
        Some(_) => (
            "badge bad",
            "stale",
            "Far older than the poll interval — check the source and the scheduler log.",
        ),
    };
    let age_note = age_seconds.map_or_else(String::new, |age| format!(" ({} old)", human_age(age)));
    format!(
        "<span class=\"{class}\" tabindex=\"0\" data-tip=\"{}\">{}{}</span>",
        html_escape(tip_text),
        label,
        html_escape(&age_note)
    )
}

/// Outcome badge for a cycle record.
#[must_use]
pub fn outcome_badge(outcome: &str) -> String {
    let class = match outcome {
        "fresh" => "badge ok",
        "not-modified" => "badge",
        "empty" => "badge warn",
        _ => "badge bad",
    };
    format!("<span class=\"{class}\">{}</span>", html_escape(outcome))
}

/// Maps an API quality label onto the loot-color class. Unknown labels and
/// absent quality fall back to the neutral common color.
#[must_use]
pub fn quality_class(quality: Option<&str>) -> &'static str {
    let lowered = quality.map(str::to_ascii_lowercase);
    match lowered.as_deref() {
        Some("poor") => "poor",
        Some("uncommon") => "uncommon",
        Some("rare") => "rare",
        Some("epic") => "epic",
        Some("legendary") => "legendary",
        _ => "common",
    }
}

/// A colored quality chip for search results.
#[must_use]
pub fn quality_chip(quality: Option<&str>) -> String {
    let label = quality.unwrap_or("common");
    format!("<span class=\"q {}\">{}</span>", quality_class(quality), html_escape(label))
}

/// Display label for an item reference.
#[must_use]
pub fn item_label(item: &ItemRef) -> String {
    match item {
        ItemRef::Item { item_id } => format!("item {item_id}"),
        ItemRef::Pet { species, level, .. } => format!("pet {species} (lvl {level})"),
    }
}

/// Detail-page link for an item reference. Pets carry their full variant
/// identity through query parameters so the metrics engine sees the exact
/// tuple (species alone is not an identity).
#[must_use]
pub fn item_link(target: &TargetId, item: &ItemRef) -> String {
    let (href, label) = match item {
        ItemRef::Item { item_id } => {
            (format!("/items/{}/{}", target.as_str(), item_id), item_label(item))
        }
        ItemRef::Pet { species, level, quality, breed } => (
            format!(
                "/items/{t}/pet/{species}?level={level}&quality={quality}&breed={breed}",
                t = target.as_str()
            ),
            item_label(item),
        ),
    };
    format!("<a href=\"{}\">{}</a>", html_escape(&href), html_escape(&label))
}

/// The shared page shell: topbar, nav (active marker), optional target
/// switcher, footer. `title` is escaped here — callers pass RAW titles; only
/// `body` is fully-rendered HTML.
#[must_use]
pub fn page(title: &str, active: &str, body: &str, switcher: Option<String>) -> Response {
    const NAV: &[(&str, &str, &str)] = &[
        ("/", "dashboard", "dashboard"),
        ("/items", "items", "items"),
        ("/opportunities", "opportunities", "opportunities"),
        ("/overview", "overview", "overview"),
        ("/watchlist", "watchlist", "watchlist"),
        ("/snapshots", "snapshots", "snapshots"),
    ];
    let title = html_escape(title);
    let mut nav = String::new();
    for (href, key, label) in NAV {
        let class = if *key == active { " class=\"active\"" } else { "" };
        let _ = write!(nav, "<a href=\"{href}\"{class}>{label}</a>");
    }
    let switcher_html = switcher.unwrap_or_else(|| "<span class=\"spacer\"></span>".into());
    Html(format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title} — auctimogger-tracker</title>\
         <link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\">\
         <link rel=\"stylesheet\" href=\"/assets/app.css\">\
         </head><body>\
         <header class=\"topbar\"><a class=\"brand\" href=\"/\"><span class=\"dot\"></span>auctimogger-tracker</a>\
         <nav class=\"main\">{nav}</nav>{switcher_html}</header>\
         <main class=\"wrap\"><h1>{title}</h1>{body}</main>\
         <footer class=\"page\">auctimogger-tracker v{} · <a href=\"/help\">terminology &amp; help</a></footer>\
         <script src=\"/assets/app.js\" defer></script></body></html>",
        html_escape(env!("CARGO_PKG_VERSION")),
    ))
    .into_response()
}

/// Alphabetical target switcher for target-scoped pages: a GET form back to
/// `path` with the preserved parameters as hidden fields. Works without JS
/// (noscript submit button); with JS the select submits on change.
#[must_use]
pub fn target_switcher(
    config: &TrackerConfig,
    path: &str,
    current: &TargetId,
    preserve: &[(&str, &str)],
) -> String {
    let mut hidden = String::new();
    for (key, value) in preserve {
        let _ = write!(
            hidden,
            "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
            html_escape(key),
            html_escape(value)
        );
    }
    let mut options = String::new();
    for target in &config.targets {
        let selected = if target.id == *current { " selected" } else { "" };
        let _ = write!(
            options,
            "<option value=\"{}\"{selected}>{}</option>",
            html_escape(target.id.as_str()),
            html_escape(target.id.as_str())
        );
    }
    format!(
        "<form class=\"switcher\" method=\"get\" action=\"{}\">{hidden}\
         <label for=\"target-select\">target</label>\
         <select id=\"target-select\" name=\"target\" data-autosubmit>{options}</select>\
         <noscript><button class=\"btn btn-sm\" type=\"submit\">go</button></noscript></form>",
        html_escape(path),
    )
}

/// Server-rendered fallback sparkline (the JS band chart replaces it when
/// it can parse the data island).
// SVG coordinates: the integer→float rounding loss is invisible at 320×64 px.
#[allow(clippy::cast_precision_loss)]
#[must_use]
pub fn sparkline(values: &[i64]) -> String {
    if values.len() < 2 {
        return String::new();
    }
    let min = values.iter().min().copied().unwrap_or(0);
    let max = values.iter().max().copied().unwrap_or(1);
    let span = i64::max(max - min, 1) as f64;
    let step = 320.0 / (values.len() - 1) as f64;
    let points: Vec<String> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let y = 60.0 - ((*v - min) as f64 / span * 56.0);
            format!("{:.1},{:.1}", i as f64 * step, y)
        })
        .collect();
    format!(
        "<svg viewBox=\"0 0 320 64\" aria-hidden=\"true\"><polyline fill=\"none\" \
         stroke=\"#5aa2ff\" stroke-width=\"2\" points=\"{}\"/></svg>",
        points.join(" ")
    )
}

/// Server-rendered price-distribution bars (today's folded buckets). Height
/// is proportional to unique listings at that price; every bar carries a
/// tooltip with price, listings and quantity.
// SVG coordinates: integer→float rounding loss is invisible at this scale.
#[allow(clippy::cast_precision_loss)]
#[must_use]
pub fn distribution_svg(buckets: &BTreeMap<i64, PriceBucket>) -> String {
    // Cap the bar count on pathologically wide distributions: keep the
    // busiest buckets, then restore price order for display.
    const MAX_BARS: usize = 60;
    if buckets.is_empty() {
        return String::new();
    }
    let mut selected: Vec<(&i64, &PriceBucket)> = buckets.iter().collect();
    if selected.len() > MAX_BARS {
        selected.sort_by_key(|(_, bucket)| std::cmp::Reverse(bucket.listings));
        #[allow(clippy::indexing_slicing)]
        {
            selected.truncate(MAX_BARS);
        }
        selected.sort_by_key(|(price, _)| **price);
    }
    let max_listings = selected.iter().map(|(_, bucket)| bucket.listings).max().unwrap_or(1);
    let width = selected.len() * 14;
    let height = 90;
    let mut svg = format!(
        "<svg viewBox=\"0 0 {width} {height}\" aria-hidden=\"true\" style=\"max-width:{width}px\">"
    );
    for (slot, (price, bucket)) in selected.iter().enumerate() {
        let bar_height = f64::max(bucket.listings as f64 / max_listings.max(1) as f64 * 72.0, 2.0);
        let x = slot * 14;
        let _ = write!(
            svg,
            "<g class=\"tip\" tabindex=\"0\" data-tip=\"{} · {} listings · {} qty\">\
             <rect x=\"{}\" y=\"{:.1}\" width=\"10\" height=\"{bar_height:.1}\" rx=\"2\" \
             fill=\"#5aa2ff\" opacity=\"0.75\"/></g>",
            html_escape(&money(**price)),
            bucket.listings,
            bucket.qty,
            x + 2,
            80.0 - bar_height,
        );
    }
    svg.push_str("</svg>");
    svg
}

/// 500 page for failed store reads: logged server-side, named for the user.
pub async fn store_error_page(error: StoreError) -> Response {
    super::log_failure("page", &error);
    let mut response = page(
        "error",
        "",
        &format!("<p class=\"bad\">store read failed: {}</p>", html_escape(&error.to_string())),
        None,
    );
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response
}

/// Kept for callers formatting small inline bars (score components).
#[must_use]
pub fn component_bar(label: &str, value: f64) -> String {
    format!(
        "<span class=\"muted\">{label}</span> <span class=\"bar\" style=\"width:{:.0}px\"></span> {:.2}",
        value * 60.0,
        value
    )
}

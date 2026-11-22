//! Page rendering: HTML shell, design-system helpers, tooltips, humanized
//! timestamps, charts and target switcher. All external data (item names,
//! query echoes, target slugs) goes through [`html_escape`] before it may
//! touch markup.

use std::{
    collections::{
        BTreeMap,
        HashMap,
    },
    fmt::Write as _,
};

use amtracker_core::{
    config::TrackerConfig,
    domain::{
        Category,
        Flavor,
        ItemRef,
        PriceBucket,
        TargetId,
    },
    error::StoreError,
    services::buy_timing::{
        HourBucket,
        WeekdayStat,
    },
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

/// Percent-encodes one query-string component (RFC 3986 unreserved bytes
/// pass through, everything else becomes `%XX`). HTML escaping alone is not
/// URL safety: a raw `&` splits the query, `#` truncates it, and `+` decodes
/// as a space — filter echoes in generated links must be encoded first,
/// then HTML-escaped for the attribute context.
#[must_use]
pub fn url_encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
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

/// A term with an explanation tooltip (hover/focus, plain text — the
/// explanation is never parsed as markup). An empty term renders a bare
/// "?" marker so icon-only columns stay hoverable and discoverable.
#[must_use]
pub fn tip(term: &str, explanation: &str) -> String {
    let label = if term.trim().is_empty() { "?" } else { term };
    format!(
        "<span class=\"tip\" tabindex=\"0\" data-tip=\"{}\">{}</span>",
        html_escape(explanation),
        html_escape(label)
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

/// Compact countdown ("45s", "12m", "3h 12m", "6d 3h") — like [`human_age`]
/// but granular under a minute and phrased for time-until, not time-ago.
#[must_use]
pub fn human_in(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
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

/// Resolved display names for one page render: items by id, pets by species.
/// Ids without a resolved name keep their raw `item 12345` labels — the maps
/// simply miss. An empty `ItemNames` renders exactly the old raw-id view.
#[derive(Default)]
pub struct ItemNames {
    /// Item id → catalog display name.
    pub items: HashMap<i64, String>,
    /// Pet species id → catalog display name.
    pub pets: HashMap<i64, String>,
}

/// Detail-page link for an item reference. Pets carry their full variant
/// identity through query parameters so the metrics engine sees the exact
/// tuple (species alone is not an identity). When a catalog name resolved
/// for this page it becomes the link text, and the raw identity moves into a
/// `data-tip` so the floating tooltip layer can show the id traders quote
/// in-game — without JS the label simply stays the name.
#[must_use]
pub fn item_link(target: &TargetId, item: &ItemRef, names: &ItemNames) -> String {
    let href = match item {
        ItemRef::Item { item_id } => {
            format!("/items/{}/{}", target.as_str(), item_id)
        }
        ItemRef::Pet { species, level, quality, breed } => format!(
            "/items/{t}/pet/{species}?level={level}&quality={quality}&breed={breed}",
            t = target.as_str()
        ),
    };
    let (label, identity) = match item {
        ItemRef::Item { item_id } => match names.items.get(item_id) {
            Some(name) => (name.clone(), format!("item {item_id}")),
            None => (item_label(item), String::new()),
        },
        ItemRef::Pet { species, level, .. } => match names.pets.get(species) {
            Some(name) => (format!("{name} (lvl {level})"), format!("pet {species}")),
            None => (item_label(item), String::new()),
        },
    };
    let tip_attr = if identity.is_empty() {
        String::new()
    } else {
        format!(" data-tip=\"{}\"", html_escape(&identity))
    };
    format!("<a href=\"{}\"{tip_attr}>{}</a>", html_escape(&href), html_escape(&label))
}

/// Table cell for an item reference: [`item_link`] wrapped in the `<td>`
/// the table builders emit, plus a `data-export` attribute carrying the raw
/// identity when a catalog name resolved — the CSV exporter reads ids (what
/// traders quote in-game), not display names.
#[must_use]
pub fn item_cell(target: &TargetId, item: &ItemRef, names: &ItemNames) -> String {
    let identity = match item {
        ItemRef::Item { item_id } if names.items.contains_key(item_id) => format!("item {item_id}"),
        ItemRef::Pet { species, .. } if names.pets.contains_key(species) => {
            format!("pet {species}")
        }
        _ => String::new(),
    };
    let export = if identity.is_empty() {
        String::new()
    } else {
        format!(" data-export=\"{}\"", html_escape(&identity))
    };
    format!("<td{export}>{}</td>", item_link(target, item, names))
}

/// Outbound Wowhead deep link for one identity under one game flavor — the
/// reference page for the id traders quote in-game. Section slugs follow the
/// PUBLIC site's per-game URLs (they differ from the data API's `nether`
/// prefixes): retail is unprefixed, the progressive classic generations carry
/// their slug, the era family (`classic`, `classic-era`, `forever`) shares
/// `/classic`, and unknown custom flavors fall through to retail exactly like
/// the item-info adapter's id-compatibility rule. Pets link to their species
/// page (`pet-species=`); every URL component is a constant or a numeric id,
/// so no escaping is needed.
#[must_use]
pub fn wowhead_link(flavor: &Flavor, item: &ItemRef) -> String {
    let section = match flavor.as_str() {
        "classic" | "classic-era" | "forever" => "/classic",
        "cata-classic" => "/cata",
        "mop-classic" => "/mop",
        _ => "",
    };
    let page = match item {
        ItemRef::Item { item_id } => format!("item={item_id}"),
        ItemRef::Pet { species, .. } => format!("pet-species={species}"),
    };
    format!(
        "<a href=\"https://www.wowhead.com{section}/{page}\" \
         target=\"_blank\" rel=\"noopener noreferrer\">wowhead ↗</a>"
    )
}

/// The shared page shell: topbar, nav (active marker), optional target
/// switcher, footer. `title` is escaped here — callers pass RAW titles; only
/// `body` is fully-rendered HTML.
#[must_use]
pub fn page(title: &str, active: &str, body: &str, switcher: Option<String>) -> Response {
    const NAV: &[(&str, &str, &str)] = &[
        ("/", "dashboard", "dashboard"),
        ("/items", "items", "items"),
        ("/browse", "browse", "browse"),
        ("/opportunities", "opportunities", "opportunities"),
        ("/overview", "overview", "overview"),
        ("/watchlist", "watchlist", "watchlist"),
        ("/priorities", "priorities", "priorities"),
        ("/compare", "compare", "compare"),
        ("/alerts", "alerts", "alerts"),
        ("/snapshots", "snapshots", "snapshots"),
    ];
    let title = html_escape(title);
    let mut nav = String::new();
    for (href, key, label) in NAV {
        let class = if *key == active { " class=\"active\"" } else { "" };
        let _ = write!(nav, "<a href=\"{href}\"{class}>{label}</a>");
    }
    let switcher_html = switcher.unwrap_or_else(|| "<span class=\"spacer\"></span>".into());
    // The opportunities ranking is the UI's widest table (11 columns, three
    // with component bars); its page opts into the wider `body.wide` measure
    // (see app.css) instead of scrolling inside the default 1180px one.
    let body_class = if active == "opportunities" { r#" class="wide""# } else { "" };
    Html(format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title} — auctimogger-tracker</title>\
         <link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\">\
         <link rel=\"stylesheet\" href=\"/assets/app.css\">\
         </head><body{body_class}>\
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

/// Category switcher: same form mechanics as [`target_switcher`], one option
/// per market category. Every market view renders one category per request;
/// the tabs switch between them (items/commodities/pets keep separate
/// evidence, so a combined view would mix markets).
#[must_use]
pub fn category_switcher(path: &str, preserve: &[(&str, &str)], current: Category) -> String {
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
    for category in Category::ALL {
        let selected = if category == current { " selected" } else { "" };
        let _ = write!(
            options,
            "<option value=\"{}\"{selected}>{}</option>",
            category.as_str(),
            category.as_str()
        );
    }
    format!(
        "<form class=\"switcher\" method=\"get\" action=\"{}\">{hidden}\
         <label for=\"category-select\">category</label>\
         <select id=\"category-select\" name=\"category\" data-autosubmit>{options}</select>\
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

/// Monday-first slot (0..=6) → axis label.
fn weekday_label(slot: usize) -> &'static str {
    match slot {
        0 => "mon",
        1 => "tue",
        2 => "wed",
        3 => "thu",
        4 => "fri",
        5 => "sat",
        _ => "sun",
    }
}
/// Monday-first slot → `0 = Sunday` weekday index.
const WEEKDAY_ORDER: [usize; 7] = [1, 2, 3, 4, 5, 6, 0];

/// Server-rendered weekday-price bars (the no-JS fallback of the when-to-buy
/// view; the browser layer re-buckets into local weekdays). Bar height ∝
/// the weekday's median; the notch marks its p05 floor. Weekdays without
/// evidence render a dim placeholder so the week's shape stays readable.
// SVG coordinates: integer→float rounding loss is invisible at this scale.
#[allow(clippy::cast_precision_loss)]
#[must_use]
pub fn weekday_bars_svg(stats: &[WeekdayStat]) -> String {
    let by_weekday = |weekday: usize| stats.iter().find(|stat| stat.weekday == weekday);
    let max = stats.iter().map(|stat| stat.median).max().unwrap_or(0);
    if max <= 0 {
        return String::new();
    }
    let (width, height) = (7 * 40 + 8, 116);
    let mut svg = format!(
        "<svg viewBox=\"0 0 {width} {height}\" role=\"img\" aria-label=\"price by weekday\" \
         style=\"max-width:{width}px\">"
    );
    for (slot, weekday) in WEEKDAY_ORDER.into_iter().enumerate() {
        let x = slot * 40 + 8;
        match by_weekday(weekday) {
            Some(stat) => {
                let bar = f64::max(stat.median as f64 / max as f64 * 72.0, 2.0);
                let floor = f64::max(stat.p05 as f64 / max as f64 * 72.0, 2.0);
                let _ = write!(
                    svg,
                    "<g class=\"tip\" tabindex=\"0\" data-tip=\"{} · median {} · floor {} · {} days\">\
                     <rect x=\"{x}\" y=\"{:.1}\" width=\"26\" height=\"{bar:.1}\" rx=\"3\" fill=\"#5aa2ff\" opacity=\"0.75\"/>\
                     <rect x=\"{x}\" y=\"{:.1}\" width=\"26\" height=\"2\" fill=\"#1b2233\"/></g>",
                    weekday_label(slot),
                    money(stat.median),
                    money(stat.p05),
                    stat.days,
                    86.0 - bar,
                    86.0 - floor,
                );
            }
            None => {
                let _ = write!(
                    svg,
                    "<text x=\"{}\" y=\"86\" class=\"muted\" text-anchor=\"middle\">—</text>",
                    x + 13
                );
            }
        }
        let _ = write!(
            svg,
            "<text x=\"{}\" y=\"{height}\" text-anchor=\"middle\" class=\"muted\">{}</text>",
            x + 13,
            weekday_label(slot)
        );
    }
    svg.push_str("</svg>");
    svg
}

/// Server-rendered intraday lines (the no-JS fallback; UTC hours — the note
/// beside it says so, and the browser layer re-buckets into local time).
/// Two polylines over hour-of-day: typical (median of the hour's per-snapshot
/// minima) and the fainter floor (min of minima), both normalized to each
/// day's fold median so days at different price levels stay comparable.
/// Hours without evidence break the line instead of dropping to zero.
// SVG coordinates + percentage math: display-only f64 rounding is invisible
// at this scale. Direct indexing is safe in `value_of`: the entry's vector
// is non-empty and `mid` lies in `1..=n` (the `price_stats_sorted`
// precedent).
#[allow(clippy::cast_precision_loss, clippy::indexing_slicing)]
#[must_use]
pub fn intraday_svg(buckets: &[HourBucket], day_medians: &BTreeMap<i64, i64>) -> String {
    // (hour → (typical pct, floor pct)) aggregated across days: per-hour
    // median of typicals / min of floors.
    let mut typicals: HashMap<i64, Vec<f64>> = HashMap::new();
    let mut floors: HashMap<i64, Vec<f64>> = HashMap::new();
    for bucket in buckets {
        let Some(median) = day_medians.get(&bucket.day).filter(|median| **median > 0) else {
            continue;
        };
        let typical = bucket.typical as f64 / *median as f64 * 100.0;
        let floor = bucket.floor as f64 / *median as f64 * 100.0;
        typicals.entry(bucket.hour).or_default().push(typical);
        floors.entry(bucket.hour).or_default().push(floor);
    }
    let mut hours: Vec<i64> = typicals.keys().copied().collect();
    hours.sort_unstable();
    if hours.is_empty() {
        return String::new();
    }
    let value_of = |hour: i64| -> Option<(f64, f64)> {
        let typicals = typicals.get(&hour)?;
        let floors = floors.get(&hour)?;
        let mut sorted = typicals.clone();
        sorted.sort_by(f64::total_cmp);
        let mid = sorted.len().div_ceil(2);
        let typical = if sorted.len() % 2 == 1 {
            sorted[mid - 1]
        } else {
            sorted[mid - 1].midpoint(sorted[mid])
        };
        let floor = floors.iter().copied().fold(f64::INFINITY, f64::min);
        Some((typical, floor))
    };
    let values: Vec<(i64, f64, f64)> =
        hours.iter().filter_map(|hour| value_of(*hour).map(|(t, f)| (*hour, t, f))).collect();
    let all: Vec<f64> = values.iter().flat_map(|(_, typical, floor)| [*typical, *floor]).collect();
    let low = all.iter().copied().fold(f64::INFINITY, f64::min).floor();
    let high = all.iter().copied().fold(f64::NEG_INFINITY, f64::max).ceil();
    let span = f64::max(high - low, 1.0);
    let (width, height, pad_t, pad_b) = (560.0_f64, 120.0, 14.0, 22.0);
    let x = |hour: i64| 12.0 + hour as f64 / 23.0 * (width - 24.0);
    let y = |value: f64| pad_t + (1.0 - (value - low) / span) * (height - pad_t - pad_b);
    let polyline = |pick: fn(&(i64, f64, f64)) -> f64| -> String {
        let mut points = String::new();
        let mut last_hour: Option<i64> = None;
        for entry in &values {
            if last_hour.is_some_and(|last| entry.0 - last > 1) {
                points.push(' '); // gap: hours without evidence break the line
            }
            let _ = write!(points, "{:.1},{:.1}", x(entry.0), y(pick(entry)));
            points.push(' ');
            last_hour = Some(entry.0);
        }
        points
    };
    let mut svg = format!(
        "<svg viewBox=\"0 0 {width} {height}\" role=\"img\" aria-label=\"price by hour of day\" \
         style=\"max-width:{width}px\">"
    );
    let _ = write!(
        svg,
        "<polyline points=\"{}\" fill=\"none\" stroke=\"#7f8ba3\" stroke-width=\"1\" \
         stroke-dasharray=\"3,3\"/>",
        polyline(|entry| entry.2)
    );
    let _ = write!(
        svg,
        "<polyline points=\"{}\" fill=\"none\" stroke=\"#5aa2ff\" stroke-width=\"2\"/>",
        polyline(|entry| entry.1)
    );
    let _ = write!(
        svg,
        "<text x=\"12\" y=\"{pad_t}\" class=\"muted\">{low:.0}%</text>\
         <text x=\"12\" y=\"{}\" class=\"muted\">{high:.0}%</text>\
         <text x=\"{width}\" y=\"{height}\" text-anchor=\"end\" class=\"muted\">UTC hour</text>",
        height - 4.0
    );
    svg.push_str("</svg>");
    svg
}

/// 500 page for failed store reads: logged server-side at full fidelity,
/// generic for the user — internals (SQL text, paths) never reach the wire.
pub async fn store_error_page(error: StoreError) -> Response {
    super::log_failure("page", &error);
    let mut response = page(
        "error",
        "",
        "<p class=\"bad\">store read failed - the server log has the details.</p>",
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

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect/panic: fixture-driven, deterministic
    // inputs (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn tip_marks_empty_terms_and_escapes_both_sides() {
        let icon = tip("", "Watchlist action (open mode only).");
        assert!(icon.contains(">?</span>"), "empty term renders the ? marker: {icon}");
        assert!(icon.contains("data-tip=\"Watchlist action (open mode only).\""));

        let quoted = tip("Δ vs d7", "can't be \"tricked\"");
        assert!(quoted.contains(">Δ vs d7</span>"));
        assert!(quoted.contains("can&#39;t be &quot;tricked&quot;"), "quotes escape: {quoted}");
    }
}

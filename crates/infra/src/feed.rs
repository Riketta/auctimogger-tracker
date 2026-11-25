//! Feed rendering and publication to the feed directory served to the
//! in-game addons.
//!
//! Two contracts live here. The tracker feed v3 is what `FeedRenderer::render`
//! produces: a Lua table literal and its JSON document. The database feed v4
//! ([`FeedCodec::carrier`] and [`FeedCodec::json_twin`]) is the packed carrier
//! of `addon/SPEC.md` and its decoded twin.
//!
//! v3 added `category` to every entry: an item trading in both auction
//! houses exports once per category, each entry carrying only its own
//! market's evidence. v4 keeps that split as three record tables.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::PathBuf,
    sync::Mutex,
};

use amtracker_core::{
    domain::{
        Category,
        DatabaseEntry,
        DatabaseSnapshot,
        FeedEntry,
        FeedFormat,
        FeedSnapshot,
        ItemRef,
        MetricBlock,
        PublishedFeed,
        Span,
        TargetId,
    },
    error::{
        RenderError,
        StoreError,
    },
    ports::FeedPublisher,
};

use crate::record;

/// Renders feed snapshots into the two contract serializations.
pub struct FeedCodec;

impl amtracker_core::ports::FeedRenderer for FeedCodec {
    fn render(&self, feed: &FeedSnapshot, format: FeedFormat) -> Result<Vec<u8>, RenderError> {
        match format {
            FeedFormat::Lua => Ok(render_lua(feed)),
            FeedFormat::Json => render_json(feed),
        }
    }
}

fn metric_fields_lua(block: &MetricBlock) -> String {
    let mut out = format!(
        "n = {}, min = {}, p05 = {}, p25 = {}, median = {}, p75 = {}, p95 = {}, max = {}, mad = {}, vwap = {}, qtySum = {}, listingCount = {}",
        block.n, block.min, block.p05, block.p25, block.median, block.p75, block.p95, block.max, block.mad, block.vwap, block.qty_sum, block.listing_count
    );
    if let Some(rate) = block.sale_rate {
        // `String`'s `fmt::Write` impl is infallible.
        let _ = write!(out, ", saleRate = {rate:.4}");
    }
    // Additive v3 fields (the addon's ingestor validates known keys and
    // ignores unknown ones): absent when the ledger has no evidence — never
    // zero, so "missing" and "genuinely zero" stay distinguishable.
    if let Some(rate) = block.sale_rate_180d {
        let _ = write!(out, ", saleRate180d = {rate:.4}");
    }
    if let Some(days) = block.days_since_last_sale {
        let _ = write!(out, ", daysSinceLastSale = {days}");
    }
    if let Some(closures) = block.closures_since_last_sale {
        let _ = write!(out, ", closuresSinceLastSale = {closures}");
    }
    if let Some(units) = block.qty_sold {
        let _ = write!(out, ", qtySold = {units}");
    }
    out
}

fn metrics_lua(metrics: &BTreeMap<Span, MetricBlock>) -> String {
    let spans: Vec<String> = metrics
        .iter()
        .map(|(span, block)| format!("{} = {{ {} }}", span.name(), metric_fields_lua(block)))
        .collect();
    format!("metrics = {{ {} }}", spans.join(", "))
}

fn entry_lua(entry: &FeedEntry) -> String {
    let identity = match &entry.item {
        ItemRef::Item { item_id } => format!("itemID = {item_id}"),
        ItemRef::Pet {
            species,
            level,
            quality,
            breed,
        } => format!(
            "kind = \"pet\", speciesID = {species}, level = {level}, qualityID = {quality}, breedID = {breed}"
        ),
    };
    format!(
        "{{ {identity}, category = \"{}\", {} }}",
        entry.category.as_str(),
        metrics_lua(&entry.metrics)
    )
}

fn render_lua(feed: &FeedSnapshot) -> Vec<u8> {
    let items: Vec<String> = feed
        .entries
        .iter()
        .filter(|entry| !entry.metrics.is_empty()) // empty entries would fail the addon's strict ingestor
        .map(entry_lua)
        .collect();
    let lua = format!(
        "AuctiMoggerTrackerFeed = {{\n  version = 3,\n  generatedAt = {},\n  items = {{\n    {}\n  }},\n}}\n",
        feed.generated_at,
        items.join(",\n    ")
    );
    lua.into_bytes()
}

// --------------------------------------------------------------- JSON side

fn metric_json(block: &MetricBlock) -> serde_json::Value {
    serde_json::json!({
        "n": block.n,
        "min": block.min,
        "p05": block.p05,
        "p25": block.p25,
        "median": block.median,
        "p75": block.p75,
        "p95": block.p95,
        "max": block.max,
        "mad": block.mad,
        "vwap": block.vwap,
        "qtySum": block.qty_sum,
        "listingCount": block.listing_count,
        "saleRate": block.sale_rate,
        "saleRate180d": block.sale_rate_180d,
        "daysSinceLastSale": block.days_since_last_sale,
        "closuresSinceLastSale": block.closures_since_last_sale,
        "qtySold": block.qty_sold,
    })
}

fn entry_json(entry: &FeedEntry) -> serde_json::Value {
    let mut metrics = serde_json::Map::new();
    for (span, block) in &entry.metrics {
        metrics.insert(span.name().to_string(), metric_json(block));
    }
    let mut object = serde_json::Map::new();
    match &entry.item {
        ItemRef::Item { item_id } => {
            object.insert("itemID".into(), serde_json::json!(item_id));
        }
        ItemRef::Pet { species, level, quality, breed } => {
            object.insert("kind".into(), serde_json::json!("pet"));
            object.insert("speciesID".into(), serde_json::json!(species));
            object.insert("level".into(), serde_json::json!(level));
            object.insert("qualityID".into(), serde_json::json!(quality));
            object.insert("breedID".into(), serde_json::json!(breed));
        }
    }
    object.insert("category".into(), serde_json::json!(entry.category.as_str()));
    object.insert("metrics".into(), serde_json::Value::Object(metrics));
    serde_json::Value::Object(object)
}

fn render_json(feed: &FeedSnapshot) -> Result<Vec<u8>, RenderError> {
    let items: Vec<serde_json::Value> =
        feed.entries.iter().filter(|entry| !entry.metrics.is_empty()).map(entry_json).collect();
    let value = serde_json::json!({
        "version": 3,
        "generatedAt": feed.generated_at,
        "items": items,
    });
    serde_json::to_vec(&value).map_err(|e| RenderError(format!("json render: {e}")))
}

// ------------------------------------------------- database feed (v4) renderers

/// The three record tables of a carrier, in the order the file lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarketTable {
    Items,
    Commodities,
    Pets,
}

impl MarketTable {
    const ALL: [Self; 3] = [Self::Items, Self::Commodities, Self::Pets];

    fn category(self) -> Category {
        match self {
            Self::Items => Category::Item,
            Self::Commodities => Category::Commodity,
            Self::Pets => Category::Pet,
        }
    }

    /// The builder field a carrier's record lines address.
    fn field(self) -> &'static str {
        match self {
            Self::Items => "items",
            Self::Commodities => "commodities",
            Self::Pets => "pets",
        }
    }
}

/// A value on its way into a Lua comment line: printable ASCII only, so a
/// stray newline cannot end the comment early.
fn comment_safe(value: &str) -> String {
    value.chars().map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' }).collect()
}

/// Renders `value` as a Lua string literal in pure ASCII: printable ASCII
/// verbatim (quote and backslash escaped), every other byte as a three-digit
/// decimal escape. No header value can break out of the carrier.
#[must_use]
pub fn lua_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for &byte in value.as_bytes() {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7E => out.push(char::from(byte)),
            other => {
                // `String`'s `fmt::Write` impl is infallible.
                let _ = write!(out, "\\{other:03}");
            }
        }
    }
    out.push('"');
    out
}

/// One header line holding a number.
fn lua_number(out: &mut String, name: &str, value: i64) {
    // `String`'s `fmt::Write` impl is infallible.
    let _ = writeln!(out, "    {name} = {value},");
}

/// One header line holding a string.
fn lua_string(out: &mut String, name: &str, value: &str) {
    // `String`'s `fmt::Write` impl is infallible.
    let _ = writeln!(out, "    {name} = {},", lua_string_literal(value));
}

/// The carrier's key of an entry: the item id, or the species id of a pet at
/// species grain.
fn table_key(table: MarketTable, entry: &DatabaseEntry) -> Result<i64, RenderError> {
    match (table, &entry.item) {
        (MarketTable::Pets, ItemRef::Pet { species, level: 0, quality: 0, breed: 0 }) => Ok(*species),
        (MarketTable::Pets, ItemRef::Pet { species, level, quality, breed }) => {
            Err(RenderError(format!(
                "pet species {species} is not at species grain (level {level}, quality {quality}, breed {breed})"
            )))
        }
        (MarketTable::Pets, ItemRef::Item { item_id }) => {
            Err(RenderError(format!("item {item_id} is filed under the pets table")))
        }
        (_, ItemRef::Item { item_id }) => Ok(*item_id),
        (table, ItemRef::Pet { species, .. }) => Err(RenderError(format!(
            "pet species {species} is filed under the {} table",
            table.field()
        ))),
    }
}

/// The snapshot's entries in carrier order: `items`, then `commodities`, then
/// `pets`, each ascending by key. Refuses what would publish a wrong file: an
/// identity filed under the wrong table, a pet below species grain, a record
/// without a span, or a key out of order (which would overwrite another).
fn carrier_order(
    snapshot: &DatabaseSnapshot,
) -> Result<Vec<(MarketTable, i64, &DatabaseEntry)>, RenderError> {
    let mut order = Vec::with_capacity(snapshot.entries.len());
    for table in MarketTable::ALL {
        let mut previous: Option<i64> = None;
        for entry in snapshot.entries.iter().filter(|entry| entry.category == table.category()) {
            let key = table_key(table, entry)?;
            if let Some(earlier) = previous {
                if key <= earlier {
                    return Err(RenderError(format!(
                        "{} entry for {key} follows {earlier}: keys must ascend",
                        table.field()
                    )));
                }
            }
            if entry.record.spans.is_empty() {
                return Err(RenderError(format!("{} entry for {key} has no span", table.field())));
            }
            previous = Some(key);
            order.push((table, key, entry));
        }
    }
    Ok(order)
}

impl FeedCodec {
    /// Renders a database snapshot as the v4 carrier (`feed.lua`): two comment
    /// lines, the market header, one record per line (`items`, then
    /// `commodities`, then `pets`, each ascending by key) and the closing call
    /// (`addon/SPEC.md`, section 5.2). Pure ASCII, and deterministic: a
    /// snapshot renders byte-for-byte identically every time.
    ///
    /// # Errors
    ///
    /// Returns `RenderError` when the snapshot would publish a wrong file (see
    /// [`carrier_order`]).
    pub fn carrier(snapshot: &DatabaseSnapshot) -> Result<Vec<u8>, RenderError> {
        let order = carrier_order(snapshot)?;
        let mut out = String::new();
        // `String`'s `fmt::Write` impl is infallible.
        let _ = writeln!(
            out,
            "-- AuctiMogger Database market file - GENERATED by auctimogger-tracker {}. Do not hand-edit.",
            comment_safe(&snapshot.tracker_version)
        );
        out.push_str("-- Install: copy over a slot of the AuctiMoggerDatabase addon (Market1.lua .. Market6.lua), then /reload.\n");
        out.push_str("local _, A = ...\n");
        out.push_str("local m = A.BeginMarket({\n");
        let meta = &snapshot.market;
        lua_number(&mut out, "version", i64::from(DatabaseSnapshot::FEED_VERSION));
        lua_string(&mut out, "trackerVersion", &snapshot.tracker_version);
        lua_string(&mut out, "target", snapshot.target.as_str());
        lua_string(&mut out, "flavor", meta.flavor.as_str());
        if let Some(region) = &meta.region {
            lua_string(&mut out, "region", region);
        }
        if let Some(id) = meta.ruleset_id {
            lua_number(&mut out, "rulesetId", id);
        }
        if let Some(id) = meta.house_id {
            lua_number(&mut out, "houseId", id);
        }
        if let Some(id) = meta.connected_realm_id {
            lua_number(&mut out, "connectedRealmId", id);
        }
        lua_number(&mut out, "generatedAt", snapshot.generated_at);
        if let Some(at) = snapshot.auctions_at {
            lua_number(&mut out, "auctionsAt", at);
        }
        if let Some(at) = snapshot.commodities_at {
            lua_number(&mut out, "commoditiesAt", at);
        }
        out.push_str("})\n");
        for (table, key, entry) in order {
            // `String`'s `fmt::Write` impl is infallible.
            let _ = writeln!(
                out,
                "m.{}[{}] = \"{}\"",
                table.field(),
                key,
                record::encode(&entry.record)
            );
        }
        out.push_str("A.EndMarket()\n");
        Ok(out.into_bytes())
    }

    /// Renders a database snapshot as the v4 JSON twin (`feed.json`): the
    /// debugging lens of the opaque carrier and the oracle of the
    /// cross-language test (`addon/SPEC.md`, section 5.10). It is rendered
    /// from the decoded records, so it shows exactly what a decoder reads:
    /// rates as basis points over 10000, saturated values as saturated. Absent
    /// groups are `null`; absent scalars are omitted.
    ///
    /// # Errors
    ///
    /// As [`Self::carrier`], plus a record that does not survive its own
    /// encoding.
    pub fn json_twin(snapshot: &DatabaseSnapshot) -> Result<Vec<u8>, RenderError> {
        let order = carrier_order(snapshot)?;
        // The twin shows what a decoder reads, so every record travels through
        // the codec first. The decoded records must outlive the entries that
        // borrow them.
        let records = order
            .iter()
            .map(|(_, key, entry)| {
                let text = record::encode(&entry.record);
                record::decode(&text).map_err(|e| RenderError(format!("record {key}: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let items = order
            .iter()
            .zip(records.iter())
            .map(|((table, key, entry), record)| {
                twin::Entry::new(*table, *key, entry.category, record)
            })
            .collect();
        let document = twin::Document::new(snapshot, items);
        serde_json::to_vec_pretty(&document).map_err(|e| RenderError(format!("json render: {e}")))
    }
}

/// The JSON twin's shape: the contract's field names in its documented order.
/// Typed structures rather than `serde_json::Value`, whose objects sort their
/// keys.
mod twin {
    use std::collections::BTreeMap;

    use amtracker_core::domain::{
        Category,
        CraftQuote,
        DatabaseSnapshot,
        DealMark,
        ItemRecord,
        LiveQuote,
        MetricBlock,
        Span,
    };
    use serde::{
        ser::SerializeMap,
        Serialize,
    };

    use super::MarketTable;

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct Document<'a> {
        /// Contract version; only 4 exists.
        pub version: u32,
        pub tracker_version: &'a str,
        pub target: &'a str,
        pub market: Market<'a>,
        pub generated_at: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub auctions_at: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub commodities_at: Option<i64>,
        pub items: Vec<Entry<'a>>,
    }

    impl<'a> Document<'a> {
        pub(super) fn new(snapshot: &'a DatabaseSnapshot, items: Vec<Entry<'a>>) -> Self {
            let meta = &snapshot.market;
            Self {
                version: DatabaseSnapshot::FEED_VERSION,
                tracker_version: &snapshot.tracker_version,
                target: snapshot.target.as_str(),
                market: Market {
                    region: meta.region.as_deref(),
                    ruleset_id: meta.ruleset_id,
                    house_id: meta.house_id,
                    connected_realm_id: meta.connected_realm_id,
                    flavor: meta.flavor.as_str(),
                },
                generated_at: snapshot.generated_at,
                auctions_at: snapshot.auctions_at,
                commodities_at: snapshot.commodities_at,
                items,
            }
        }
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct Market<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub region: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub ruleset_id: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub house_id: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub connected_realm_id: Option<i64>,
        pub flavor: &'a str,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct Entry<'a> {
        #[serde(rename = "itemID", skip_serializing_if = "Option::is_none")]
        pub item_id: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub kind: Option<&'static str>,
        #[serde(rename = "speciesID", skip_serializing_if = "Option::is_none")]
        pub species_id: Option<i64>,
        pub category: &'static str,
        #[serde(serialize_with = "serialize_spans")]
        pub spans: &'a BTreeMap<Span, MetricBlock>,
        /// `null` when the item is neither listed nor seen today.
        pub live: Option<Live>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub value7: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub sold_median14: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub samples7: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub trend_bp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub sold_count: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub expired_count: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub supply_days: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub low30: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub low_rank_30d: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub lows: Option<&'a [u8]>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub vendor_price: Option<i64>,
        pub craft: Option<Craft>,
        pub deal: Option<Deal>,
    }

    impl<'a> Entry<'a> {
        pub(super) fn new(
            table: MarketTable,
            key: i64,
            category: Category,
            record: &'a ItemRecord,
        ) -> Self {
            let (item_id, kind, species_id) = match table {
                MarketTable::Pets => (None, Some("pet"), Some(key)),
                MarketTable::Items | MarketTable::Commodities => (Some(key), None, None),
            };
            let value = record.value.as_ref();
            Self {
                item_id,
                kind,
                species_id,
                category: category.as_str(),
                spans: &record.spans,
                live: record.live.as_ref().map(Live::from),
                value7: value.map(|value| value.value7),
                sold_median14: value.and_then(|value| value.sold_median14),
                samples7: value.map(|value| value.samples7),
                trend_bp: value.and_then(|value| value.trend_bp),
                sold_count: record.sold_count,
                expired_count: record.expired_count,
                supply_days: record.supply_days,
                low30: record.low30,
                low_rank_30d: record.low_rank_30d_bp,
                lows: record.lows.as_deref(),
                vendor_price: record.vendor_price,
                craft: record.craft.as_ref().map(Craft::from),
                deal: record.deal.as_ref().map(Deal::from),
            }
        }
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct Live {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub floor: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub floor_qty: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub listed: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub qty_sum: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub seen_floor: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub seen_qty: Option<i64>,
    }

    impl From<&LiveQuote> for Live {
        fn from(live: &LiveQuote) -> Self {
            let listed = live.listed.as_ref();
            Self {
                floor: listed.map(|listing| listing.floor),
                floor_qty: listed.map(|listing| listing.floor_qty),
                listed: listed.map(|listing| listing.listed),
                qty_sum: listed.and_then(|listing| listing.qty_sum),
                seen_floor: live.seen.as_ref().map(|seen| seen.floor),
                seen_qty: live.seen.as_ref().map(|seen| seen.qty),
            }
        }
    }

    /// One span block: the seventeen fields of the v3 feed, in its order.
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct SpanJson {
        pub n: i64,
        pub min: i64,
        pub p05: i64,
        pub p25: i64,
        pub median: i64,
        pub p75: i64,
        pub p95: i64,
        pub max: i64,
        pub mad: i64,
        pub vwap: i64,
        pub qty_sum: i64,
        pub listing_count: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub sale_rate: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub sale_rate_180d: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub days_since_last_sale: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub closures_since_last_sale: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub qty_sold: Option<i64>,
    }

    impl From<&MetricBlock> for SpanJson {
        fn from(block: &MetricBlock) -> Self {
            Self {
                n: block.n,
                min: block.min,
                p05: block.p05,
                p25: block.p25,
                median: block.median,
                p75: block.p75,
                p95: block.p95,
                max: block.max,
                mad: block.mad,
                vwap: block.vwap,
                qty_sum: block.qty_sum,
                listing_count: block.listing_count,
                sale_rate: block.sale_rate,
                sale_rate_180d: block.sale_rate_180d,
                days_since_last_sale: block.days_since_last_sale,
                closures_since_last_sale: block.closures_since_last_sale,
                qty_sold: block.qty_sold,
            }
        }
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct Craft {
        pub cost: i64,
        pub profit: i64,
        pub expected_profit: i64,
    }

    impl From<&CraftQuote> for Craft {
        fn from(craft: &CraftQuote) -> Self {
            Self { cost: craft.cost, profit: craft.profit, expected_profit: craft.expected_profit }
        }
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct Deal {
        pub rank: u32,
        pub score_bp: u32,
        pub discount_bp: u32,
    }

    impl From<&DealMark> for Deal {
        fn from(deal: &DealMark) -> Self {
            Self { rank: deal.rank, score_bp: deal.score_bp, discount_bp: deal.discount_bp }
        }
    }

    /// The span blocks as one object, in span order (`d1`, `d7`, `d14`, `d30`).
    fn serialize_spans<S: serde::Serializer>(
        spans: &BTreeMap<Span, MetricBlock>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(spans.len()))?;
        for span in Span::ALL {
            if let Some(block) = spans.get(&span) {
                map.serialize_entry(span.name(), &SpanJson::from(block))?;
            }
        }
        map.end()
    }
}

// --------------------------------------------------------------- publisher

/// Writes rendered feeds to `<dir>/<target>/feed.<ext>` atomically and keeps
/// the last-publication info for the web layer.
pub struct FileFeedPublisher {
    dir: PathBuf,
    last: Mutex<std::collections::HashMap<(String, FeedFormat), PublishedFeed>>,
}

impl FileFeedPublisher {
    /// Publisher rooted at `dir`; every target gets a subdirectory.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, last: Mutex::new(std::collections::HashMap::new()) }
    }

    /// The feed root directory this publisher writes into.
    #[must_use]
    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }
}

fn extension(format: FeedFormat) -> &'static str {
    match format {
        FeedFormat::Lua => "lua",
        FeedFormat::Json => "json",
    }
}

impl FeedPublisher for FileFeedPublisher {
    fn publish(
        &self,
        target: &TargetId,
        format: FeedFormat,
        bytes: &[u8],
    ) -> Result<(), StoreError> {
        let dir = self.dir.join(target.as_str());
        std::fs::create_dir_all(&dir).map_err(|e| StoreError::Backend(format!("feed dir: {e}")))?;
        let final_path = dir.join(format!("feed.{}", extension(format)));
        // Unique tmp name per call: a scheduled publish and a manual-fetch
        // publish for the same target can overlap, and CLI one-offs against
        // the same volume are documented — interleaved writes to one shared
        // tmp path would tear the file before the rename.
        let tmp = dir.join(format!(
            ".feed.{}.{}.tmp",
            extension(format),
            crate::store::unique_tmp_suffix()
        ));
        std::fs::write(&tmp, bytes).map_err(|e| StoreError::Backend(format!("feed write: {e}")))?;
        std::fs::rename(&tmp, &final_path)
            .map_err(|e| StoreError::Backend(format!("feed rename: {e}")))?;
        let at = now_unix();
        let size = bytes.len() as u64;
        // A poisoned lock means another thread panicked mid-publish; recovering
        // the map is safe and matches the store's lock handling.
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((target.0.clone(), format), PublishedFeed { at, size });
        Ok(())
    }

    /// NOTE: write-only in production — no handler or service reads the
    /// stamp today; the reader exists for diagnostics and is asserted by the
    /// `publisher_writes_files_and_tracks_last_publication` integration test.
    fn last_published(
        &self,
        target: &TargetId,
        format: FeedFormat,
    ) -> Result<Option<PublishedFeed>, StoreError> {
        Ok(self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(target.0.clone(), format))
            .copied())
    }

    fn read(&self, target: &TargetId, format: FeedFormat) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.dir.join(target.as_str()).join(format!("feed.{}", extension(format)));
        std::fs::read(&path).map(Some).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(StoreError::Backend(format!("feed read: {e}")))
            }
        })
    }
}

fn now_unix() -> i64 {
    // `u64` unix seconds only exceed `i64` far beyond any real date, so the
    // conversion saturates; a clock before the epoch reads as 0.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap: fixture-driven, deterministic inputs.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

    use amtracker_core::{
        domain::Category,
        ports::FeedRenderer,
    };

    use super::*;

    fn sample_feed() -> FeedSnapshot {
        let block = MetricBlock {
            n: 5,
            min: 100,
            p05: 100,
            p25: 200,
            median: 300,
            p75: 400,
            p95: 500,
            max: 500,
            mad: 100,
            vwap: 366,
            qty_sum: 150,
            listing_count: 5,
            sale_rate: Some(0.25),
            sale_rate_180d: Some(0.2),
            days_since_last_sale: Some(9),
            closures_since_last_sale: Some(14),
            qty_sold: Some(220),
        };
        let mut metrics = BTreeMap::new();
        metrics.insert(Span::D1, block.clone());
        let mut pet_metrics = BTreeMap::new();
        pet_metrics.insert(Span::D1, block);
        FeedSnapshot {
            target: TargetId("us-4".into()),
            generated_at: 1_760_000_000,
            entries: vec![
                FeedEntry {
                    item: ItemRef::Item { item_id: 194_784 },
                    category: Category::Commodity,
                    metrics,
                },
                FeedEntry {
                    item: ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 },
                    category: Category::Pet,
                    metrics: pet_metrics,
                },
            ],
        }
    }

    #[test]
    fn lua_render_matches_the_contract_shape() {
        let mut feed = sample_feed();
        // entries with no metrics are omitted upstream; drop here to test the
        // renderer against well-formed input only
        feed.entries.truncate(1);
        let lua = FeedCodec.render(&feed, FeedFormat::Lua).expect("render");
        let text = String::from_utf8(lua).expect("utf-8");
        let expected = "AuctiMoggerTrackerFeed = {\n  \
            version = 3,\n  generatedAt = 1760000000,\n  items = {\n    \
            { itemID = 194784, category = \"commodity\", metrics = { d1 = { n = 5, min = 100, p05 = 100, p25 = 200, median = 300, p75 = 400, p95 = 500, max = 500, mad = 100, vwap = 366, qtySum = 150, listingCount = 5, saleRate = 0.2500, saleRate180d = 0.2000, daysSinceLastSale = 9, closuresSinceLastSale = 14, qtySold = 220 } } }\n  },\n}\n";
        assert_eq!(text, expected);
    }

    /// Ledger evidence absent from a block must render ABSENT, never zero:
    /// the zero-vs-missing lesson from TSM's `value > 0 and value or nil`.
    #[test]
    fn ledger_absence_renders_absent_not_zero() {
        let mut feed = sample_feed();
        feed.entries.truncate(1);
        let d1 = feed.entries[0].metrics.remove(&Span::D1).expect("d1");
        feed.entries[0].metrics.insert(
            Span::D1,
            MetricBlock {
                sale_rate: None,
                sale_rate_180d: None,
                days_since_last_sale: None,
                closures_since_last_sale: None,
                qty_sold: None,
                ..d1
            },
        );
        let lua = String::from_utf8(FeedCodec.render(&feed, FeedFormat::Lua).expect("render"))
            .expect("utf-8");
        assert!(lua.contains("listingCount = 5 }"), "no trailing fields after listingCount: {lua}");
        assert!(!lua.contains("saleRate180d"), "{lua}");
        assert!(!lua.contains("daysSinceLastSale"), "{lua}");
        assert!(!lua.contains("closuresSinceLastSale"), "{lua}");
        assert!(!lua.contains("qtySold"), "{lua}");
    }

    /// JSON pins the same absence as explicit `null`s: the keys are always
    /// present, only the Lua feed omits them.
    #[test]
    fn json_absence_renders_null_not_missing() {
        let mut feed = sample_feed();
        feed.entries.truncate(1);
        let d1 = feed.entries[0].metrics.remove(&Span::D1).expect("d1");
        feed.entries[0].metrics.insert(
            Span::D1,
            MetricBlock {
                sale_rate: None,
                sale_rate_180d: None,
                days_since_last_sale: None,
                closures_since_last_sale: None,
                qty_sold: None,
                ..d1
            },
        );
        let bytes = FeedCodec.render(&feed, FeedFormat::Json).expect("render");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("valid json");
        let d1 = &value["items"][0]["metrics"]["d1"];
        assert_eq!(d1["saleRate"], serde_json::Value::Null);
        assert_eq!(d1["saleRate180d"], serde_json::Value::Null);
        assert_eq!(d1["daysSinceLastSale"], serde_json::Value::Null);
        assert_eq!(d1["closuresSinceLastSale"], serde_json::Value::Null);
        assert_eq!(d1["qtySold"], serde_json::Value::Null);
    }

    #[test]
    fn pet_entries_render_the_contract_identity() {
        let mut feed = sample_feed();
        feed.entries.truncate(2);
        let d1_block = feed.entries[0].metrics[&Span::D1].clone();
        feed.entries[1].metrics.insert(Span::D7, MetricBlock { sale_rate: None, ..d1_block });
        let lua = String::from_utf8(FeedCodec.render(&feed, FeedFormat::Lua).expect("render"))
            .expect("utf-8");
        assert!(
            lua.contains(
                "kind = \"pet\", speciesID = 2546, level = 1, qualityID = 3, breedID = 11"
            ),
            "{lua}"
        );
        assert!(lua.contains("d7 = { n = 5"), "{lua}");
    }

    #[test]
    fn json_render_round_trips_through_serde() {
        let feed = sample_feed();
        let bytes = FeedCodec.render(&feed, FeedFormat::Json).expect("render");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("valid json");
        assert_eq!(value["version"], 3);
        assert_eq!(value["generatedAt"], 1_760_000_000);
        assert_eq!(value["items"][0]["itemID"], 194_784);
        assert_eq!(value["items"][0]["category"], "commodity");
        assert_eq!(value["items"][0]["metrics"]["d1"]["median"], 300);
        assert_eq!(value["items"][0]["metrics"]["d1"]["saleRate"], 0.25);
        assert_eq!(value["items"][0]["metrics"]["d1"]["saleRate180d"], 0.2);
        assert_eq!(value["items"][0]["metrics"]["d1"]["daysSinceLastSale"], 9);
        assert_eq!(value["items"][0]["metrics"]["d1"]["closuresSinceLastSale"], 14);
        assert_eq!(value["items"][0]["metrics"]["d1"]["qtySold"], 220);
        assert_eq!(value["items"][1]["kind"], "pet");
        assert_eq!(value["items"][1]["category"], "pet");
        assert_eq!(value["items"][1]["speciesID"], 2546);
    }
}

#[cfg(test)]
mod database_tests {
    // Test assertions may unwrap: fixture-driven, deterministic inputs.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

    use std::collections::BTreeMap;

    use amtracker_core::domain::{
        Category,
        CraftQuote,
        DatabaseEntry,
        DatabaseSnapshot,
        DealMark,
        Flavor,
        ItemRecord,
        ItemRef,
        LiveListing,
        LiveQuote,
        MarketMeta,
        MetricBlock,
        SeenListing,
        Span,
        TargetId,
        ValueQuote,
    };

    use super::{
        lua_string_literal,
        record,
        FeedCodec,
        RenderError,
    };

    fn block() -> MetricBlock {
        MetricBlock {
            n: 5,
            min: 100,
            p05: 100,
            p25: 200,
            median: 300,
            p75: 400,
            p95: 500,
            max: 500,
            mad: 100,
            vwap: 366,
            qty_sum: 150,
            listing_count: 5,
            sale_rate: Some(0.25),
            sale_rate_180d: None,
            days_since_last_sale: None,
            closures_since_last_sale: None,
            qty_sold: None,
        }
    }

    /// A record holding nothing but one span.
    fn bare(item: ItemRef, category: Category) -> DatabaseEntry {
        DatabaseEntry {
            item,
            category,
            record: ItemRecord {
                spans: BTreeMap::from([(Span::D1, block())]),
                live: None,
                value: None,
                sold_count: None,
                expired_count: None,
                supply_days: None,
                low30: None,
                low_rank_30d_bp: None,
                lows: None,
                vendor_price: None,
                craft: None,
                deal: None,
            },
        }
    }

    /// A record holding every group and every span.
    fn full(item: ItemRef, category: Category) -> DatabaseEntry {
        let mut spans = BTreeMap::new();
        for span in Span::ALL {
            spans.insert(
                span,
                MetricBlock {
                    sale_rate_180d: Some(0.2),
                    days_since_last_sale: Some(9),
                    closures_since_last_sale: Some(14),
                    qty_sold: Some(220),
                    ..block()
                },
            );
        }
        DatabaseEntry {
            item,
            category,
            record: ItemRecord {
                spans,
                live: Some(LiveQuote {
                    listed: Some(LiveListing {
                        floor: 3_805_000,
                        floor_qty: 214,
                        listed: 618,
                        qty_sum: Some(1234),
                    }),
                    seen: Some(SeenListing { floor: 3_750_000, qty: 5 }),
                }),
                value: Some(ValueQuote {
                    value7: 4_123_500,
                    samples7: 214,
                    sold_median14: Some(4_021_000),
                    trend_bp: Some(-300),
                }),
                sold_count: Some(34),
                expired_count: Some(60),
                supply_days: Some(39),
                low30: Some(3_500_000),
                low_rank_30d_bp: Some(8000),
                lows: Some(vec![128, 130, 127]),
                vendor_price: Some(125_000),
                craft: Some(CraftQuote {
                    cost: 2_100_000,
                    profit: 190_000,
                    expected_profit: 68_400,
                }),
                deal: Some(DealMark { rank: 12, score_bp: 7_100, discount_bp: 3_800 }),
            },
        }
    }

    fn item(id: i64, category: Category) -> DatabaseEntry {
        bare(ItemRef::Item { item_id: id }, category)
    }

    fn pet(species: i64) -> DatabaseEntry {
        bare(ItemRef::Pet { species, level: 0, quality: 0, breed: 0 }, Category::Pet)
    }

    fn snapshot(entries: Vec<DatabaseEntry>) -> DatabaseSnapshot {
        DatabaseSnapshot {
            target: TargetId("forever-eu-pvp-alliance".into()),
            market: MarketMeta {
                region: Some("eu".into()),
                ruleset_id: Some(1),
                house_id: Some(111),
                connected_realm_id: None,
                flavor: Flavor("forever".into()),
            },
            tracker_version: "0.1.0".into(),
            generated_at: 1_795_559_034,
            auctions_at: Some(1_795_558_000),
            commodities_at: Some(1_795_556_000),
            entries,
        }
    }

    fn carrier_text(snapshot: &DatabaseSnapshot) -> String {
        String::from_utf8(FeedCodec::carrier(snapshot).expect("carrier")).expect("utf-8")
    }

    fn twin_text(snapshot: &DatabaseSnapshot) -> String {
        String::from_utf8(FeedCodec::json_twin(snapshot).expect("twin")).expect("utf-8")
    }

    /// Whether the keys occur in `text` in exactly the given order.
    fn keys_in_order(text: &str, keys: &[&str]) -> bool {
        let positions: Vec<usize> = keys
            .iter()
            .map(|key| {
                text.find(&format!("\"{key}\"")).unwrap_or_else(|| panic!("`{key}` is missing"))
            })
            .collect();
        positions.windows(2).all(|pair| pair[0] < pair[1])
    }

    #[test]
    fn carrier_renders_the_documented_file() {
        let snapshot = snapshot(vec![item(194_784, Category::Item), pet(2546)]);
        let head = [
            "-- AuctiMogger Database market file - GENERATED by auctimogger-tracker 0.1.0. Do not hand-edit.",
            "-- Install: copy over a slot of the AuctiMoggerDatabase addon (Market1.lua .. Market6.lua), then /reload.",
            "local _, A = ...",
            "local m = A.BeginMarket({",
            "    version = 4,",
            "    trackerVersion = \"0.1.0\",",
            "    target = \"forever-eu-pvp-alliance\",",
            "    flavor = \"forever\",",
            "    region = \"eu\",",
            "    rulesetId = 1,",
            "    houseId = 111,",
            "    generatedAt = 1795559034,",
            "    auctionsAt = 1795558000,",
            "    commoditiesAt = 1795556000,",
            "})",
        ]
        .join("\n");
        let expected = format!(
            "{head}\nm.items[194784] = \"{}\"\nm.pets[2546] = \"{}\"\nA.EndMarket()\n",
            record::encode(&bare(ItemRef::Item { item_id: 194_784 }, Category::Item).record),
            record::encode(&pet(2546).record)
        );
        assert_eq!(carrier_text(&snapshot), expected);
    }

    #[test]
    fn carrier_omits_absent_header_fields() {
        let mut snapshot = snapshot(vec![]);
        snapshot.market = MarketMeta {
            region: None,
            ruleset_id: None,
            house_id: None,
            connected_realm_id: Some(42),
            flavor: Flavor("retail".into()),
        };
        snapshot.auctions_at = None;
        snapshot.commodities_at = None;
        let text = carrier_text(&snapshot);
        let header = [
            "local m = A.BeginMarket({",
            "    version = 4,",
            "    trackerVersion = \"0.1.0\",",
            "    target = \"forever-eu-pvp-alliance\",",
            "    flavor = \"retail\",",
            "    connectedRealmId = 42,",
            "    generatedAt = 1795559034,",
            "})",
            "A.EndMarket()",
        ]
        .join("\n");
        assert!(text.contains(&header), "{text}");
        // Two comment lines, the vararg line, then the header block itself.
        assert_eq!(text.lines().count(), 2 + 1 + header.lines().count());
    }

    #[test]
    fn carrier_lists_tables_in_order_and_keys_ascending() {
        // Sorted by identity, then category: the snapshot's own order.
        let snapshot = snapshot(vec![
            item(25, Category::Item),
            item(2589, Category::Item),
            item(191_460, Category::Commodity),
            item(194_784, Category::Item),
            item(194_784, Category::Commodity),
            pet(293),
            pet(2546),
        ]);
        let text = carrier_text(&snapshot);
        let lines: Vec<&str> = text.lines().filter(|line| line.starts_with("m.")).collect();
        let tables: Vec<&str> =
            lines.iter().map(|line| line.split('[').next().unwrap_or_default()).collect();
        assert_eq!(
            tables,
            ["m.items", "m.items", "m.items", "m.commodities", "m.commodities", "m.pets", "m.pets"]
        );
        let keys: Vec<&str> = lines
            .iter()
            .map(|line| {
                line.split_once('[')
                    .and_then(|(_, rest)| rest.split_once(']'))
                    .map_or("", |(key, _)| key)
            })
            .collect();
        assert_eq!(keys, ["25", "2589", "194784", "191460", "194784", "293", "2546"]);
    }

    #[test]
    fn twin_lists_the_same_entries_in_the_carrier_order() {
        // Sorted by identity, then category: the snapshot's own order.
        let snapshot = snapshot(vec![
            item(25, Category::Item),
            item(191_460, Category::Commodity),
            item(194_784, Category::Item),
            item(194_784, Category::Commodity),
            pet(2546),
        ]);
        let value: serde_json::Value =
            serde_json::from_str(&twin_text(&snapshot)).expect("valid json");
        let listed: Vec<String> = value["items"]
            .as_array()
            .expect("array")
            .iter()
            .map(|entry| {
                let id = entry["itemID"]
                    .as_i64()
                    .or_else(|| entry["speciesID"].as_i64())
                    .unwrap_or_default();
                format!("{}:{id}", entry["category"].as_str().unwrap_or_default())
            })
            .collect();
        // `items` (ascending), then `commodities`, then `pets`.
        assert_eq!(
            listed,
            ["item:25", "item:194784", "commodity:191460", "commodity:194784", "pet:2546"]
        );
    }

    #[test]
    fn lua_string_literal_keeps_the_file_ascii_and_valid() {
        assert_eq!(lua_string_literal("plain-slug_1.0"), "\"plain-slug_1.0\"");
        assert_eq!(lua_string_literal("a\"b"), "\"a\\\"b\"");
        assert_eq!(lua_string_literal("a\\b"), "\"a\\\\b\"");
        assert_eq!(lua_string_literal("a\nb"), "\"a\\010b\"");
        assert_eq!(lua_string_literal("é"), "\"\\195\\169\"");
        assert_eq!(lua_string_literal("\u{7f}"), "\"\\127\"");
        assert_eq!(lua_string_literal(""), "\"\"");
    }

    /// Header strings come from configuration: whatever they hold, the file
    /// stays ASCII and no value can end its line early.
    #[test]
    fn header_values_cannot_break_out_of_the_file() {
        let mut snapshot = snapshot(vec![item(1, Category::Item)]);
        snapshot.target = TargetId("eu\"] = 1 -- \"\n".into());
        snapshot.tracker_version = "0.1.0\n-- injected".into();
        snapshot.market.region = Some("é\n\r\\".into());
        let bytes = FeedCodec::carrier(&snapshot).expect("carrier");
        assert!(bytes.is_ascii(), "the carrier is pure ASCII");
        let text = String::from_utf8(bytes).expect("utf-8");
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some("-- AuctiMogger Database market file - GENERATED by auctimogger-tracker 0.1.0?-- injected. Do not hand-edit.")
        );
        assert_eq!(
            lines.next(),
            Some("-- Install: copy over a slot of the AuctiMoggerDatabase addon (Market1.lua .. Market6.lua), then /reload.")
        );
        assert!(text.contains("    target = \"eu\\\"] = 1 -- \\\"\\010\",\n"), "{text}");
        assert!(text.contains("    trackerVersion = \"0.1.0\\010-- injected\",\n"), "{text}");
        assert!(text.contains("    region = \"\\195\\169\\010\\013\\\\\",\n"), "{text}");
        // Two comment lines, the vararg line, `local m`, ten header fields
        // (connectedRealmId absent), the close, one record, the closing call.
        assert_eq!(text.lines().count(), 2 + 1 + 1 + 10 + 1 + 1 + 1);
    }

    #[test]
    fn rendering_is_deterministic() {
        let snapshot = snapshot(vec![
            full(ItemRef::Item { item_id: 194_784 }, Category::Commodity),
            pet(2546),
        ]);
        assert_eq!(FeedCodec::carrier(&snapshot), FeedCodec::carrier(&snapshot));
        assert_eq!(FeedCodec::json_twin(&snapshot), FeedCodec::json_twin(&snapshot));
    }

    #[test]
    fn carrier_records_decode_back_to_the_snapshot() {
        let snapshot = snapshot(vec![
            full(ItemRef::Item { item_id: 194_784 }, Category::Commodity),
            full(ItemRef::Pet { species: 2546, level: 0, quality: 0, breed: 0 }, Category::Pet),
        ]);
        for line in carrier_text(&snapshot).lines().filter(|line| line.starts_with("m.")) {
            let (head, tail) = line.split_once(" = \"").expect("record line");
            let word = tail.strip_suffix('"').expect("closing quote");
            let decoded = record::decode(word).unwrap_or_else(|e| panic!("{head}: {e}"));
            let expected = if head.starts_with("m.pets") {
                &snapshot.entries[1]
            } else {
                &snapshot.entries[0]
            };
            assert_eq!(&decoded, &expected.record);
        }
    }

    // ------------------------------------------------------------ refusals

    #[test]
    fn a_pet_identity_cannot_be_filed_as_an_item() {
        let snapshot = snapshot(vec![bare(
            ItemRef::Pet { species: 2546, level: 0, quality: 0, breed: 0 },
            Category::Item,
        )]);
        let error = FeedCodec::carrier(&snapshot).expect_err("misfiled pet");
        assert_eq!(error, RenderError("pet species 2546 is filed under the items table".into()));
        assert!(FeedCodec::json_twin(&snapshot).is_err());
    }

    #[test]
    fn an_item_identity_cannot_be_filed_as_a_pet() {
        let snapshot = snapshot(vec![item(194_784, Category::Pet)]);
        let error = FeedCodec::carrier(&snapshot).expect_err("misfiled item");
        assert_eq!(error, RenderError("item 194784 is filed under the pets table".into()));
        assert!(FeedCodec::json_twin(&snapshot).is_err());
    }

    #[test]
    fn a_pet_below_species_grain_is_refused() {
        let snapshot = snapshot(vec![bare(
            ItemRef::Pet { species: 2546, level: 3, quality: 2, breed: 11 },
            Category::Pet,
        )]);
        let error = FeedCodec::carrier(&snapshot).expect_err("variant pet");
        assert_eq!(
            error,
            RenderError(
                "pet species 2546 is not at species grain (level 3, quality 2, breed 11)".into()
            )
        );
    }

    #[test]
    fn keys_out_of_order_or_duplicated_are_refused() {
        let duplicate = snapshot(vec![item(25, Category::Item), item(25, Category::Item)]);
        assert_eq!(
            FeedCodec::carrier(&duplicate),
            Err(RenderError("items entry for 25 follows 25: keys must ascend".into()))
        );
        let descending = snapshot(vec![item(2589, Category::Item), item(25, Category::Item)]);
        assert_eq!(
            FeedCodec::carrier(&descending),
            Err(RenderError("items entry for 25 follows 2589: keys must ascend".into()))
        );
        // The same id in two tables is two records, not a duplicate.
        let dual =
            snapshot(vec![item(194_784, Category::Item), item(194_784, Category::Commodity)]);
        assert!(FeedCodec::carrier(&dual).is_ok());
    }

    #[test]
    fn a_record_without_spans_is_refused() {
        let mut entry = item(25, Category::Item);
        entry.record.spans.clear();
        assert_eq!(
            FeedCodec::carrier(&snapshot(vec![entry])),
            Err(RenderError("items entry for 25 has no span".into()))
        );
    }

    // ------------------------------------------------------------------ twin

    #[test]
    fn twin_renders_null_groups_and_omits_absent_scalars() {
        let text = twin_text(&snapshot(vec![item(194_784, Category::Item)]));
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value["version"], 4);
        assert_eq!(value["trackerVersion"], "0.1.0");
        assert_eq!(value["target"], "forever-eu-pvp-alliance");
        assert_eq!(value["market"]["region"], "eu");
        assert_eq!(value["market"]["rulesetId"], 1);
        assert_eq!(value["market"]["houseId"], 111);
        assert_eq!(value["market"]["flavor"], "forever");
        assert_eq!(value["generatedAt"], 1_795_559_034);
        assert_eq!(value["auctionsAt"], 1_795_558_000);
        assert_eq!(value["commoditiesAt"], 1_795_556_000);

        let entry = &value["items"][0];
        assert_eq!(entry["itemID"], 194_784);
        assert_eq!(entry["category"], "item");
        assert!(entry.get("kind").is_none(), "only pets carry a kind");
        assert!(entry.get("speciesID").is_none());
        assert_eq!(entry["live"], serde_json::Value::Null, "an absent group is null");
        assert_eq!(entry["craft"], serde_json::Value::Null);
        assert_eq!(entry["deal"], serde_json::Value::Null);
        for scalar in [
            "value7",
            "soldMedian14",
            "samples7",
            "trendBp",
            "soldCount",
            "expiredCount",
            "supplyDays",
            "low30",
            "lowRank30d",
            "lows",
            "vendorPrice",
        ] {
            assert!(entry.get(scalar).is_none(), "`{scalar}` must be omitted");
        }
        let d1 = &entry["spans"]["d1"];
        assert_eq!(d1["n"], 5);
        assert_eq!(d1["saleRate"], 0.25);
        assert!(d1.get("saleRate180d").is_none());
        assert!(d1.get("qtySold").is_none());
        assert!(entry["spans"].get("d7").is_none());
    }

    #[test]
    fn twin_mirrors_every_group() {
        let text = twin_text(&snapshot(vec![full(
            ItemRef::Item { item_id: 194_784 },
            Category::Commodity,
        )]));
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        let entry = &value["items"][0];
        assert_eq!(
            entry["live"],
            serde_json::json!({ "floor": 3_805_000, "floorQty": 214, "listed": 618, "qtySum": 1234, "seenFloor": 3_750_000, "seenQty": 5 })
        );
        assert_eq!(entry["value7"], 4_123_500);
        assert_eq!(entry["soldMedian14"], 4_021_000);
        assert_eq!(entry["samples7"], 214);
        assert_eq!(entry["trendBp"], -300);
        assert_eq!(entry["soldCount"], 34);
        assert_eq!(entry["expiredCount"], 60);
        assert_eq!(entry["supplyDays"], 39);
        assert_eq!(entry["low30"], 3_500_000);
        assert_eq!(entry["lowRank30d"], 8000);
        assert_eq!(entry["lows"], serde_json::json!([128, 130, 127]));
        assert_eq!(entry["vendorPrice"], 125_000);
        assert_eq!(
            entry["craft"],
            serde_json::json!({ "cost": 2_100_000, "profit": 190_000, "expectedProfit": 68_400 })
        );
        assert_eq!(
            entry["deal"],
            serde_json::json!({ "rank": 12, "scoreBp": 7100, "discountBp": 3800 })
        );
        assert_eq!(entry["spans"]["d30"]["qtySold"], 220);
        assert_eq!(entry["spans"]["d30"]["saleRate180d"], 0.2);
        assert_eq!(entry["spans"]["d30"]["daysSinceLastSale"], 9);
        assert_eq!(entry["spans"]["d30"]["closuresSinceLastSale"], 14);
    }

    #[test]
    fn twin_keeps_absent_scalars_absent_inside_a_present_group() {
        // Listed, but neither a quantity sum nor anything seen gone.
        let mut entry = item(25, Category::Item);
        entry.record.live = Some(LiveQuote {
            listed: Some(LiveListing { floor: 15000, floor_qty: 1, listed: 58, qty_sum: None }),
            seen: None,
        });
        let text = twin_text(&snapshot(vec![entry]));
        assert!(
            text.contains("\"live\": {\n        \"floor\": 15000,\n        \"floorQty\": 1,\n        \"listed\": 58\n      }"),
            "{text}"
        );
        // Seen today, already gone: no floor at all.
        let mut entry = item(25, Category::Item);
        entry.record.live =
            Some(LiveQuote { listed: None, seen: Some(SeenListing { floor: 2500, qty: 20 }) });
        let text = twin_text(&snapshot(vec![entry]));
        assert!(
            text.contains(
                "\"live\": {\n        \"seenFloor\": 2500,\n        \"seenQty\": 20\n      }"
            ),
            "{text}"
        );
    }

    #[test]
    fn twin_keys_follow_the_documented_order() {
        let text = twin_text(&snapshot(vec![
            full(ItemRef::Item { item_id: 194_784 }, Category::Item),
            full(ItemRef::Pet { species: 2546, level: 0, quality: 0, breed: 0 }, Category::Pet),
        ]));
        assert!(
            keys_in_order(
                &text,
                &[
                    "version",
                    "trackerVersion",
                    "target",
                    "market",
                    "generatedAt",
                    "auctionsAt",
                    "commoditiesAt",
                    "items"
                ]
            ),
            "{text}"
        );
        assert!(keys_in_order(&text, &["region", "rulesetId", "houseId", "flavor"]), "{text}");
        assert!(
            keys_in_order(
                &text,
                &[
                    "itemID",
                    "category",
                    "spans",
                    "live",
                    "value7",
                    "soldMedian14",
                    "samples7",
                    "trendBp",
                    "soldCount",
                    "expiredCount",
                    "supplyDays",
                    "low30",
                    "lowRank30d",
                    "lows",
                    "vendorPrice",
                    "craft",
                    "deal"
                ]
            ),
            "{text}"
        );
        assert!(keys_in_order(&text, &["kind", "speciesID"]), "{text}");
        assert!(
            keys_in_order(
                &text,
                &[
                    "n",
                    "min",
                    "p05",
                    "p25",
                    "median",
                    "p75",
                    "p95",
                    "max",
                    "mad",
                    "vwap",
                    "qtySum",
                    "listingCount",
                    "saleRate",
                    "saleRate180d",
                    "daysSinceLastSale",
                    "closuresSinceLastSale",
                    "qtySold"
                ]
            ),
            "{text}"
        );
        assert!(keys_in_order(&text, &["d1", "d7", "d14", "d30"]), "{text}");
        // `qtySum` also names a span field, so the live group is checked alone.
        let live = text
            .split("\"live\": {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("the first entry has a live group");
        assert!(
            keys_in_order(live, &["floor", "floorQty", "listed", "qtySum", "seenFloor", "seenQty"]),
            "{live}"
        );
        assert!(keys_in_order(&text, &["cost", "profit", "expectedProfit"]), "{text}");
        assert!(keys_in_order(&text, &["rank", "scoreBp", "discountBp"]), "{text}");
    }

    /// The twin is the decoded carrier, digit for digit: a rate the ranking
    /// computes as 34 / 94 travels as 3617 basis points and reads 0.3617.
    #[test]
    fn twin_is_rendered_from_the_decoded_records() {
        let mut entry = item(194_784, Category::Commodity);
        entry.record.spans.get_mut(&Span::D1).expect("d1").sale_rate = Some(34.0 / 94.0);
        entry.record.supply_days = Some(u16::MAX);
        let text = twin_text(&snapshot(vec![entry]));
        // Last key of its span block, so no comma follows.
        assert!(text.contains("\"saleRate\": 0.3617\n"), "{text}");
        assert!(text.contains("\"supplyDays\": 65535"), "{text}");
        assert!(!text.contains("0.361702"), "{text}");
    }

    #[test]
    fn twin_is_pretty_printed_with_two_space_indent() {
        let text = twin_text(&snapshot(vec![item(25, Category::Item)]));
        assert!(
            text.starts_with("{\n  \"version\": 4,\n  \"trackerVersion\": \"0.1.0\",\n"),
            "{text}"
        );
        assert!(text.ends_with("\n  ]\n}"), "no trailing newline: the file is the document");
    }
}

//! Domain types shared by ports and services. Plain data + small helpers only.

use std::collections::BTreeMap;
use std::fmt;

// ---------------------------------------------------------------- identity

/// Slug identifying one configured source target (see `config::TargetConfig`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetId(pub String);

impl TargetId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TargetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Canonical item identity. Equipment/_consumables roll up to their base
/// `item_id` (variant data cannot map onto the in-game item key); pets are
/// first-class with their full identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ItemRef {
    Item {
        item_id: i64,
    },
    Pet {
        species: i64,
        level: i32,
        quality: i32,
        breed: i32,
    },
}

// ---------------------------------------------------------------- listings

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingKind {
    Commodity,
    Item,
}

/// Server `time_left` bands with their lower bounds (seconds). A listing that
/// vanishes earlier than the lower bound must have sold, not expired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeLeftBand {
    Short,
    Medium,
    Long,
    VeryLong,
}

impl TimeLeftBand {
    pub fn as_str(&self) -> &'static str {
        match self {
            TimeLeftBand::Short => "SHORT",
            TimeLeftBand::Medium => "MEDIUM",
            TimeLeftBand::Long => "LONG",
            TimeLeftBand::VeryLong => "VERY_LONG",
        }
    }

    /// Conservative lower bound of the band in seconds.
    pub fn lower_bound_secs(&self) -> i64 {
        match self {
            TimeLeftBand::Short => 0,
            TimeLeftBand::Medium => 30 * 60,
            TimeLeftBand::Long => 2 * 60 * 60,
            TimeLeftBand::VeryLong => 12 * 60 * 60,
        }
    }

    /// Parses the source spelling; unknown values fall back to `Short` (the
    /// least credit-prone band).
    pub fn parse(value: &str) -> TimeLeftBand {
        match value {
            "MEDIUM" => TimeLeftBand::Medium,
            "LONG" => TimeLeftBand::Long,
            "VERY_LONG" => TimeLeftBand::VeryLong,
            _ => TimeLeftBand::Short,
        }
    }

    /// Sell-through credit rule (plan D7): a listing that vanishes earlier
    /// than the lower bound of its last observed remaining-time band cannot
    /// have expired naturally — it is a sale candidate. SHORT (lower bound 0)
    /// never credits.
    pub fn credits_sale(&self, last_seen_at: i64, vanished_at: i64) -> bool {
        vanished_at - last_seen_at < self.lower_bound_secs()
    }
}

/// One clean listing row — the ACL output every downstream stage consumes.
/// All copper amounts are integers; `unit_price` is always per unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedListing {
    pub auction_id: i64,
    pub item_ref: ItemRef,
    pub kind: ListingKind,
    pub quantity: i64,
    pub unit_price: i64,
    pub bid: Option<i64>,
    pub time_left: TimeLeftBand,
    pub context: Option<i64>,
    pub bonus_lists: Vec<i64>,
}

// ---------------------------------------------------------------- snapshots

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadKind {
    Commodities,
    Auctions,
}

impl PayloadKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PayloadKind::Commodities => "commodities",
            PayloadKind::Auctions => "auctions",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotStatus {
    Ok,
    ParseFailed,
    HttpFailed,
}

impl SnapshotStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SnapshotStatus::Ok => "ok",
            SnapshotStatus::ParseFailed => "parse_failed",
            SnapshotStatus::HttpFailed => "http_error",
        }
    }
}

/// Metadata of a stored snapshot. `sha256`/`path` details live on the storage
/// side; the core only needs identity, freshness, and counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotMeta {
    pub id: i64,
    pub target: TargetId,
    pub kind: PayloadKind,
    pub fetched_at: i64,
    pub source_last_modified: Option<i64>,
    pub entry_count: u64,
    pub dropped_count: u64,
    pub status: SnapshotStatus,
}

/// What a source adapter hands to the ingestion cycle after its ACL pass:
/// raw bytes for archival, plus normalized listings and a drop count.
#[derive(Clone, Debug)]
pub struct FetchedPayload {
    pub kind: PayloadKind,
    pub source_last_modified: Option<i64>,
    pub listings: Vec<NormalizedListing>,
    pub dropped_count: u64,
    pub raw: Vec<u8>,
}

/// Conditional-GET outcome.
#[derive(Clone, Debug)]
pub enum FetchOutcome {
    Payload(FetchedPayload),
    NotModified,
}

/// New-snapshot request (storage computes hash/path from the raw bytes).
#[derive(Clone, Debug)]
pub struct NewSnapshot {
    pub target: TargetId,
    pub kind: PayloadKind,
    pub fetched_at: i64,
    pub source_last_modified: Option<i64>,
    pub entry_count: u64,
    pub dropped_count: u64,
    pub status: SnapshotStatus,
}

// ---------------------------------------------------------------- analytics

/// Aggregate price histogram for one item-day: per distinct unit price, the
/// number of unique auctions and their summed quantity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemDayHistogram {
    pub buckets: BTreeMap<i64, PriceBucket>,
    pub listing_count: u64,
    pub qty_sum: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PriceBucket {
    pub listings: u64,
    pub qty: i64,
}

/// A vanished sighting credited as a sale candidate (plan D7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaleCredit {
    pub auction_id: i64,
    pub item: ItemRef,
    pub quantity: i64,
    pub at: i64,
}

/// Memoized analytics block with its computation timestamp (the cache
/// consumer decides staleness).
#[derive(Clone, Debug, PartialEq)]
pub struct CachedBlock {
    pub block: MetricBlock,
    pub computed_at: i64,
}

/// Analytics time spans. Declaration order defines ordering (D1 < D7 < ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Span {
    D1,
    D7,
    D14,
    D30,
}

impl Span {
    pub const ALL: [Span; 4] = [Span::D1, Span::D7, Span::D14, Span::D30];

    pub fn name(&self) -> &'static str {
        match self {
            Span::D1 => "d1",
            Span::D7 => "d7",
            Span::D14 => "d14",
            Span::D30 => "d30",
        }
    }

    pub fn days(&self) -> i64 {
        match self {
            Span::D1 => 1,
            Span::D7 => 7,
            Span::D14 => 14,
            Span::D30 => 30,
        }
    }

    pub fn parse(value: &str) -> Option<Span> {
        match value {
            "d1" => Some(Span::D1),
            "d7" => Some(Span::D7),
            "d14" => Some(Span::D14),
            "d30" => Some(Span::D30),
            _ => None,
        }
    }
}

/// One aggregated analytics block — bit-identical semantics to the in-game
/// addon's `summarize()`: nearest-rank percentiles, floor-even median/MAD,
/// floored VWAP with median fallback.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricBlock {
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
    pub sale_rate: Option<f64>,
}

// ---------------------------------------------------------------- feed

/// Which serialization a feed publisher should emit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedFormat {
    Lua,
    Json,
}

/// One feed contract entry (tracker feed v2): an item or pet identity plus its
/// per-span metric blocks.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedEntry {
    pub item: ItemRef,
    pub metrics: BTreeMap<Span, MetricBlock>,
}

/// The full feed snapshot for one target (`AuctiMoggerTrackerFeed` payload).
#[derive(Clone, Debug, PartialEq)]
pub struct FeedSnapshot {
    pub target: TargetId,
    pub generated_at: i64,
    pub entries: Vec<FeedEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedFeed {
    pub at: i64,
    pub size: u64,
}

// ---------------------------------------------------------------- catalog / watchlist

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameRecord {
    pub item_id: i64,
    pub name: String,
    pub item_class: Option<String>,
    pub quality: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub item_id: i64,
    pub name: String,
    pub item_class: Option<String>,
    pub quality: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewWatchlistEntry {
    pub target: TargetId,
    pub item: ItemRef,
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchlistEntry {
    pub target: TargetId,
    pub item: ItemRef,
    pub note: Option<String>,
    pub added_at: i64,
}

/// UTC day number for a unix timestamp (86400-second days).
pub fn utc_day(unix_ts: i64) -> i64 {
    unix_ts.div_euclid(86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sale_credit_rule_follows_band_lower_bounds() {
        let t0 = 10_000;
        // SHORT never credits (lower bound 0).
        assert!(!TimeLeftBand::Short.credits_sale(t0, t0 + 60));
        // MEDIUM (>= 30 min): vanishing earlier credits, later does not.
        assert!(TimeLeftBand::Medium.credits_sale(t0, t0 + 600));
        assert!(!TimeLeftBand::Medium.credits_sale(t0, t0 + 1_800));
        // LONG (>= 2 h).
        assert!(TimeLeftBand::Long.credits_sale(t0, t0 + 3_600));
        assert!(!TimeLeftBand::Long.credits_sale(t0, t0 + 7_200));
        // VERY_LONG (>= 12 h).
        assert!(TimeLeftBand::VeryLong.credits_sale(t0, t0 + 11 * 3_600));
        assert!(!TimeLeftBand::VeryLong.credits_sale(t0, t0 + 12 * 3_600));
    }
}

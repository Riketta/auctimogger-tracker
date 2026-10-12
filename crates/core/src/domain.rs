//! Domain types shared by ports and services. Plain data + small helpers only.

use std::{
    collections::BTreeMap,
    fmt,
};

// ---------------------------------------------------------------- identity

/// Slug identifying one configured source target (see `config::TargetConfig`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetId(pub String);

impl TargetId {
    /// The underlying slug, borrowed.
    #[must_use]
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
    /// A plain item (equipment, consumable, material) keyed by its id.
    Item {
        /// The in-game item id.
        item_id: i64,
    },
    /// A battle pet, identified by its full appearance tuple.
    Pet {
        /// The pet species id.
        species: i64,
        /// The pet's current level.
        level: i32,
        /// The pet's quality (0 = poor … 6 = artifact, Blizzard spelling).
        quality: i32,
        /// The pet's breed id (stat distribution, e.g. 4 = P/P … 11 = S/S).
        breed: i32,
    },
}

// ---------------------------------------------------------------- listings

/// Whether a listing was seen in the commodity or the item auction house.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingKind {
    /// Stackable commodity traded in the commodity AH (per-unit price).
    Commodity,
    /// Item-level auction from the item AH (whole-listing buyout/bid).
    Item,
}

/// Server `time_left` bands with their lower bounds (seconds). A listing that
/// vanishes earlier than the lower bound must have sold, not expired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeLeftBand {
    /// Under 30 minutes remaining.
    Short,
    /// 30 minutes – 2 hours remaining.
    Medium,
    /// 2 – 12 hours remaining.
    Long,
    /// Over 12 hours remaining.
    VeryLong,
}

impl TimeLeftBand {
    /// The source wire spelling of the band (stored verbatim in feeds).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            TimeLeftBand::Short => "SHORT",
            TimeLeftBand::Medium => "MEDIUM",
            TimeLeftBand::Long => "LONG",
            TimeLeftBand::VeryLong => "VERY_LONG",
        }
    }

    /// Conservative lower bound of the band in seconds.
    #[must_use]
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
    #[must_use]
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
    #[must_use]
    pub fn credits_sale(&self, last_seen_at: i64, vanished_at: i64) -> bool {
        vanished_at - last_seen_at < self.lower_bound_secs()
    }
}

/// One clean listing row — the ACL output every downstream stage consumes.
/// All copper amounts are integers; `unit_price` is always per unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedListing {
    /// Blizzard auction id; stable while a listing stays up.
    pub auction_id: i64,
    /// Canonical item identity of the listing.
    pub item_ref: ItemRef,
    /// Which auction house the row came from (pricing semantics differ).
    pub kind: ListingKind,
    /// Number of units on the listing.
    pub quantity: i64,
    /// Buyout price per unit, in copper (commodities are per-unit already).
    pub unit_price: i64,
    /// Current bid in copper (item AH only; `None` for commodities).
    pub bid: Option<i64>,
    /// Remaining-time band last reported by the source.
    pub time_left: TimeLeftBand,
    /// Blizzard context id (e.g. the pet species a cage came from), if any.
    pub context: Option<i64>,
    /// Bonus list ids attached to the item (empty for plain items/pets).
    pub bonus_lists: Vec<i64>,
}

// ---------------------------------------------------------------- snapshots

/// Payload kind of one snapshot source endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PayloadKind {
    /// The commodity AH dump (one snapshot for the whole realm).
    Commodities,
    /// The per-item auction AH dump.
    Auctions,
}

impl PayloadKind {
    /// Lowercase wire/log spelling of the kind.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            PayloadKind::Commodities => "commodities",
            PayloadKind::Auctions => "auctions",
        }
    }
}

/// Terminal state of one snapshot fetch+ingest attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotStatus {
    /// Fetched, parsed and persisted.
    Ok,
    /// Fetched but the payload could not be parsed.
    ParseFailed,
    /// The HTTP fetch itself failed.
    HttpFailed,
}

impl SnapshotStatus {
    /// Lowercase wire/log spelling of the status.
    #[must_use]
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
    /// Storage-assigned snapshot id (monotonic per store).
    pub id: i64,
    /// Target the snapshot was fetched for.
    pub target: TargetId,
    /// Which source endpoint produced the snapshot.
    pub kind: PayloadKind,
    /// Cycle timestamp of the fetch (unix seconds).
    pub fetched_at: i64,
    /// Source `Last-Modified` header (unix seconds) if the source sent one.
    pub source_last_modified: Option<i64>,
    /// Number of entries that survived the ACL pass.
    pub entry_count: u64,
    /// Number of entries dropped by the ACL pass.
    pub dropped_count: u64,
    /// Terminal ingest state of the snapshot.
    pub status: SnapshotStatus,
}

/// What a source adapter hands to the ingestion cycle after its ACL pass:
/// raw bytes for archival, plus normalized listings and a drop count.
#[derive(Clone, Debug)]
pub struct FetchedPayload {
    /// Which source endpoint this payload came from.
    pub kind: PayloadKind,
    /// Source `Last-Modified` header (unix seconds), if present.
    pub source_last_modified: Option<i64>,
    /// Cleaned listings (the only rows downstream stages see).
    pub listings: Vec<NormalizedListing>,
    /// How many source entries the ACL pass discarded.
    pub dropped_count: u64,
    /// Verbatim raw payload bytes, for archival and re-parse.
    pub raw: Vec<u8>,
}

/// Conditional-GET outcome.
#[derive(Clone, Debug)]
pub enum FetchOutcome {
    /// A fresh payload was returned.
    Payload(FetchedPayload),
    /// The source answered 304 — the stored snapshot is still current.
    NotModified,
}

/// New-snapshot request (storage computes hash/path from the raw bytes).
#[derive(Clone, Debug)]
pub struct NewSnapshot {
    /// Target the snapshot belongs to.
    pub target: TargetId,
    /// Which source endpoint produced the snapshot.
    pub kind: PayloadKind,
    /// Cycle timestamp of the fetch (unix seconds).
    pub fetched_at: i64,
    /// Source `Last-Modified` header (unix seconds) if the source sent one.
    pub source_last_modified: Option<i64>,
    /// Number of entries that survived the ACL pass.
    pub entry_count: u64,
    /// Number of entries dropped by the ACL pass.
    pub dropped_count: u64,
    /// Terminal ingest state of the snapshot.
    pub status: SnapshotStatus,
}

// ---------------------------------------------------------------- analytics

/// Aggregate price histogram for one item-day: per distinct unit price, the
/// number of unique auctions and their summed quantity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemDayHistogram {
    /// Unit price (copper) → bucket of unique auctions at that price.
    pub buckets: BTreeMap<i64, PriceBucket>,
    /// Number of unique auctions folded into the day (not snapshot sightings).
    pub listing_count: u64,
    /// Total quantity across all unique auctions of the day.
    pub qty_sum: i64,
}

/// One histogram bucket: all unique auctions of a day at one unit price.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PriceBucket {
    /// Unique auctions folded at this price.
    pub listings: u64,
    /// Summed quantity of those auctions.
    pub qty: i64,
}

/// Per-item-day aggregate used by ranking/overview views (derived from the
/// day fold; median/min are listing-level nearest-rank over that day).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemDayRow {
    /// The item the row aggregates.
    pub item: ItemRef,
    /// UTC day number the row aggregates (see `utc_day`).
    pub day: i64,
    /// Unique auctions of the day.
    pub listing_count: u64,
    /// Total quantity of the day's unique auctions.
    pub qty_sum: i64,
    /// Cheapest listing of the day (copper per unit).
    pub min: i64,
    /// Listing-level nearest-rank median (copper per unit).
    pub median: i64,
    /// Median absolute deviation of the day's unit prices.
    pub mad: i64,
}

/// A vanished sighting credited as a sale candidate (plan D7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaleCredit {
    /// The auction id of the vanished listing.
    pub auction_id: i64,
    /// The item that was sold.
    pub item: ItemRef,
    /// Quantity the listing carried when last seen.
    pub quantity: i64,
    /// When the listing was observed missing (unix seconds).
    pub at: i64,
}

/// Memoized analytics block with its computation timestamp (the cache
/// consumer decides staleness).
#[derive(Clone, Debug, PartialEq)]
pub struct CachedBlock {
    /// The computed metric block.
    pub block: MetricBlock,
    /// When the block was computed (unix seconds).
    pub computed_at: i64,
}

/// Analytics time spans. Declaration order defines ordering (D1 < D7 < ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Span {
    /// Trailing 1 day.
    D1,
    /// Trailing 7 days.
    D7,
    /// Trailing 14 days.
    D14,
    /// Trailing 30 days.
    D30,
}

impl Span {
    /// Every span, in ascending declaration order.
    pub const ALL: [Span; 4] = [Span::D1, Span::D7, Span::D14, Span::D30];

    /// Lowercase wire spelling of the span (feed keys, query params).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Span::D1 => "d1",
            Span::D7 => "d7",
            Span::D14 => "d14",
            Span::D30 => "d30",
        }
    }

    /// Window length of the span in whole days.
    #[must_use]
    pub fn days(&self) -> i64 {
        match self {
            Span::D1 => 1,
            Span::D7 => 7,
            Span::D14 => 14,
            Span::D30 => 30,
        }
    }

    /// Parses the lowercase wire spelling; `None` for unknown spans.
    #[must_use]
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
    /// Sample size: unique auctions folded into the window.
    pub n: i64,
    /// Cheapest unit price in the window (copper).
    pub min: i64,
    /// 5th percentile unit price (nearest rank, copper).
    pub p05: i64,
    /// 25th percentile unit price (nearest rank, copper).
    pub p25: i64,
    /// Floor-even median unit price (copper).
    pub median: i64,
    /// 75th percentile unit price (nearest rank, copper).
    pub p75: i64,
    /// 95th percentile unit price (nearest rank, copper).
    pub p95: i64,
    /// Most expensive unit price in the window (copper).
    pub max: i64,
    /// Floor-even median absolute deviation of unit prices.
    pub mad: i64,
    /// Volume-weighted average price, floored; median when qty is 0 (copper).
    pub vwap: i64,
    /// Summed quantity across the window's unique auctions.
    pub qty_sum: i64,
    /// Unique auctions folded into the window (not snapshot sightings).
    pub listing_count: i64,
    /// Fraction of vanished listings credited as sold over the trailing
    /// 14 days; `None` when no sightings exist.
    pub sale_rate: Option<f64>,
}

// ---------------------------------------------------------------- feed

/// Which serialization a feed publisher should emit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FeedFormat {
    /// Lua table literal, loadable by the in-game addon.
    Lua,
    /// JSON document.
    Json,
}

/// One feed contract entry (tracker feed v2): an item or pet identity plus its
/// per-span metric blocks.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedEntry {
    /// The tracked item or pet identity.
    pub item: ItemRef,
    /// Metric block per span; spans without data are absent.
    pub metrics: BTreeMap<Span, MetricBlock>,
}

/// The full feed snapshot for one target (`AuctiMoggerTrackerFeed` payload).
#[derive(Clone, Debug, PartialEq)]
pub struct FeedSnapshot {
    /// The target the feed was built for.
    pub target: TargetId,
    /// Data freshness stamp (unix seconds) — newest source `Last-Modified`
    /// across the target's payload kinds, or the publish time if unknown.
    pub generated_at: i64,
    /// Feed entries, sorted by item identity.
    pub entries: Vec<FeedEntry>,
}

/// Publication info for one rendered feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublishedFeed {
    /// When the feed was published (unix seconds).
    pub at: i64,
    /// Serialized size of the published feed (bytes).
    pub size: u64,
}

// ---------------------------------------------------------------- catalog / watchlist

/// Human-readable catalog record for one item id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameRecord {
    /// The item id the record describes.
    pub item_id: i64,
    /// Localized item name as reported by the item API.
    pub name: String,
    /// Item class label (e.g. "Consumable"), when the API reports one.
    pub item_class: Option<String>,
    /// Quality label (e.g. "epic"), when the API reports one.
    pub quality: Option<String>,
}

/// Catalog search result: the item id plus display fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    /// The item id of the hit.
    pub item_id: i64,
    /// Localized item name.
    pub name: String,
    /// Item class label, when known.
    pub item_class: Option<String>,
    /// Quality label, when known.
    pub quality: Option<String>,
}

/// Watchlist insertion request; storage stamps the insertion time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewWatchlistEntry {
    /// The target the entry belongs to.
    pub target: TargetId,
    /// The item or pet to track.
    pub item: ItemRef,
    /// Free-form user note, if any.
    pub note: Option<String>,
}

/// A stored watchlist entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchlistEntry {
    /// The target the entry belongs to.
    pub target: TargetId,
    /// The tracked item or pet.
    pub item: ItemRef,
    /// Free-form user note, if any.
    pub note: Option<String>,
    /// When the entry was added (unix seconds).
    pub added_at: i64,
}

/// UTC day number for a unix timestamp (86400-second days).
#[must_use]
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

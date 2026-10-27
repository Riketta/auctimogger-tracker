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

/// Game-flavor scope for catalog data: which game version's item ids the
/// catalog entries describe. Two targets may share one database while
/// tracking different game versions, and the same numeric id can carry a
/// different name per version — so catalog rows are keyed per flavor.
/// Flavors the adapters have no dedicated endpoint preset for (private
/// servers, beta keys) resolve through the retail-compatible endpoints.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Flavor(pub String);

impl Default for Flavor {
    /// The documented default: retail data. Manual because a derived
    /// `Default` would key catalog rows under an empty string that no
    /// adapter prefix or lookup ever reads.
    fn default() -> Self {
        Self(Self::RETAIL.into())
    }
}

impl Flavor {
    /// The default flavor: current retail data.
    pub const RETAIL: &'static str = "retail";

    /// The flavor key, borrowed.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Flavor {
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

/// Market category of an aggregate: which auction house a listing came from
/// and what shape of good it is. Derived per listing (see
/// [`Category::derive`]), never read off the wire: the commodities payload is
/// all `Commodity`, while the auctions payload splits into `Item` and `Pet`
/// by identity shape. The split dimension for storage, ranking, metrics,
/// feeds, API and UI — an item trading in both auction houses is two
/// categories' worth of evidence, never one merged population.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// Plain items traded in the item auction house.
    Item,
    /// Stackable commodities traded in the commodity auction house.
    Commodity,
    /// Battle pets (all variants) traded in the item auction house.
    Pet,
}

impl Category {
    /// Every category, in canonical (`as_str`) order. The all-categories API
    /// responses and the scheduler's refresh loop walk this list.
    pub const ALL: [Category; 3] = [Category::Item, Category::Commodity, Category::Pet];

    /// Derives the category of one listing from its auction house and its
    /// identity shape. Total: every listing maps to exactly one category.
    #[must_use]
    pub fn derive(kind: ListingKind, item: &ItemRef) -> Category {
        match kind {
            ListingKind::Commodity => Category::Commodity,
            ListingKind::Item => match item {
                ItemRef::Pet { .. } => Category::Pet,
                ItemRef::Item { .. } => Category::Item,
            },
        }
    }

    /// Lowercase wire/storage spelling (storage column, API parameter).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Category::Item => "item",
            Category::Commodity => "commodity",
            Category::Pet => "pet",
        }
    }

    /// Parses the lowercase wire/storage spelling (see [`Category::as_str`]).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "item" => Some(Category::Item),
            "commodity" => Some(Category::Commodity),
            "pet" => Some(Category::Pet),
            _ => None,
        }
    }
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
    /// never credits. A vanish at or before its last sighting (a clock step
    /// or same-instant anomaly) is never a credit for ANY band: fabricated
    /// sales must not follow from timestamp anomalies.
    #[must_use]
    pub fn credits_sale(&self, last_seen_at: i64, vanished_at: i64) -> bool {
        if vanished_at <= last_seen_at {
            return false;
        }
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

    /// Parses the lowercase wire/log spelling (see [`PayloadKind::as_str`]).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "commodities" => Some(PayloadKind::Commodities),
            "auctions" => Some(PayloadKind::Auctions),
            _ => None,
        }
    }
}

/// Terminal state of one snapshot fetch+ingest attempt. Failed fetches/parse
/// attempts never persist a snapshot row (the cycle errors out before
/// `record`), so `Ok` is the only state that can occur today; the enum stays
/// so the stored `status` column has a typed spelling for future states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotStatus {
    /// Fetched, parsed and persisted.
    Ok,
}

impl SnapshotStatus {
    /// Lowercase wire/log spelling of the status.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            SnapshotStatus::Ok => "ok",
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
    /// Drop breakdown by reason (why the ACL pass discarded rows). Empty
    /// when nothing was dropped or the source does not classify reasons.
    pub dropped_reasons: std::collections::BTreeMap<String, u64>,
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
    /// Drop breakdown by reason (see `SnapshotMeta::dropped_reasons`).
    pub dropped_reasons: std::collections::BTreeMap<String, u64>,
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
    /// Drop breakdown by reason (see `SnapshotMeta::dropped_reasons`).
    pub dropped_reasons: std::collections::BTreeMap<String, u64>,
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
/// day fold; min/p05/median are listing-level nearest-rank over that day).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemDayRow {
    /// The item the row aggregates.
    pub item: ItemRef,
    /// Market category the row aggregates (which auction house the fold's
    /// listings came from; pets split off the item-AH rows).
    pub category: Category,
    /// UTC day number the row aggregates (see `utc_day`).
    pub day: i64,
    /// Unique auctions of the day.
    pub listing_count: u64,
    /// Total quantity of the day's unique auctions.
    pub qty_sum: i64,
    /// Cheapest listing of the day (copper per unit).
    pub min: i64,
    /// 5th-percentile unit price of the day (nearest rank) — the robust
    /// floor behind discount scoring; equals `min` on days with ≤ 20
    /// listings.
    pub p05: i64,
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
    /// Same estimator over the trailing 180 days — the slow, stable demand
    /// signal behind fast-window spikes; `None` when no sightings exist in
    /// the window (the ledger only accrues this much history over time).
    pub sale_rate_180d: Option<f64>,
    /// Whole days since the last credited sale (ledger-derived, window-
    /// independent); `None` when no sale was ever credited for the identity.
    pub days_since_last_sale: Option<i64>,
    /// Net listings that closed after the last credited sale day (closes −
    /// reversals); `None` for the same reason as `days_since_last_sale` —
    /// the "stopped selling" churn signal.
    pub closures_since_last_sale: Option<i64>,
}

/// Ledger-derived recency for one identity: the last day a sale was credited
/// and how many listings have closed since (the raw material behind a
/// `MetricBlock`'s `days_since_last_sale` / `closures_since_last_sale`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaleRecency {
    /// UTC day number of the last credited sale (see `utc_day`).
    pub last_sale_day: i64,
    /// Net listings closed strictly after `last_sale_day` — same-day closures
    /// belong to the sale day's own ledger row.
    pub closures_since: i64,
}

/// Ledger-derived stats attached to every span block of one identity (they
/// are window-independent, so all spans of the same (item, category) carry
/// identical values).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BlockLedger {
    /// Trailing 14-day sale rate (the block cache's fast window).
    pub sale_rate: Option<f64>,
    /// Trailing 180-day sale rate (the slow demand window).
    pub sale_rate_slow: Option<f64>,
    /// Days since the last credited sale; `None` = never sold in the ledger.
    pub days_since_last_sale: Option<i64>,
    /// Net closures after the last credited sale; `None` alongside
    /// `days_since_last_sale`.
    pub closures_since_last_sale: Option<i64>,
}

impl BlockLedger {
    /// No ledger evidence at all — the historic `block_from_histograms(..,
    /// None)` argument.
    pub const NONE: Self = Self {
        sale_rate: None,
        sale_rate_slow: None,
        days_since_last_sale: None,
        closures_since_last_sale: None,
    };

    /// Builds the ledger from a sale recency reading, counting whole days
    /// since the last credited sale as of `today` (day number).
    #[must_use]
    pub fn from_recency(
        sale_rate: Option<f64>,
        sale_rate_slow: Option<f64>,
        recency: Option<SaleRecency>,
        today: i64,
    ) -> Self {
        match recency {
            None => Self { sale_rate, sale_rate_slow, ..Self::NONE },
            Some(recency) => Self {
                sale_rate,
                sale_rate_slow,
                days_since_last_sale: Some(today - recency.last_sale_day),
                closures_since_last_sale: Some(recency.closures_since),
            },
        }
    }
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

/// One feed contract entry (tracker feed v3): an item or pet identity plus its
/// per-span metric blocks.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedEntry {
    /// The tracked item or pet identity (canonical grain).
    pub item: ItemRef,
    /// Which market the entry's metrics describe: an item trading in both
    /// auction houses exports one entry per category.
    pub category: Category,
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
    /// Item subclass label within the class (e.g. "Cloth"), when the API
    /// reports one.
    pub item_subclass: Option<String>,
    /// Equipment slot label (e.g. "Two-Hand"), when the API reports one.
    pub inventory_slot: Option<String>,
    /// Quality label (e.g. "epic"), when the API reports one.
    pub quality: Option<String>,
    /// Icon slug (e.g. `inv_sword_1h_04`), renderable through the game's icon
    /// CDN; when the API reports one.
    pub icon: Option<String>,
    /// Vendor sell price in copper, when the API reports one.
    pub sell_price: Option<i64>,
    /// Unix seconds of the last successful source fetch (enrichment window
    /// bookkeeping), stamped by the store on upsert — the adapters deliver
    /// records without a clock. `0` for search/browse hits, which do not
    /// carry it.
    pub fetched_at: i64,
}

impl NameRecord {
    /// Whether the record predates the enriched catalog columns and carries
    /// none of the newer fields. Sparse rows are re-fetched once their fetch
    /// time leaves the enrichment window so old rows gain the richer fields
    /// without a mass re-fetch.
    #[must_use]
    pub fn is_sparse(&self) -> bool {
        self.item_subclass.is_none()
            && self.inventory_slot.is_none()
            && self.icon.is_none()
            && self.sell_price.is_none()
    }
}

/// Human-readable catalog record for one battle-pet species (variant-free:
/// species identity only — level/quality/breed are per-auction attributes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PetNameRecord {
    /// The pet species id the record describes.
    pub species_id: i64,
    /// Localized species name as reported by the pet API.
    pub name: String,
}

/// Catalog search result: one named item or one named pet species.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchHit {
    /// A named item (the classic catalog record).
    Item(NameRecord),
    /// A named battle-pet species; detail pages pick the canonical variant.
    Pet(PetNameRecord),
}

impl SearchHit {
    /// The display name of the hit, whatever the kind.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Item(record) => &record.name,
            Self::Pet(record) => &record.name,
        }
    }
}

/// Keyset pagination cursor for alphabetical catalog browsing: identity of
/// the last row of the previous page, exclusive. `kind` sorts before `id`
/// so (name, kind, id) is a total order across item/pet rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowseCursor {
    /// Name of the last row on the previous page.
    pub name: String,
    /// Catalog kind of that row (`"item"` or `"pet"`).
    pub kind: String,
    /// Item or species id of that row (the within-name tiebreaker).
    pub id: i64,
}

/// How many named records the catalog holds, per kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogCounts {
    /// Named item records.
    pub items: u64,
    /// Named battle-pet species records.
    pub pets: u64,
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

/// A user-defined alert rule: "tell me when this condition holds for this
/// identity". Evaluation runs after ingest cycles; a rule fires at most once
/// per cooldown window (the cooldown anchor is `last_fired_at`).
#[derive(Clone, Debug, PartialEq)]
pub struct AlertRule {
    /// Storage-assigned rule id.
    pub id: i64,
    /// Target the rule watches.
    pub target: TargetId,
    /// The tracked item or pet.
    pub item: ItemRef,
    /// What the rule watches for.
    pub condition: AlertCondition,
    /// Free-form label carried into notifications and the events page.
    pub note: Option<String>,
    /// When the rule was created (unix seconds).
    pub created_at: i64,
    /// When the rule last fired (unix seconds) — the cooldown anchor.
    pub last_fired_at: Option<i64>,
}

/// A rule as handed to the store; `id`, `created_at` and `last_fired_at` are
/// assigned by the storage layer.
#[derive(Clone, Debug, PartialEq)]
pub struct NewAlertRule {
    /// Target the rule watches.
    pub target: TargetId,
    /// The tracked item or pet.
    pub item: ItemRef,
    /// What the rule watches for.
    pub condition: AlertCondition,
    /// Free-form label carried into notifications.
    pub note: Option<String>,
}

/// What an alert rule watches for. Price and floor conditions observe the
/// same cached metric blocks the UI shows, so an alert can never contradict
/// its page; the sale-rate condition reads the trailing sale ledger live
/// (the ledger is the same source the blocks' cached rate was computed
/// from — it only updates more often than the block cache).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AlertCondition {
    /// The current day's median unit price drops to `copper` or below —
    /// a literal price target.
    PriceBelow {
        /// Threshold in copper per unit.
        copper: i64,
    },
    /// The span's median drops to its own floor percentile or below — the
    /// whole window is trading at floor prices (a market-level signal,
    /// independent of absolute price levels).
    BelowFloor {
        /// Window whose median/floor are compared.
        span: Span,
        /// Which floor percentile to compare against.
        percentile: FloorPercentile,
    },
    /// The trailing 14-day sale rate reaches `rate` or above — demand is
    /// accelerating.
    SaleRateAbove {
        /// Threshold fraction (0.0..=1.0).
        rate: f64,
    },
    /// Whole days since the ledger last credited a sale reach `days` or more
    /// — the item has gone quiet (the `NumExpires` lesson from TSM). Identities
    /// with no credited sale at all never fire: a young ledger is not evidence
    /// of a stalled market.
    DaysSinceLastSaleAbove {
        /// Threshold in whole days (≥ 1).
        days: i64,
    },
}

/// Floor percentile behind [`AlertCondition::BelowFloor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloorPercentile {
    /// The 5th percentile — deep-floor deals.
    P05,
    /// The 25th percentile — the broad lower quartile.
    P25,
}

impl FloorPercentile {
    /// Lowercase wire/UI spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FloorPercentile::P05 => "p05",
            FloorPercentile::P25 => "p25",
        }
    }

    /// Parses the lowercase spelling (see [`FloorPercentile::as_str`]).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "p05" => Some(FloorPercentile::P05),
            "p25" => Some(FloorPercentile::P25),
            _ => None,
        }
    }
}

/// One fired alert: the persisted history row (events page, JSON API) and the
/// payload dispatch adapters receive.
#[derive(Clone, Debug, PartialEq)]
pub struct AlertEvent {
    /// Storage-assigned event id.
    pub id: i64,
    /// The rule that fired.
    pub rule_id: i64,
    /// Target the rule watches.
    pub target: TargetId,
    /// The tracked item or pet.
    pub item: ItemRef,
    /// The condition that held.
    pub condition: AlertCondition,
    /// The observed value that fired the rule (median copper for price
    /// conditions, the sale-rate fraction for `SaleRateAbove`); `None` when
    /// the condition fired without a single comparable number.
    pub observed: Option<f64>,
    /// Human-readable one-liner describing what was observed.
    pub message: String,
    /// When the rule fired (unix seconds).
    pub fired_at: i64,
}

/// A fired alert as handed to the store; `id` is assigned by the storage
/// layer.
#[derive(Clone, Debug, PartialEq)]
pub struct NewAlertEvent {
    /// The rule that fired.
    pub rule_id: i64,
    /// Target the rule watches.
    pub target: TargetId,
    /// The tracked item or pet.
    pub item: ItemRef,
    /// The condition that held.
    pub condition: AlertCondition,
    /// The observed value that fired the rule (see [`AlertEvent::observed`]).
    pub observed: Option<f64>,
    /// Human-readable one-liner describing what was observed.
    pub message: String,
    /// When the rule fired (unix seconds).
    pub fired_at: i64,
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
        // A clock stepped backwards between sighting and vanish must not
        // manufacture a credit either (the negative delta clamps to 0).
        assert!(!TimeLeftBand::Short.credits_sale(t0, t0 - 60));
        assert!(!TimeLeftBand::Medium.credits_sale(t0, t0 - 60));
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

    #[test]
    fn category_derivation_covers_every_payload_and_identity_shape() {
        let item = ItemRef::Item { item_id: 7909 };
        let pet = ItemRef::Pet { species: 264, level: 1, quality: 3, breed: 4 };
        // The commodities payload is all commodity, regardless of identity
        // shape (pets never arrive there, and the mapping must stay total
        // even if an emulator ever sent one).
        assert_eq!(Category::derive(ListingKind::Commodity, &item), Category::Commodity);
        assert_eq!(Category::derive(ListingKind::Commodity, &pet), Category::Commodity);
        // The auctions payload splits by identity shape.
        assert_eq!(Category::derive(ListingKind::Item, &item), Category::Item);
        assert_eq!(Category::derive(ListingKind::Item, &pet), Category::Pet);
    }

    #[test]
    fn category_wire_spelling_round_trips_and_rejects_unknowns() {
        for category in Category::ALL {
            assert_eq!(Category::parse(category.as_str()), Some(category));
        }
        // Canonical order matches the wire spellings' documented order.
        assert_eq!(Category::ALL.map(|c| c.as_str()), ["item", "commodity", "pet"]);
        assert_eq!(Category::parse("commodities"), None);
        assert_eq!(Category::parse("auctions"), None);
        assert_eq!(Category::parse(""), None);
    }
}

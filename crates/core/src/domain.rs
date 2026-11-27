//! Domain types shared by ports and services. Plain data + small helpers only.

use std::{
    collections::BTreeMap,
    fmt,
};

use crate::error::StoreError;

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

    /// The payload dump a category's raw listings live in: commodities in
    /// the commodity dump, items and pet cages in the auctions dump. The
    /// `listing` table carries no category column — raw-listing queries
    /// scope by the owning snapshot's payload kind through this mapping.
    #[must_use]
    pub fn payload_kind(&self) -> PayloadKind {
        match self {
            Category::Commodity => PayloadKind::Commodities,
            Category::Item | Category::Pet => PayloadKind::Auctions,
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

    /// Credit eligibility: whether a close of a listing last seen in this
    /// band belongs in the sale-rate denominator. At the one-hour poll
    /// cadence (the Blizzard APIs' rotation) MEDIUM's and SHORT's lower
    /// bounds sit at or below one poll gap, so their listings can
    /// structurally never credit — counting them in the denominator dilutes
    /// rate. `LONG` (2 h) and `VERY_LONG` (12 h) clear the cadence with
    /// margin. A close that actually credits is always eligible regardless
    /// of band (the credit itself proves the listing could have credited).
    #[must_use]
    pub fn is_credit_eligible(&self) -> bool {
        matches!(self, TimeLeftBand::Long | TimeLeftBand::VeryLong)
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

// ------------------------------------------------------- snapshot browsing

/// Fixed number of listing rows per browse page (the snapshot viewer's page
/// size; vanilla-AH page arrows sit on top of it).
pub const LISTING_PAGE_SIZE: usize = 50;

/// Column the snapshot viewer can order listings by. Global name ordering is
/// intentionally absent — name access goes through the prefix/id search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingSortKey {
    /// Unit price in copper, cheapest first by default (the AH "min price").
    Price,
    /// Listing stack size.
    Quantity,
    /// Remaining-time band, soonest-expiring first (`Short` < `Medium` <
    /// `Long` < `VeryLong`), the semantic order — not the wire string order.
    TimeLeft,
}

impl ListingSortKey {
    /// Lowercase wire/URL spelling (the `sort` page/API parameter).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            ListingSortKey::Price => "price",
            ListingSortKey::Quantity => "qty",
            ListingSortKey::TimeLeft => "time",
        }
    }

    /// Parses the lowercase spelling; anything else falls back to
    /// [`ListingSortKey::Price`] (the AH default column).
    #[must_use]
    pub fn parse(value: &str) -> ListingSortKey {
        match value {
            "qty" => ListingSortKey::Quantity,
            "time" => ListingSortKey::TimeLeft,
            _ => ListingSortKey::Price,
        }
    }
}

/// One browse page request: catalog-backed filters, one sort column, and the
/// 1-based page index over [`LISTING_PAGE_SIZE`]-sized pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingQuery {
    /// Case-insensitive item/pet name prefix (catalog names; empty or absent
    /// disables the filter).
    pub name_prefix: Option<String>,
    /// Exact item id (not species id — pets are addressed by their tuple
    /// through the identity search instead).
    pub item_id: Option<i64>,
    /// Exact catalog item-class label (e.g. `"Consumable"`); only enriched
    /// rows can match.
    pub class: Option<String>,
    /// Exact catalog quality label (e.g. `"Common"`); only enriched rows can
    /// match.
    pub quality: Option<String>,
    /// Sort column.
    pub sort: ListingSortKey,
    /// `true` orders the sort column descending (largest/last first).
    pub desc: bool,
    /// 1-based page index; values below 1 are clamped by the service.
    pub page: u32,
}

/// One stored listing joined to its catalog identity — the snapshot viewer's
/// row model. Names/quality/level arrive from the item (or pet) catalog and
/// are `None` while the identity is still sparse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingRow {
    /// Blizzard auction id (stable while the listing stays up).
    pub auction_id: i64,
    /// Canonical identity of the listing (pets carry their full variant
    /// tuple, so the viewer can show pet level without a catalog join).
    pub item: ItemRef,
    /// Stack size of the listing.
    pub quantity: i64,
    /// Buyout per unit, copper (commodities are per-unit already).
    pub unit_price: i64,
    /// Current bid, copper (auctions-kind listings only).
    pub bid: Option<i64>,
    /// Remaining-time band last reported by the source.
    pub time_left: TimeLeftBand,
    /// Display name from the catalog; `None` = sparse identity (rendered as
    /// the raw `item {id}` / `pet {species}` label).
    pub name: Option<String>,
    /// Catalog quality label (items only; pets carry quality in the tuple).
    pub quality: Option<String>,
    /// Catalog item level (items only; `None` until the API enrichment
    /// reports one — non-equipment never has a level).
    pub item_level: Option<i64>,
}

/// One page of [`ListingRow`]s plus the paging metadata the viewer renders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingPage {
    /// The page's rows, in query order (sort column, then `auction_id`).
    pub rows: Vec<ListingRow>,
    /// Total listings matching the filters (across all pages).
    pub total: u64,
    /// The served 1-based page index (clamped).
    pub page: u32,
    /// Number of non-empty pages (`0` when nothing matches).
    pub pages: u32,
}

/// Per-item-class listing count for one snapshot — the class filter's honest
/// dropdown (aggregated on first read, then cached against a catalog
/// watermark and recomputed when later enrichment moves it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassSummary {
    /// Catalog item-class label; only enriched listings are counted.
    pub class: String,
    /// Listings of that class in the snapshot.
    pub listings: u64,
}

/// Snapshot resolution outcome for the browse viewer.
#[derive(Debug, PartialEq, Eq)]
pub enum BrowseError {
    /// No explicit snapshot was requested and the target/kind pair has no
    /// snapshots at all (the "run a fetch cycle" empty state).
    NoSnapshots,
    /// The requested snapshot id is not in the store.
    UnknownSnapshot,
    /// The snapshot exists but belongs to a different target/kind pair than
    /// the URL addresses — the link is malformed, not the data.
    SnapshotMismatch,
    /// Underlying store failure.
    Store(StoreError),
}

impl From<StoreError> for BrowseError {
    fn from(error: StoreError) -> Self {
        BrowseError::Store(error)
    }
}

impl core::fmt::Display for BrowseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BrowseError::NoSnapshots => write!(f, "no snapshots stored for this target/kind"),
            BrowseError::UnknownSnapshot => write!(f, "unknown snapshot id"),
            BrowseError::SnapshotMismatch => {
                write!(f, "snapshot belongs to a different target/kind")
            }
            BrowseError::Store(error) => write!(f, "{error}"),
        }
    }
}

// ---------------------------------------------------------- live snapshots

/// One item's listing summary inside a single stored snapshot — the live side
/// of one market at the moment of that fetch. Written with the snapshot, so
/// "what is listed right now" is a point read instead of a listing scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotFloor {
    /// Cheapest unit price listed for the item in the snapshot (copper).
    pub floor_price: i64,
    /// Quantity listed at exactly that price — how deep the deal is.
    pub floor_qty: i64,
    /// Listings of the item in the snapshot.
    pub listing_count: u64,
    /// Total quantity of the item listed in the snapshot.
    pub qty_sum: i64,
}

/// One listing of an item in one stored snapshot, joined to its sighting
/// bookkeeping — the per-listing view behind the sniper alert conditions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotListing {
    /// Blizzard auction id; stable while the listing stays up.
    pub auction_id: i64,
    /// Buyout price per unit (copper).
    pub unit_price: i64,
    /// Number of units on the listing.
    pub quantity: i64,
    /// Snapshot that first sighted the listing (`None` without a sighting
    /// row). Equals the queried snapshot's id exactly for listings new to it.
    pub first_seen_snapshot: Option<i64>,
    /// Unix seconds of that first sighting (`None` without a sighting row).
    pub first_seen_at: Option<i64>,
}

// ------------------------------------------------------------------ crafting

/// One reagent of a recipe: the item consumed and how many units one make
/// uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reagent {
    /// The consumed item.
    pub item: ItemRef,
    /// Units consumed per make (≥ 1).
    pub qty: i64,
}

/// One manual recipe: the item crafted, how many units a single make yields
/// and the reagents it consumes. The recipe book is hand-maintained (see
/// `[crafts].recipes` in the config); the tracker only supplies the prices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    /// The crafted item.
    pub item: ItemRef,
    /// Units one make yields (≥ 1).
    pub makes: i64,
    /// Reagents consumed per make, at least one.
    pub reagents: Vec<Reagent>,
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
    /// Quantity of the day's cheapest listing — how deep the day's floor was.
    pub min_qty: i64,
    /// 5th-percentile unit price of the day (nearest rank) — the robust
    /// floor behind discount scoring; equals `min` on days with ≤ 20
    /// listings.
    pub p05: i64,
    /// Listing-level nearest-rank median (copper per unit).
    pub median: i64,
    /// Quantity-weighted median unit price of the day: the price half the
    /// listed quantity sits at (the bulk price for stackable goods; equals
    /// `median` when every listing is a single unit). Falls back to `median`
    /// when nothing carries quantity.
    pub qty_median: i64,
    /// Median absolute deviation of the day's unit prices.
    pub mad: i64,
}

/// One UTC `(day, hour)` cell of the permanent intraday fold: everything the
/// buy-timing view needs about an identity's cheapest asks in that hour.
/// `mins` keeps the exact per-snapshot minimums (append order at ingest;
/// backfilled cells carry them in unspecified order — every consumer reads
/// them with order-insensitive statistics), so the typical (median) stays
/// recomputable — and re-bucketable into any timezone — forever, long after
/// the raw working set that produced it was pruned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemHourRow {
    /// UTC day number the cell belongs to.
    pub day: i64,
    /// Hour of the UTC day, `0..=23`.
    pub hour: i64,
    /// Cheapest of the hour's per-snapshot minimum unit prices (copper) —
    /// the outright deal floor seen that hour.
    pub floor: i64,
    /// The hour's per-snapshot minimum unit prices (copper), one entry per
    /// snapshot that carried the identity in this hour.
    pub mins: Vec<i64>,
}

/// One identity's lowest ask on one UTC day, read off the permanent intraday
/// fold: the minimum of the day's per-snapshot floors. Narrow long-horizon
/// fuel (no day-fold JSON to parse) for the deal ranking's 30-day-low
/// indicator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemDayLow {
    /// The item or pet the low describes.
    pub item: ItemRef,
    /// UTC day number.
    pub day: i64,
    /// Cheapest unit price seen that day (copper).
    pub min: i64,
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
    /// Fraction of credit-eligible listings credited as sold over the
    /// trailing 14 days (eligible = last seen in a band that can clear the
    /// poll cadence, or actually credited); `None` when no sightings exist.
    pub sale_rate: Option<f64>,
    /// Same estimator over the trailing 180 days — the slow, stable demand
    /// signal behind fast-window spikes; `None` when no sightings exist in
    /// the window (the ledger only accrues this much history over time).
    pub sale_rate_180d: Option<f64>,
    /// Whole days since the last credited sale (ledger-derived, window-
    /// independent; counted from the sold sight's open day, so it includes
    /// the listing's time on market); `None` when no sale was ever credited
    /// for the identity.
    pub days_since_last_sale: Option<i64>,
    /// Net listings that closed after the last credited sale day (closes −
    /// reversals); `None` for the same reason as `days_since_last_sale` —
    /// the "stopped selling" churn signal.
    pub closures_since_last_sale: Option<i64>,
    /// Estimated units sold over the span window — credited vanish
    /// quantities plus commodity partial-fill deltas, each booked on the day
    /// the units left the market; `None` when the ledger has no sales rows
    /// for the identity at all ("no evidence" stays distinct from a
    /// genuine zero).
    pub qty_sold: Option<i64>,
}

/// The ledger counts behind a sale rate over a window: credited sales and
/// credit-eligible closes that did not credit. A close books `credited` and
/// `eligible` together on the sight's open day, so `expired = eligible −
/// credited` is exact for any window (reopened sights reverse both). An
/// expired close is a listing last seen with ≥ 2 h left that outlived its
/// band's lower bound — it ran out unsold (the "relisted, not selling" half
/// of the rate); SHORT/MEDIUM-band vanishes cannot be told apart from natural
/// expiry at the poll cadence and belong to neither count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaleCounts {
    /// Listings credited as sold in the window.
    pub sold: i64,
    /// Credit-eligible closes that ran out unsold.
    pub expired: i64,
}

impl SaleCounts {
    /// The counts of one ledger window from its credited/eligible sums;
    /// `None` when nothing credit-eligible closed (no evidence, never a zero —
    /// the `sale_rate` contract).
    #[must_use]
    pub fn from_ledger(credited: i64, eligible: i64) -> Option<Self> {
        if eligible <= 0 {
            return None;
        }
        Some(Self { sold: credited.max(0), expired: (eligible - credited).max(0) })
    }

    /// The sale rate these counts imply: `sold / (sold + expired)`, the same
    /// figure `sale_rate` reads off the ledger.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // ledger counts into a display ratio
    pub fn rate(&self) -> Option<f64> {
        let eligible = self.sold.saturating_add(self.expired);
        (eligible > 0).then(|| self.sold as f64 / eligible as f64)
    }
}

/// Ledger-derived recency for one identity: the last day a sale was credited
/// and how many listings have closed since (the raw material behind a
/// `MetricBlock`'s `days_since_last_sale` / `closures_since_last_sale`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaleRecency {
    /// UTC day number of the last credited sale (see `utc_day`). Credits
    /// book on the sold sight's OPEN day — the same day its sale-rate
    /// denominator was booked — so "days since last sale" includes the
    /// listing's time on market.
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
    /// Units sold over the block's span window; `None` when the identity
    /// has no sales rows.
    pub qty_sold: Option<i64>,
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
        qty_sold: None,
        days_since_last_sale: None,
        closures_since_last_sale: None,
    };

    /// Builds the ledger from a sale recency reading, counting whole days
    /// since the last credited sale as of `today` (day number).
    #[must_use]
    pub fn from_recency(
        sale_rate: Option<f64>,
        sale_rate_slow: Option<f64>,
        qty_sold: Option<i64>,
        recency: Option<SaleRecency>,
        today: i64,
    ) -> Self {
        match recency {
            None => Self { sale_rate, sale_rate_slow, qty_sold, ..Self::NONE },
            Some(recency) => Self {
                sale_rate,
                sale_rate_slow,
                qty_sold,
                days_since_last_sale: Some(today - recency.last_sale_day),
                closures_since_last_sale: Some(recency.closures_since),
            },
        }
    }
}

// ---------------------------------------------------------------- feed

/// Which serialization of a database snapshot a feed publisher emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FeedFormat {
    /// The carrier (`feed.lua`): a Lua chunk the `AuctiMoggerDatabase`
    /// addon loads as one market's file.
    Lua,
    /// The JSON twin (`feed.json`): the same records, decoded.
    Json,
}

/// Publication info for one rendered feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublishedFeed {
    /// When the feed was published (unix seconds).
    pub at: i64,
    /// Serialized size of the published feed (bytes).
    pub size: u64,
}

// ------------------------------------------------------------ database feed (v4)

/// One market's complete database feed: the content of one carrier file and
/// of its JSON twin (contract: `addon/SPEC.md`, section 5). Built by the
/// tracker's export service and rendered through the `FeedRenderer` port.
#[derive(Clone, Debug, PartialEq)]
pub struct DatabaseSnapshot {
    /// The target the snapshot was built for: the market's key.
    pub target: TargetId,
    /// The market's static identity, mirrored from the target's configuration.
    pub market: MarketMeta,
    /// Version of the tracker that built the snapshot (diagnostics only).
    pub tracker_version: String,
    /// The market's freshness stamp (unix seconds): the newest source
    /// `Last-Modified` across payload kinds, else the publish time.
    pub generated_at: i64,
    /// Fetch time of the newest auctions snapshot, when one is stored.
    pub auctions_at: Option<i64>,
    /// Fetch time of the newest commodities snapshot, when one is stored.
    pub commodities_at: Option<i64>,
    /// Every record of the market, sorted by item identity, then category.
    pub entries: Vec<DatabaseEntry>,
}

impl DatabaseSnapshot {
    /// Contract version written into every carrier and JSON twin; the addon
    /// accepts no other.
    pub const FEED_VERSION: u32 = 4;
}

/// The market's static identity, mirrored from the tracker's target
/// configuration into every carrier header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketMeta {
    /// API region slug (for example `eu`), when the target declares one.
    pub region: Option<String>,
    /// Forever ruleset id, when the target declares one.
    pub ruleset_id: Option<i64>,
    /// Forever auction house id, when the target declares one.
    pub house_id: Option<i64>,
    /// Connected-realm id (retail targets), when the target declares one.
    pub connected_realm_id: Option<i64>,
    /// Catalog flavor: which game version's item ids the market trades.
    pub flavor: Flavor,
}

/// One record of a database snapshot: an identity's statistics in one market
/// category.
#[derive(Clone, Debug, PartialEq)]
pub struct DatabaseEntry {
    /// The item, or the pet species, the record describes, at canonical grain
    /// (a pet's level, quality and breed are zeroed).
    pub item: ItemRef,
    /// Which auction house's evidence the record carries: an item trading in
    /// both exports one record per category.
    pub category: Category,
    /// The statistics themselves, as one packed record carries them.
    pub record: ItemRecord,
}

/// Every statistic of one identity in one market category: the content of one
/// packed record. An optional group is absent, never zero, when the tracker
/// has no evidence for it, and the decoder returns it absent again.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemRecord {
    /// Metric block per span. A record always carries at least one span.
    pub spans: BTreeMap<Span, MetricBlock>,
    /// What the newest snapshot lists, what was seen today and is gone, and
    /// how long ago the item was last listed.
    pub live: Option<LiveQuote>,
    /// The item's value: what buyers paid once enough of them did, else what
    /// sellers ask.
    pub value: Option<ValueQuote>,
    /// Listings credited sold over the trailing 14 days; absent without
    /// credit-eligible evidence, which stays distinct from a genuine zero.
    pub sold_count: Option<i64>,
    /// Credit-eligible closes over the same window that ran out unsold.
    pub expired_count: Option<i64>,
    /// Days the listed stock takes to clear at the window's sale pace,
    /// saturating at 65535; absent without sold evidence, or when nothing
    /// sold (such stock never clears).
    pub supply_days: Option<u16>,
    /// Lowest ask of the previous 30 days (copper).
    pub low30: Option<i64>,
    /// Share of those 30 days whose lowest ask sat above the live floor, in
    /// basis points; needs a live floor and at least five days of history.
    pub low_rank_30d_bp: Option<u16>,
    /// The previous 30 days' lowest asks, oldest first, each stored as
    /// `clamp(round(low * 128 / reference), 0, 255)`: a sparkline, not prices.
    pub lows: Option<Vec<u8>>,
    /// Vendor sell price (copper) when the catalog knows one.
    pub vendor_price: Option<i64>,
    /// The best-ranked recipe that makes this item.
    pub craft: Option<CraftQuote>,
    /// Where the live listing ranks among its category's deals; ranked live
    /// deals only.
    pub deal: Option<DealMark>,
}

/// The `live` group: what the newest snapshot lists, what was seen today
/// and is no longer listed, and how long ago the item was last listed. The
/// group exists when any of the three is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveQuote {
    /// The item's listings in the newest snapshot; `None` when nothing of the
    /// item is listed there.
    pub listed: Option<LiveListing>,
    /// Today's cheapest listing that is no longer listed (bought or
    /// cancelled); `None` when everything seen today is still listed.
    pub seen: Option<SeenListing>,
    /// Whole UTC days since the item was last seen listed; 0 when the newest
    /// snapshot lists it or the hour fold lists it today. Absent without
    /// hour-fold evidence.
    pub days_since_listed: Option<u16>,
}

/// An item's listings in the newest snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveListing {
    /// Cheapest unit price listed right now (copper).
    pub floor: i64,
    /// Quantity listed at exactly that price.
    pub floor_qty: i64,
    /// Listings of the item in the snapshot.
    pub listed: i64,
    /// Total quantity listed.
    pub qty_sum: Option<i64>,
}

/// A listing seen earlier today that the newest snapshot no longer lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeenListing {
    /// The price it asked (copper).
    pub floor: i64,
    /// The quantity it offered.
    pub qty: i64,
}

/// The `value` group: what the item is worth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueQuote {
    /// Quantity-weighted 7-day median listing price (copper); always above
    /// zero, since without one there is no value group.
    pub value7: i64,
    /// Unique listings behind `value7`.
    pub samples7: i64,
    /// 14-day quantity-weighted median of sold prices (copper); present only
    /// once enough priced units sold.
    pub sold_median14: Option<i64>,
    /// Today's listing median against the previous six days, in basis points
    /// (signed).
    pub trend_bp: Option<i64>,
}

/// The `craft` group: what one make of the item costs and earns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CraftQuote {
    /// Reagent cost per make (copper).
    pub cost: i64,
    /// Output value minus the house cut and the cost, per make (copper); a
    /// loss is negative.
    pub profit: i64,
    /// Profit times the sale rate, rounded (copper); a loss is negative.
    pub expected_profit: i64,
}

/// The `deal` group: where a live listing ranks among its category's deals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DealMark {
    /// 1-based rank by score.
    pub rank: u32,
    /// The ranking score, in basis points.
    pub score_bp: u32,
    /// `1 - floor / reference`, in basis points.
    pub discount_bp: u32,
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
    /// Item level, when the API reports one (equipment only — non-equipment
    /// items never carry a level and read as `None` forever).
    pub item_level: Option<i64>,
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
            && self.item_level.is_none()
    }

    /// Fills this record's absent enrichment fields from `other` (the source
    /// chain's merge step: the first leg that answers keeps its name, later
    /// legs top up the fields it could not see — e.g. Wowhead carries the
    /// icon, Blizzard the class and level). `self`'s id and name always win.
    #[must_use]
    pub fn merged_with(mut self, other: NameRecord) -> NameRecord {
        if self.item_class.is_none() {
            self.item_class = other.item_class;
        }
        if self.item_subclass.is_none() {
            self.item_subclass = other.item_subclass;
        }
        if self.inventory_slot.is_none() {
            self.inventory_slot = other.inventory_slot;
        }
        if self.quality.is_none() {
            self.quality = other.quality;
        }
        if self.icon.is_none() {
            self.icon = other.icon;
        }
        if self.sell_price.is_none() {
            self.sell_price = other.sell_price;
        }
        if self.item_level.is_none() {
            self.item_level = other.item_level;
        }
        self
    }

    /// Whether every enrichment field is present — the source chain's
    /// stop-early signal: no later leg could contribute anything, so asking
    /// one would only burn a request.
    #[must_use]
    pub fn is_enrichment_complete(&self) -> bool {
        self.item_class.is_some()
            && self.item_subclass.is_some()
            && self.inventory_slot.is_some()
            && self.quality.is_some()
            && self.icon.is_some()
            && self.sell_price.is_some()
            && self.item_level.is_some()
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
    /// The newest snapshot's cheapest listing of the item is at `copper` or
    /// below — a literal price target on the live floor (what a buyer can
    /// buy right now), independent of how the rest of the day traded.
    FloorBelow {
        /// Threshold in copper per unit.
        copper: i64,
    },
    /// The newest snapshot's cheapest listing sits at `fraction` of the
    /// item's 7-day quantity-weighted value or below — a listing priced as a
    /// deal relative to where the market normally trades.
    FloorBelowValue {
        /// Threshold fraction of the value (0.0..=1.0).
        fraction: f64,
    },
    /// At least one listing first sighted in the newest snapshot sits at
    /// `fraction` of the item's 7-day quantity-weighted value or below — the
    /// sniper operand: a freshly posted underpriced listing. Listings first
    /// seen at or before the rule's last firing never count again, so a
    /// listing that stays up cannot re-fire the rule.
    NewListingBelow {
        /// Threshold fraction of the value (0.0..=1.0).
        fraction: f64,
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

/// A fraction stored as the wire's integer: `round(fraction * 10000)`, half
/// away from zero. Not a number, and anything at or below zero, read as zero;
/// a value beyond a `u32` saturates at `u32::MAX`. The one definition behind
/// every basis-point figure the feed carries — a span block's sale rate no
/// less than a deal's score — so two of them can never round differently.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn basis_points(fraction: f64) -> u32 {
    // The float-to-integer `as` cast saturates: not a number and anything at
    // or below zero read as zero, anything beyond `u32` as `u32::MAX`.
    (fraction * 10_000.0).round() as u32
}

/// Rounds `value` into a `u16`, saturating at both ends. Not a number, and
/// anything at or below zero, read as zero.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn saturate_u16(value: f64) -> u16 {
    // Saturating for the same reason as `basis_points`.
    value.round() as u16
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

    #[test]
    fn sale_counts_split_the_eligible_denominator_and_rate_like_the_ledger() {
        let counts = SaleCounts { sold: 3, expired: 9 };
        assert_eq!(SaleCounts::from_ledger(3, 12), Some(counts), "twelve eligible closes");
        assert_eq!(counts.rate(), Some(0.25), "3 / (3 + 9) is the sale rate");
        assert_eq!(
            SaleCounts::from_ledger(0, 5),
            Some(SaleCounts { sold: 0, expired: 5 }),
            "rows with no sale read as a genuine zero rate"
        );
        assert_eq!(
            SaleCounts::from_ledger(0, 5).and_then(|counts| counts.rate()),
            Some(0.0),
            "a genuine zero, not missing evidence"
        );
        assert_eq!(SaleCounts::from_ledger(0, 0), None, "no eligible close is no evidence");
        assert_eq!(SaleCounts::from_ledger(2, -1), None, "a corrupt negative sum is no evidence");
    }

    #[test]
    fn basis_points_rounds_half_away_from_zero_and_saturates() {
        let cases = [
            (0.25, 2_500),
            (0.8889, 8_889),
            (0.3617, 3_617),
            (1.0, 10_000),
            (0.000_050_001, 1),
            (0.000_049_999, 0),
            (0.3333, 3_333),
            (2.5, 25_000),
            (0.0, 0),
            (-0.5, 0),
            (f64::NAN, 0),
            (f64::INFINITY, u32::MAX),
            (f64::MAX, u32::MAX),
        ];
        for (fraction, expected) in cases {
            assert_eq!(basis_points(fraction), expected, "bp({fraction})");
        }
    }

    #[test]
    fn saturate_u16_clamps_at_both_ends() {
        let cases = [
            (39.44, 39),
            (0.4, 0),
            (65_535.4, u16::MAX),
            (70_000.0, u16::MAX),
            (0.0, 0),
            (-1.0, 0),
            (f64::NAN, 0),
            (f64::INFINITY, u16::MAX),
        ];
        for (value, expected) in cases {
            assert_eq!(saturate_u16(value), expected, "u16({value})");
        }
    }
}

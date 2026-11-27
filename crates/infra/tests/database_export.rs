//! Database feed export over a real store: the export reads identities off
//! the day folds, their span blocks through the cached bulk paths, every other
//! group off the ranking's prepared scans, the crafting view and the newest
//! snapshot — and what it builds renders losslessly into the carrier and its
//! JSON twin.

// Test assertions may unwrap: fixture-driven, deterministic inputs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        atomic::{
            AtomicU64,
            Ordering,
        },
        Arc,
    },
};

use amtracker_core::{
    config::{
        AlertsConfig,
        CatalogConfig,
        CraftsConfig,
        MetricsConfig,
        OpportunitiesConfig,
        PollConfig,
        ScoreWeights,
        ServerConfig,
        StorageConfig,
        TargetConfig,
        TargetKind,
        TrackerConfig,
        VacuumPolicy,
    },
    domain::{
        basis_points,
        utc_day,
        Category,
        CraftQuote,
        DatabaseEntry,
        DatabaseSnapshot,
        FeedFormat,
        Flavor,
        ItemRef,
        ListingKind,
        LiveListing,
        LiveQuote,
        NameRecord,
        NewSnapshot,
        NormalizedListing,
        PayloadKind,
        Reagent,
        Recipe,
        SeenListing,
        SnapshotStatus,
        Span,
        TargetId,
        TimeLeftBand,
        ValueQuote,
    },
    fold::DayFold,
    ports::{
        ClockPort,
        FeedRenderer,
        ItemCatalogRepository,
        PriceHistoryRepository,
        SalesRepository,
        SnapshotRepository,
    },
    services::{
        craft::CraftService,
        database_export::{
            DatabaseExportService,
            ExportOptions,
        },
        metrics::MetricsService,
        opportunity::{
            OpportunityService,
            OpportunitySort,
        },
    },
};
use amtracker_infra::{
    feed::FeedCodec,
    record,
    store::{
        AutoVacuum,
        SqliteStore,
    },
};

/// A fixed "now": 08:53 UTC, so every ledger booking below lands on today.
const NOW: i64 = 1_760_000_000;
/// The version the export under test writes into its headers.
const VERSION: &str = "0.1.0";

struct FixedClock(i64);

impl ClockPort for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "amtracker-dbexport-{}-{}-{}",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    amtracker_infra::testdir::fresh(&dir).expect("create temp dir")
}

fn listing(auction_id: i64, item: &ItemRef, price: i64, qty: i64) -> NormalizedListing {
    NormalizedListing {
        auction_id,
        item_ref: item.clone(),
        kind: ListingKind::Commodity,
        quantity: qty,
        unit_price: price,
        bid: None,
        time_left: TimeLeftBand::VeryLong,
        context: None,
        bonus_lists: Vec::new(),
    }
}

/// An item-auction-house listing (pets and plain items trade there).
fn auction(auction_id: i64, item: &ItemRef, price: i64, qty: i64) -> NormalizedListing {
    NormalizedListing { kind: ListingKind::Item, ..listing(auction_id, item, price, qty) }
}

fn item(id: i64) -> ItemRef {
    ItemRef::Item { item_id: id }
}

fn recipe(output: i64, reagents: &[(i64, i64)]) -> Recipe {
    Recipe {
        item: item(output),
        makes: 1,
        reagents: reagents.iter().map(|(id, qty)| Reagent { item: item(*id), qty: *qty }).collect(),
    }
}

fn config(target: TargetConfig, recipes: Vec<Recipe>) -> TrackerConfig {
    TrackerConfig {
        server: ServerConfig {
            bind: "127.0.0.1:0".into(),
            token_env: None,
            allow_open: true,
            feed_publish_secs: 300,
        },
        storage: StorageConfig {
            database: "mem.sqlite3".into(),
            raw_listing_days: 7,
            sight_retention_days: 0,
            item_hour_retention_days: 45,
            archive_dir: "raw".into(),
            feed_dir: "feeds".into(),
            backup_dir: None,
            backup_secs: 0,
            backup_keep: 7,
            vacuum: VacuumPolicy::Off,
        },
        metrics: MetricsConfig { spans: Span::ALL.to_vec() },
        opportunities: OpportunitiesConfig {
            min_samples: 3,
            weights: ScoreWeights { discount: 0.4, sale: 0.3, churn: 0.2, stability: 0.1 },
        },
        alerts: AlertsConfig {
            enabled: false,
            cooldown_secs: 21_600,
            delivery: "log".into(),
            webhook_env: None,
            token_env: None,
            chat_id_env: None,
            max_data_age_secs: 0,
        },
        crafts: CraftsConfig { recipes_file: None, recipes, ..CraftsConfig::default() },
        catalog: CatalogConfig::default(),
        targets: vec![target],
    }
}

/// One market over a real store, with the seeding the export reads.
struct Market {
    dir: PathBuf,
    store: Arc<SqliteStore>,
    clock: Arc<dyn ClockPort>,
    target: TargetConfig,
}

impl Market {
    fn open(tag: &str, kind: TargetKind, flavor: &str) -> Self {
        let dir = temp_dir(tag);
        let clock: Arc<dyn ClockPort> = Arc::new(FixedClock(NOW));
        let store = Arc::new(
            SqliteStore::open(
                &dir.join("db.sqlite3"),
                &dir.join("raw"),
                clock.clone(),
                AutoVacuum::Off,
            )
            .expect("open store"),
        );
        Self {
            dir,
            store,
            clock,
            target: TargetConfig {
                id: TargetId("market".into()),
                kind,
                auth: None,
                poll: PollConfig { commodities_secs: 3600, auctions_secs: 1800 },
                flavor: Flavor(flavor.into()),
            },
        }
    }

    fn today(&self) -> i64 {
        utc_day(self.clock.now())
    }

    /// One day of `count` single-unit listings of `item` at `price`, folded
    /// under one category; the listings take the house the category trades in.
    fn fold(&self, item: &ItemRef, day: i64, category: Category, price: i64, count: i64) {
        let prices = vec![price; usize::try_from(count).unwrap_or(usize::MAX)];
        self.fold_prices(item, day, category, &prices);
    }

    /// One day of item-auction listings of `item` (a plain item or a pet
    /// variant), one per price.
    fn fold_auctions(&self, item: &ItemRef, day: i64, prices: &[i64]) {
        self.fold_prices(item, day, Category::derive(ListingKind::Item, item), prices);
    }

    fn fold_prices(&self, item: &ItemRef, day: i64, category: Category, prices: &[i64]) {
        let make = |n: usize, price: i64| {
            let id = day * 1_000 + i64::try_from(n).unwrap_or(i64::MAX);
            match category.payload_kind() {
                PayloadKind::Auctions => auction(id, item, price, 1),
                PayloadKind::Commodities => listing(id, item, price, 1),
            }
        };
        let listings: Vec<NormalizedListing> =
            prices.iter().enumerate().map(|(n, price)| make(n, *price)).collect();
        let mut fold = DayFold::new(item.clone());
        fold.apply(&listings);
        self.store.save_fold(&self.target.id, item, day, category, &fold).expect("save fold");
    }

    /// Records one snapshot fetched at `at` (recorded in call order, so the
    /// last one recorded is the newest).
    fn snapshot(
        &self,
        kind: PayloadKind,
        at: i64,
        last_modified: Option<i64>,
        listings: &[NormalizedListing],
    ) {
        self.store
            .record(
                &NewSnapshot {
                    target: self.target.id.clone(),
                    kind,
                    fetched_at: at,
                    source_last_modified: last_modified,
                    entry_count: u64::try_from(listings.len()).unwrap_or(u64::MAX),
                    dropped_count: 0,
                    dropped_reasons: BTreeMap::new(),
                    status: SnapshotStatus::Ok,
                },
                listings,
                b"[]",
            )
            .expect("record snapshot");
    }

    /// A listing sighted in an earlier snapshot and gone `later` seconds
    /// after: a credited sale when it vanishes inside its band's lower bound,
    /// an expiry when it outlives it.
    fn close_after(&self, seen: &NormalizedListing, later: i64) {
        let seen_at = NOW - 10_000;
        let kind = Category::derive(seen.kind, &seen.item_ref).payload_kind();
        self.store
            .observe(&self.target.id, kind, std::slice::from_ref(seen), -1, seen_at)
            .expect("observe");
        self.store.close_missing(&self.target.id, kind, 0, seen_at + later).expect("close");
    }

    /// A stack of `qty` that sold a minute after it was sighted at `price`.
    fn sold(&self, item: &ItemRef, price: i64, qty: i64) {
        let ItemRef::Item { item_id } = item else {
            panic!("the ledger fixtures key their auction ids by item id");
        };
        let mut seen =
            listing(900_000 + item_id * 10_000 + (price / 10) % 10_000, item, price, qty);
        seen.time_left = TimeLeftBand::Medium;
        self.close_after(&seen, 60);
    }

    /// A listing that outlived its LONG band and ran out unsold.
    fn expired(&self, item: &ItemRef, price: i64, qty: i64) {
        let ItemRef::Item { item_id } = item else {
            panic!("the ledger fixtures key their auction ids by item id");
        };
        let mut seen =
            listing(950_000 + item_id * 10_000 + (price / 10) % 10_000, item, price, qty);
        seen.time_left = TimeLeftBand::Long;
        self.close_after(&seen, 3 * 3_600);
    }
}

/// Builds the snapshot exactly as the scheduler does: every sibling service
/// over the same store and clock, the spans from the config.
fn build(market: &Market, recipes: Vec<Recipe>) -> (DatabaseSnapshot, Arc<OpportunityService>) {
    build_with_spans(market, recipes, Span::ALL.to_vec())
}

/// [`build`] with caller-chosen spans.
fn build_with_spans(
    market: &Market,
    recipes: Vec<Recipe>,
    spans: Vec<Span>,
) -> (DatabaseSnapshot, Arc<OpportunityService>) {
    let config = config(market.target.clone(), recipes);
    let store = Arc::clone(&market.store);
    let clock = market.clock.clone();
    let metrics = Arc::new(MetricsService::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::clone(&clock),
    ));
    let opportunities = Arc::new(OpportunityService::new(
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::clone(&clock),
        &config,
    ));
    let crafts = Arc::new(CraftService::new(
        Arc::clone(&opportunities),
        Arc::clone(&metrics),
        &config.crafts,
    ));
    let export = DatabaseExportService::new(
        store.clone(),
        store,
        metrics,
        opportunities.clone(),
        crafts,
        clock,
        VERSION,
    );
    let snapshot =
        export.build(&market.target, &ExportOptions { spans }).expect("build the snapshot");
    (snapshot, opportunities)
}

fn entry<'a>(
    snapshot: &'a DatabaseSnapshot,
    item: &ItemRef,
    category: Category,
) -> &'a DatabaseEntry {
    snapshot
        .entries
        .iter()
        .find(|entry| &entry.item == item && entry.category == category)
        .unwrap_or_else(|| panic!("no {category:?} record for {item:?}"))
}

/// Decodes every record of a carrier, as the addon's loader would: the
/// table it was filed under, its key, and the record.
fn decode_carrier(lua: &[u8]) -> Vec<(&'static str, i64, amtracker_core::domain::ItemRecord)> {
    let text = std::str::from_utf8(lua).expect("the carrier is ASCII");
    text.lines()
        .filter_map(|line| line.strip_prefix("m."))
        .filter_map(|line| {
            let (key, value) = line.split_once("] = \"")?;
            let value = value.strip_suffix('"')?;
            let (table, id) = key.split_once('[')?;
            let table = match table {
                "items" => "items",
                "commodities" => "commodities",
                "pets" => "pets",
                _ => panic!("unknown record table `{table}`"),
            };
            Some((
                table,
                id.parse().expect("integer key"),
                record::decode(value).expect("the record decodes"),
            ))
        })
        .collect()
}

/// The carrier table an entry is filed under.
fn table_of(category: Category) -> &'static str {
    match category {
        Category::Item => "items",
        Category::Commodity => "commodities",
        Category::Pet => "pets",
    }
}

/// The key an entry is filed under: the item id, or the species id.
fn key_of(item: &ItemRef) -> i64 {
    match item {
        ItemRef::Item { item_id } => *item_id,
        ItemRef::Pet { species, .. } => *species,
    }
}

/// A commodity market with every shape the export attaches groups for.
fn commodity_market() -> Market {
    let market = Market::open(
        "commodity",
        TargetKind::BlizzardConnectedRealm { region: "us".into(), connected_realm_id: 4 },
        "retail",
    );
    let today = market.today();
    // A week of single-unit listings: item 1 at 100 000 rising to 110 000
    // today, item 2 flat at 200 000, item 5 in BOTH houses (1 000 as a
    // commodity, 3 000 as an item); two reagents priced today only.
    for offset in 1..=6 {
        market.fold(&item(1), today - offset, Category::Commodity, 100_000, 10);
        market.fold(&item(2), today - offset, Category::Commodity, 200_000, 10);
        market.fold(&item(5), today - offset, Category::Commodity, 1_000, 10);
        market.fold(&item(5), today - offset, Category::Item, 3_000, 10);
    }
    market.fold(&item(1), today, Category::Commodity, 110_000, 10);
    market.fold(&item(2), today, Category::Commodity, 200_000, 10);
    market.fold(&item(5), today, Category::Commodity, 1_000, 10);
    market.fold(&item(5), today, Category::Item, 3_000, 10);
    market.fold(&item(6), today, Category::Commodity, 10_000, 4);
    market.fold(&item(7), today, Category::Commodity, 100, 4);
    // Item 3 was seen today at 50 000 and is gone; item 4 is listed level
    // with its sighting.
    market.fold(&item(3), today, Category::Commodity, 50_000, 3);
    market.fold(&item(4), today, Category::Commodity, 40_000, 2);

    // Five prior days of hour cells at 70 000 for item 1 — recorded FIRST, so
    // the live snapshot below stays the newest by id.
    for days_ago in 1..=5 {
        let listings: Vec<NormalizedListing> =
            (0..3).map(|n| listing(80_000 + days_ago * 10 + n, &item(1), 70_000, 1)).collect();
        market.snapshot(PayloadKind::Commodities, NOW - days_ago * 86_400 + 3_600, None, &listings);
    }
    // The live snapshot: item 1 cheap (five stacks of two at 60 000, five
    // singles at 90 000), item 2 a little under its value, item 4 still up.
    let mut live = Vec::new();
    for n in 0..5 {
        live.push(listing(100 + n, &item(1), 60_000, 2));
        live.push(listing(110 + n, &item(1), 90_000, 1));
        live.push(listing(200 + n, &item(2), 180_000, 1));
    }
    live.extend([listing(300, &item(4), 40_000, 4), listing(301, &item(4), 40_000, 4)]);
    market.snapshot(PayloadKind::Commodities, NOW, Some(NOW - 60), &live);

    // The ledger: item 1 sold a stack of 12 at 80 000 a minute after it was
    // sighted (credited) and had one listing expire unsold; item 5 sold three.
    market.sold(&item(1), 80_000, 12);
    market.expired(&item(1), 90_000, 5);
    market.sold(&item(5), 1_200, 3);

    // A vendor pays 12 500 for item 1.
    market
        .store
        .upsert(
            &Flavor::default(),
            &[NameRecord {
                item_id: 1,
                name: "Linen Cloth".into(),
                item_class: None,
                item_subclass: None,
                inventory_slot: None,
                quality: None,
                icon: None,
                sell_price: Some(12_500),
                item_level: None,
                fetched_at: 0,
            }],
        )
        .expect("catalog");
    market
}

/// The whole export end to end: every group of a listed, selling, discounted
/// item comes off the store and lands in the record — and the ranking's own
/// numbers are what the record carries.
#[test]
#[allow(clippy::too_many_lines)]
fn a_listed_item_carries_its_live_value_evidence_position_and_rank() {
    let market = commodity_market();
    let (snapshot, opportunities) = build(&market, Vec::new());

    // The header mirrors the target's configuration and the newest snapshot.
    assert_eq!(snapshot.target, market.target.id);
    assert_eq!(snapshot.tracker_version, VERSION);
    assert_eq!(snapshot.generated_at, NOW - 60, "the source's Last-Modified");
    assert_eq!(snapshot.commodities_at, Some(NOW));
    assert_eq!(snapshot.auctions_at, None, "no item-auction snapshot is stored");
    assert_eq!(snapshot.market.region.as_deref(), Some("us"));
    assert_eq!(snapshot.market.connected_realm_id, Some(4));
    assert_eq!(snapshot.market.house_id, None);
    assert_eq!(snapshot.market.flavor, Flavor::default());

    let one = entry(&snapshot, &item(1), Category::Commodity);
    assert_eq!(one.record.spans.len(), 4, "a week of folds fills every span");
    assert_eq!(
        one.record.live,
        Some(LiveQuote {
            listed: Some(LiveListing {
                floor: 60_000,
                floor_qty: 10,
                listed: 10,
                qty_sum: Some(15),
                // Half the listings sit at the floor: the 5th percentile is the floor.
                p05: Some(60_000),
            }),
            // Today's fold floor of 110 000 sits above the live floor: the
            // cheap stacks are still up, nothing is "gone".
            seen: None,
            // The newest snapshot lists it: last listed today.
            days_since_listed: Some(0),
        })
    );
    assert_eq!(
        one.record.value,
        Some(ValueQuote {
            value7: 100_000,
            samples7: 70,
            sold_median14: Some(80_000),
            // 110 000 today against 100 000 before it: +10 %.
            trend_bp: Some(1_000),
        })
    );
    assert_eq!((one.record.sold_count, one.record.expired_count), (Some(1), Some(1)));
    // 15 listed ÷ (12 sold / 14 days) = 17.5 days of supply.
    assert_eq!(one.record.supply_days, Some(18));
    // Every one of the five prior days sat above the 60 000 floor.
    assert_eq!(one.record.low30, Some(70_000));
    assert_eq!(one.record.low_rank_30d_bp, Some(10_000));
    // Each 70 000 low × 128 ÷ the sold reference of 80 000.
    assert_eq!(one.record.lows.as_deref(), Some(&[112_u8; 5][..]));
    assert_eq!(one.record.vendor_price, Some(12_500));
    assert!(one.record.craft.is_none(), "no recipe book");

    // The rank and its figures are the ranking's own.
    let ranked = opportunities
        .opportunities(&market.target.id, Category::Commodity, 0.0, 10, OpportunitySort::Score)
        .expect("ranked");
    assert_eq!(ranked.len(), 2, "items 1 and 2 are listed with enough evidence");
    assert_eq!(ranked[0].item, item(1));
    assert_eq!(ranked[1].item, item(2));
    assert_eq!(one.record.deal.unwrap().rank, 1);
    assert_eq!(one.record.deal.unwrap().discount_bp, 2_500, "1 - 60 000 / 80 000");
    assert_eq!(
        i64::from(one.record.deal.unwrap().score_bp),
        i64::from(basis_points(ranked[0].score))
    );

    let two = entry(&snapshot, &item(2), Category::Commodity);
    assert_eq!(
        two.record.live,
        Some(LiveQuote {
            listed: Some(LiveListing {
                floor: 180_000,
                floor_qty: 5,
                listed: 5,
                qty_sum: Some(5),
                p05: Some(180_000),
            }),
            seen: None,
            days_since_listed: Some(0),
        })
    );
    assert_eq!(
        two.record.value,
        Some(ValueQuote { value7: 200_000, samples7: 70, sold_median14: None, trend_bp: Some(0) }),
        "a flat week is a present zero, not an absent trend"
    );
    assert_eq!((two.record.sold_count, two.record.supply_days), (None, None), "no ledger evidence");
    assert_eq!((two.record.low30, two.record.lows.is_none()), (None, true), "no hour-fold history");
    assert_eq!(two.record.deal.unwrap().rank, 2);
    assert_eq!(two.record.deal.unwrap().discount_bp, 1_000, "1 - 180 000 / 200 000");

    std::fs::remove_dir_all(&market.dir).ok();
}

/// Seen today and gone, listed level with its sighting, thin evidence, and an
/// item trading in both houses: the live group tells the four apart, and each
/// house's record carries only its own evidence.
#[test]
fn seen_today_gone_dual_category_and_thin_evidence_records() {
    let market = commodity_market();
    let (snapshot, _opportunities) = build(&market, Vec::new());

    // Item 3 folded today at 50 000 and is not in the newest snapshot. Its
    // fold never went through a snapshot, so the hour fold holds no row for
    // it: seen today, gone — and nothing to date the last listing.
    let gone = entry(&snapshot, &item(3), Category::Commodity);
    assert_eq!(
        gone.record.live,
        Some(LiveQuote {
            listed: None,
            seen: Some(SeenListing { floor: 50_000, qty: 3 }),
            days_since_listed: None,
        })
    );
    assert_eq!(
        gone.record.value,
        Some(ValueQuote { value7: 50_000, samples7: 3, sold_median14: None, trend_bp: None })
    );
    assert!(gone.record.deal.is_none(), "nothing to buy");

    // Item 4's sighting is level with its live floor: that listing, still up.
    let level = entry(&snapshot, &item(4), Category::Commodity);
    assert_eq!(
        level.record.live,
        Some(LiveQuote {
            listed: Some(LiveListing {
                floor: 40_000,
                floor_qty: 8,
                listed: 2,
                qty_sum: Some(8),
                p05: Some(40_000),
            }),
            seen: None,
            days_since_listed: Some(0),
        })
    );
    assert!(level.record.deal.is_none(), "two listings of evidence sit under the ranking's floor");

    // Item 5 trades in both houses: two records, each on its own evidence.
    // No item-auction snapshot is stored, so that record has no live side at
    // all, while the commodity record saw it today and finds it gone.
    assert_eq!(
        snapshot.entries.iter().filter(|entry| entry.item == item(5)).count(),
        2,
        "one record per category"
    );
    let as_commodity = entry(&snapshot, &item(5), Category::Commodity);
    assert_eq!(as_commodity.record.value.as_ref().map(|value| value.value7), Some(1_000));
    assert_eq!(
        as_commodity.record.live,
        Some(LiveQuote {
            listed: None,
            seen: Some(SeenListing { floor: 1_000, qty: 10 }),
            days_since_listed: None,
        }),
        "seen today as a commodity, gone"
    );
    let as_item = entry(&snapshot, &item(5), Category::Item);
    assert_eq!(as_item.record.value.as_ref().map(|value| value.value7), Some(3_000));
    assert_eq!(
        as_item.record.live, None,
        "the item house has no snapshot: nothing to compare against"
    );
    assert!(as_item.record.deal.is_none());

    // Entries are sorted by identity, then category (the declaration order:
    // item, commodity, pet).
    let mut sorted = snapshot.entries.clone();
    sorted.sort_by(|a, b| a.item.cmp(&b.item).then_with(|| a.category.cmp(&b.category)));
    assert_eq!(snapshot.entries, sorted);

    std::fs::remove_dir_all(&market.dir).ok();
}

/// `daysSinceListed` dates the last listing off the hour fold: today for what
/// the newest snapshot lists, the age of the fold's newest day for what is
/// neither listed nor seen today — a live group holding nothing else — and
/// absent when the fold holds no row at all. It reaches the twin as well.
#[test]
fn days_since_listed_comes_off_the_newest_snapshot_then_the_hour_fold() {
    let market = Market::open(
        "days-listed",
        TargetKind::BlizzardConnectedRealm { region: "us".into(), connected_realm_id: 4 },
        "retail",
    );
    let today = market.today();
    // Item 1 traded twelve and three days ago, in the snapshots of those days
    // too (so the hour fold lists it on both), and not since.
    for offset in [12, 3] {
        market.fold(&item(1), today - offset, Category::Commodity, 100_000, 5);
        market.snapshot(
            PayloadKind::Commodities,
            NOW - offset * 86_400 + 3_600,
            None,
            &[listing(100 + offset, &item(1), 100_000, 1)],
        );
    }
    // Item 2 was listed two days ago and is listed now: the newest snapshot
    // wins, so it was last listed today.
    market.fold(&item(2), today - 2, Category::Commodity, 50_000, 5);
    market.snapshot(
        PayloadKind::Commodities,
        NOW - 2 * 86_400 + 3_600,
        None,
        &[listing(200, &item(2), 50_000, 1)],
    );
    market.fold(&item(2), today, Category::Commodity, 50_000, 5);
    // The newest snapshot lists item 2 only.
    market.snapshot(
        PayloadKind::Commodities,
        NOW,
        Some(NOW - 60),
        &[listing(201, &item(2), 50_000, 1)],
    );
    // Item 3 folded a week ago and today, but was never in a snapshot: seen
    // today and gone, with no hour-fold row to date.
    market.fold(&item(3), today - 7, Category::Commodity, 70_000, 5);
    market.fold(&item(3), today, Category::Commodity, 70_000, 5);

    let (snapshot, _opportunities) = build(&market, Vec::new());

    let stale = entry(&snapshot, &item(1), Category::Commodity);
    assert_eq!(
        stale.record.live,
        Some(LiveQuote { listed: None, seen: None, days_since_listed: Some(3) }),
        "listed three days ago and not since: the live group holds nothing else"
    );
    // The fold's two days are the sparkline too, oldest first.
    assert_eq!(stale.record.lows.as_deref(), Some(&[128_u8, 128][..]));

    let listed = entry(&snapshot, &item(2), Category::Commodity);
    assert_eq!(
        listed.record.live,
        Some(LiveQuote {
            listed: Some(LiveListing {
                floor: 50_000,
                floor_qty: 1,
                listed: 1,
                qty_sum: Some(1),
                p05: Some(50_000),
            }),
            seen: None,
            days_since_listed: Some(0),
        }),
        "the newest snapshot lists it: today, whatever the fold remembers"
    );

    let undated = entry(&snapshot, &item(3), Category::Commodity);
    assert_eq!(
        undated.record.live,
        Some(LiveQuote {
            listed: None,
            seen: Some(SeenListing { floor: 70_000, qty: 5 }),
            days_since_listed: None,
        }),
        "seen today and gone, but no hour-fold row to date it"
    );

    let json = FeedCodec.render(&snapshot, FeedFormat::Json).expect("twin");
    let twin: serde_json::Value = serde_json::from_slice(&json).expect("valid json");
    let live_of = |id: i64| {
        twin["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|entry| entry["itemID"].as_i64() == Some(id) && entry["category"] == "commodity")
            .unwrap_or_else(|| panic!("no commodity record for item {id}"))["live"]
            .clone()
    };
    assert_eq!(live_of(1), serde_json::json!({ "daysSinceListed": 3 }));
    assert_eq!(live_of(2)["daysSinceListed"], 0);
    assert!(live_of(3).get("daysSinceListed").is_none(), "{}", live_of(3));

    std::fs::remove_dir_all(&market.dir).ok();
}

/// The live `p05` is the newest snapshot's own 5th percentile, nearest rank
/// over its listings: it reaches the record's live group and the JSON twin
/// exactly as stored — above the floor when the listings spread, at the floor
/// when they do not — while an item seen today and gone carries no listing
/// summary and so no `p05` at all.
#[test]
fn live_p05_comes_off_the_newest_snapshot_into_the_record_and_the_twin() {
    let market = Market::open(
        "live-p05",
        TargetKind::BlizzardConnectedRealm { region: "us".into(), connected_realm_id: 4 },
        "retail",
    );
    let today = market.today();
    // Each item folded today, so each has a d1 block to carry its groups. Item
    // 3 is folded but not listed in the newest snapshot: seen today, gone.
    market.fold(&item(1), today, Category::Commodity, 1010, 21);
    market.fold(&item(2), today, Category::Commodity, 2500, 20);
    market.fold(&item(3), today, Category::Commodity, 500, 3);
    // The newest snapshot lists item 1 by twenty-one single-unit listings at
    // 1000, 1001, …: nearest rank puts the 5th percentile at the second
    // cheapest, one above the floor. Item 2 is twenty stacks at one price: its
    // p05 is that price, the floor itself.
    let spread: Vec<NormalizedListing> =
        (0..21).map(|n| listing(1000 + n, &item(1), 1000 + n, 1)).collect();
    let flat: Vec<NormalizedListing> =
        (0..20).map(|n| listing(2000 + n, &item(2), 2500, 5)).collect();
    market.snapshot(PayloadKind::Commodities, NOW, Some(NOW - 60), &[spread, flat].concat());

    let (snapshot, _opportunities) = build(&market, Vec::new());
    let listed = |id: i64| {
        entry(&snapshot, &item(id), Category::Commodity)
            .record
            .live
            .as_ref()
            .and_then(|live| live.listed.as_ref())
            .map(|listing| (listing.floor, listing.listed, listing.qty_sum, listing.p05))
    };
    assert_eq!(listed(1), Some((1000, 21, Some(21), Some(1001))));
    assert_eq!(listed(2), Some((2500, 20, Some(100), Some(2500))));

    // Seen today and gone: no listing, so no p05.
    assert_eq!(
        entry(&snapshot, &item(3), Category::Commodity).record.live,
        Some(LiveQuote {
            listed: None,
            seen: Some(SeenListing { floor: 500, qty: 3 }),
            days_since_listed: None,
        })
    );

    // The twin says the same, key for key.
    let json = FeedCodec.render(&snapshot, FeedFormat::Json).expect("twin");
    let twin: serde_json::Value = serde_json::from_slice(&json).expect("valid json");
    let live_of = |id: i64| {
        twin["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|entry| entry["itemID"].as_i64() == Some(id) && entry["category"] == "commodity")
            .unwrap_or_else(|| panic!("no commodity record for item {id}"))["live"]
            .clone()
    };
    assert_eq!(
        live_of(1),
        serde_json::json!({ "floor": 1000, "floorQty": 1, "listed": 21, "qtySum": 21, "p05": 1001, "daysSinceListed": 0 })
    );
    assert_eq!(live_of(2)["p05"], 2500);
    assert_eq!(
        live_of(3),
        serde_json::json!({ "seenFloor": 500, "seenQty": 3 }),
        "gone: no floor, no p05"
    );

    std::fs::remove_dir_all(&market.dir).ok();
}

/// The craft group: the ranked recipe with the highest expected profit per
/// output, attached to the output's record in the category it was priced in.
#[test]
fn crafts_attach_their_best_ranked_recipe_to_the_priced_category() {
    let market = commodity_market();
    let (snapshot, _opportunities) =
        build(&market, vec![recipe(1, &[(2, 1)]), recipe(1, &[(6, 1)]), recipe(5, &[(7, 1)])]);

    // Reagent 2 costs more than item 1 nets: that recipe is not ranked, so
    // the record carries the profitable route instead.
    assert_eq!(
        entry(&snapshot, &item(1), Category::Commodity).record.craft,
        Some(CraftQuote {
            cost: 10_000,
            // 80 000 - 4 000 cut - 10 000 reagents.
            profit: 66_000,
            // × the 50 % sale rate.
            expected_profit: 33_000,
        })
    );
    // Item 5 ties on evidence, so it is priced in the commodity house: the
    // craft lands on that record and only on that record.
    let craft = CraftQuote {
        cost: 100,
        // 1 200 - 60 cut - 100 reagents.
        profit: 1_040,
        expected_profit: 1_040,
    };
    assert_eq!(entry(&snapshot, &item(5), Category::Commodity).record.craft, Some(craft));
    assert_eq!(entry(&snapshot, &item(5), Category::Item).record.craft, None);
    // A losing recipe leaves no group behind.
    assert_eq!(entry(&snapshot, &item(2), Category::Commodity).record.craft, None);

    std::fs::remove_dir_all(&market.dir).ok();
}

/// What the export builds is what the addon reads: every carrier record
/// decodes back to its entry, and the JSON twin says the same thing.
#[test]
fn the_export_renders_losslessly_into_the_carrier_and_its_twin() {
    let market = commodity_market();
    let (snapshot, _opportunities) = build(&market, vec![recipe(1, &[(6, 1)])]);

    let lua = FeedCodec.render(&snapshot, FeedFormat::Lua).expect("carrier");
    assert!(lua.is_ascii(), "a carrier is pure ASCII");
    let decoded = decode_carrier(&lua);
    assert_eq!(decoded.len(), snapshot.entries.len(), "one record line per entry");
    for entry in &snapshot.entries {
        let (_, _, record) = decoded
            .iter()
            .find(|(table, id, _)| *table == table_of(entry.category) && *id == key_of(&entry.item))
            .unwrap_or_else(|| {
                panic!("no carrier record for {:?} in {:?}", entry.item, entry.category)
            });
        assert_eq!(record, &entry.record, "the record decodes back to what was built");
    }

    let json = FeedCodec.render(&snapshot, FeedFormat::Json).expect("twin");
    let twin: serde_json::Value = serde_json::from_slice(&json).expect("valid json");
    assert_eq!(twin["version"], 4);
    assert_eq!(twin["trackerVersion"], VERSION);
    assert_eq!(twin["generatedAt"], NOW - 60);
    assert_eq!(twin["market"]["region"], "us");
    let one = twin["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|entry| entry["itemID"] == 1 && entry["category"] == "commodity")
        .expect("item 1's commodity record");
    assert_eq!(one["live"]["floor"], 60_000);
    assert_eq!(one["live"]["qtySum"], 15);
    assert_eq!(one["live"]["p05"], 60_000);
    assert_eq!(one["live"]["daysSinceListed"], 0, "the newest snapshot lists it");
    assert_eq!(one["value7"], 100_000);
    assert_eq!(one["soldMedian14"], 80_000);
    assert_eq!(one["trendBp"], 1_000);
    assert_eq!(one["soldCount"], 1);
    assert_eq!(one["expiredCount"], 1);
    assert_eq!(one["supplyDays"], 18);
    assert_eq!(one["lowRank30d"], 10_000);
    assert_eq!(one["lows"], serde_json::json!([112, 112, 112, 112, 112]));
    assert_eq!(one["vendorPrice"], 12_500);
    assert_eq!(one["craft"]["cost"], 10_000);
    assert_eq!(one["deal"]["rank"], 1);

    std::fs::remove_dir_all(&market.dir).ok();
}

/// Pets export per species. A species pooled from several variants has no
/// single identity to read a floor or a ledger for, so it carries spans only;
/// a single-variant species gets every group — and the carrier keys it by
/// species, never by variant.
#[test]
fn pet_species_export_per_species_with_groups_only_when_single_variant() {
    let market = Market::open(
        "pets",
        TargetKind::BlizzardForever { region: "eu".into(), ruleset_id: 1, house_id: 111 },
        "forever",
    );
    let today = market.today();
    let low = ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 };
    let high = ItemRef::Pet { species: 2546, level: 25, quality: 2, breed: 4 };
    let solo = ItemRef::Pet { species: 42, level: 10, quality: 1, breed: 3 };

    // Species 2546: a level-1 variant with two listings today and one last
    // week, a level-25 variant with three today and one last week.
    market.fold_auctions(&low, today, &[200, 220]);
    market.fold_auctions(&low, today - 6, &[300]);
    market.fold_auctions(&high, today, &[200, 220, 240]);
    market.fold_auctions(&high, today - 6, &[400]);
    // Species 42: a single variant, seen today and listed right now.
    market.fold_auctions(&solo, today, &[300]);

    // The newest item-auction snapshot lists the level-25 variant and species
    // 42 — the pooled species has a live side, but no single identity to
    // attach it under.
    market.snapshot(
        PayloadKind::Auctions,
        NOW,
        Some(NOW - 30),
        &[auction(1, &high, 240, 1), auction(2, &solo, 250, 1), auction(3, &solo, 250, 1)],
    );

    let (snapshot, _opportunities) = build(&market, Vec::new());
    assert_eq!(snapshot.market.house_id, Some(111), "the Forever house id travels into the header");
    assert_eq!(snapshot.market.flavor, Flavor("forever".into()));
    assert_eq!(snapshot.auctions_at, Some(NOW));
    assert_eq!(snapshot.generated_at, NOW - 30);

    let species = ItemRef::Pet { species: 2546, level: 0, quality: 0, breed: 0 };
    let pooled = entry(&snapshot, &species, Category::Pet);
    let d1 = pooled.record.spans.get(&Span::D1).expect("d1");
    assert_eq!(
        (d1.n, d1.min, d1.median),
        (5, 200, 220),
        "both variants pooled into one population"
    );
    assert_eq!(pooled.record.live, None, "spans only: no single identity to read groups for");
    assert!(pooled.record.value.is_none() && pooled.record.deal.is_none());

    let single = entry(
        &snapshot,
        &ItemRef::Pet { species: 42, level: 0, quality: 0, breed: 0 },
        Category::Pet,
    );
    assert_eq!(
        single.record.live,
        Some(LiveQuote {
            listed: Some(LiveListing {
                floor: 250,
                floor_qty: 2,
                listed: 2,
                qty_sum: Some(2),
                p05: Some(250),
            }),
            seen: None,
            days_since_listed: Some(0),
        })
    );
    assert_eq!(single.record.value.as_ref().map(|value| value.value7), Some(300));

    // The carrier keys pets by species id and files nothing under items.
    let lua = FeedCodec.render(&snapshot, FeedFormat::Lua).expect("carrier");
    let text = String::from_utf8(lua.clone()).expect("ascii");
    assert!(text.contains("m.pets[2546] = \""), "{text}");
    assert!(text.contains("m.pets[42] = \""), "{text}");
    assert!(!text.contains("m.items["), "no plain item trades in this market");

    std::fs::remove_dir_all(&market.dir).ok();
}

/// More groups than spans, across two spans: every group gets its own blocks.
/// (Regression: a span-major matrix consumed as group-major cross-
/// contaminated early entries and silently dropped the tail.)
#[test]
fn multi_span_export_attributes_every_span_to_its_own_group() {
    let market = Market::open(
        "multispan",
        TargetKind::BlizzardConnectedRealm { region: "us".into(), connected_realm_id: 4 },
        "retail",
    );
    let today = market.today();
    let low = ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 };
    let high = ItemRef::Pet { species: 2546, level: 25, quality: 2, breed: 4 };
    // Plain items: distinct d1 vs d7 evidence per item.
    market.fold(&item(1), today, Category::Item, 100, 1);
    market.fold(&item(1), today - 6, Category::Item, 300, 1);
    market.fold(&item(2), today, Category::Item, 500, 1);
    market.fold(&item(2), today - 6, Category::Item, 700, 1);
    market.fold(&item(3), today, Category::Item, 900, 1);
    // Species 2546 across both days: d1 n=5 median 220, d7 n=7 median 220.
    market.fold_auctions(&low, today, &[200, 220]);
    market.fold_auctions(&low, today - 6, &[300]);
    market.fold_auctions(&high, today, &[200, 220, 240]);
    market.fold_auctions(&high, today - 6, &[400]);
    // Item 9 is listed by the history but its only fold is empty: it has no
    // span to carry and must not be exported.
    let hollow = item(9);
    market
        .store
        .save_fold(&market.target.id, &hollow, today, Category::Item, &DayFold::new(hollow.clone()))
        .expect("save the empty fold");

    let (snapshot, _opportunities) =
        build_with_spans(&market, Vec::new(), vec![Span::D1, Span::D7]);

    assert_eq!(snapshot.entries.len(), 4, "three items + one species; the empty fold is omitted");
    assert!(snapshot.entries.iter().all(|entry| entry.item != hollow));
    let expected =
        [(1i64, (1i64, 100i64), (2i64, 200i64)), (2, (1, 500), (2, 600)), (3, (1, 900), (1, 900))];
    for (id, d1, d7) in expected {
        let entry = entry(&snapshot, &item(id), Category::Item);
        let d1_block =
            entry.record.spans.get(&Span::D1).unwrap_or_else(|| panic!("item {id} d1 missing"));
        assert_eq!((d1_block.n, d1_block.median), d1, "item {id} d1 block");
        let d7_block =
            entry.record.spans.get(&Span::D7).unwrap_or_else(|| panic!("item {id} d7 missing"));
        assert_eq!((d7_block.n, d7_block.median), d7, "item {id} d7 block");
    }
    let species = ItemRef::Pet { species: 2546, level: 0, quality: 0, breed: 0 };
    let pet = entry(&snapshot, &species, Category::Pet);
    let pet_d1 = pet.record.spans.get(&Span::D1).expect("pet d1 block");
    assert_eq!((pet_d1.n, pet_d1.median), (5, 220));
    let pet_d7 = pet.record.spans.get(&Span::D7).expect("pet d7 block");
    assert_eq!((pet_d7.n, pet_d7.median), (7, 220));

    std::fs::remove_dir_all(&market.dir).ok();
}

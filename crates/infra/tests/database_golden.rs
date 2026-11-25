//! Golden fixtures of the database feed (`addon/SPEC.md`, section 5.11).
//!
//! One hand-written snapshot, exercising every flag, both signs, the
//! saturation values, pets and a dual-category id, is rendered three ways:
//! the carrier (`database_v4.lua`), its JSON twin (`database_v4.json`) and the
//! Lua oracle of what a decoder must read back (`database_v4_decoded.lua`).
//! Each must be byte-identical to the committed file, so a layout change
//! fails here until the fixtures are regenerated on purpose:
//!
//! ```sh
//! AMTRACKER_UPDATE_GOLDEN=1 cargo test -p amtracker-infra --test database_golden
//! ```

// Test assertions may unwrap: fixture-driven, deterministic inputs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_arguments
)]

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{
        Path,
        PathBuf,
    },
};

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
use amtracker_infra::{
    feed::{
        lua_string_literal,
        FeedCodec,
    },
    pack::basis_points,
    record,
};

const UPDATE: &str = "AMTRACKER_UPDATE_GOLDEN";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

// ------------------------------------------------------------ the snapshot

/// A span block with no ledger evidence.
#[allow(clippy::too_many_arguments)]
fn span(
    n: i64,
    min: i64,
    p05: i64,
    p25: i64,
    median: i64,
    p75: i64,
    p95: i64,
    max: i64,
    mad: i64,
    vwap: i64,
    qty_sum: i64,
) -> MetricBlock {
    MetricBlock {
        n,
        min,
        p05,
        p25,
        median,
        p75,
        p95,
        max,
        mad,
        vwap,
        qty_sum,
        listing_count: n,
        sale_rate: None,
        sale_rate_180d: None,
        days_since_last_sale: None,
        closures_since_last_sale: None,
        qty_sold: None,
    }
}

/// A span block whose every listing asked `unit`.
fn flat(n: i64, unit: i64) -> MetricBlock {
    span(n, unit, unit, unit, unit, unit, unit, unit, 0, unit, n)
}

/// Adds the ledger evidence to a span block: each field takes the given
/// value, and stays absent for `None`.
#[allow(clippy::needless_pass_by_value)] // the builder consumes its block
fn ledger(
    block: MetricBlock,
    sale_rate: Option<f64>,
    sale_rate_180d: Option<f64>,
    days_since_last_sale: Option<i64>,
    closures_since_last_sale: Option<i64>,
    qty_sold: Option<i64>,
) -> MetricBlock {
    MetricBlock {
        sale_rate,
        sale_rate_180d,
        days_since_last_sale,
        closures_since_last_sale,
        qty_sold,
        ..block
    }
}

fn entry(item: ItemRef, category: Category, record: ItemRecord) -> DatabaseEntry {
    DatabaseEntry { item, category, record }
}

fn spans_only(spans: Vec<(Span, MetricBlock)>) -> ItemRecord {
    ItemRecord {
        spans: BTreeMap::from_iter(spans),
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
    }
}

/// A listed item in the newest snapshot, with nothing seen gone.
fn listing(floor: i64, floor_qty: i64, listed: i64, qty_sum: Option<i64>) -> LiveQuote {
    LiveQuote { listed: Some(LiveListing { floor, floor_qty, listed, qty_sum }), seen: None }
}

fn item(id: i64) -> ItemRef {
    ItemRef::Item { item_id: id }
}

fn pet(species: i64) -> ItemRef {
    ItemRef::Pet { species, level: 0, quality: 0, breed: 0 }
}

/// A steady seller: every span, listed, sold evidence, a sparkline, a vendor
/// price, and no craft or deal.
fn steady_seller() -> DatabaseEntry {
    let d1 = ledger(
        span(58, 575, 600, 1575, 15000, 19985, 50100, 50100, 5100, 15912, 58),
        Some(0.8889),
        Some(0.8889),
        Some(0),
        Some(0),
        Some(9),
    );
    let d7 = ledger(
        span(188, 100, 300, 1000, 2499, 7500, 20100, 50100, 2049, 7030, 188),
        Some(0.8889),
        Some(0.8889),
        Some(0),
        Some(0),
        Some(25),
    );
    let d14 = ledger(
        span(220, 100, 300, 800, 2275, 7500, 20100, 50100, 1725, 6410, 220),
        Some(0.8889),
        Some(0.8889),
        Some(0),
        Some(0),
        Some(32),
    );
    // The slow rate has no evidence in this window.
    let d30 = ledger(d14.clone(), Some(0.8889), None, Some(0), Some(0), Some(32));
    entry(
        item(25),
        Category::Item,
        ItemRecord {
            live: Some(listing(15000, 1, 58, Some(58))),
            value: Some(ValueQuote {
                value7: 7030,
                samples7: 188,
                sold_median14: None,
                trend_bp: Some(55934),
            }),
            sold_count: Some(32),
            expired_count: Some(4),
            supply_days: Some(25),
            low30: Some(100),
            low_rank_30d_bp: Some(8000),
            lows: Some(vec![2, 1, 3, 36, 120, 128, 131, 127, 130, 126, 133, 129]),
            vendor_price: Some(1250),
            ..spans_only(vec![(Span::D1, d1), (Span::D7, d7), (Span::D14, d14), (Span::D30, d30)])
        },
    )
}

/// A rare listing that never sells: multi-byte prices, a present zero sale
/// rate, saturated supply, a trend of exactly zero and the top deal.
fn never_sells() -> DatabaseEntry {
    let d1 = ledger(flat(1, 9_999_999_999), Some(0.0), None, None, None, Some(0));
    let d7 = ledger(
        span(
            3,
            8_500_000_000,
            8_500_000_000,
            9_000_000_000,
            9_500_000_000,
            9_999_999_999,
            9_999_999_999,
            9_999_999_999,
            250_000_000,
            9_333_333_333,
            3,
        ),
        Some(0.0),
        None,
        None,
        None,
        Some(0),
    );
    entry(
        item(117),
        Category::Item,
        ItemRecord {
            live: Some(listing(9_999_999_999, 1, 1, Some(1))),
            value: Some(ValueQuote {
                value7: 9_500_000_000,
                samples7: 3,
                sold_median14: None,
                trend_bp: Some(0),
            }),
            sold_count: Some(0),
            expired_count: Some(3),
            supply_days: Some(u16::MAX),
            low30: Some(8_500_000_000),
            low_rank_30d_bp: Some(0),
            deal: Some(DealMark { rank: 1, score_bp: 9250, discount_bp: 5000 }),
            ..spans_only(vec![(Span::D1, d1), (Span::D7, d7)])
        },
    )
}

/// A bargain seen today and already gone: nothing of it is listed any more.
fn gone_today() -> DatabaseEntry {
    let d1 = ledger(
        span(410, 2500, 2600, 2800, 3000, 3200, 3500, 4000, 200, 3010, 4100),
        Some(0.8),
        Some(0.75),
        Some(1),
        Some(2),
        Some(120),
    );
    let d7 = ledger(
        span(2600, 2400, 2500, 2700, 2950, 3100, 3400, 3800, 150, 2980, 26_000),
        Some(0.8),
        Some(0.75),
        Some(1),
        Some(2),
        Some(780),
    );
    let d14 = ledger(
        span(5100, 2400, 2500, 2700, 2950, 3100, 3400, 3800, 150, 2980, 51_000),
        Some(0.8),
        Some(0.75),
        Some(1),
        Some(2),
        Some(1530),
    );
    let d30 = ledger(
        span(10_900, 2350, 2480, 2700, 2940, 3120, 3450, 3900, 155, 2975, 109_000),
        Some(0.8),
        Some(0.75),
        Some(1),
        Some(2),
        Some(3270),
    );
    entry(
        item(2589),
        Category::Item,
        ItemRecord {
            live: Some(LiveQuote {
                listed: None,
                seen: Some(SeenListing { floor: 2500, qty: 20 }),
            }),
            value: Some(ValueQuote {
                value7: 3000,
                samples7: 2600,
                sold_median14: Some(3200),
                trend_bp: None,
            }),
            sold_count: Some(120),
            expired_count: Some(30),
            ..spans_only(vec![(Span::D1, d1), (Span::D7, d7), (Span::D14, d14), (Span::D30, d30)])
        },
    )
}

/// One side of the dual-category id: spans only, and never a credited sale.
fn dual_item_side() -> DatabaseEntry {
    let d1 = ledger(flat(2, 4_500_000), Some(0.0), None, None, None, None);
    let d7 = ledger(flat(5, 4_400_000), Some(0.0), None, None, None, None);
    entry(item(194_784), Category::Item, spans_only(vec![(Span::D1, d1), (Span::D7, d7)]))
}

/// A craft that loses money: nothing has ever sold (a present zero rate), a
/// positive trend, no quantity sum, a one-day sparkline, and an expected
/// profit that is a present zero because nothing sells.
fn losing_craft() -> DatabaseEntry {
    let d1 = ledger(flat(3, 90_000), Some(0.0), Some(0.0), None, None, Some(0));
    let d7 = ledger(flat(21, 88_000), Some(0.0), Some(0.0), None, None, Some(0));
    let d14 = ledger(flat(48, 86_000), Some(0.0), Some(0.0), None, None, Some(0));
    let d30 = ledger(flat(103, 84_000), Some(0.0), Some(0.0), None, None, Some(0));
    entry(
        item(191_460),
        Category::Commodity,
        ItemRecord {
            live: Some(listing(90_000, 100, 3, None)),
            value: Some(ValueQuote {
                value7: 80_000,
                samples7: 21,
                sold_median14: None,
                trend_bp: Some(2500),
            }),
            sold_count: Some(0),
            expired_count: Some(10),
            low30: Some(60_000),
            low_rank_30d_bp: Some(2000),
            lows: Some(vec![96]),
            vendor_price: Some(50_000),
            craft: Some(CraftQuote { cost: 400_000, profit: -125_000, expected_profit: 0 }),
            ..spans_only(vec![(Span::D1, d1), (Span::D7, d7), (Span::D14, d14), (Span::D30, d30)])
        },
    )
}

/// A bulk commodity: quantities past a billion, a sold reference, a falling
/// trend and a full 30-day sparkline.
fn bulk_commodity() -> DatabaseEntry {
    let d1 = ledger(
        span(4321, 12, 13, 15, 18, 22, 40, 95, 3, 19, 1_234_567_890),
        Some(0.9565),
        Some(0.9411),
        Some(0),
        Some(0),
        Some(5_500_000),
    );
    let d7 = ledger(
        span(30_112, 11, 12, 14, 18, 21, 38, 95, 3, 18, 8_001_234_567),
        Some(0.9565),
        Some(0.9411),
        Some(0),
        Some(0),
        Some(38_500_000),
    );
    let d14 = ledger(
        span(61_540, 11, 12, 14, 17, 21, 38, 101, 3, 18, 16_400_000_000),
        Some(0.9565),
        Some(0.9411),
        Some(0),
        Some(0),
        Some(77_000_000),
    );
    let d30 = ledger(
        span(130_004, 10, 11, 13, 17, 21, 40, 105, 3, 17, 35_000_000_000),
        Some(0.9565),
        Some(0.9411),
        Some(0),
        Some(0),
        Some(164_000_000),
    );
    entry(
        item(191_461),
        Category::Commodity,
        ItemRecord {
            live: Some(listing(12, 200_000, 4321, Some(1_234_567_890))),
            value: Some(ValueQuote {
                value7: 18,
                samples7: 30_112,
                sold_median14: Some(19),
                trend_bp: Some(-150),
            }),
            sold_count: Some(110),
            expired_count: Some(5),
            supply_days: Some(3),
            low30: Some(11),
            low_rank_30d_bp: Some(10_000),
            lows: Some(vec![
                81, 81, 88, 95, 108, 108, 108, 115, 122, 122, 128, 135, 135, 142, 149, 155, 162,
                168, 175, 175, 182, 189, 195, 202, 202, 209, 215, 215, 222, 229,
            ]),
            vendor_price: Some(1),
            deal: Some(DealMark { rank: 87, score_bp: 4200, discount_bp: 500 }),
            ..spans_only(vec![(Span::D1, d1), (Span::D7, d7), (Span::D14, d14), (Span::D30, d30)])
        },
    )
}

/// The other side of the dual-category id, and the record that holds every
/// group at once: the example of the contract's JSON twin.
fn everything() -> DatabaseEntry {
    let d1 = ledger(
        span(
            618, 3_805_000, 3_820_000, 3_960_000, 4_010_000, 4_120_000, 4_310_000, 5_200_000,
            95_000, 4_032_000, 1234,
        ),
        Some(0.3617),
        Some(0.3125),
        Some(2),
        Some(61),
        Some(9),
    );
    let d7 = ledger(
        span(
            1305, 3_805_000, 3_850_000, 4_000_000, 4_310_000, 4_400_000, 5_120_000, 5_200_000,
            120_000, 4_123_500, 4900,
        ),
        Some(0.3617),
        Some(0.3125),
        Some(2),
        Some(61),
        Some(31),
    );
    let d14 = ledger(
        span(
            2612, 3_805_000, 3_850_000, 4_000_000, 4_310_000, 4_400_000, 5_120_000, 5_200_000,
            120_000, 4_123_500, 9800,
        ),
        Some(0.3617),
        Some(0.3125),
        Some(2),
        Some(61),
        Some(63),
    );
    let d30 = ledger(
        span(
            5480, 3_805_000, 3_850_000, 4_000_000, 4_310_000, 4_400_000, 5_120_000, 5_200_000,
            120_000, 4_123_500, 20_600,
        ),
        Some(0.3617),
        Some(0.3125),
        Some(2),
        Some(61),
        Some(121),
    );
    entry(
        item(194_784),
        Category::Commodity,
        ItemRecord {
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
                value7: 5_980_000,
                samples7: 1305,
                sold_median14: Some(6_137_000),
                trend_bp: Some(-300),
            }),
            sold_count: Some(34),
            expired_count: Some(60),
            supply_days: Some(274),
            low30: Some(3_500_000),
            low_rank_30d_bp: Some(8000),
            lows: Some(vec![
                73, 75, 74, 80, 90, 110, 128, 130, 127, 131, 126, 133, 129, 128, 125, 122, 118,
                110, 104, 98, 95, 92, 88, 85, 83, 80, 78, 76, 74, 73,
            ]),
            vendor_price: Some(125_000),
            craft: Some(CraftQuote { cost: 5_550_000, profit: 280_150, expected_profit: 101_358 }),
            deal: Some(DealMark { rank: 12, score_bp: 7100, discount_bp: 3800 }),
            ..spans_only(vec![(Span::D1, d1), (Span::D7, d7), (Span::D14, d14), (Span::D30, d30)])
        },
    )
}

/// A species with a single variant: full groups, keyed by species.
fn single_variant_pet() -> DatabaseEntry {
    let d1 = ledger(flat(2, 2_500_000), Some(1.0), None, Some(0), Some(0), Some(1));
    let d7 = ledger(flat(12, 2_400_000), Some(1.0), None, Some(0), Some(0), Some(6));
    entry(
        pet(2546),
        Category::Pet,
        ItemRecord {
            live: Some(listing(2_500_000, 1, 2, Some(2))),
            value: Some(ValueQuote {
                value7: 2_400_000,
                samples7: 12,
                sold_median14: Some(2_450_000),
                trend_bp: None,
            }),
            sold_count: Some(6),
            expired_count: Some(0),
            supply_days: Some(4),
            low30: Some(2_400_000),
            low_rank_30d_bp: Some(0),
            deal: Some(DealMark { rank: 3, score_bp: 5500, discount_bp: 200 }),
            ..spans_only(vec![(Span::D1, d1), (Span::D7, d7)])
        },
    )
}

/// A species pooled from several variants: spans only, with a span block
/// that carries no ledger field at all.
fn pooled_pet() -> DatabaseEntry {
    entry(pet(293), Category::Pet, spans_only(vec![(Span::D30, flat(1, 8_000_000))]))
}

/// The hand-written snapshot behind the three golden files. Sorted by
/// identity, then category, as the contract requires.
fn fixture_snapshot() -> DatabaseSnapshot {
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
        entries: vec![
            steady_seller(),
            never_sells(),
            gone_today(),
            losing_craft(),
            bulk_commodity(),
            dual_item_side(),
            everything(),
            pooled_pet(),
            single_variant_pet(),
        ],
    }
}

// --------------------------------------------------------------- the oracle

/// The Lua oracle: the market header, then every record as a decoder must
/// read it back, one table per carrier table. Rates are written the way the
/// decoder computes them, so the two doubles are identical.
fn oracle(snapshot: &DatabaseSnapshot) -> String {
    let mut out = String::new();
    out.push_str(
        "-- Oracle for database_v4.lua: the market header a carrier hands to\n\
         -- A.BeginMarket, and the records a correct decoder must read back from\n\
         -- it, one table per carrier table. GENERATED from the hand-written\n\
         -- snapshot in crates/infra/tests/database_golden.rs. Do not hand-edit.\n\
         return {\n",
    );
    out.push_str("    meta = {\n");
    let meta = &snapshot.market;
    let _ = writeln!(out, "        version = {},", DatabaseSnapshot::FEED_VERSION);
    let _ = writeln!(
        out,
        "        trackerVersion = {},",
        lua_string_literal(&snapshot.tracker_version)
    );
    let _ = writeln!(out, "        target = {},", lua_string_literal(snapshot.target.as_str()));
    let _ = writeln!(out, "        flavor = {},", lua_string_literal(meta.flavor.as_str()));
    if let Some(region) = &meta.region {
        let _ = writeln!(out, "        region = {},", lua_string_literal(region));
    }
    if let Some(id) = meta.ruleset_id {
        let _ = writeln!(out, "        rulesetId = {id},");
    }
    if let Some(id) = meta.house_id {
        let _ = writeln!(out, "        houseId = {id},");
    }
    if let Some(id) = meta.connected_realm_id {
        let _ = writeln!(out, "        connectedRealmId = {id},");
    }
    let _ = writeln!(out, "        generatedAt = {},", snapshot.generated_at);
    if let Some(at) = snapshot.auctions_at {
        let _ = writeln!(out, "        auctionsAt = {at},");
    }
    if let Some(at) = snapshot.commodities_at {
        let _ = writeln!(out, "        commoditiesAt = {at},");
    }
    out.push_str("    },\n");

    for (table, category) in
        [("items", Category::Item), ("commodities", Category::Commodity), ("pets", Category::Pet)]
    {
        let _ = writeln!(out, "    {table} = {{");
        for entry in snapshot.entries.iter().filter(|entry| entry.category == category) {
            let key = match entry.item {
                ItemRef::Item { item_id } => item_id,
                ItemRef::Pet { species, .. } => species,
            };
            let decoded =
                record::decode(&record::encode(&entry.record)).expect("the record decodes");
            oracle_record(&mut out, key, &decoded);
        }
        out.push_str("    },\n");
    }
    out.push_str("}\n");
    out
}

fn oracle_span(out: &mut String, name: &str, block: &MetricBlock) {
    let _ = write!(
        out,
        "                {name} = {{ n = {}, min = {}, p05 = {}, p25 = {}, median = {}, p75 = {}, p95 = {}, max = {}, mad = {}, vwap = {}, qtySum = {}, listingCount = {}",
        block.n,
        block.min,
        block.p05,
        block.p25,
        block.median,
        block.p75,
        block.p95,
        block.max,
        block.mad,
        block.vwap,
        block.qty_sum,
        block.listing_count
    );
    if let Some(rate) = block.sale_rate {
        let _ = write!(out, ", saleRate = {} / 10000", basis_points(rate));
    }
    if let Some(rate) = block.sale_rate_180d {
        let _ = write!(out, ", saleRate180d = {} / 10000", basis_points(rate));
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
    out.push_str(" },\n");
}

/// One decoded record as a plain Lua table, absent fields left out.
fn oracle_record(out: &mut String, key: i64, record: &ItemRecord) {
    let _ = writeln!(out, "        [{key}] = {{");
    out.push_str("            spans = {\n");
    for span in Span::ALL {
        if let Some(block) = record.spans.get(&span) {
            oracle_span(out, span.name(), block);
        }
    }
    out.push_str("            },\n");
    if let Some(live) = &record.live {
        let mut fields = Vec::new();
        if let Some(listing) = &live.listed {
            fields.push(format!("floor = {}", listing.floor));
            fields.push(format!("floorQty = {}", listing.floor_qty));
            fields.push(format!("listed = {}", listing.listed));
            if let Some(qty_sum) = listing.qty_sum {
                fields.push(format!("qtySum = {qty_sum}"));
            }
        }
        if let Some(seen) = &live.seen {
            fields.push(format!("seenFloor = {}", seen.floor));
            fields.push(format!("seenQty = {}", seen.qty));
        }
        let _ = writeln!(out, "            live = {{ {} }},", fields.join(", "));
    }
    if let Some(value) = &record.value {
        oracle_scalar(out, "value7", Some(value.value7));
        oracle_scalar(out, "soldMedian14", value.sold_median14);
        oracle_scalar(out, "samples7", Some(value.samples7));
        oracle_scalar(out, "trendBp", value.trend_bp);
    }
    oracle_scalar(out, "soldCount", record.sold_count);
    oracle_scalar(out, "expiredCount", record.expired_count);
    oracle_scalar(out, "supplyDays", record.supply_days.map(i64::from));
    oracle_scalar(out, "low30", record.low30);
    oracle_scalar(out, "lowRank30d", record.low_rank_30d_bp.map(i64::from));
    if let Some(lows) = &record.lows {
        let bytes: Vec<String> = lows.iter().map(u8::to_string).collect();
        let _ = writeln!(out, "            lows = {{ {} }},", bytes.join(", "));
    }
    oracle_scalar(out, "vendorPrice", record.vendor_price);
    if let Some(craft) = &record.craft {
        let _ = writeln!(
            out,
            "            craft = {{ cost = {}, profit = {}, expectedProfit = {} }},",
            craft.cost, craft.profit, craft.expected_profit
        );
    }
    if let Some(deal) = &record.deal {
        let _ = writeln!(
            out,
            "            deal = {{ rank = {}, scoreBp = {}, discountBp = {} }},",
            deal.rank, deal.score_bp, deal.discount_bp
        );
    }
    out.push_str("        },\n");
}

fn oracle_scalar(out: &mut String, name: &str, value: Option<i64>) {
    if let Some(value) = value {
        // `String`'s `fmt::Write` impl is infallible.
        let _ = writeln!(out, "            {name} = {value},");
    }
}

// ------------------------------------------------------------------ checks

/// Compares rendered bytes with the committed file, or rewrites the file
/// when `AMTRACKER_UPDATE_GOLDEN=1`.
fn check(name: &str, rendered: &[u8]) {
    let path = fixtures().join(name);
    if std::env::var_os(UPDATE).is_some_and(|value| value == "1") {
        std::fs::write(&path, rendered).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        eprintln!("rewrote {}", path.display());
        return;
    }
    let committed = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}; regenerate with {UPDATE}=1", path.display()));
    assert_eq!(
        rendered,
        committed.as_slice(),
        "{name} differs from the committed fixture; if the change is intended, regenerate with {UPDATE}=1"
    );
}

struct CarrierRecord {
    table: String,
    key: i64,
    record: ItemRecord,
}

/// Every record line of the carrier, decoded.
fn carrier_records(carrier: &str) -> Vec<CarrierRecord> {
    carrier
        .lines()
        .filter(|line| line.starts_with("m."))
        .map(|line| {
            let (head, tail) = line.split_once(" = \"").expect("record line");
            let word = tail.strip_suffix('"').expect("closing quote");
            let (table, rest) =
                head.strip_prefix("m.").expect("`m.` prefix").split_once('[').expect("key");
            let key: i64 =
                rest.strip_suffix(']').expect("closing bracket").parse().expect("numeric key");
            CarrierRecord {
                table: table.to_string(),
                key,
                record: record::decode(word).unwrap_or_else(|e| panic!("{head}: {e}")),
            }
        })
        .collect()
}

#[test]
fn golden_files_match_the_committed_fixtures() {
    let snapshot = fixture_snapshot();
    check("database_v4.lua", &FeedCodec::carrier(&snapshot).expect("carrier"));
    check("database_v4.json", &FeedCodec::json_twin(&snapshot).expect("twin"));
    check("database_v4_decoded.lua", oracle(&snapshot).as_bytes());
}

#[test]
fn the_carrier_decodes_back_to_the_snapshot() {
    let snapshot = fixture_snapshot();
    let carrier =
        String::from_utf8(FeedCodec::carrier(&snapshot).expect("carrier")).expect("utf-8");
    let decoded = carrier_records(&carrier);
    assert_eq!(decoded.len(), snapshot.entries.len());
    for CarrierRecord { table, key, record } in &decoded {
        let category = match table.as_str() {
            "items" => Category::Item,
            "commodities" => Category::Commodity,
            "pets" => Category::Pet,
            other => panic!("unknown table {other}"),
        };
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.category == category && key_of(&entry.item) == *key)
            .unwrap_or_else(|| panic!("{table}[{key}] is not in the snapshot"));
        assert_eq!(record, &entry.record, "{table}[{key}]");
    }
}

fn key_of(item: &ItemRef) -> i64 {
    match item {
        ItemRef::Item { item_id } => *item_id,
        ItemRef::Pet { species, .. } => *species,
    }
}

#[test]
fn the_twin_mirrors_the_carrier() {
    let snapshot = fixture_snapshot();
    let carrier =
        String::from_utf8(FeedCodec::carrier(&snapshot).expect("carrier")).expect("utf-8");
    let twin = String::from_utf8(FeedCodec::json_twin(&snapshot).expect("twin")).expect("utf-8");
    let value: serde_json::Value = serde_json::from_str(&twin).expect("valid json");
    let decoded = carrier_records(&carrier);
    let items = value["items"].as_array().expect("items array");
    assert_eq!(items.len(), decoded.len());
    assert_eq!(value["version"], i64::from(DatabaseSnapshot::FEED_VERSION));
    assert_eq!(value["target"], "forever-eu-pvp-alliance");
    for (json, CarrierRecord { table, key, record }) in items.iter().zip(decoded) {
        let category = match table.as_str() {
            "items" => Category::Item,
            "commodities" => Category::Commodity,
            "pets" => Category::Pet,
            other => panic!("unknown table {other}"),
        };
        assert_eq!(json["category"], category.as_str());
        let identity = if table == "pets" { &json["speciesID"] } else { &json["itemID"] };
        assert_eq!(identity.as_i64(), Some(key));
        // The twin is the decoded record: every span count and rate agree.
        for (span, block) in &record.spans {
            let json_span = &json["spans"][span.name()];
            assert_eq!(json_span["n"], block.n);
            assert_eq!(json_span["median"], block.median);
            assert_eq!(json_span["qtySum"], block.qty_sum);
            match block.sale_rate {
                Some(rate) => assert_eq!(json_span["saleRate"], rate),
                None => assert!(json_span.get("saleRate").is_none()),
            }
        }
        match &record.live {
            Some(live) => {
                match &live.listed {
                    Some(listing) => {
                        assert_eq!(json["live"]["floor"], listing.floor);
                        assert_eq!(json["live"]["floorQty"], listing.floor_qty);
                    }
                    None => assert!(json["live"].get("floor").is_none(), "gone today: no floor"),
                }
                match &live.seen {
                    Some(seen) => assert_eq!(json["live"]["seenQty"], seen.qty),
                    None => assert!(json["live"].get("seenQty").is_none()),
                }
            }
            None => assert_eq!(json["live"], serde_json::Value::Null),
        }
        match record.supply_days {
            Some(days) => assert_eq!(json["supplyDays"], i64::from(days)),
            None => assert!(json.get("supplyDays").is_none()),
        }
        match &record.deal {
            Some(deal) => assert_eq!(json["deal"]["rank"], deal.rank),
            None => assert_eq!(json["deal"], serde_json::Value::Null),
        }
    }
}

/// Every record of the fixture's carrier, decoded, in carrier order.
fn fixture_records() -> Vec<ItemRecord> {
    let snapshot = fixture_snapshot();
    let carrier =
        String::from_utf8(FeedCodec::carrier(&snapshot).expect("carrier")).expect("utf-8");
    carrier_records(&carrier).into_iter().map(|entry| entry.record).collect()
}

/// How many records satisfy `test`.
fn count_matching(records: &[ItemRecord], test: fn(&ItemRecord) -> bool) -> usize {
    records.iter().filter(|record| test(record)).count()
}

#[test]
fn the_fixture_lists_every_table_in_order() {
    let snapshot = fixture_snapshot();
    let carrier =
        String::from_utf8(FeedCodec::carrier(&snapshot).expect("carrier")).expect("utf-8");
    let decoded = carrier_records(&carrier);
    let tables: Vec<&str> = decoded.iter().map(|entry| entry.table.as_str()).collect();
    assert_eq!(
        tables,
        [
            "items",
            "items",
            "items",
            "items",
            "commodities",
            "commodities",
            "commodities",
            "pets",
            "pets"
        ]
    );
    let keys: Vec<i64> = decoded.iter().map(|entry| entry.key).collect();
    assert_eq!(
        keys,
        [25, 117, 2589, 194_784, 191_460, 191_461, 194_784, 293, 2546],
        "each table ascends by key"
    );
    // The dual-category id appears once in each of its tables.
    assert_eq!(keys.iter().filter(|key| **key == 194_784).count(), 2);
}

/// The fixture must keep exercising every group and every flag of a record;
/// this fails when an edit to the snapshot quietly drops a case.
#[test]
fn the_fixture_exercises_every_group_and_flag() {
    let records = fixture_records();
    let matching = |test: fn(&ItemRecord) -> bool| count_matching(&records, test);
    assert!(matching(|record| record.live.is_some()) >= 5, "live group");
    assert!(matching(|record| record.live.is_none()) >= 2, "not listed and not seen");
    assert!(
        matching(|record| record.live.as_ref().is_some_and(|live| live.listed.is_none())) >= 1,
        "gone today"
    );
    assert!(
        matching(|record| record.live.as_ref().is_some_and(|live| live.seen.is_some())) >= 1,
        "a cheaper one seen"
    );
    assert!(
        matching(|record| record.live.as_ref().is_some_and(|live| live.seen.is_none())) >= 4,
        "nothing cheaper seen"
    );
    let quantity_sum = |record: &ItemRecord| {
        record
            .live
            .as_ref()
            .and_then(|live| live.listed.as_ref())
            .is_some_and(|listing| listing.qty_sum.is_some())
    };
    assert!(matching(quantity_sum) >= 4, "a quantity sum");
    let listing_without_sum = |record: &ItemRecord| {
        record
            .live
            .as_ref()
            .and_then(|live| live.listed.as_ref())
            .is_some_and(|listing| listing.qty_sum.is_none())
    };
    assert!(matching(listing_without_sum) >= 1, "a listing without a quantity sum");
    let sold_median = |record: &ItemRecord| {
        record.value.as_ref().is_some_and(|value| value.sold_median14.is_some())
    };
    assert!(matching(sold_median) >= 3, "a sold-source value");
    let listed_value = |record: &ItemRecord| {
        record.value.as_ref().is_some_and(|value| value.sold_median14.is_none())
    };
    assert!(matching(listed_value) >= 3, "a listed-source value");
    let trends: Vec<Option<i64>> = records
        .iter()
        .map(|record| record.value.as_ref().and_then(|value| value.trend_bp))
        .collect();
    assert!(trends.iter().filter(|trend| trend.is_none()).count() >= 2, "no trend yet");
    assert!(trends.contains(&Some(0)), "a flat trend is present");
    assert!(trends.iter().any(|trend| trend.is_some_and(|trend| trend < 0)), "falling");
    assert!(trends.iter().any(|trend| trend.is_some_and(|trend| trend > 0)), "rising");
    assert!(matching(|record| record.sold_count.is_some()) >= 5, "sold evidence");
    assert!(matching(|record| record.sold_count == Some(0)) >= 2, "evidence of nothing sold");
    assert!(matching(|record| record.expired_count.is_some()) >= 4, "expired evidence");
    assert!(matching(|record| record.supply_days.is_some()) >= 4, "supply");
    assert!(matching(|record| record.supply_days == Some(u16::MAX)) >= 1, "saturated supply");
    assert!(
        matching(|record| record.supply_days.is_none() && record.sold_count.is_some()) >= 1,
        "unbounded supply"
    );
    assert!(matching(|record| record.low30.is_some()) >= 5, "30-day low");
    assert!(matching(|record| record.low_rank_30d_bp.is_some()) >= 5, "low rank");
    assert!(
        matching(|record| record.low_rank_30d_bp == Some(0)) >= 1,
        "a low rank of zero is present"
    );
    assert!(
        matching(|record| record.low_rank_30d_bp == Some(10_000)) >= 1,
        "a low rank of one is present"
    );
    assert!(
        matching(|record| record.lows.as_ref().is_some_and(|lows| lows.len() == 30)) >= 2,
        "a full sparkline"
    );
    assert!(
        matching(|record| record.lows.as_ref().is_some_and(|lows| lows.len() == 1)) >= 1,
        "a one-day sparkline"
    );
    assert!(matching(|record| record.vendor_price.is_some()) >= 3, "vendor price");
    assert!(
        matching(|record| record.craft.as_ref().is_some_and(|craft| craft.profit < 0)) >= 1,
        "a loss"
    );
    assert!(
        matching(|record| record.craft.as_ref().is_some_and(|craft| craft.profit > 0)) >= 1,
        "a gain"
    );
    assert!(
        matching(|record| record.craft.as_ref().is_some_and(|craft| craft.expected_profit == 0))
            >= 1,
        "a present zero expected profit"
    );
    assert!(
        matching(|record| record.deal.as_ref().is_some_and(|deal| deal.rank == 1)) >= 1,
        "the top deal"
    );
    assert!(matching(|record| record.deal.is_some()) >= 3, "deals");
}

/// Every span flag is both set and clear somewhere, and every span shape is
/// present: each span, a single-span record, a block without ledger evidence,
/// a present zero rate and multi-byte prices.
#[test]
fn the_fixture_exercises_every_span_flag() {
    let records = fixture_records();
    let present = |block: &MetricBlock, flag: u32| match flag {
        0 => block.sale_rate.is_some(),
        1 => block.sale_rate_180d.is_some(),
        2 => block.days_since_last_sale.is_some(),
        3 => block.closures_since_last_sale.is_some(),
        _ => block.qty_sold.is_some(),
    };
    for flag in 0..5u32 {
        let mut set = 0;
        let mut clear = 0;
        for block in records.iter().flat_map(|record| record.spans.values()) {
            if present(block, flag) {
                set += 1;
            } else {
                clear += 1;
            }
        }
        assert!(set > 0 && clear > 0, "span flag {flag} must be exercised both ways");
    }
    for span in Span::ALL {
        assert!(records.iter().any(|record| record.spans.contains_key(&span)), "{span:?} present");
    }
    let blocks = |test: fn(&MetricBlock) -> bool| {
        records.iter().flat_map(|record| record.spans.values()).filter(|block| test(block)).count()
    };
    assert!(records.iter().any(|record| record.spans.len() == 1), "a single-span record");
    assert!(blocks(|block| block.sale_rate.is_none()) >= 1, "a block without ledger evidence");
    assert!(blocks(|block| block.sale_rate == Some(0.0)) >= 5, "a present zero rate");
    assert!(blocks(|block| block.qty_sold == Some(0)) >= 5, "present zero units sold");
    assert!(blocks(|block| block.max >= 1_000_000_000) >= 2, "multi-byte prices");
}

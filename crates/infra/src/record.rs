//! Record codec of the database feed (`addon/SPEC.md`, section 5.5): one
//! [`ItemRecord`] in, one printable word out, and back.
//!
//! [`encode`] is the tracker's writer. [`decode`] is the Rust reference
//! decoder the tests hold the addon's Lua decoder against: it refuses exactly
//! what the contract calls malformed and nothing an encoder can write, so
//! every text it accepts re-encodes to itself.
//!
//! The wire is unsigned. A negative value in an unsigned field (a tracker bug,
//! never real data) saturates at zero instead of wrapping, and a value above
//! the contract's largest integer saturates there.

use std::collections::BTreeMap;

use amtracker_core::domain::{
    CraftQuote,
    DealMark,
    ItemRecord,
    LiveListing,
    LiveQuote,
    MetricBlock,
    SeenListing,
    Span,
    ValueQuote,
};

use crate::pack::{
    self,
    PackError,
    Reader,
};

// Group presence, in the order the groups follow the spans.
const GROUP_LIVE: u64 = 0x01;
const GROUP_VALUE: u64 = 0x02;
const GROUP_EVIDENCE: u64 = 0x04;
const GROUP_POSITION: u64 = 0x08;
const GROUP_VENDOR: u64 = 0x10;
const GROUP_CRAFT: u64 = 0x20;
const GROUP_DEAL: u64 = 0x40;
const GROUP_MASK: u64 = 0x7F;

/// The spans in wire order, each with its presence bit.
const SPAN_BITS: [(u64, Span); 4] =
    [(0x01, Span::D1), (0x02, Span::D7), (0x04, Span::D14), (0x08, Span::D30)];
const SPAN_MASK: u64 = 0x0F;

const SFLAG_SALE_RATE: u64 = 0x01;
const SFLAG_SALE_RATE_180D: u64 = 0x02;
const SFLAG_DAYS_SINCE_LAST_SALE: u64 = 0x04;
const SFLAG_CLOSURES_SINCE_LAST_SALE: u64 = 0x08;
const SFLAG_QTY_SOLD: u64 = 0x10;
const SFLAG_MASK: u64 = 0x1F;

const LFLAG_LISTED: u64 = 0x01;
const LFLAG_QTY_SUM: u64 = 0x02;
const LFLAG_SEEN: u64 = 0x04;
const LFLAG_DAYS_LISTED: u64 = 0x08;
const LFLAG_MASK: u64 = 0x0F;

const VFLAG_SOLD_MEDIAN14: u64 = 0x01;
const VFLAG_TREND: u64 = 0x02;
const VFLAG_MASK: u64 = 0x03;

const EFLAG_SOLD_COUNT: u64 = 0x01;
const EFLAG_EXPIRED_COUNT: u64 = 0x02;
const EFLAG_SUPPLY_DAYS: u64 = 0x04;
const EFLAG_MASK: u64 = 0x07;

const PFLAG_LOW30: u64 = 0x01;
const PFLAG_LOW_RANK: u64 = 0x02;
const PFLAG_LOWS: u64 = 0x04;
const PFLAG_MASK: u64 = 0x07;

/// The `lows` sparkline covers the previous 30 days, one byte each. A longer
/// count cannot be real data, so it is refused before anything is allocated.
const MAX_LOWS: u64 = 30;

// ------------------------------------------------------------------- writing

/// Writes a field the wire carries unsigned: a negative value saturates at
/// zero and anything above the contract's ceiling at the ceiling.
fn put_uint(out: &mut Vec<u8>, value: i64) {
    let clamped = u64::try_from(value).unwrap_or(0).min(pack::MAX_INTEGER);
    pack::put_varint(out, clamped);
}

/// Writes a signed field, zigzagged, clamped to what the wire carries back.
fn put_signed(out: &mut Vec<u8>, value: i64) {
    let limit = i64::try_from(pack::MAX_INTEGER).unwrap_or(i64::MAX);
    pack::put_zigzag(out, value.clamp(-limit, limit));
}

/// A rate as the wire carries it: basis points. A non-finite rate is a tracker
/// bug, never evidence, so it stays absent instead of reading as a zero.
fn rate_bp(rate: Option<f64>) -> Option<u32> {
    rate.filter(|rate| rate.is_finite()).map(pack::basis_points)
}

/// A rate read off the wire, back to the fraction the span block carries.
fn rate_from_bp(bp: u64) -> f64 {
    f64::from(u32::try_from(bp).unwrap_or(u32::MAX)) / 10_000.0
}

/// Packs a record into its printable word. Deterministic and infallible: the
/// record is written as given, including one a decoder then refuses (a record
/// without a span, say; see [`decode`]).
#[must_use]
pub fn encode(record: &ItemRecord) -> String {
    let mut out = Vec::new();
    let mut flags = 0;
    if record.live.is_some() {
        flags |= GROUP_LIVE;
    }
    if record.value.is_some() {
        flags |= GROUP_VALUE;
    }
    if record.sold_count.is_some() || record.expired_count.is_some() || record.supply_days.is_some()
    {
        flags |= GROUP_EVIDENCE;
    }
    if record.low30.is_some() || record.low_rank_30d_bp.is_some() || record.lows.is_some() {
        flags |= GROUP_POSITION;
    }
    if record.vendor_price.is_some() {
        flags |= GROUP_VENDOR;
    }
    if record.craft.is_some() {
        flags |= GROUP_CRAFT;
    }
    if record.deal.is_some() {
        flags |= GROUP_DEAL;
    }
    pack::put_varint(&mut out, flags);

    let mut span_mask = 0;
    for (bit, span) in SPAN_BITS {
        if record.spans.contains_key(&span) {
            span_mask |= bit;
        }
    }
    pack::put_varint(&mut out, span_mask);
    for (_, span) in SPAN_BITS {
        if let Some(block) = record.spans.get(&span) {
            put_span(&mut out, block);
        }
    }

    if let Some(live) = &record.live {
        put_live(&mut out, live);
    }
    if let Some(value) = &record.value {
        put_value(&mut out, value);
    }
    put_evidence(&mut out, record);
    put_position(&mut out, record);
    if let Some(price) = record.vendor_price {
        put_uint(&mut out, price);
    }
    if let Some(craft) = &record.craft {
        put_uint(&mut out, craft.cost);
        put_signed(&mut out, craft.profit);
        put_signed(&mut out, craft.expected_profit);
    }
    if let Some(deal) = &record.deal {
        pack::put_varint(&mut out, u64::from(deal.rank));
        pack::put_varint(&mut out, u64::from(deal.score_bp));
        pack::put_varint(&mut out, u64::from(deal.discount_bp));
    }
    pack::to_text(&out)
}

/// One span block: its flag word, the twelve fixed fields, then the optional
/// five in flag order.
fn put_span(out: &mut Vec<u8>, block: &MetricBlock) {
    let sale_rate = rate_bp(block.sale_rate);
    let sale_rate_180d = rate_bp(block.sale_rate_180d);
    let mut sflags = 0;
    if sale_rate.is_some() {
        sflags |= SFLAG_SALE_RATE;
    }
    if sale_rate_180d.is_some() {
        sflags |= SFLAG_SALE_RATE_180D;
    }
    if block.days_since_last_sale.is_some() {
        sflags |= SFLAG_DAYS_SINCE_LAST_SALE;
    }
    if block.closures_since_last_sale.is_some() {
        sflags |= SFLAG_CLOSURES_SINCE_LAST_SALE;
    }
    if block.qty_sold.is_some() {
        sflags |= SFLAG_QTY_SOLD;
    }
    pack::put_varint(out, sflags);

    for value in [
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
        block.listing_count,
    ] {
        put_uint(out, value);
    }

    if let Some(bp) = sale_rate {
        pack::put_varint(out, u64::from(bp));
    }
    if let Some(bp) = sale_rate_180d {
        pack::put_varint(out, u64::from(bp));
    }
    if let Some(days) = block.days_since_last_sale {
        put_uint(out, days);
    }
    if let Some(closures) = block.closures_since_last_sale {
        put_uint(out, closures);
    }
    if let Some(units) = block.qty_sold {
        put_uint(out, units);
    }
}

/// The `live` group: the listing summary, then what was seen and is gone,
/// then how long ago the item was last listed.
fn put_live(out: &mut Vec<u8>, live: &LiveQuote) {
    let mut lflags = 0;
    if live.listed.is_some() {
        lflags |= LFLAG_LISTED;
    }
    if live.listed.as_ref().is_some_and(|listed| listed.qty_sum.is_some()) {
        lflags |= LFLAG_QTY_SUM;
    }
    if live.seen.is_some() {
        lflags |= LFLAG_SEEN;
    }
    if live.days_since_listed.is_some() {
        lflags |= LFLAG_DAYS_LISTED;
    }
    pack::put_varint(out, lflags);

    if let Some(listed) = &live.listed {
        put_uint(out, listed.floor);
        put_uint(out, listed.floor_qty);
        put_uint(out, listed.listed);
        if let Some(qty_sum) = listed.qty_sum {
            put_uint(out, qty_sum);
        }
    }
    if let Some(seen) = &live.seen {
        put_uint(out, seen.floor);
        put_uint(out, seen.qty);
    }
    if let Some(days) = live.days_since_listed {
        put_uint(out, i64::from(days));
    }
}

/// The `value` group: its flag word, then value, sold median, samples, trend.
fn put_value(out: &mut Vec<u8>, value: &ValueQuote) {
    let mut vflags = 0;
    if value.sold_median14.is_some() {
        vflags |= VFLAG_SOLD_MEDIAN14;
    }
    if value.trend_bp.is_some() {
        vflags |= VFLAG_TREND;
    }
    pack::put_varint(out, vflags);

    put_uint(out, value.value7);
    if let Some(median) = value.sold_median14 {
        put_uint(out, median);
    }
    put_uint(out, value.samples7);
    if let Some(trend) = value.trend_bp {
        put_signed(out, trend);
    }
}

fn put_evidence(out: &mut Vec<u8>, record: &ItemRecord) {
    let mut eflags = 0;
    if record.sold_count.is_some() {
        eflags |= EFLAG_SOLD_COUNT;
    }
    if record.expired_count.is_some() {
        eflags |= EFLAG_EXPIRED_COUNT;
    }
    if record.supply_days.is_some() {
        eflags |= EFLAG_SUPPLY_DAYS;
    }
    if eflags == 0 {
        return;
    }
    pack::put_varint(out, eflags);
    if let Some(sold) = record.sold_count {
        put_uint(out, sold);
    }
    if let Some(expired) = record.expired_count {
        put_uint(out, expired);
    }
    if let Some(days) = record.supply_days {
        put_uint(out, i64::from(days));
    }
}

fn put_position(out: &mut Vec<u8>, record: &ItemRecord) {
    let mut pflags = 0;
    if record.low30.is_some() {
        pflags |= PFLAG_LOW30;
    }
    if record.low_rank_30d_bp.is_some() {
        pflags |= PFLAG_LOW_RANK;
    }
    if record.lows.is_some() {
        pflags |= PFLAG_LOWS;
    }
    if pflags == 0 {
        return;
    }
    pack::put_varint(out, pflags);
    if let Some(low) = record.low30 {
        put_uint(out, low);
    }
    if let Some(rank) = record.low_rank_30d_bp {
        put_uint(out, i64::from(rank));
    }
    if let Some(lows) = &record.lows {
        // The count is a `Vec` length: bounded by memory, so it always fits.
        put_uint(out, i64::try_from(lows.len()).unwrap_or(i64::MAX));
        out.extend_from_slice(lows);
    }
}

// ------------------------------------------------------------------- reading

/// Unpacks a record from its printable word.
///
/// # Errors
///
/// Returns [`PackError`] for every malformed record and never panics: a
/// character outside the alphabet, wrong padding, a field cut short, bytes
/// after the last field, a value above `2^53 - 1` or above its field's range,
/// a flag bit with no meaning, an announced group with nothing in it, more
/// than 30 lows, or no span at all.
pub fn decode(text: &str) -> Result<ItemRecord, PackError> {
    let bytes = pack::from_text(text)?;
    let mut reader = Reader::new(&bytes);
    let flags = reader.flag("flags", GROUP_MASK)?;
    let span_mask = reader.flag("spans", SPAN_MASK)?;
    if span_mask == 0 {
        return Err(PackError::NoSpans);
    }

    let mut spans = BTreeMap::new();
    for (bit, span) in SPAN_BITS {
        if span_mask & bit != 0 {
            spans.insert(span, read_span(&mut reader)?);
        }
    }

    let live = if flags & GROUP_LIVE != 0 { Some(read_live(&mut reader)?) } else { None };
    let value = if flags & GROUP_VALUE != 0 { Some(read_value(&mut reader)?) } else { None };
    let (sold_count, expired_count, supply_days) =
        if flags & GROUP_EVIDENCE != 0 { read_evidence(&mut reader)? } else { (None, None, None) };
    let (low30, low_rank_30d_bp, lows) =
        if flags & GROUP_POSITION != 0 { read_position(&mut reader)? } else { (None, None, None) };
    let vendor_price =
        if flags & GROUP_VENDOR != 0 { Some(reader.uint("vendor.vendorPrice")?) } else { None };
    let craft = if flags & GROUP_CRAFT != 0 { Some(read_craft(&mut reader)?) } else { None };
    let deal = if flags & GROUP_DEAL != 0 { Some(read_deal(&mut reader)?) } else { None };

    reader.finish()?;
    Ok(ItemRecord {
        spans,
        live,
        value,
        sold_count,
        expired_count,
        supply_days,
        low30,
        low_rank_30d_bp,
        lows,
        vendor_price,
        craft,
        deal,
    })
}

fn read_span(reader: &mut Reader<'_>) -> Result<MetricBlock, PackError> {
    let sflags = reader.flag("span.sflags", SFLAG_MASK)?;
    let mut block = MetricBlock {
        n: reader.uint("span.n")?,
        min: reader.uint("span.min")?,
        p05: reader.uint("span.p05")?,
        p25: reader.uint("span.p25")?,
        median: reader.uint("span.median")?,
        p75: reader.uint("span.p75")?,
        p95: reader.uint("span.p95")?,
        max: reader.uint("span.max")?,
        mad: reader.uint("span.mad")?,
        vwap: reader.uint("span.vwap")?,
        qty_sum: reader.uint("span.qtySum")?,
        listing_count: reader.uint("span.listingCount")?,
        sale_rate: None,
        sale_rate_180d: None,
        days_since_last_sale: None,
        closures_since_last_sale: None,
        qty_sold: None,
    };
    if sflags & SFLAG_SALE_RATE != 0 {
        block.sale_rate = Some(rate_from_bp(reader.uint::<u64>("span.saleRate")?));
    }
    if sflags & SFLAG_SALE_RATE_180D != 0 {
        block.sale_rate_180d = Some(rate_from_bp(reader.uint::<u64>("span.saleRate180d")?));
    }
    if sflags & SFLAG_DAYS_SINCE_LAST_SALE != 0 {
        block.days_since_last_sale = Some(reader.uint("span.daysSinceLastSale")?);
    }
    if sflags & SFLAG_CLOSURES_SINCE_LAST_SALE != 0 {
        block.closures_since_last_sale = Some(reader.uint("span.closuresSinceLastSale")?);
    }
    if sflags & SFLAG_QTY_SOLD != 0 {
        block.qty_sold = Some(reader.uint("span.qtySold")?);
    }
    Ok(block)
}

fn read_live(reader: &mut Reader<'_>) -> Result<LiveQuote, PackError> {
    let lflags = reader.flag("live.lflags", LFLAG_MASK)?;
    // `qtySum` describes the listing summary: without `listed` it reads nothing.
    if lflags & LFLAG_QTY_SUM != 0 && lflags & LFLAG_LISTED == 0 {
        return Err(PackError::UnknownFlag { field: "live.lflags", bits: LFLAG_QTY_SUM });
    }
    let listed = if lflags & LFLAG_LISTED != 0 {
        let floor = reader.uint("live.floor")?;
        let floor_qty = reader.uint("live.floorQty")?;
        let listed = reader.uint("live.listed")?;
        let qty_sum =
            if lflags & LFLAG_QTY_SUM != 0 { Some(reader.uint("live.qtySum")?) } else { None };
        Some(LiveListing { floor, floor_qty, listed, qty_sum })
    } else {
        None
    };
    let seen = if lflags & LFLAG_SEEN != 0 {
        Some(SeenListing {
            floor: reader.uint("live.seenFloor")?,
            qty: reader.uint("live.seenQty")?,
        })
    } else {
        None
    };
    let days_since_listed = if lflags & LFLAG_DAYS_LISTED != 0 {
        Some(reader.uint::<u16>("live.daysSinceListed")?)
    } else {
        None
    };
    Ok(LiveQuote { listed, seen, days_since_listed })
}

fn read_value(reader: &mut Reader<'_>) -> Result<ValueQuote, PackError> {
    let vflags = reader.flag("value.vflags", VFLAG_MASK)?;
    let value7 = reader.uint("value.value7")?;
    let sold_median14 = if vflags & VFLAG_SOLD_MEDIAN14 != 0 {
        Some(reader.uint("value.soldMedian14")?)
    } else {
        None
    };
    let samples7 = reader.uint("value.samples7")?;
    let trend_bp =
        if vflags & VFLAG_TREND != 0 { Some(reader.zigzag("value.trendBp")?) } else { None };
    Ok(ValueQuote { value7, samples7, sold_median14, trend_bp })
}

type Evidence = (Option<i64>, Option<i64>, Option<u16>);

fn read_evidence(reader: &mut Reader<'_>) -> Result<Evidence, PackError> {
    let eflags = reader.flag("evidence.eflags", EFLAG_MASK)?;
    // The encoder leaves the whole group out when nothing is set, so a zero
    // flag word cannot be its output.
    if eflags == 0 {
        return Err(PackError::EmptyGroup { field: "evidence.eflags" });
    }
    let sold_count = if eflags & EFLAG_SOLD_COUNT != 0 {
        Some(reader.uint("evidence.soldCount")?)
    } else {
        None
    };
    let expired_count = if eflags & EFLAG_EXPIRED_COUNT != 0 {
        Some(reader.uint("evidence.expiredCount")?)
    } else {
        None
    };
    let supply_days = if eflags & EFLAG_SUPPLY_DAYS != 0 {
        Some(reader.uint("evidence.supplyDays")?)
    } else {
        None
    };
    Ok((sold_count, expired_count, supply_days))
}

type Position = (Option<i64>, Option<u16>, Option<Vec<u8>>);

fn read_position(reader: &mut Reader<'_>) -> Result<Position, PackError> {
    let pflags = reader.flag("position.pflags", PFLAG_MASK)?;
    // As the evidence group: the encoder writes no empty position group.
    if pflags == 0 {
        return Err(PackError::EmptyGroup { field: "position.pflags" });
    }
    let low30 = if pflags & PFLAG_LOW30 != 0 { Some(reader.uint("position.low30")?) } else { None };
    let low_rank_30d_bp =
        if pflags & PFLAG_LOW_RANK != 0 { Some(reader.uint("position.lowRank30d")?) } else { None };
    let lows = if pflags & PFLAG_LOWS != 0 { Some(read_lows(reader)?) } else { None };
    Ok((low30, low_rank_30d_bp, lows))
}

/// Reads the sparkline: its count, then one byte per day. The count is
/// untrusted, so it is checked against its ceiling before a single byte is read.
fn read_lows(reader: &mut Reader<'_>) -> Result<Vec<u8>, PackError> {
    let count = reader.varint("position.lows")?;
    if count > MAX_LOWS {
        return Err(PackError::OutOfRange { field: "position.lows", value: count });
    }
    let mut lows = Vec::new();
    for _ in 0..count {
        lows.push(reader.byte("position.lows")?);
    }
    Ok(lows)
}

fn read_craft(reader: &mut Reader<'_>) -> Result<CraftQuote, PackError> {
    Ok(CraftQuote {
        cost: reader.uint("craft.cost")?,
        profit: reader.zigzag("craft.profit")?,
        expected_profit: reader.zigzag("craft.expectedProfit")?,
    })
}

fn read_deal(reader: &mut Reader<'_>) -> Result<DealMark, PackError> {
    Ok(DealMark {
        rank: reader.uint("deal.rank")?,
        score_bp: reader.uint("deal.scoreBp")?,
        discount_bp: reader.uint("deal.discountBp")?,
    })
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap: fixture-driven, deterministic inputs.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    /// A span block whose optional fields are present exactly where `sflags`
    /// says, holding zeros where present: a present zero must stay present.
    fn block(sflags: u64) -> MetricBlock {
        MetricBlock {
            n: 58,
            min: 575,
            p05: 600,
            p25: 1575,
            median: 15000,
            p75: 19985,
            p95: 50100,
            max: 50100,
            mad: 5100,
            vwap: 15912,
            qty_sum: 1234,
            listing_count: 618,
            sale_rate: (sflags & SFLAG_SALE_RATE != 0).then_some(0.8889),
            sale_rate_180d: (sflags & SFLAG_SALE_RATE_180D != 0).then_some(0.5),
            days_since_last_sale: (sflags & SFLAG_DAYS_SINCE_LAST_SALE != 0).then_some(0),
            closures_since_last_sale: (sflags & SFLAG_CLOSURES_SINCE_LAST_SALE != 0).then_some(61),
            qty_sold: (sflags & SFLAG_QTY_SOLD != 0).then_some(0),
        }
    }

    /// The spans whose bits are set in `mask` (bit 0 = d1).
    fn spans(mask: u8, sflags: u64) -> BTreeMap<Span, MetricBlock> {
        let mut spans = BTreeMap::new();
        for (index, span) in Span::ALL.iter().enumerate() {
            if mask & (1 << index) != 0 {
                spans.insert(*span, block(sflags));
            }
        }
        spans
    }

    fn spans_only(mask: u8, sflags: u64) -> ItemRecord {
        ItemRecord {
            spans: spans(mask, sflags),
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

    /// Every group and every flag, with distinct values so a swapped field
    /// cannot round-trip unnoticed.
    fn kitchen_sink() -> ItemRecord {
        let mut spans = BTreeMap::new();
        spans.insert(Span::D1, MetricBlock { n: 618, qty_sold: Some(9), ..block(0b11111) });
        spans.insert(Span::D7, MetricBlock { n: 1305, qty_sold: Some(31), ..block(0b11111) });
        spans.insert(
            Span::D14,
            MetricBlock {
                n: 2612,
                qty_sold: Some(63),
                closures_since_last_sale: None,
                ..block(0b01111)
            },
        );
        spans.insert(
            Span::D30,
            MetricBlock { n: 5480, qty_sold: Some(121), sale_rate_180d: None, ..block(0b10111) },
        );
        ItemRecord {
            spans,
            live: Some(LiveQuote {
                listed: Some(LiveListing {
                    floor: 3_805_000,
                    floor_qty: 214,
                    listed: 618,
                    qty_sum: Some(1234),
                }),
                seen: Some(SeenListing { floor: 3_750_000, qty: 5 }),
                // The newest snapshot lists it: last listed today.
                days_since_listed: Some(0),
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
            deal: Some(DealMark { rank: 12, score_bp: 7_100, discount_bp: 3_800 }),
        }
    }

    fn round_trip(record: &ItemRecord) -> ItemRecord {
        let text = encode(record);
        let decoded =
            decode(&text).unwrap_or_else(|e| panic!("decode refused its own output: {e}"));
        assert_eq!(encode(&decoded), text, "a decoded record re-encodes to the same word");
        decoded
    }

    #[test]
    fn full_record_round_trips() {
        let record = kitchen_sink();
        assert_eq!(round_trip(&record), record);
    }

    #[test]
    fn records_are_single_alphabet_words() {
        let text = encode(&kitchen_sink());
        assert!(text.is_ascii());
        assert_eq!(text.chars().filter(|c| !pack::ALPHABET.contains(*c)).count(), 0, "{text}");
        assert!(!text.contains(['"', '\\', '[', ']', ' ']));
        assert_eq!(decode(""), Err(PackError::Truncated { field: "flags" }));
    }

    /// One bit per decision: 2^18 records, from a bare span set to every
    /// group at once, each flag on its own and in every combination.
    fn from_bits(bits: u32) -> ItemRecord {
        let on = |bit: u32| bits & (1 << bit) != 0;
        let listed = on(0).then(|| LiveListing {
            floor: 3_805_000,
            floor_qty: 214,
            listed: 618,
            qty_sum: on(1).then_some(1234),
        });
        let seen = on(2).then_some(SeenListing { floor: 3_750_000, qty: 5 });
        let days_since_listed = on(17).then_some(12);
        let live = (listed.is_some() || seen.is_some() || on(3) || days_since_listed.is_some())
            .then_some(LiveQuote { listed, seen, days_since_listed });
        let value = on(4).then(|| ValueQuote {
            value7: 5_980_000,
            samples7: 1_305,
            sold_median14: on(5).then_some(6_137_000),
            trend_bp: on(6).then_some(-300),
        });
        let days = match (on(12), on(13)) {
            (false, _) => 0,
            (true, false) => 1,
            (true, true) => 30,
        };
        ItemRecord {
            spans: spans(0b1001, 0b10001),
            live,
            value,
            sold_count: on(7).then_some(34),
            expired_count: on(8).then_some(60),
            supply_days: on(9).then_some(274),
            low30: on(10).then_some(3_500_000),
            low_rank_30d_bp: on(11).then_some(8000),
            lows: on(12).then(|| vec![7u8; days]),
            vendor_price: on(14).then_some(125_000),
            craft: on(15).then_some(CraftQuote {
                cost: 5_550_000,
                profit: 280_150,
                expected_profit: 101_358,
            }),
            deal: on(16).then_some(DealMark { rank: 12, score_bp: 7_100, discount_bp: 3_800 }),
        }
    }

    #[test]
    fn every_flag_combination_round_trips() {
        for bits in 0..(1 << 18) {
            let record = from_bits(bits);
            assert_eq!(round_trip(&record), record, "flag bits {bits:#x}");
        }
    }

    #[test]
    fn every_span_set_and_span_flag_combination_round_trips() {
        for mask in 1..=15u8 {
            for sflags in 0..=SFLAG_MASK {
                let record = spans_only(mask, sflags);
                let decoded = round_trip(&record);
                assert_eq!(decoded, record, "spans {mask:#06b}, sflags {sflags:#07b}");
                let written: Vec<Span> = decoded.spans.keys().copied().collect();
                let expected: Vec<Span> = Span::ALL
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| mask & (1 << index) != 0)
                    .map(|(_, span)| *span)
                    .collect();
                assert_eq!(written, expected, "spans keep their order");
            }
        }
    }

    #[test]
    fn signed_fields_keep_both_signs_and_a_present_zero() {
        for trend in [-300, 0, 150] {
            for (profit, expected) in [(-125_000, -45_000), (0, 0), (190_000, 68_400)] {
                let record = ItemRecord {
                    value: Some(ValueQuote {
                        value7: 4_123_500,
                        samples7: 214,
                        sold_median14: None,
                        trend_bp: Some(trend),
                    }),
                    craft: Some(CraftQuote { cost: 2_100_000, profit, expected_profit: expected }),
                    ..spans_only(0b0001, 0)
                };
                let decoded = round_trip(&record);
                assert_eq!(decoded.value.as_ref().and_then(|v| v.trend_bp), Some(trend));
                let craft = decoded.craft.expect("craft group");
                assert_eq!((craft.profit, craft.expected_profit), (profit, expected));
            }
        }
    }

    /// Both saturating fields: a `u16` each, so their ceiling is the wire's.
    #[test]
    fn saturated_values_round_trip() {
        let record = ItemRecord {
            supply_days: Some(u16::MAX),
            low_rank_30d_bp: Some(u16::MAX),
            ..spans_only(0b0001, 0)
        };
        assert_eq!(round_trip(&record), record);
    }

    #[test]
    fn a_present_zero_is_written_never_dropped() {
        let record = ItemRecord {
            spans: BTreeMap::from([(
                Span::D1,
                MetricBlock {
                    sale_rate: Some(0.0),
                    days_since_last_sale: Some(0),
                    closures_since_last_sale: Some(0),
                    qty_sold: Some(0),
                    ..block(0)
                },
            )]),
            live: Some(LiveQuote {
                listed: Some(LiveListing { floor: 0, floor_qty: 0, listed: 0, qty_sum: Some(0) }),
                seen: Some(SeenListing { floor: 0, qty: 0 }),
                days_since_listed: Some(0),
            }),
            value: Some(ValueQuote {
                value7: 1,
                samples7: 0,
                sold_median14: Some(0),
                trend_bp: Some(0),
            }),
            sold_count: Some(0),
            expired_count: Some(0),
            supply_days: Some(0),
            low30: Some(0),
            low_rank_30d_bp: Some(0),
            lows: Some(Vec::new()),
            vendor_price: Some(0),
            craft: Some(CraftQuote { cost: 0, profit: 0, expected_profit: 0 }),
            deal: Some(DealMark { rank: 0, score_bp: 0, discount_bp: 0 }),
        };
        let decoded = round_trip(&record);
        let d1 = decoded.spans.get(&Span::D1).expect("d1");
        assert_eq!(d1.sale_rate, Some(0.0));
        assert_eq!(d1.days_since_last_sale, Some(0));
        assert_eq!(d1.closures_since_last_sale, Some(0));
        assert_eq!(d1.qty_sold, Some(0));
        assert_eq!(d1.sale_rate_180d, None, "absent stays absent");
        let live = decoded.live.expect("live group");
        assert_eq!(live.listed.as_ref().and_then(|l| l.qty_sum), Some(0));
        assert_eq!(live.seen.as_ref().map(|s| s.qty), Some(0));
        assert_eq!(live.days_since_listed, Some(0), "listed today is a present zero");
        let value = decoded.value.expect("value group");
        assert_eq!((value.sold_median14, value.trend_bp, value.samples7), (Some(0), Some(0), 0));
        assert_eq!(
            (decoded.sold_count, decoded.expired_count, decoded.supply_days),
            (Some(0), Some(0), Some(0))
        );
        assert_eq!(
            (decoded.low30, decoded.low_rank_30d_bp, decoded.vendor_price),
            (Some(0), Some(0), Some(0))
        );
        assert_eq!(decoded.lows, Some(Vec::new()));
        assert_eq!(decoded.deal, Some(DealMark { rank: 0, score_bp: 0, discount_bp: 0 }));
    }

    #[test]
    fn an_empty_live_quote_is_an_empty_group() {
        let record = ItemRecord {
            live: Some(LiveQuote { listed: None, seen: None, days_since_listed: None }),
            ..spans_only(0b0001, 0)
        };
        assert_eq!(round_trip(&record), record);
    }

    /// The ledger's closure count is a net figure that a reversal can push
    /// below zero; the wire is unsigned, so it reads as zero, never wrapped.
    #[test]
    fn negative_values_in_unsigned_fields_saturate_at_zero() {
        let record = ItemRecord {
            spans: BTreeMap::from([(
                Span::D1,
                MetricBlock {
                    min: -5,
                    days_since_last_sale: Some(-2),
                    closures_since_last_sale: Some(-9),
                    qty_sold: Some(-1),
                    ..block(0b01111)
                },
            )]),
            live: Some(LiveQuote {
                listed: Some(LiveListing {
                    floor: -1,
                    floor_qty: -2,
                    listed: -3,
                    qty_sum: Some(-4),
                }),
                seen: Some(SeenListing { floor: -6, qty: -7 }),
                // A `u16` on both sides: there is no negative to saturate.
                days_since_listed: None,
            }),
            value: Some(ValueQuote {
                value7: -8,
                samples7: -9,
                sold_median14: Some(-10),
                trend_bp: None,
            }),
            sold_count: Some(-11),
            expired_count: Some(-12),
            low30: Some(-13),
            vendor_price: Some(-14),
            craft: Some(CraftQuote { cost: -15, profit: 7, expected_profit: 7 }),
            ..spans_only(0b0001, 0)
        };
        let decoded = round_trip(&record);
        let d1 = decoded.spans.get(&Span::D1).expect("d1");
        assert_eq!(
            (d1.min, d1.days_since_last_sale, d1.closures_since_last_sale, d1.qty_sold),
            (0, Some(0), Some(0), Some(0))
        );
        let live = decoded.live.expect("live group");
        let listed = live.listed.expect("listed");
        assert_eq!(
            (listed.floor, listed.floor_qty, listed.listed, listed.qty_sum),
            (0, 0, 0, Some(0))
        );
        assert_eq!(live.seen.map(|s| (s.floor, s.qty)), Some((0, 0)));
        let value = decoded.value.expect("value group");
        assert_eq!((value.value7, value.samples7, value.sold_median14), (0, 0, Some(0)));
        assert_eq!(
            (decoded.sold_count, decoded.expired_count, decoded.low30, decoded.vendor_price),
            (Some(0), Some(0), Some(0), Some(0))
        );
        // Signed fields keep their sign.
        let craft = decoded.craft.expect("craft group");
        assert_eq!((craft.cost, craft.profit), (0, 7));
    }

    #[test]
    fn values_above_the_contract_ceiling_saturate() {
        let limit = i64::try_from(pack::MAX_INTEGER).unwrap();
        let record = ItemRecord {
            spans: BTreeMap::from([(
                Span::D1,
                MetricBlock {
                    n: i64::MAX,
                    max: limit + 1,
                    qty_sold: Some(i64::MAX),
                    ..block(0b10000)
                },
            )]),
            value: Some(ValueQuote {
                value7: i64::MAX,
                samples7: limit + 1,
                sold_median14: None,
                trend_bp: Some(i64::MAX),
            }),
            ..spans_only(0b0001, 0)
        };
        let decoded = round_trip(&record);
        let d1 = decoded.spans.get(&Span::D1).expect("d1");
        assert_eq!((d1.n, d1.max, d1.qty_sold), (limit, limit, Some(limit)));
        let value = decoded.value.expect("value group");
        assert_eq!((value.value7, value.samples7, value.trend_bp), (limit, limit, Some(limit)));
    }

    /// Rates travel as basis points, which is exactly the four decimals the
    /// v3 feed wrote: the addon reads back what it always read.
    #[test]
    fn rates_travel_as_basis_points() {
        for bp in [0u32, 1, 2_500, 3_333, 8_889, 10_000] {
            // Built the way the decoder builds it, so equality is exact.
            let rate = f64::from(bp) / 10_000.0;
            let mut record = spans_only(0b1111, 0b00011);
            for block in record.spans.values_mut() {
                block.sale_rate = Some(rate);
                block.sale_rate_180d = Some(rate);
            }
            assert_eq!(round_trip(&record), record, "rate {rate}");
        }
        // The ranking computes 34 / 94; the wire carries it to four decimals.
        let record = ItemRecord {
            spans: BTreeMap::from([(
                Span::D1,
                MetricBlock {
                    sale_rate: Some(34.0 / 94.0),
                    sale_rate_180d: Some(-0.5),
                    ..block(0b00011)
                },
            )]),
            ..spans_only(0b0001, 0)
        };
        let decoded = round_trip(&record);
        let d1 = decoded.spans.get(&Span::D1).expect("d1");
        assert_eq!(d1.sale_rate, Some(0.3617));
        assert_eq!(d1.sale_rate_180d, Some(0.0), "a negative rate saturates, it never wraps");

        // Not a number: no evidence, so absent rather than a zero rate.
        let record = ItemRecord {
            spans: BTreeMap::from([(
                Span::D1,
                MetricBlock {
                    sale_rate: Some(f64::NAN),
                    sale_rate_180d: Some(f64::INFINITY),
                    ..block(0b00011)
                },
            )]),
            ..spans_only(0b0001, 0)
        };
        let decoded = round_trip(&record);
        let d1 = decoded.spans.get(&Span::D1).expect("d1");
        assert_eq!((d1.sale_rate, d1.sale_rate_180d), (None, None));
    }

    /// The byte layout, written field by field in the contract's order with
    /// nothing but the packing primitives: flags, span mask, each span's flag
    /// word and fields, then the groups live, value, evidence, position,
    /// vendor, craft, deal.
    #[test]
    fn wire_layout_follows_the_contract() {
        let record = ItemRecord {
            spans: BTreeMap::from([(
                Span::D7,
                MetricBlock {
                    n: 1,
                    min: 2,
                    p05: 3,
                    p25: 4,
                    median: 5,
                    p75: 6,
                    p95: 7,
                    max: 8,
                    mad: 9,
                    vwap: 10,
                    qty_sum: 11,
                    listing_count: 12,
                    sale_rate: Some(0.25),
                    sale_rate_180d: None,
                    days_since_last_sale: Some(0),
                    closures_since_last_sale: None,
                    qty_sold: Some(40),
                },
            )]),
            live: Some(LiveQuote {
                listed: Some(LiveListing { floor: 100, floor_qty: 5, listed: 7, qty_sum: None }),
                seen: None,
                // Last in the live group, after everything seen gone.
                days_since_listed: Some(6),
            }),
            value: Some(ValueQuote {
                value7: 900,
                samples7: 12,
                sold_median14: Some(950),
                trend_bp: Some(-3),
            }),
            sold_count: Some(3),
            expired_count: None,
            supply_days: Some(4),
            low30: Some(800),
            low_rank_30d_bp: None,
            lows: Some(vec![1, 2]),
            vendor_price: Some(50),
            craft: Some(CraftQuote { cost: 600, profit: -60, expected_profit: -20 }),
            deal: Some(DealMark { rank: 2, score_bp: 7000, discount_bp: 300 }),
        };

        let mut expected = Vec::new();
        pack::put_varint(&mut expected, 0x7F); // every group
        pack::put_varint(&mut expected, 0x02); // d7 only
                                               // d7: sflags (saleRate, daysSinceLastSale, qtySold), the twelve fixed
                                               // fields, then the three optional ones in flag order.
        pack::put_varint(&mut expected, 0x15);
        for value in 1..=12u64 {
            pack::put_varint(&mut expected, value);
        }
        for value in [2500u64, 0, 40] {
            pack::put_varint(&mut expected, value);
        }
        // live: lflags (listed + daysSinceListed), floor, floorQty, listed, then
        // daysSinceListed, the group's last field.
        for value in [0x09u64, 100, 5, 7, 6] {
            pack::put_varint(&mut expected, value);
        }
        for value in [0x03u64, 900, 950, 12] {
            pack::put_varint(&mut expected, value); // value: vflags, value7, soldMedian14, samples7
        }
        pack::put_zigzag(&mut expected, -3); // trendBp
        for value in [0x05u64, 3, 4] {
            pack::put_varint(&mut expected, value); // evidence: eflags, soldCount, supplyDays
        }
        for value in [0x05u64, 800, 2, 1, 2] {
            pack::put_varint(&mut expected, value); // position: pflags, low30, lows count, lows
        }
        pack::put_varint(&mut expected, 50); // vendorPrice
        pack::put_varint(&mut expected, 600); // craftCost
        pack::put_zigzag(&mut expected, -60); // profit
        pack::put_zigzag(&mut expected, -20); // expectedProfit
        for value in [2u64, 7000, 300] {
            pack::put_varint(&mut expected, value); // deal: rank, scoreBp, discountBp
        }

        let text = encode(&record);
        assert_eq!(text, pack::to_text(&expected));
        assert_eq!(pack::from_text(&text), Ok(expected));
        assert_eq!(decode(&text), Ok(record));
    }

    // ---------------------------------------------------------- hostile input

    /// A record's opening: the given groups, then one all-zero d1 span.
    fn opening(groups: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        pack::put_varint(&mut bytes, groups);
        pack::put_varint(&mut bytes, 0x01);
        pack::put_varint(&mut bytes, 0);
        for _ in 0..12 {
            pack::put_varint(&mut bytes, 0);
        }
        bytes
    }

    fn decode_bytes(bytes: &[u8]) -> Result<ItemRecord, PackError> {
        decode(&pack::to_text(bytes))
    }

    #[test]
    fn every_proper_prefix_of_a_record_is_an_error() {
        let text = encode(&kitchen_sink());
        let characters: Vec<char> = text.chars().collect();
        for end in 0..characters.len() {
            let prefix: String = characters.iter().take(end).collect();
            assert!(decode(&prefix).is_err(), "prefix of {end} characters decoded");
        }
    }

    #[test]
    fn unknown_flag_bits_are_refused() {
        let cases: [(Vec<u8>, PackError); 7] = [
            (vec![0x80, 0x01], PackError::UnknownFlag { field: "flags", bits: 0x80 }),
            (vec![0x00, 0x10], PackError::UnknownFlag { field: "spans", bits: 0x10 }),
            (vec![0x00, 0x01, 0x20], PackError::UnknownFlag { field: "span.sflags", bits: 0x20 }),
            (
                [opening(GROUP_LIVE), vec![0x10]].concat(),
                PackError::UnknownFlag { field: "live.lflags", bits: 0x10 },
            ),
            (
                [opening(GROUP_VALUE), vec![0x04]].concat(),
                PackError::UnknownFlag { field: "value.vflags", bits: 0x04 },
            ),
            (
                [opening(GROUP_EVIDENCE), vec![0x08]].concat(),
                PackError::UnknownFlag { field: "evidence.eflags", bits: 0x08 },
            ),
            (
                [opening(GROUP_POSITION), vec![0x08]].concat(),
                PackError::UnknownFlag { field: "position.pflags", bits: 0x08 },
            ),
        ];
        for (bytes, expected) in cases {
            assert_eq!(decode_bytes(&bytes), Err(expected), "{bytes:02x?}");
        }
    }

    #[test]
    fn a_record_without_spans_is_refused() {
        // The encoder writes what it is given; the decoder calls it malformed.
        let empty = ItemRecord { spans: BTreeMap::new(), ..spans_only(0, 0) };
        let text = encode(&empty);
        assert_eq!(decode(&text), Err(PackError::NoSpans));
        assert_eq!(decode_bytes(&[0x00, 0x00]), Err(PackError::NoSpans));
    }

    #[test]
    fn bytes_after_the_last_field_are_refused() {
        let mut bytes = opening(0);
        bytes.push(0x00);
        assert_eq!(decode_bytes(&bytes), Err(PackError::Trailing { count: 1 }));
        // A stray zero symbol after a complete word is six more bits: with the
        // word's own padding that is a whole extra byte. Only after a word
        // that had no padding do the six bits stand alone.
        let text = encode(&spans_only(0b0001, 0));
        assert_eq!(text.len() % 4, 3, "this word pads with two bits");
        assert_eq!(decode(&format!("{text}0")), Err(PackError::Trailing { count: 1 }), "{text}");
        assert_eq!(decode("0"), Err(PackError::Padding));
    }

    #[test]
    fn wrong_padding_is_refused() {
        // 16 bytes leave four padding bits: the lowest bits of the last symbol.
        let bytes = [opening(GROUP_VENDOR), vec![0x50]].concat();
        let text = pack::to_text(&bytes);
        assert_eq!(text.len() * 6 - bytes.len() * 8, 4);
        assert!(decode(&text).is_ok(), "{text}");
        let mut corrupted: String = text.chars().take(text.chars().count() - 1).collect();
        corrupted.push('1');
        assert_eq!(decode(&corrupted), Err(PackError::Padding), "{text}");
    }

    /// `qtySum` qualifies the listing summary; on its own it is a flag without
    /// meaning, refused rather than silently dropped.
    #[test]
    fn qty_sum_without_a_listing_is_refused() {
        // listed, floor, floorQty, listed count, qtySum
        let listed = [opening(GROUP_LIVE), vec![0x03, 100, 5, 7, 9]].concat();
        assert!(decode_bytes(&listed).is_ok(), "qtySum rides with a listing");
        let orphan = [opening(GROUP_LIVE), vec![0x02]].concat();
        assert_eq!(
            decode_bytes(&orphan),
            Err(PackError::UnknownFlag { field: "live.lflags", bits: LFLAG_QTY_SUM })
        );
    }

    /// A group announced in `flags` whose flag word is zero holds nothing. The
    /// encoder leaves such a group out entirely, so the word is not an
    /// encoder's output and is refused: every accepted text re-encodes to itself.
    #[test]
    fn an_announced_group_with_nothing_in_it_is_refused() {
        let cases: [(Vec<u8>, PackError); 4] = [
            (
                [opening(GROUP_EVIDENCE), vec![0x00]].concat(),
                PackError::EmptyGroup { field: "evidence.eflags" },
            ),
            (
                [opening(GROUP_EVIDENCE | GROUP_VENDOR), vec![0x00, 0x00]].concat(),
                PackError::EmptyGroup { field: "evidence.eflags" },
            ),
            (
                [opening(GROUP_POSITION), vec![0x00]].concat(),
                PackError::EmptyGroup { field: "position.pflags" },
            ),
            (
                [opening(GROUP_POSITION | GROUP_VENDOR), vec![0x00, 0x00]].concat(),
                PackError::EmptyGroup { field: "position.pflags" },
            ),
        ];
        for (bytes, expected) in cases {
            assert_eq!(decode_bytes(&bytes), Err(expected), "{bytes:02x?}");
        }
        // One set flag is a group: a present zero sold count is still evidence.
        let present = [opening(GROUP_EVIDENCE), vec![0x01, 0x00]].concat();
        let record = decode_bytes(&present).expect("a zero sold count is evidence");
        assert_eq!(record.sold_count, Some(0));
    }

    #[test]
    fn values_beyond_their_field_are_refused() {
        let over = |groups: u64, tail: &[u8]| [opening(groups), tail.to_vec()].concat();
        let varint = |value: u64| {
            let mut out = Vec::new();
            pack::put_varint(&mut out, value);
            out
        };
        let cases: [(Vec<u8>, PackError); 7] = [
            (
                over(GROUP_LIVE, &[0x08, 0x80, 0x80, 0x04]),
                PackError::OutOfRange { field: "live.daysSinceListed", value: 65_536 },
            ),
            (
                over(GROUP_EVIDENCE, &[0x04, 0x80, 0x80, 0x04]),
                PackError::OutOfRange { field: "evidence.supplyDays", value: 65_536 },
            ),
            (
                over(GROUP_POSITION, &[0x02, 0x80, 0x80, 0x04]),
                PackError::OutOfRange { field: "position.lowRank30d", value: 65_536 },
            ),
            (
                [opening(GROUP_POSITION), vec![0x04, 0x1F], vec![0u8; 31]].concat(),
                PackError::OutOfRange { field: "position.lows", value: 31 },
            ),
            (
                [opening(GROUP_POSITION), vec![0x04], varint(1 << 40)].concat(),
                PackError::OutOfRange { field: "position.lows", value: 1 << 40 },
            ),
            (
                over(GROUP_DEAL, &varint(1 << 32)),
                PackError::OutOfRange { field: "deal.rank", value: 1 << 32 },
            ),
            (over(GROUP_VENDOR, &varint(pack::MAX_INTEGER + 1)), PackError::Overflow),
        ];
        for (bytes, expected) in cases {
            assert_eq!(decode_bytes(&bytes), Err(expected), "{bytes:02x?}");
        }
    }

    #[test]
    fn a_truncated_lows_series_is_an_error() {
        // A count of three with one byte left.
        let bytes = [opening(GROUP_POSITION), vec![0x04, 0x03, 0x80]].concat();
        assert_eq!(decode_bytes(&bytes), Err(PackError::Truncated { field: "position.lows" }));
    }

    /// Every text of up to three symbols, exhaustively: each either decodes
    /// to the record whose encoding it is, or is an error. Nothing panics.
    #[test]
    fn every_short_text_decodes_or_errors() {
        let symbols: Vec<char> = pack::ALPHABET.chars().collect();
        assert!(decode("").is_err());
        let mut accepted = 0u32;
        let mut check = |text: &str| {
            if let Ok(record) = decode(text) {
                accepted += 1;
                assert_eq!(encode(&record), text, "an accepted text is its record's encoding");
            }
        };
        for &a in &symbols {
            let one: String = [a].iter().collect();
            check(&one);
            for &b in &symbols {
                let two: String = [a, b].iter().collect();
                check(&two);
                for &c in &symbols {
                    let three: String = [a, b, c].iter().collect();
                    check(&three);
                }
            }
        }
        // Only texts whose two bytes read as flags, a span mask and nothing
        // more can decode: the span block itself needs twelve more fields.
        assert_eq!(accepted, 0, "no text of three symbols holds a span block");
    }

    /// Every one-symbol mutation of a full record: decoded output re-encodes
    /// to the mutated text, a changed accepted text is a different record,
    /// and nothing panics.
    #[test]
    fn every_single_symbol_mutation_is_handled() {
        let text = encode(&kitchen_sink());
        let original = decode(&text).expect("the record decodes");
        let characters: Vec<char> = text.chars().collect();
        let mut rejected = 0u32;
        let mut changed = 0u32;
        for (index, &replaced) in characters.iter().enumerate() {
            for replacement in pack::ALPHABET.chars() {
                let mutated: String = characters
                    .iter()
                    .enumerate()
                    .map(|(i, &c)| if i == index { replacement } else { c })
                    .collect();
                if let Ok(record) = decode(&mutated) {
                    assert_eq!(encode(&record), mutated, "accepted text at {index} is canonical");
                    if replacement == replaced {
                        assert_eq!(record, original);
                    } else {
                        changed += 1;
                        assert_ne!(record, original, "a changed word is a changed record");
                    }
                } else {
                    rejected += 1;
                    assert_ne!(replacement, replaced, "the original symbol is accepted");
                }
            }
        }
        assert!(rejected > 0 && changed > 0, "the sweep must exercise both outcomes");
    }
}

//! Database feed export (feed v4): gathers, per identity and market category,
//! everything one packed record carries — the span metric blocks plus the
//! live, value, evidence, position, vendor, craft and deal groups — and shapes
//! it into a [`DatabaseSnapshot`] (contract: `addon/SPEC.md`, sections 5 and
//! 6.3). Rendering and publication are outbound ports; this service owns the
//! selection, freshness, grain and attachment rules.
//!
//! Grain rule: the addon resolves every pet variant onto its species (the cage
//! item key), so pet records are exported per SPECIES — all variants' day-folds
//! merge into one span population and their sale rates pool. A species with
//! several variants has no single identity to read a floor, a ledger or a rank
//! for, so its record carries spans only; a single-variant species gets every
//! group.
//!
//! Facts come from the ranking's prepared scans and the crafting view, so a
//! record says exactly what the opportunities and crafts pages say. The live
//! group is the one exception: it is read from the newest snapshot directly,
//! so a floor never lags an ingest cycle behind a scan refresh.

use std::{
    collections::{
        BTreeMap,
        HashMap,
    },
    sync::Arc,
};

use crate::{
    config::{
        TargetConfig,
        TargetKind,
    },
    domain::{
        basis_points,
        saturate_u16,
        utc_day,
        Category,
        CraftQuote,
        DatabaseEntry,
        DatabaseSnapshot,
        DealMark,
        ItemRecord,
        ItemRef,
        LiveListing,
        LiveQuote,
        MarketMeta,
        MetricBlock,
        PayloadKind,
        SeenListing,
        SnapshotFloor,
        Span,
        TargetId,
        ValueQuote,
    },
    error::StoreError,
    ports::{
        ClockPort,
        PriceHistoryRepository,
        SnapshotRepository,
    },
    services::{
        craft::CraftService,
        metrics::MetricsService,
        opportunity::{
            low_rank,
            supply_days,
            ItemFacts,
            OpportunityService,
            OpportunitySort,
            ScanFacts,
        },
    },
};

/// Database feed build options.
#[derive(Clone, Debug)]
pub struct ExportOptions {
    /// The spans to include in every record.
    pub spans: Vec<Span>,
}

/// Builds database feed snapshots from cached metrics, the ranking's prepared
/// scans, the crafting view and the newest snapshots.
pub struct DatabaseExportService {
    history: Arc<dyn PriceHistoryRepository>,
    snapshots: Arc<dyn SnapshotRepository>,
    metrics: Arc<MetricsService>,
    opportunities: Arc<OpportunityService>,
    crafts: Arc<CraftService>,
    clock: Arc<dyn ClockPort>,
    /// Version written into every carrier header (diagnostics only).
    tracker_version: String,
}

/// One payload kind's newest snapshot with what it lists: the live side of the
/// export, read once per publish.
struct LiveMarket {
    /// Unix seconds of the fetch.
    fetched_at: i64,
    /// The source's `Last-Modified`, when it reported one.
    source_last_modified: Option<i64>,
    /// What the snapshot lists, per identity.
    floors: HashMap<ItemRef, SnapshotFloor>,
}

/// One feed-grain group with its span blocks: what the bulk metric paths
/// answer for it.
struct GroupBlocks {
    /// A member of the group. For a single-member group this is the identity
    /// its facts, floor, craft and rank are read under.
    member: ItemRef,
    /// How many identities folded into the group; above one, the record
    /// carries spans only.
    members: usize,
    /// Metric block per span; spans without data are absent.
    spans: BTreeMap<Span, MetricBlock>,
}

impl DatabaseExportService {
    /// Wires the service to its ports and sibling services. `metrics` is
    /// shared with the query side so export hits the same cache; `crafts` and
    /// `opportunities` are the services behind the crafts and opportunities
    /// pages, so the feed and the pages price an item identically.
    pub fn new(
        history: Arc<dyn PriceHistoryRepository>,
        snapshots: Arc<dyn SnapshotRepository>,
        metrics: Arc<MetricsService>,
        opportunities: Arc<OpportunityService>,
        crafts: Arc<CraftService>,
        clock: Arc<dyn ClockPort>,
        tracker_version: impl Into<String>,
    ) -> Self {
        Self {
            history,
            snapshots,
            metrics,
            opportunities,
            crafts,
            clock,
            tracker_version: tracker_version.into(),
        }
    }

    /// Builds the database snapshot of one target.
    ///
    /// `generated_at` is the newest source `Last-Modified` across the target's
    /// payload kinds (falling back to publish time when unknown), so the
    /// addon's 24-hour freshness gate reflects actual data age, not export
    /// time. `auctions_at` / `commodities_at` are the fetch times of the newest
    /// snapshot of each kind.
    ///
    /// Records are per (category, feed grain): an item trading in both auction
    /// houses exports twice, once per category, each on its own evidence. All
    /// span blocks are computed through the bulk paths — one cache / fold /
    /// sale-rate pass per span chunk per category — and every other group is
    /// read from the cached prepared scans, so a whole-market publish costs a
    /// handful of chunked queries plus the two floor reads of the newest
    /// snapshots.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when snapshot metadata, the prepared
    /// scans, the crafting view or a metric block cannot be read.
    pub fn build(
        &self,
        target: &TargetConfig,
        options: &ExportOptions,
    ) -> Result<DatabaseSnapshot, StoreError> {
        let id = &target.id;
        let now = self.clock.now();
        let today = utc_day(now);
        let window_days = options.spans.iter().map(Span::days).max().unwrap_or(1);

        let commodities = self.read_live(id, PayloadKind::Commodities)?;
        let auctions = self.read_live(id, PayloadKind::Auctions)?;
        let live_of = |category: Category| match category.payload_kind() {
            PayloadKind::Commodities => commodities.as_ref(),
            PayloadKind::Auctions => auctions.as_ref(),
        };
        let generated_at = [&commodities, &auctions]
            .into_iter()
            .flatten()
            .filter_map(|market| market.source_last_modified)
            .filter(|last_modified| *last_modified > 0)
            .max()
            .unwrap_or(now);
        let crafts = self.craft_quotes(id)?;

        let mut entries = Vec::new();
        for category in Category::ALL {
            let items = self.history.items_with_data(id, today - window_days + 1, category)?;
            if items.is_empty() {
                continue;
            }
            let groups = group_blocks(
                &self.metrics,
                id,
                category,
                group_by_feed_grain(items),
                &options.spans,
            )?;
            if groups.is_empty() {
                continue;
            }
            // One prepared scan per category: facts and ranks read from the
            // same view describe one market state.
            let facts = self.opportunities.facts(id, category)?;
            let deals = self.deals(&facts, category);
            let live = live_of(category);
            let lists_anything = live.is_some_and(|market| !market.floors.is_empty());
            for group in groups {
                let record = if group.members == 1 {
                    attach(
                        group.spans,
                        facts.get(&group.member),
                        live.and_then(|market| market.floors.get(&group.member)).copied(),
                        lists_anything,
                        today,
                        crafts.get(&(group.member.clone(), category)).copied(),
                        deals.get(&group.member).copied(),
                    )
                } else {
                    // A species pooled from several variants has no single
                    // identity to read anything but spans for.
                    spans_only(group.spans)
                };
                entries.push(DatabaseEntry {
                    item: canonical_item(&group.member),
                    category,
                    record,
                });
            }
        }
        entries.sort_by(|a, b| a.item.cmp(&b.item).then_with(|| a.category.cmp(&b.category)));
        Ok(DatabaseSnapshot {
            target: id.clone(),
            market: market_meta(target),
            tracker_version: self.tracker_version.clone(),
            generated_at,
            auctions_at: auctions.as_ref().map(|market| market.fetched_at),
            commodities_at: commodities.as_ref().map(|market| market.fetched_at),
            entries,
        })
    }

    /// The newest snapshot of one payload kind with what it lists; `None` when
    /// the target has no snapshot of that kind.
    fn read_live(
        &self,
        target: &TargetId,
        kind: PayloadKind,
    ) -> Result<Option<LiveMarket>, StoreError> {
        let Some(meta) = self.snapshots.latest(target, kind)? else {
            return Ok(None);
        };
        Ok(Some(LiveMarket {
            fetched_at: meta.fetched_at,
            source_last_modified: meta.source_last_modified,
            floors: self.snapshots.floors(target, kind, meta.id)?,
        }))
    }

    /// The craft group of every ranked recipe's output, keyed by output and
    /// the category the craft was priced in. `ranked` sorts by expected profit,
    /// so the first row of an output is the recipe its record carries.
    fn craft_quotes(
        &self,
        target: &TargetId,
    ) -> Result<HashMap<(ItemRef, Category), CraftQuote>, StoreError> {
        let mut quotes: HashMap<(ItemRef, Category), CraftQuote> = HashMap::new();
        for row in self.crafts.crafts(target)?.ranked {
            let Some(output) = row.output else {
                continue;
            };
            let (Some(cost), Some(profit), Some(expected)) = (row.cost, row.profit, row.expected)
            else {
                continue;
            };
            quotes.entry((row.recipe.item, output.category)).or_insert(CraftQuote {
                cost,
                profit,
                expected_profit: rounded(expected),
            });
        }
        Ok(quotes)
    }

    /// The category's live deals with their rank: the order the ranking sorts
    /// them in by score is the rank, from 1.
    fn deals(&self, facts: &ScanFacts, category: Category) -> HashMap<ItemRef, DealMark> {
        self.opportunities
            .opportunities_in(facts, category, 0.0, usize::MAX, OpportunitySort::Score)
            .into_iter()
            .enumerate()
            .map(|(index, deal)| {
                (
                    deal.item.clone(),
                    DealMark {
                        rank: u32::try_from(index + 1).unwrap_or(u32::MAX),
                        score_bp: basis_points(deal.score),
                        discount_bp: basis_points(deal.discount),
                    },
                )
            })
            .collect()
    }
}

/// Groups flat identities by feed grain and computes every group's span
/// blocks through the cached bulk paths.
///
/// Groups split by computation path, preserving group order: single-member
/// groups go through the cached per-item bulk path, multi-member populations
/// through the merged bulk path (folds live under the original variant
/// identity, not the canonical grain). The bulk paths are span-major (one
/// full-market pass per span), but each group needs its own blocks across all
/// spans — so every group gets exactly one full-width row and the stitch
/// cannot under- or over-run.
fn group_blocks(
    metrics: &MetricsService,
    target: &TargetId,
    category: Category,
    groups: BTreeMap<PetGroupKey, Vec<ItemRef>>,
    spans: &[Span],
) -> Result<Vec<GroupBlocks>, StoreError> {
    let mut singles: Vec<ItemRef> = Vec::new();
    let mut populations: Vec<Vec<ItemRef>> = Vec::new();
    for members in groups.values() {
        match members.first() {
            Some(only) if members.len() == 1 => singles.push(only.clone()),
            _ => populations.push(members.clone()),
        }
    }

    // Per-span block matrices: outer index = span, inner = group.
    let single_blocks = spans
        .iter()
        .map(|span| metrics.blocks(target, &singles, category, *span))
        .collect::<Result<Vec<_>, _>>()?;
    let population_refs: Vec<&[ItemRef]> = populations.iter().map(Vec::as_slice).collect();
    let merged_blocks = spans
        .iter()
        .map(|span| metrics.blocks_merged(target, &population_refs, category, *span))
        .collect::<Result<Vec<_>, _>>()?;

    // Transpose to per-group rows: row i = [block(i, span_0), ..,
    // block(i, span_n)].
    let single_rows: Vec<Vec<Option<MetricBlock>>> = (0..singles.len())
        .map(|i| single_blocks.iter().map(|row| row.get(i).and_then(Clone::clone)).collect())
        .collect();
    let merged_rows: Vec<Vec<Option<MetricBlock>>> = (0..populations.len())
        .map(|i| merged_blocks.iter().map(|row| row.get(i).and_then(Clone::clone)).collect())
        .collect();

    // Stitch: walk the groups in the same order the split ran, taking each
    // single group's row off `single_rows` and each population group's row off
    // `merged_rows`. Both cursors hold exactly one full-width row per matching
    // group, so they cannot under-run.
    let mut single_rows = single_rows.into_iter();
    let mut merged_rows = merged_rows.into_iter();
    let mut out = Vec::with_capacity(groups.len());
    for members in groups.into_values() {
        let member = members
            .first()
            .cloned()
            .ok_or_else(|| StoreError::Backend("feed group lost its members".into()))?;
        let rows = if members.len() == 1 {
            single_rows
                .next()
                .ok_or_else(|| StoreError::Backend("feed single group lost its row".into()))?
        } else {
            merged_rows
                .next()
                .ok_or_else(|| StoreError::Backend("feed population group lost its row".into()))?
        };
        let mut blocks = BTreeMap::new();
        for (span, block) in spans.iter().zip(rows) {
            if let Some(block) = block {
                blocks.insert(*span, block);
            }
        }
        // A record without any span would be rejected by the addon's strict
        // decoder — omit it.
        if !blocks.is_empty() {
            out.push(GroupBlocks { member, members: members.len(), spans: blocks });
        }
    }
    Ok(out)
}

/// Assembles the record of one single identity: its span blocks plus every
/// group its facts, its live listing, its recipe and its rank support. An
/// optional group is absent, never zero, without the evidence behind it.
/// `today` is the UTC day number the export dates the last listing against.
fn attach(
    spans: BTreeMap<Span, MetricBlock>,
    facts: Option<ItemFacts<'_>>,
    floor: Option<SnapshotFloor>,
    snapshot_lists_anything: bool,
    today: i64,
    craft: Option<CraftQuote>,
    deal: Option<DealMark>,
) -> ItemRecord {
    // The live group, straight from the newest snapshot: the stored `p05`
    // travels exactly as read, absent when the row predates the column.
    let listed = floor.map(|floor| LiveListing {
        floor: floor.floor_price,
        floor_qty: floor.floor_qty.max(0),
        listed: i64::try_from(floor.listing_count).unwrap_or(i64::MAX),
        qty_sum: (floor.qty_sum > 0).then_some(floor.qty_sum),
        p05: floor.p05,
    });
    // Read off the facts before the sighting below consumes them.
    let last_listed_day = facts.as_ref().and_then(|facts| facts.last_listed_day);
    // Seen today and gone: today's cumulative fold remembers a listing the
    // newest snapshot no longer lists. An equal floor is that listing, still
    // up; and an empty snapshot (a maintenance dump) proves nothing is gone.
    let seen = facts
        .and_then(|facts| facts.seen_today)
        .filter(|(seen_floor, _)| {
            *seen_floor > 0
                && snapshot_lists_anything
                && listed.as_ref().is_none_or(|listed| *seen_floor < listed.floor)
        })
        .map(|(floor, qty)| SeenListing { floor, qty: qty.max(0) });
    // How long ago the item was last listed: today for anything the newest
    // snapshot lists, else the age of the hour fold's newest day for it. A day
    // past today (the clock moved back under the fold) reads as today, never
    // as a negative age; the count is a month at most, so the cast holds.
    let days_since_listed =
        if listed.is_some() { Some(0) } else { last_listed_day.map(|day| (today - day).max(0)) }
            .map(|days| u16::try_from(days).unwrap_or(u16::MAX));
    let live = (listed.is_some() || seen.is_some() || days_since_listed.is_some())
        .then_some(LiveQuote { listed, seen, days_since_listed });

    let facts = facts.unwrap_or_default();
    let value = (facts.value7 > 0).then(|| ValueQuote {
        value7: facts.value7,
        samples7: facts.samples7.max(0),
        sold_median14: facts.sold_median14.filter(|median| *median > 0),
        trend_bp: trend_bp(facts.median_now, facts.median_prev),
    });
    // The position group needs the value group (the sparkline is scaled
    // against the reference) and at least one daily low.
    let position = value
        .as_ref()
        .map(|value| value.sold_median14.unwrap_or(value.value7))
        .filter(|_| !facts.lows.is_empty());

    ItemRecord {
        spans,
        live,
        value,
        sold_count: facts.counts.map(|counts| counts.sold),
        expired_count: facts.counts.map(|counts| counts.expired),
        supply_days: floor
            .and_then(|floor| supply_days(floor.qty_sum, facts.sold_units))
            .map(saturate_u16),
        low30: position.and_then(|_| facts.lows.iter().min().copied()),
        low_rank_30d_bp: position
            .and(floor)
            .and_then(|floor| low_rank(facts.lows, floor.floor_price))
            .map(|rank| saturate_u16(rank * 10_000.0)),
        lows: position.map(|reference| sparkline(facts.lows, reference)),
        vendor_price: facts.vendor_price.filter(|price| *price > 0),
        craft,
        deal,
    }
}

/// The record of a species pooled from several variants: spans only.
fn spans_only(spans: BTreeMap<Span, MetricBlock>) -> ItemRecord {
    ItemRecord {
        spans,
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

/// The market identity a target's configuration declares, mirrored into every
/// carrier header.
fn market_meta(target: &TargetConfig) -> MarketMeta {
    let (region, ruleset_id, house_id, connected_realm_id) = match &target.kind {
        TargetKind::BlizzardConnectedRealm { region, connected_realm_id } => {
            (Some(region.clone()), None, None, Some(*connected_realm_id))
        }
        TargetKind::BlizzardForever { region, ruleset_id, house_id } => {
            (Some(region.clone()), Some(*ruleset_id), Some(*house_id), None)
        }
        TargetKind::StaticUrls { .. } => (None, None, None, None),
    };
    MarketMeta { region, ruleset_id, house_id, connected_realm_id, flavor: target.flavor.clone() }
}

/// Today's listing median against the days before it, in basis points (signed).
/// `None` without a fold today, without a baseline, or for a ratio that is not
/// a number.
fn trend_bp(median_now: i64, median_prev: f64) -> Option<i64> {
    if median_now <= 0 || !median_prev.is_finite() || median_prev <= 0.0 {
        return None;
    }
    // Copper prices into a display ratio only: auction-scale magnitudes sit
    // far inside f64's exact-integer range.
    #[allow(clippy::cast_precision_loss)]
    let change = median_now as f64 / median_prev - 1.0;
    let bp = (change * 10_000.0).round();
    // The `as` cast saturates; a ratio this large is not real data anyway.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(bp as i64)
}

/// The sparkline of a month of daily lows: each day as
/// `round(low × 128 ÷ reference)` clamped to one byte, so the reference itself
/// sits at mid-scale and anything above twice it reads as the ceiling. Exact
/// integer math: a low can be any copper amount up to the contract's ceiling.
fn sparkline(lows: &[i64], reference: i64) -> Vec<u8> {
    lows.iter()
        .map(|low| {
            let scaled = (i128::from((*low).max(0)) * 128 + i128::from(reference) / 2)
                / i128::from(reference);
            u8::try_from(scaled).unwrap_or(u8::MAX)
        })
        .collect()
}

/// A money figure computed in `f64`, as whole copper (the `as` cast saturates).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rounded(value: f64) -> i64 {
    value.round() as i64
}

/// Groups flat identities by feed grain: plain items per id, all pet variants
/// of a species together (the addon resolves every pet record onto the
/// cage-species key).
fn group_by_feed_grain(items: Vec<ItemRef>) -> BTreeMap<PetGroupKey, Vec<ItemRef>> {
    let mut groups: BTreeMap<PetGroupKey, Vec<ItemRef>> = BTreeMap::new();
    for item in items {
        let key = match &item {
            ItemRef::Item { item_id } => PetGroupKey::Item(*item_id),
            ItemRef::Pet { species, .. } => PetGroupKey::PetSpecies(*species),
        };
        groups.entry(key).or_default().push(item);
    }
    groups
}

/// Feed-grain grouping key: plain items per id, all pet variants of a species
/// together (the addon resolves every pet record onto the cage-species key).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PetGroupKey {
    Item(i64),
    PetSpecies(i64),
}

/// The identity a record carries for a group. Pet groups export the
/// species-level tuple (level/quality/breed zeroed — the addon ignores them
/// and JSON consumers should see the canonical grain).
fn canonical_item(item: &ItemRef) -> ItemRef {
    match *item {
        ItemRef::Item { item_id } => ItemRef::Item { item_id },
        ItemRef::Pet { species, .. } => ItemRef::Pet { species, level: 0, quality: 0, breed: 0 },
    }
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap: fixture-driven, deterministic inputs.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        config::PollConfig,
        domain::{
            Flavor,
            SaleCounts,
        },
    };

    /// The UTC day number the attach rules date the last listing against.
    const TODAY: i64 = 20_000;

    fn block(n: i64) -> MetricBlock {
        MetricBlock {
            n,
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
            listing_count: n,
            sale_rate: Some(0.25),
            sale_rate_180d: None,
            days_since_last_sale: None,
            closures_since_last_sale: None,
            qty_sold: None,
        }
    }

    fn spans() -> BTreeMap<Span, MetricBlock> {
        let mut spans = BTreeMap::new();
        spans.insert(Span::D1, block(5));
        spans
    }

    /// A listing summary of three listings; its `p05` sits a little above the
    /// floor, distinct from every other figure here.
    fn floor(price: i64, qty: i64) -> SnapshotFloor {
        SnapshotFloor {
            floor_price: price,
            floor_qty: qty,
            listing_count: 3,
            qty_sum: 100,
            p05: Some(price + 500),
        }
    }

    /// A steady seller: a sold-source value, today's median under the week
    /// before, ledger evidence, three prior days and a vendor price. No
    /// hour-fold date for the last listing unless a test scripts one.
    fn facts() -> ItemFacts<'static> {
        ItemFacts {
            value7: 100_000,
            samples7: 40,
            sold_median14: Some(120_000),
            median_now: 90_000,
            median_prev: 100_000.0,
            seen_today: Some((90_000, 2)),
            counts: Some(SaleCounts { sold: 4, expired: 6 }),
            sold_units: Some(14),
            lows: &[150_000, 140_000, 130_000],
            last_listed_day: None,
            vendor_price: Some(500),
        }
    }

    #[test]
    fn a_listed_identity_carries_every_group_its_evidence_supports() {
        let craft = CraftQuote { cost: 50_000, profit: 60_000, expected_profit: 24_000 };
        let deal = DealMark { rank: 3, score_bp: 6_500, discount_bp: 2_500 };
        let record = attach(
            spans(),
            Some(facts()),
            Some(floor(90_000, 5)),
            true,
            TODAY,
            Some(craft),
            Some(deal),
        );

        assert_eq!(record.spans.len(), 1);
        assert_eq!(
            record.live,
            Some(LiveQuote {
                listed: Some(LiveListing {
                    floor: 90_000,
                    floor_qty: 5,
                    listed: 3,
                    qty_sum: Some(100),
                    p05: Some(90_500),
                }),
                // A sighting level with the live floor is that listing, still up.
                seen: None,
                // The newest snapshot lists it: last listed today.
                days_since_listed: Some(0),
            })
        );
        assert_eq!(
            record.value,
            Some(ValueQuote {
                value7: 100_000,
                samples7: 40,
                sold_median14: Some(120_000),
                // 90 000 today against 100 000 before it: -10 %.
                trend_bp: Some(-1_000),
            })
        );
        assert_eq!((record.sold_count, record.expired_count), (Some(4), Some(6)));
        // 100 listed ÷ (14 sold / 14 days) = 100 days of supply.
        assert_eq!(record.supply_days, Some(100));
        assert_eq!(record.low30, Some(130_000));
        // Three days of lows cannot place a price inside a month.
        assert_eq!(record.low_rank_30d_bp, None);
        // Each low × 128 ÷ the sold reference of 120 000, rounded.
        assert_eq!(record.lows.as_deref(), Some(&[160_u8, 149, 139][..]));
        assert_eq!(record.vendor_price, Some(500));
        assert_eq!(record.craft, Some(craft));
        assert_eq!(record.deal, Some(deal));
    }

    #[test]
    fn seen_today_and_gone_needs_a_snapshot_that_lists_something() {
        // Not listed any more, under a snapshot that lists other items: gone.
        let gone = attach(spans(), Some(facts()), None, true, TODAY, None, None);
        assert_eq!(
            gone.live,
            Some(LiveQuote {
                listed: None,
                seen: Some(SeenListing { floor: 90_000, qty: 2 }),
                // No hour-fold row scripted: the sighting alone dates nothing.
                days_since_listed: None,
            })
        );
        // The same sighting under an empty snapshot (a maintenance dump)
        // proves nothing is gone.
        let maintenance = attach(spans(), Some(facts()), None, false, TODAY, None, None);
        assert_eq!(maintenance.live, None);
        // Listed dearer than the sighting: the cheap one is gone, the rest up.
        let partly =
            attach(spans(), Some(facts()), Some(floor(110_000, 1)), true, TODAY, None, None);
        assert_eq!(
            partly.live,
            Some(LiveQuote {
                listed: Some(LiveListing {
                    floor: 110_000,
                    floor_qty: 1,
                    listed: 3,
                    qty_sum: Some(100),
                    p05: Some(110_500),
                }),
                seen: Some(SeenListing { floor: 90_000, qty: 2 }),
                days_since_listed: Some(0),
            })
        );
    }

    /// `daysSinceListed` follows the snapshot first, then the hour fold: today
    /// for anything the newest snapshot lists, the age of the fold's newest day
    /// for the rest, absent without a fold row in the low window — and when it
    /// is the only thing known, it is a live group of its own.
    #[test]
    fn days_since_listed_follows_the_snapshot_then_the_fold() {
        let listed_twelve_days_ago = ItemFacts { last_listed_day: Some(TODAY - 12), ..facts() };
        let listed = attach(
            spans(),
            Some(listed_twelve_days_ago),
            Some(floor(90_000, 5)),
            true,
            TODAY,
            None,
            None,
        );
        assert_eq!(
            listed.live.as_ref().map(|live| live.days_since_listed),
            Some(Some(0)),
            "the snapshot lists it: today, whatever the fold remembers"
        );

        // Not listed, not seen today, last listed twelve days ago: the live
        // group holds that date and nothing else.
        let stale = ItemFacts { seen_today: None, last_listed_day: Some(TODAY - 12), ..facts() };
        let stale = attach(spans(), Some(stale), None, true, TODAY, None, None);
        assert_eq!(
            stale.live,
            Some(LiveQuote { listed: None, seen: None, days_since_listed: Some(12) })
        );

        // Seen today and gone: the sighting already says today, the fold agrees.
        let gone = attach(
            spans(),
            Some(ItemFacts { last_listed_day: Some(TODAY), ..facts() }),
            None,
            true,
            TODAY,
            None,
            None,
        );
        assert_eq!(
            gone.live,
            Some(LiveQuote {
                listed: None,
                seen: Some(SeenListing { floor: 90_000, qty: 2 }),
                days_since_listed: Some(0),
            })
        );

        // No fold row: no date — and with nothing listed and nothing seen, no
        // live group at all.
        let undated = ItemFacts { seen_today: None, last_listed_day: None, ..facts() };
        let undated = attach(spans(), Some(undated), None, true, TODAY, None, None);
        assert_eq!(undated.live, None);

        // A fold day past today (the clock moved back under the fold) is
        // today, never a negative age.
        let ahead = ItemFacts { seen_today: None, last_listed_day: Some(TODAY + 3), ..facts() };
        let ahead = attach(spans(), Some(ahead), None, true, TODAY, None, None);
        assert_eq!(
            ahead.live,
            Some(LiveQuote { listed: None, seen: None, days_since_listed: Some(0) })
        );
    }

    #[test]
    fn an_identity_without_a_scan_row_carries_only_what_the_snapshot_knows() {
        let listed = attach(spans(), None, Some(floor(90_000, 0)), true, TODAY, None, None);
        assert_eq!(
            listed.live,
            Some(LiveQuote {
                listed: Some(LiveListing {
                    floor: 90_000,
                    floor_qty: 0,
                    listed: 3,
                    qty_sum: Some(100),
                    p05: Some(90_500),
                }),
                seen: None,
                days_since_listed: Some(0),
            })
        );
        assert!(
            listed.value.is_none()
                && listed.sold_count.is_none()
                && listed.supply_days.is_none()
                && listed.low30.is_none()
                && listed.vendor_price.is_none()
        );
        let bare = attach(spans(), None, None, true, TODAY, None, None);
        assert_eq!(bare, spans_only(spans()));
    }

    /// The live group carries the snapshot's stored `p05` exactly as read — it
    /// is not filtered the way `qtySum` is — and a row without one (written
    /// before the column) leaves it absent rather than inventing a figure.
    #[test]
    fn the_live_group_carries_the_stored_p05_and_leaves_an_absent_one_absent() {
        let listed = |floor: SnapshotFloor| {
            attach(spans(), None, Some(floor), true, TODAY, None, None)
                .live
                .and_then(|live| live.listed)
        };
        assert_eq!(listed(floor(90_000, 5)).map(|listing| listing.p05), Some(Some(90_500)));
        // A pre-column row: no p05, everything else intact.
        let legacy = listed(SnapshotFloor { p05: None, ..floor(90_000, 5) });
        assert_eq!(
            legacy,
            Some(LiveListing {
                floor: 90_000,
                floor_qty: 5,
                listed: 3,
                qty_sum: Some(100),
                p05: None,
            })
        );
        // Nothing carries quantity: `qtySum` reads as absent, `p05` does not
        // depend on it and stays.
        let dry = listed(SnapshotFloor { qty_sum: 0, ..floor(90_000, 5) });
        assert_eq!(dry.map(|listing| (listing.qty_sum, listing.p05)), Some((None, Some(90_500))));
    }

    #[test]
    fn the_position_group_needs_a_value_and_a_daily_low() {
        // Daily lows but no 7-day value: no reference to scale against.
        let valueless = attach(
            spans(),
            Some(ItemFacts { value7: 0, ..facts() }),
            Some(floor(90_000, 1)),
            true,
            TODAY,
            None,
            None,
        );
        assert!(valueless.value.is_none());
        assert_eq!(
            (valueless.low30, valueless.low_rank_30d_bp, valueless.lows.is_none()),
            (None, None, true)
        );
        // A value but no hour-fold history: the value group stands alone.
        let historyless = attach(
            spans(),
            Some(ItemFacts { lows: &[], ..facts() }),
            Some(floor(90_000, 1)),
            true,
            TODAY,
            None,
            None,
        );
        assert!(historyless.value.is_some());
        assert_eq!(
            (historyless.low30, historyless.low_rank_30d_bp, historyless.lows.is_none()),
            (None, None, true)
        );
    }

    #[test]
    fn the_30_day_rank_needs_a_week_of_lows() {
        // Five prior days, four of them above the 90 000 floor: 80 %.
        let lows = [150_000, 140_000, 130_000, 110_000, 80_000];
        let month = attach(
            spans(),
            Some(ItemFacts { lows: &lows, ..facts() }),
            Some(floor(90_000, 1)),
            true,
            TODAY,
            None,
            None,
        );
        assert_eq!(month.low_rank_30d_bp, Some(8_000));
        assert_eq!(month.low30, Some(80_000));
        // Without a live floor there is nothing to place inside the month.
        let unlisted = attach(
            spans(),
            Some(ItemFacts { lows: &lows, ..facts() }),
            None,
            true,
            TODAY,
            None,
            None,
        );
        assert_eq!((unlisted.low30, unlisted.low_rank_30d_bp), (Some(80_000), None));
    }

    #[test]
    fn days_of_supply_need_listed_quantity_and_sold_units() {
        let unlisted = attach(spans(), Some(facts()), None, true, TODAY, None, None);
        assert_eq!(unlisted.supply_days, None, "nothing listed, nothing to clear");
        let unsold = attach(
            spans(),
            Some(ItemFacts { sold_units: Some(0), ..facts() }),
            Some(floor(90_000, 1)),
            true,
            TODAY,
            None,
            None,
        );
        assert_eq!(unsold.supply_days, None, "nothing sold in a fortnight: never clears");
        // Ten million listed ÷ (1 / 14 per day) saturates at the field's ceiling.
        let glut = attach(
            spans(),
            Some(ItemFacts { sold_units: Some(1), ..facts() }),
            Some(SnapshotFloor { qty_sum: 10_000_000, ..floor(90_000, 1) }),
            true,
            TODAY,
            None,
            None,
        );
        assert_eq!(glut.supply_days, Some(u16::MAX));
    }

    #[test]
    fn the_trend_is_today_against_the_days_before() {
        assert_eq!(trend_bp(110_000, 100_000.0), Some(1_000));
        assert_eq!(trend_bp(90_000, 100_000.0), Some(-1_000));
        assert_eq!(trend_bp(100_000, 100_000.0), Some(0), "a flat week is a present zero");
        assert_eq!(trend_bp(0, 100_000.0), None, "no fold today");
        assert_eq!(trend_bp(100_000, 0.0), None, "no day before today");
        assert_eq!(trend_bp(100_000, f64::NAN), None);
    }

    #[test]
    fn the_sparkline_puts_the_reference_at_mid_scale() {
        assert_eq!(
            sparkline(&[0, 60_000, 120_000, 180_000, 240_000, 300_000], 120_000),
            vec![0, 64, 128, 192, 255, 255]
        );
        assert_eq!(sparkline(&[], 120_000), Vec::<u8>::new());
    }

    #[test]
    fn the_market_header_mirrors_the_target_kind() {
        let mut config = TargetConfig {
            id: TargetId("t".into()),
            kind: TargetKind::BlizzardForever { region: "eu".into(), ruleset_id: 1, house_id: 111 },
            auth: None,
            poll: PollConfig { commodities_secs: 3600, auctions_secs: 1800 },
            flavor: Flavor("forever".into()),
        };
        assert_eq!(
            market_meta(&config),
            MarketMeta {
                region: Some("eu".into()),
                ruleset_id: Some(1),
                house_id: Some(111),
                connected_realm_id: None,
                flavor: Flavor("forever".into()),
            }
        );
        config.kind =
            TargetKind::BlizzardConnectedRealm { region: "us".into(), connected_realm_id: 4 };
        assert_eq!(
            market_meta(&config),
            MarketMeta {
                region: Some("us".into()),
                ruleset_id: None,
                house_id: None,
                connected_realm_id: Some(4),
                flavor: Flavor("forever".into()),
            }
        );
        config.kind = TargetKind::StaticUrls {
            commodities_url: "https://example.org/c".into(),
            auctions_url: "https://example.org/a".into(),
        };
        assert_eq!(
            market_meta(&config),
            MarketMeta {
                region: None,
                ruleset_id: None,
                house_id: None,
                connected_realm_id: None,
                flavor: Flavor("forever".into()),
            }
        );
    }
}

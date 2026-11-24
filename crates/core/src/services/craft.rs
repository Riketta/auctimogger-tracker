//! Crafting analysis ("which crafts are worth running here?"): the manual
//! recipe book evaluated against one target's market. Every item — reagent or
//! output — is priced exactly as the deal ranking prices it (the
//! quantity-weighted 7-day median of the market with the most evidence, plus
//! the newest snapshot's live floor), the output is scaled by how reliably it
//! sells, and the auction-house cut comes off the top. Reads the scans the
//! scheduler already prepares, so a request costs no store read of its own
//! beyond the outputs' cached metric blocks.

use std::{
    collections::BTreeMap,
    sync::Arc,
};

use crate::{
    config::CraftsConfig,
    domain::{
        Category,
        ItemRef,
        Recipe,
        Span,
        TargetId,
    },
    error::StoreError,
    services::{
        metrics::MetricsService,
        opportunity::{
            MarketValue,
            OpportunityService,
            ValueSource,
        },
    },
};

/// Window behind the outputs' sold-per-day figure (days).
const SOLD_WINDOW_DAYS: i64 = 14;

/// One reagent of an evaluated recipe, priced.
#[derive(Clone, Debug, PartialEq)]
pub struct ReagentCost {
    /// The consumed item.
    pub item: ItemRef,
    /// Units one make consumes.
    pub qty: i64,
    /// The market the reagent was priced in (`None` = no reference price in
    /// any).
    pub category: Option<Category>,
    /// Reference unit value (copper; 0 without a price): the 14-day sold
    /// median once enough priced units sold, else the 7-day listing value.
    pub unit_value: i64,
    /// Cheapest listing in the newest snapshot (copper), when one is listed.
    pub live_unit_price: Option<i64>,
}

/// The crafted item's market facts behind a craft row.
#[derive(Clone, Debug, PartialEq)]
pub struct CraftOutput {
    /// The market the output was priced in.
    pub category: Category,
    /// Reference unit value (copper) — the price the craft is assumed to
    /// fetch: the 14-day sold median once enough priced units sold, else the
    /// 7-day listing value.
    pub unit_value: i64,
    /// The 14-day sold median behind `unit_value` when `value_source` is
    /// `Sold`.
    pub sold_median14: Option<i64>,
    /// Which value `unit_value` resolved to.
    pub value_source: ValueSource,
    /// 14-day sale rate (`None` = no credit-eligible evidence).
    pub sale_rate: Option<f64>,
    /// Units the market absorbed per day over the trailing 14 days.
    pub units_sold_per_day: Option<f64>,
}

/// Why a recipe is not ranked as an efficient craft.
#[derive(Clone, Debug, PartialEq)]
pub enum CraftIssue {
    /// A reagent has no market price in the 7-day window.
    ReagentWithoutPrice(ItemRef),
    /// The crafted item itself has no market value.
    OutputWithoutPrice,
    /// No credit-eligible close for the output: no evidence that it sells.
    NoSaleEvidence,
    /// The output's 14-day sale rate is under the configured floor (the rate
    /// is carried along).
    SlowSeller(f64),
    /// Reagents cost as much as or more than the output nets.
    NotProfitable,
}

impl CraftIssue {
    /// Wire/UI spelling of the issue.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            CraftIssue::ReagentWithoutPrice(_) => "reagent-without-price",
            CraftIssue::OutputWithoutPrice => "output-without-price",
            CraftIssue::NoSaleEvidence => "no-sale-evidence",
            CraftIssue::SlowSeller(_) => "slow-seller",
            CraftIssue::NotProfitable => "not-profitable",
        }
    }
}

/// One recipe evaluated against the target's market. The money fields are
/// per make, in copper, and `None` whenever an input they need is missing.
#[derive(Clone, Debug, PartialEq)]
pub struct CraftRow {
    /// The recipe as configured.
    pub recipe: Recipe,
    /// The reagents with their prices, in recipe order.
    pub reagents: Vec<ReagentCost>,
    /// The output's market facts (`None` = no 7-day price for it).
    pub output: Option<CraftOutput>,
    /// Σ quantity × 7-day unit value; `None` when a reagent is unpriced.
    pub cost: Option<i64>,
    /// The same at the newest snapshot's floors; `None` unless every reagent
    /// is listed right now.
    pub cost_now: Option<i64>,
    /// Output value × makes, before the house cut.
    pub gross: Option<i64>,
    /// Gross − cut − cost: what one make nets if the output sells.
    pub profit: Option<i64>,
    /// Profit × sale rate: what one make is worth given the risk it does not
    /// sell — the ranking key.
    pub expected: Option<f64>,
    /// Profit ÷ cost.
    pub roi: Option<f64>,
    /// Why the row is not ranked; `None` for an efficient craft.
    pub issue: Option<CraftIssue>,
}

/// The crafting view's answer for one target.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CraftReport {
    /// Efficient crafts, highest expected profit first.
    pub ranked: Vec<CraftRow>,
    /// The rest, in recipe order, each with the reason it did not rank.
    pub unranked: Vec<CraftRow>,
    /// The auction-house cut applied, as a fraction of the output value.
    pub cut: f64,
    /// The sale-rate floor applied.
    pub min_sale_rate: f64,
}

/// Evaluates the manual recipe book against the prepared market scans.
pub struct CraftService {
    opportunities: Arc<OpportunityService>,
    metrics: Arc<MetricsService>,
    settings: CraftsConfig,
}

impl CraftService {
    /// Wires the service to the ranking's prepared scans, the cached metric
    /// blocks and the `[crafts]` settings.
    pub fn new(
        opportunities: Arc<OpportunityService>,
        metrics: Arc<MetricsService>,
        settings: &CraftsConfig,
    ) -> Self {
        Self { opportunities, metrics, settings: settings.clone() }
    }

    /// Whether any recipe is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.settings.recipes.is_empty()
    }

    /// The configured recipe file path, for the view's provenance note.
    #[must_use]
    pub fn recipes_file(&self) -> Option<&str> {
        self.settings.recipes_file.as_deref()
    }

    /// Evaluates every recipe against one target's market.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the cold-start inline scans or the
    /// outputs' metric blocks cannot be read.
    pub fn crafts(&self, target: &TargetId) -> Result<CraftReport, StoreError> {
        let cut = self.settings.ah_cut_percent / 100.0;
        let min_sale_rate = self.settings.min_sale_rate;
        let mut report = CraftReport { cut, min_sale_rate, ..Default::default() };
        if self.settings.recipes.is_empty() {
            return Ok(report);
        }
        let priced = self.price(target)?;
        for recipe in &self.settings.recipes {
            let row = evaluate(recipe, &priced, cut, min_sale_rate);
            if row.issue.is_none() {
                report.ranked.push(row);
            } else {
                report.unranked.push(row);
            }
        }
        report.ranked.sort_by(|a, b| {
            b.expected
                .partial_cmp(&a.expected)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.recipe.item.cmp(&b.recipe.item))
        });
        Ok(report)
    }

    /// Prices every item the book mentions. Both auction houses' scans are
    /// read; each item takes the market with more 7-day evidence (ties keep
    /// the commodity house, where stackables trade) and that market's
    /// reference value — the 14-day sold median once enough priced units
    /// sold, else the 7-day listing value — and the outputs' cached 14-day
    /// blocks add how many units they sell per day.
    fn price(&self, target: &TargetId) -> Result<BTreeMap<ItemRef, PricedItem>, StoreError> {
        let commodity = self.opportunities.market(target, Category::Commodity)?;
        let item = self.opportunities.market(target, Category::Item)?;
        let mut priced: BTreeMap<ItemRef, PricedItem> = BTreeMap::new();
        let mut outputs: BTreeMap<Category, Vec<ItemRef>> = BTreeMap::new();
        for recipe in &self.settings.recipes {
            let mentioned =
                std::iter::once(&recipe.item).chain(recipe.reagents.iter().map(|r| &r.item));
            for identity in mentioned {
                if priced.contains_key(identity) {
                    continue;
                }
                let Some((category, value)) =
                    better_evidenced(commodity.get(identity), item.get(identity))
                else {
                    priced.insert(identity.clone(), PricedItem::unpriced());
                    continue;
                };
                if *identity == recipe.item {
                    outputs.entry(category).or_default().push(identity.clone());
                }
                priced.insert(
                    identity.clone(),
                    PricedItem {
                        category: Some(category),
                        unit_value: value.reference(),
                        sold_median14: value.sold_median14,
                        value_source: value.value_source(),
                        live_unit_price: value.live.map(|live| live.floor_price),
                        sale_rate: value.sale_rate,
                        units_sold_per_day: None,
                    },
                );
            }
        }
        for (category, identities) in outputs {
            let blocks = self.metrics.blocks(target, &identities, category, Span::D14)?;
            for (identity, block) in identities.iter().zip(blocks.iter().flatten()) {
                let Some(entry) = priced.get_mut(identity) else {
                    continue;
                };
                entry.sale_rate = block.sale_rate.or(entry.sale_rate);
                entry.units_sold_per_day = block.qty_sold.map(units_per_day);
            }
        }
        Ok(priced)
    }
}

/// Everything the evaluator needs about one item on the target.
#[derive(Clone, Debug, PartialEq)]
struct PricedItem {
    /// The market the item was priced in; `None` = no 7-day evidence.
    category: Option<Category>,
    /// Reference unit value (copper; 0 when unpriced).
    unit_value: i64,
    /// The sold median behind `unit_value`, when it supplied it.
    sold_median14: Option<i64>,
    /// Which value `unit_value` resolved to.
    value_source: ValueSource,
    /// Cheapest listing in the newest snapshot (copper).
    live_unit_price: Option<i64>,
    /// 14-day sale rate (`None` = no credit-eligible evidence).
    sale_rate: Option<f64>,
    /// Units the market absorbed per day over the last 14 days.
    units_sold_per_day: Option<f64>,
}

impl PricedItem {
    fn unpriced() -> Self {
        Self {
            category: None,
            unit_value: 0,
            sold_median14: None,
            value_source: ValueSource::Listed,
            live_unit_price: None,
            sale_rate: None,
            units_sold_per_day: None,
        }
    }
}

/// The market that prices an item: the one with evidence, then with more
/// evidence; an exact tie keeps the commodity house. `None` when neither
/// market has a 7-day price for it.
fn better_evidenced(
    commodity: Option<MarketValue>,
    item: Option<MarketValue>,
) -> Option<(Category, MarketValue)> {
    let has_price = |value: &MarketValue| value.value7 > 0;
    match (commodity, item) {
        (Some(c), Some(i)) => {
            let keep_commodity = !has_price(&i) || (has_price(&c) && c.samples7 >= i.samples7);
            Some(if keep_commodity { (Category::Commodity, c) } else { (Category::Item, i) })
        }
        (Some(c), None) => has_price(&c).then_some((Category::Commodity, c)),
        (None, Some(i)) => has_price(&i).then_some((Category::Item, i)),
        (None, None) => None,
    }
}

/// The house's share of a gross amount, in whole copper.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn cut_copper(gross: i64, cut: f64) -> i64 {
    (gross as f64 * cut).round() as i64
}

/// Units credited sold over the window, as a per-day figure (display math).
#[allow(clippy::cast_precision_loss)]
fn units_per_day(units: i64) -> f64 {
    units as f64 / SOLD_WINDOW_DAYS as f64
}

/// Evaluates one recipe over the priced items: the money fields in copper per
/// make, and the first reason the row does not rank (in that order of
/// severity).
// Copper feeds ranking/display math only; auction-scale amounts sit far
// inside f64's exact-integer range.
#[allow(clippy::cast_precision_loss)]
fn evaluate(
    recipe: &Recipe,
    priced: &BTreeMap<ItemRef, PricedItem>,
    cut: f64,
    min_sale_rate: f64,
) -> CraftRow {
    let mut reagents = Vec::with_capacity(recipe.reagents.len());
    let mut cost = 0i64;
    let mut cost_now = Some(0i64);
    let mut unpriced: Option<ItemRef> = None;
    for reagent in &recipe.reagents {
        let found = priced.get(&reagent.item);
        let unit_value = found.map_or(0, |entry| entry.unit_value);
        let live_unit_price = found.and_then(|entry| entry.live_unit_price);
        if unit_value > 0 {
            cost = cost.saturating_add(reagent.qty.saturating_mul(unit_value));
            cost_now = match (live_unit_price, cost_now) {
                (Some(live), Some(total)) if live > 0 => {
                    Some(total.saturating_add(reagent.qty.saturating_mul(live)))
                }
                _ => None,
            };
        } else {
            unpriced.get_or_insert(reagent.item.clone());
            cost_now = None;
        }
        reagents.push(ReagentCost {
            item: reagent.item.clone(),
            qty: reagent.qty,
            category: found.and_then(|entry| entry.category).filter(|_| unit_value > 0),
            unit_value,
            live_unit_price: live_unit_price.filter(|_| unit_value > 0),
        });
    }
    let cost = unpriced.is_none().then_some(cost);
    let output =
        priced.get(&recipe.item).filter(|entry| entry.unit_value > 0).map(|entry| CraftOutput {
            category: entry.category.unwrap_or(Category::Item),
            unit_value: entry.unit_value,
            sold_median14: entry.sold_median14,
            value_source: entry.value_source,
            sale_rate: entry.sale_rate,
            units_sold_per_day: entry.units_sold_per_day,
        });
    let gross = output.as_ref().map(|output| output.unit_value.saturating_mul(recipe.makes));
    let net = gross.map(|gross| gross - cut_copper(gross, cut));
    let profit = match (net, cost) {
        (Some(net), Some(cost)) => Some(net - cost),
        _ => None,
    };
    let expected = match (profit, output.as_ref().and_then(|output| output.sale_rate)) {
        (Some(profit), Some(rate)) => Some(profit as f64 * rate),
        _ => None,
    };
    let roi = match (profit, cost) {
        (Some(profit), Some(cost)) if cost > 0 => Some(profit as f64 / cost as f64),
        _ => None,
    };
    let issue = if let Some(missing) = unpriced {
        Some(CraftIssue::ReagentWithoutPrice(missing))
    } else if output.is_none() {
        Some(CraftIssue::OutputWithoutPrice)
    } else if let Some(output) = &output {
        match output.sale_rate {
            None => Some(CraftIssue::NoSaleEvidence),
            Some(rate) if rate < min_sale_rate => Some(CraftIssue::SlowSeller(rate)),
            Some(_) if profit.is_some_and(|profit| profit <= 0) => Some(CraftIssue::NotProfitable),
            Some(_) => None,
        }
    } else {
        None
    };
    CraftRow {
        recipe: recipe.clone(),
        reagents,
        output,
        cost,
        cost_now,
        gross,
        profit,
        expected,
        roi,
        issue,
    }
}

#[cfg(test)]
mod tests {
    // Test construction is allowed to unwrap/expect/index: fixed synthetic
    // inputs, not hostile data (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use crate::domain::Reagent;

    fn item(id: i64) -> ItemRef {
        ItemRef::Item { item_id: id }
    }

    fn recipe(output: i64, makes: i64, reagents: &[(i64, i64)]) -> Recipe {
        Recipe {
            item: item(output),
            makes,
            reagents: reagents
                .iter()
                .map(|(id, qty)| Reagent { item: item(*id), qty: *qty })
                .collect(),
        }
    }

    fn entry(
        category: Category,
        unit_value: i64,
        live: Option<i64>,
        sale_rate: Option<f64>,
    ) -> PricedItem {
        PricedItem {
            category: Some(category),
            unit_value,
            sold_median14: None,
            value_source: ValueSource::Listed,
            live_unit_price: live,
            sale_rate,
            units_sold_per_day: sale_rate.map(|_| 3.5),
        }
    }

    /// Output 1 sells at 1 000 (half its listings sell); reagents 2 and 3
    /// cost 200 and 100.
    fn priced_book() -> BTreeMap<ItemRef, PricedItem> {
        BTreeMap::from([
            (item(1), entry(Category::Commodity, 1_000, Some(900), Some(0.5))),
            (item(2), entry(Category::Commodity, 200, Some(180), None)),
            (item(3), entry(Category::Commodity, 100, Some(90), None)),
        ])
    }

    const CUT: f64 = 0.05;
    const FLOOR: f64 = 0.2;

    /// The ranked row's arithmetic: cost 400, gross 1 000, cut 50, profit 550,
    /// expected 275 at a 50% sale rate, ROI 1.375, live cost 360.
    #[test]
    fn a_profitable_selling_craft_ranks_with_its_arithmetic() {
        let row = evaluate(&recipe(1, 1, &[(2, 1), (3, 2)]), &priced_book(), CUT, FLOOR);
        assert_eq!(row.issue, None);
        assert_eq!(row.cost, Some(400));
        assert_eq!(row.cost_now, Some(360), "every reagent is listed right now");
        assert_eq!(row.gross, Some(1_000));
        assert_eq!(row.profit, Some(550));
        assert_eq!(row.expected, Some(275.0));
        assert_eq!(row.roi, Some(1.375));
        assert_eq!(row.output.as_ref().map(|output| output.sale_rate), Some(Some(0.5)));
        assert_eq!(row.output.as_ref().and_then(|output| output.units_sold_per_day), Some(3.5));
        assert_eq!(row.reagents[1].category, Some(Category::Commodity));
        assert_eq!(row.reagents[1].live_unit_price, Some(90));
    }

    /// `makes` scales the output, and therefore the cut, not the reagents.
    #[test]
    fn makes_scales_the_output_side_only() {
        let row = evaluate(&recipe(1, 2, &[(2, 1)]), &priced_book(), CUT, FLOOR);
        assert_eq!(row.cost, Some(200));
        assert_eq!(row.gross, Some(2_000));
        assert_eq!(row.profit, Some(1_700), "2 000 − 100 cut − 200 reagents");
    }

    /// The live cost needs every reagent listed; one missing blanks it.
    #[test]
    fn live_cost_needs_every_reagent_listed() {
        let mut priced = priced_book();
        priced.insert(item(3), entry(Category::Commodity, 100, None, None));
        let row = evaluate(&recipe(1, 1, &[(2, 1), (3, 2)]), &priced, CUT, FLOOR);
        assert_eq!(row.cost, Some(400), "the 7-day cost is unaffected");
        assert_eq!(row.cost_now, None);
    }

    /// Issues come in order of severity, and each blanks the fields that
    /// depend on what is missing.
    #[test]
    fn issues_follow_the_inputs_they_depend_on() {
        let mut priced = priced_book();
        priced.remove(&item(3));
        let row = evaluate(&recipe(1, 1, &[(2, 1), (3, 2)]), &priced, CUT, FLOOR);
        assert_eq!(row.issue, Some(CraftIssue::ReagentWithoutPrice(item(3))));
        assert_eq!(row.cost, None);
        assert_eq!(row.profit, None, "no cost, no profit — but the output is still valued");
        assert_eq!(row.gross, Some(1_000));

        let priced = BTreeMap::from([(item(2), entry(Category::Commodity, 200, None, None))]);
        let row = evaluate(&recipe(1, 1, &[(2, 1)]), &priced, CUT, FLOOR);
        assert_eq!(row.issue, Some(CraftIssue::OutputWithoutPrice));
        assert_eq!(row.cost, Some(200));
        assert_eq!(row.gross, None);

        let mut priced = priced_book();
        priced.insert(item(1), entry(Category::Item, 1_000, None, None));
        let row = evaluate(&recipe(1, 1, &[(2, 1)]), &priced, CUT, FLOOR);
        assert_eq!(row.issue, Some(CraftIssue::NoSaleEvidence));
        assert_eq!(row.profit, Some(750));
        assert_eq!(row.expected, None, "no sale rate, no expectation");
        assert_eq!(row.output.as_ref().map(|output| output.category), Some(Category::Item));

        let mut priced = priced_book();
        priced.insert(item(1), entry(Category::Commodity, 1_000, None, Some(0.1)));
        let row = evaluate(&recipe(1, 1, &[(2, 1)]), &priced, CUT, FLOOR);
        assert_eq!(row.issue, Some(CraftIssue::SlowSeller(0.1)));
        assert_eq!(row.expected, Some(75.0), "the arithmetic still shows what it would be worth");
    }

    /// Reagents worth more than the output nets do not rank, whatever sells.
    #[test]
    fn a_craft_that_loses_money_does_not_rank() {
        let priced = BTreeMap::from([
            (item(1), entry(Category::Commodity, 1_000, None, Some(0.9))),
            (item(2), entry(Category::Commodity, 1_200, None, None)),
        ]);
        let row = evaluate(&recipe(1, 1, &[(2, 1)]), &priced, CUT, FLOOR);
        assert_eq!(row.issue, Some(CraftIssue::NotProfitable));
        assert_eq!(row.profit, Some(-250), "1 000 − 50 cut − 1 200 reagents");
        assert_eq!(row.expected, Some(-225.0));
        assert_eq!(row.roi, Some(-0.208_333_333_333_333_34));
    }

    /// The cut rounds to whole copper half-up like the rest of the money
    /// fields.
    #[test]
    fn the_house_cut_rounds_to_whole_copper() {
        assert_eq!(cut_copper(1_000, 0.05), 50);
        assert_eq!(cut_copper(999, 0.05), 50, "49.95 rounds up");
        assert_eq!(cut_copper(0, 0.05), 0);
        assert_eq!(cut_copper(10_000, 0.0), 0);
    }

    /// An item trading in both houses is priced where the evidence is, an
    /// exact tie keeps the commodity house, and no evidence means no price.
    #[test]
    fn the_market_with_more_evidence_prices_an_item() {
        let value = |value7: i64, samples7: i64| MarketValue {
            value7,
            sold_median14: None,
            samples7,
            sale_rate: None,
            live: None,
        };
        let (category, chosen) =
            better_evidenced(Some(value(100, 10)), Some(value(90, 50))).expect("priced");
        assert_eq!((category, chosen.value7), (Category::Item, 90), "the busier market wins");
        let (category, _) =
            better_evidenced(Some(value(100, 30)), Some(value(90, 30))).expect("priced");
        assert_eq!(category, Category::Commodity, "ties keep the commodity house");
        let (category, _) =
            better_evidenced(Some(value(0, 30)), Some(value(90, 5))).expect("priced");
        assert_eq!(category, Category::Item, "no price is no evidence");
        assert_eq!(better_evidenced(Some(value(0, 30)), None), None);
        assert_eq!(better_evidenced(None, None), None);
    }

    /// Wire spellings of the issues.
    #[test]
    fn issues_spell_themselves() {
        assert_eq!(CraftIssue::ReagentWithoutPrice(item(3)).as_str(), "reagent-without-price");
        assert_eq!(CraftIssue::OutputWithoutPrice.as_str(), "output-without-price");
        assert_eq!(CraftIssue::NoSaleEvidence.as_str(), "no-sale-evidence");
        assert_eq!(CraftIssue::SlowSeller(0.1).as_str(), "slow-seller");
        assert_eq!(CraftIssue::NotProfitable.as_str(), "not-profitable");
    }
}

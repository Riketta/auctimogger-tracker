//! Alert evaluation (R-alerts): runs a target's alert rules against the same
//! cached metric blocks the UI shows, the live floors of the newest snapshot
//! and the trailing sale ledger, records fired events, stamps cooldown
//! anchors and hands each event to the configured dispatch adapter (log,
//! Discord, Telegram — whatever the composition root wired in). Rules fire at
//! most once per cooldown window, anchored on the rule's `last_fired_at`.

use std::sync::Arc;

use crate::{
    domain::{
        utc_day,
        AlertCondition,
        AlertEvent,
        AlertRule,
        Category,
        FloorPercentile,
        ItemRef,
        NewAlertEvent,
        NewAlertRule,
        PayloadKind,
        SnapshotListing,
        SnapshotMeta,
        Span,
        TargetId,
    },
    error::StoreError,
    ports::{
        AlertDispatchPort,
        AlertEventRepository,
        AlertRuleRepository,
        ClockPort,
        PriceHistoryRepository,
        SalesRepository,
        SnapshotRepository,
    },
    services::{
        metrics::MetricsService,
        opportunity::{
            quantity_value,
            sold_reference,
            ValueSource,
            SALE_RATE_WINDOW_DAYS,
            SCAN_WINDOW_DAYS,
            SOLD_PRICE_WINDOW_DAYS,
        },
    },
};

/// Counters of one evaluation pass over one target's rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AlertRun {
    /// Rules examined (regardless of outcome).
    pub evaluated: usize,
    /// Rules whose condition held and were recorded.
    pub fired: usize,
    /// Fired events successfully handed to the dispatch adapter.
    pub dispatched: usize,
    /// Fired events whose dispatch failed (logged; never retried here).
    pub dispatch_failures: usize,
    /// Rules blocked by the freshness gate: at least one of the rule's market
    /// categories was stale-gated and the rule did not fire on any fresh
    /// category. Cooldown-skipped rules are neither counted here nor in
    /// `fired` — they simply never reach observation.
    pub skipped_stale: usize,
}

/// Evaluates alert rules and persists/dispatches the events they fire.
pub struct AlertEvaluationService {
    rules: Arc<dyn AlertRuleRepository>,
    events: Arc<dyn AlertEventRepository>,
    metrics: Arc<MetricsService>,
    sales: Arc<dyn SalesRepository>,
    snapshots: Arc<dyn SnapshotRepository>,
    history: Arc<dyn PriceHistoryRepository>,
    dispatcher: Option<Arc<dyn AlertDispatchPort>>,
    clock: Arc<dyn ClockPort>,
    /// Freshness gate in seconds: a market category only observes when its
    /// feed payload's snapshot is fresher (rules blocked this way are counted
    /// as [`AlertRun::skipped_stale`]). `0` disables the gate.
    max_data_age_secs: i64,
    /// Evidence floor for the value-relative deal conditions — the same
    /// `[opportunities].min_samples` the ranking applies: a 7-day value from
    /// fewer listings than this is not worth alerting against.
    min_samples: u32,
}

impl AlertEvaluationService {
    /// Wires the evaluator to its ports. `dispatcher` is the configured
    /// delivery adapter; `None` (the `log` delivery) still records events —
    /// they surface in the UI and JSON API without any outbound message.
    /// `snapshots` fuels the freshness gate and the live-floor conditions;
    /// `max_data_age_secs` of `0` disables gating.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rules: Arc<dyn AlertRuleRepository>,
        events: Arc<dyn AlertEventRepository>,
        metrics: Arc<MetricsService>,
        sales: Arc<dyn SalesRepository>,
        snapshots: Arc<dyn SnapshotRepository>,
        history: Arc<dyn PriceHistoryRepository>,
        dispatcher: Option<Arc<dyn AlertDispatchPort>>,
        clock: Arc<dyn ClockPort>,
        max_data_age_secs: i64,
        min_samples: u32,
    ) -> Self {
        Self {
            rules,
            events,
            metrics,
            sales,
            snapshots,
            history,
            dispatcher,
            clock,
            max_data_age_secs,
            min_samples,
        }
    }

    // ------------------------------------------------------------- rule CRUD

    /// Stores a new alert rule.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the rule cannot be written.
    pub fn add_rule(&self, rule: &NewAlertRule) -> Result<AlertRule, StoreError> {
        self.rules.add(rule)
    }

    /// Removes an alert rule by id; returns whether anything was removed.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the rule cannot be deleted.
    pub fn remove_rule(&self, rule_id: i64) -> Result<bool, StoreError> {
        self.rules.remove(rule_id)
    }

    /// Lists a target's alert rules in insertion order.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when rules cannot be read.
    pub fn list_rules(&self, target: &TargetId) -> Result<Vec<AlertRule>, StoreError> {
        self.rules.list(target)
    }

    /// Lists a target's newest fired alerts, newest first.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when events cannot be read.
    pub fn recent_events(
        &self,
        target: &TargetId,
        limit: usize,
    ) -> Result<Vec<AlertEvent>, StoreError> {
        self.events.recent(target, limit)
    }

    // ------------------------------------------------------------ evaluation

    /// Evaluates every rule of one target: skips rules inside their cooldown
    /// window, observes the surviving rules' conditions against the cached
    /// metric blocks, the newest snapshots' live listings and the trailing
    /// ledger (each market category only when its own feed is fresh — see the
    /// freshness gate below), records an event per fired rule (and stamps its
    /// cooldown anchor) and dispatches it best effort.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when rules, blocks, live listings or
    /// event writes fail; dispatch failures are counted in
    /// [`AlertRun::dispatch_failures`] and reported by the adapter, not
    /// returned here.
    pub fn evaluate(&self, target: &TargetId, cooldown_secs: i64) -> Result<AlertRun, StoreError> {
        let rules = self.rules.list(target)?;
        let now = self.clock.now();
        // Freshness gate: a rule may only fire on data fresh enough to mean
        // anything. The TSM addon colors stale prices red but still trades on
        // them (and its own freshness bookkeeping forgot the commodity feeds
        // entirely) — we refuse to fire instead. The gate is per market: each
        // category observes only when ITS feed payload is fresh, so one live
        // feed cannot launder stale evidence from the other. No snapshot at
        // all for a kind reads as "no data yet", not "stale": observation
        // naturally finds nothing.
        let newest = self.newest_snapshots(target)?;
        let mut run = AlertRun::default();
        for rule in rules {
            run.evaluated += 1;
            if rule.last_fired_at.is_some_and(|last| now - last < cooldown_secs) {
                continue;
            }
            // Walk the categories here so the gate is per market while the
            // first fresh breach still wins in canonical order.
            let mut gated = false;
            let mut fired = false;
            for category in Category::ALL {
                let kind = category.payload_kind();
                if newest.gates(self.max_data_age_secs, kind, now) {
                    gated = true;
                    continue;
                }
                let Some(observed) = self.observe_category(&rule, category, newest.get(kind))?
                else {
                    continue;
                };
                fired = true;
                let event = NewAlertEvent {
                    rule_id: rule.id,
                    target: rule.target.clone(),
                    item: rule.item.clone(),
                    condition: rule.condition,
                    observed: observed.value,
                    message: observed.message,
                    fired_at: now,
                };
                // Atomic: event + cooldown anchor in one store transaction, so
                // a failed anchor write can never produce a re-fired duplicate.
                let event_id = self.rules.record_firing(&event)?;
                run.fired += 1;
                if let Some(dispatcher) = &self.dispatcher {
                    let delivered = AlertEvent {
                        id: event_id,
                        rule_id: event.rule_id,
                        target: event.target,
                        item: event.item,
                        condition: event.condition,
                        observed: event.observed,
                        message: event.message,
                        fired_at: event.fired_at,
                    };
                    match dispatcher.dispatch(&delivered) {
                        Ok(()) => run.dispatched += 1,
                        // The core never logs (zero deps): the caller surfaces
                        // the failure from the returned counters.
                        Err(_) => run.dispatch_failures += 1,
                    }
                }
                break;
            }
            if gated && !fired {
                run.skipped_stale += 1;
            }
        }
        Ok(run)
    }

    /// One category's observation against its feed's newest snapshot (which
    /// the freshness gate already let through): `Ok(Some(_))` only when the
    /// condition held there; no data or a non-breach reads as `Ok(None)` and
    /// the next category gets its turn.
    fn observe_category(
        &self,
        rule: &AlertRule,
        category: Category,
        newest: Option<&SnapshotMeta>,
    ) -> Result<Option<Observation>, StoreError> {
        let label = format!("{} [{}]", item_label(&rule.item), category.as_str());
        match rule.condition {
            AlertCondition::PriceBelow { copper } => {
                match self.metrics.block(&rule.target, &rule.item, category, Span::D1)? {
                    Some(block) if block.median <= copper => Ok(Some(Observation {
                        value: Some(copper_as_f64(block.median)),
                        message: format!(
                            "{label}: d1 median {} copper (target ≤ {copper})",
                            block.median
                        ),
                    })),
                    _ => Ok(None),
                }
            }
            AlertCondition::BelowFloor { span, percentile } => {
                match self.metrics.block(&rule.target, &rule.item, category, span)? {
                    Some(block) if block.median <= floor(&block, percentile) => {
                        Ok(Some(Observation {
                            value: Some(copper_as_f64(block.median)),
                            message: format!(
                                "{label}: {} median {} copper at/below {} floor {}",
                                span.name(),
                                block.median,
                                percentile.as_str(),
                                floor(&block, percentile)
                            ),
                        }))
                    }
                    _ => Ok(None),
                }
            }
            AlertCondition::FloorBelow { .. }
            | AlertCondition::FloorBelowValue { .. }
            | AlertCondition::NewListingBelow { .. } => {
                self.observe_deal(rule, category, newest, &label)
            }
            AlertCondition::SaleRateAbove { rate } => {
                let counts = self.sales.sale_counts(
                    &rule.target,
                    &rule.item,
                    SALE_RATE_WINDOW_DAYS,
                    category,
                )?;
                match self.sales.sale_rate(
                    &rule.target,
                    &rule.item,
                    SALE_RATE_WINDOW_DAYS,
                    category,
                )? {
                    Some(actual) if actual >= rate => Ok(Some(Observation {
                        value: Some(actual),
                        // The raw ledger behind the rate rides along when the
                        // adapter keeps one: how many sold, how many relisted.
                        message: match counts {
                            Some(counts) => format!(
                                "{label}: 14d sale rate {actual:.2} (target ≥ {rate:.2}) — {} sold, {} expired",
                                counts.sold, counts.expired
                            ),
                            None => {
                                format!("{label}: 14d sale rate {actual:.2} (target ≥ {rate:.2})")
                            }
                        },
                    })),
                    _ => Ok(None),
                }
            }
            AlertCondition::DaysSinceLastSaleAbove { days } => {
                let today = utc_day(self.clock.now());
                let observed_days = self
                    .sales
                    .sale_recency(&rule.target, &rule.item, category)?
                    .map(|recency| today - recency.last_sale_day);
                match observed_days {
                    Some(idle_days) if idle_days >= days => Ok(Some(Observation {
                        value: Some(days_observation(idle_days)),
                        message: format!(
                            "{label}: {idle_days} days since last sale (target ≥ {days})"
                        ),
                    })),
                    _ => Ok(None),
                }
            }
        }
    }

    /// The live-deal conditions: the newest snapshot's listings against the
    /// item's reference value — the 14-day sold median once enough priced
    /// units sold, else the 7-day quantity-weighted listing value (see the
    /// variants' docs). `newest` is the snapshot the freshness gate let
    /// through; `label` prefixes the message.
    fn observe_deal(
        &self,
        rule: &AlertRule,
        category: Category,
        newest: Option<&SnapshotMeta>,
        label: &str,
    ) -> Result<Option<Observation>, StoreError> {
        match rule.condition {
            AlertCondition::FloorBelow { copper } => {
                let Some(snapshot_id) = newest.map(|meta| meta.id) else {
                    return Ok(None);
                };
                match self.snapshots.floor(&rule.target, &rule.item, snapshot_id)? {
                    Some(live) if live.floor_price <= copper => Ok(Some(Observation {
                        value: Some(copper_as_f64(live.floor_price)),
                        message: format!(
                            "{label}: live floor {} copper (target ≤ {copper})",
                            live.floor_price
                        ),
                    })),
                    _ => Ok(None),
                }
            }
            AlertCondition::FloorBelowValue { fraction } => {
                let Some(snapshot_id) = newest.map(|meta| meta.id) else {
                    return Ok(None);
                };
                let Some((value, _, value_label)) = self.deal_value(rule, category)? else {
                    return Ok(None);
                };
                match self.snapshots.floor(&rule.target, &rule.item, snapshot_id)? {
                    Some(live) if copper_as_f64(live.floor_price) <= fraction * value => {
                        Ok(Some(Observation {
                            value: Some(copper_as_f64(live.floor_price)),
                            message: format!(
                                "{label}: live floor {} copper ≤ {}% of {value_label} {:.0}",
                                live.floor_price,
                                percent(fraction),
                                value
                            ),
                        }))
                    }
                    _ => Ok(None),
                }
            }
            AlertCondition::NewListingBelow { fraction } => {
                let Some(snapshot) = newest else {
                    return Ok(None);
                };
                let Some((value, _, value_label)) = self.deal_value(rule, category)? else {
                    return Ok(None);
                };
                let listings = self.snapshots.listings_for_item(
                    &rule.target,
                    &rule.item,
                    category.payload_kind(),
                    snapshot.id,
                )?;
                // Fresh to this snapshot, and never counted for a firing the
                // rule already did: a listing that stays up cannot re-fire it.
                let mut hits: Vec<&SnapshotListing> = listings
                    .iter()
                    .filter(|listing| {
                        listing.first_seen_snapshot == Some(snapshot.id)
                            && rule.last_fired_at.is_none_or(|fired| {
                                listing.first_seen_at.is_none_or(|at| at > fired)
                            })
                            && copper_as_f64(listing.unit_price) <= fraction * value
                    })
                    .collect();
                if hits.is_empty() {
                    return Ok(None);
                }
                hits.sort_unstable_by_key(|listing| listing.unit_price);
                let cheapest = hits.first().map_or(0, |listing| listing.unit_price);
                let units: i64 = hits
                    .iter()
                    .fold(0i64, |sum, listing| sum.saturating_add(listing.quantity.max(0)));
                Ok(Some(Observation {
                    value: Some(copper_as_f64(cheapest)),
                    message: format!(
                        "{label}: {} new listing(s) from {} copper (≤ {}% of {value_label} {:.0}), \
                         {units} units",
                        hits.len(),
                        cheapest,
                        percent(fraction),
                        value
                    ),
                }))
            }
            // Every other condition is observed by `observe_category`.
            _ => Ok(None),
        }
    }

    /// The rule item's reference value behind the deal conditions — the
    /// ranking's discount reference, so an alert can never contradict the
    /// page: the 14-day sold median once enough priced units sold, else the
    /// 7-day quantity-weighted listing value. Admitted only with the
    /// configured listing evidence behind it; the third element names the
    /// value for the message.
    fn deal_value(
        &self,
        rule: &AlertRule,
        category: Category,
    ) -> Result<Option<(f64, i64, &'static str)>, StoreError> {
        let today = utc_day(self.clock.now());
        let rows = self.history.item_day_rows_for_item(
            &rule.target,
            &rule.item,
            today - SCAN_WINDOW_DAYS + 1,
            category,
        )?;
        let Some((listed, samples)) =
            quantity_value(&rows).filter(|(_, samples)| *samples >= i64::from(self.min_samples))
        else {
            return Ok(None);
        };
        let sold = self.sales.sold_price_each(
            &rule.target,
            std::slice::from_ref(&rule.item),
            SOLD_PRICE_WINDOW_DAYS,
            category,
        )?;
        Ok(match sold.first().and_then(|histogram| sold_reference(histogram)) {
            Some(median) if median > 0.0 => Some((median, samples, ValueSource::Sold.label())),
            _ => Some((listed, samples, ValueSource::Listed.label())),
        })
    }

    /// Newest snapshot per payload kind for a target; a kind without any
    /// reads as `None` ("no data yet").
    fn newest_snapshots(&self, target: &TargetId) -> Result<NewestSnapshots, StoreError> {
        Ok(NewestSnapshots {
            commodities: self.snapshots.latest(target, PayloadKind::Commodities)?,
            auctions: self.snapshots.latest(target, PayloadKind::Auctions)?,
        })
    }
}

/// Newest snapshot per payload kind for one target. The freshness gate is per
/// market: each [`Category`] only observes against its own kind's snapshot,
/// so a live commodities feed cannot arm rules whose evidence is the (dead)
/// auctions feed, and vice versa; the live-floor conditions read the same
/// snapshot the gate let through.
struct NewestSnapshots {
    commodities: Option<SnapshotMeta>,
    auctions: Option<SnapshotMeta>,
}

impl NewestSnapshots {
    fn get(&self, kind: PayloadKind) -> Option<&SnapshotMeta> {
        match kind {
            PayloadKind::Commodities => self.commodities.as_ref(),
            PayloadKind::Auctions => self.auctions.as_ref(),
        }
    }

    /// `true` when the gate is enabled and the kind's feed is older than the
    /// maximum data age (`None` = no snapshot of that kind: "no data yet",
    /// not stale — observation naturally finds nothing).
    fn gates(&self, max_data_age_secs: i64, kind: PayloadKind, now: i64) -> bool {
        if max_data_age_secs <= 0 {
            return false;
        }
        self.get(kind)
            .map(|meta| now - meta.source_last_modified.unwrap_or(meta.fetched_at))
            .is_some_and(|age| age > max_data_age_secs)
    }
}

/// The floor percentile a [`AlertCondition::BelowFloor`] rule compares
/// against.
fn floor(block: &crate::domain::MetricBlock, percentile: FloorPercentile) -> i64 {
    match percentile {
        FloorPercentile::P05 => block.p05,
        FloorPercentile::P25 => block.p25,
    }
}

/// One fired observation: the observed number and the human-readable
/// one-liner for the event row. `observe` returns `Some` only when a
/// category's condition held, so there is no separate fired flag.
struct Observation {
    value: Option<f64>,
    message: String,
}

/// A copper amount widened to f64 for the event's observed value and the
/// value-threshold comparisons (display/ranking math, not money decisions).
#[allow(clippy::cast_precision_loss)]
fn copper_as_f64(copper: i64) -> f64 {
    copper as f64
}

/// Days-since-last-sale as the event's observed value (display-only widening).
#[allow(clippy::cast_precision_loss)]
fn days_observation(days: i64) -> f64 {
    days as f64
}

/// A value fraction as whole percent for messages (display-only rounding).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn percent(fraction: f64) -> i64 {
    (fraction * 100.0).round() as i64
}

/// Compact identity label for event messages: items by id, pets by their
/// full variant tuple.
fn item_label(item: &ItemRef) -> String {
    match item {
        ItemRef::Item { item_id } => format!("item {item_id}"),
        ItemRef::Pet { species, level, quality, breed } => {
            format!("pet {species} (level {level}, quality {quality}, breed {breed})")
        }
    }
}

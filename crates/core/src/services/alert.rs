//! Alert evaluation (R-alerts): runs a target's alert rules against the same
//! cached metric blocks the UI shows, records fired events, stamps cooldown
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
        Span,
        TargetId,
    },
    error::StoreError,
    ports::{
        AlertDispatchPort,
        AlertEventRepository,
        AlertRuleRepository,
        ClockPort,
        SalesRepository,
        SnapshotRepository,
    },
    services::metrics::MetricsService,
};

/// Trailing window (days) behind [`AlertCondition::SaleRateAbove`] — the same
/// window every metric block's `sale_rate` uses.
const SALE_RATE_WINDOW_DAYS: i64 = 14;

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
    dispatcher: Option<Arc<dyn AlertDispatchPort>>,
    clock: Arc<dyn ClockPort>,
    /// Freshness gate in seconds: a market category only observes when its
    /// feed payload's snapshot is fresher (rules blocked this way are counted
    /// as [`AlertRun::skipped_stale`]). `0` disables the gate.
    max_data_age_secs: i64,
}

impl AlertEvaluationService {
    /// Wires the evaluator to its ports. `dispatcher` is the configured
    /// delivery adapter; `None` (the `log` delivery) still records events —
    /// they surface in the UI and JSON API without any outbound message.
    /// `snapshots` fuels the freshness gate; `max_data_age_secs` of `0`
    /// disables gating.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rules: Arc<dyn AlertRuleRepository>,
        events: Arc<dyn AlertEventRepository>,
        metrics: Arc<MetricsService>,
        sales: Arc<dyn SalesRepository>,
        snapshots: Arc<dyn SnapshotRepository>,
        dispatcher: Option<Arc<dyn AlertDispatchPort>>,
        clock: Arc<dyn ClockPort>,
        max_data_age_secs: i64,
    ) -> Self {
        Self { rules, events, metrics, sales, snapshots, dispatcher, clock, max_data_age_secs }
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
    /// metric blocks (each market category only when its own feed is fresh —
    /// see the freshness gate below), records an event per fired rule (and
    /// stamps its cooldown anchor) and dispatches it best effort.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when rules, blocks or event writes fail;
    /// dispatch failures are counted in [`AlertRun::dispatch_failures`] and
    /// reported by the adapter, not returned here.
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
        let ages = self.snapshot_ages(target, now)?;
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
                if ages.gates(self.max_data_age_secs, feed_kind(category)) {
                    gated = true;
                    continue;
                }
                let Some(observed) = self.observe_category(&rule, category)? else {
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

    /// One category's observation: `Ok(Some(_))` only when the condition
    /// held there; no data or a non-breach reads as `Ok(None)` and the next
    /// category gets its turn.
    fn observe_category(
        &self,
        rule: &AlertRule,
        category: Category,
    ) -> Result<Option<Observation>, StoreError> {
        let label = format!("{} [{}]", item_label(&rule.item), category.as_str());
        match rule.condition {
            AlertCondition::PriceBelow { copper } => {
                match self.metrics.block(&rule.target, &rule.item, category, Span::D1)? {
                    Some(block) if block.median <= copper => Ok(Some(Observation {
                        value: Some(median_observation(block.median)),
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
                            value: Some(median_observation(block.median)),
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
            AlertCondition::SaleRateAbove { rate } => {
                match self.sales.sale_rate(
                    &rule.target,
                    &rule.item,
                    SALE_RATE_WINDOW_DAYS,
                    category,
                )? {
                    Some(actual) if actual >= rate => Ok(Some(Observation {
                        value: Some(actual),
                        message: format!("{label}: 14d sale rate {actual:.2} (target ≥ {rate:.2})"),
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

    /// Ages in seconds of the target's newest feed snapshot per payload kind
    /// (source `Last-Modified` when present, else the fetch stamp); a kind
    /// with no snapshot at all reads as `None`.
    fn snapshot_ages(&self, target: &TargetId, now: i64) -> Result<FeedAges, StoreError> {
        Ok(FeedAges {
            commodities: self.kind_age(target, now, PayloadKind::Commodities)?,
            auctions: self.kind_age(target, now, PayloadKind::Auctions)?,
        })
    }

    /// Age of one kind's newest snapshot, in seconds.
    fn kind_age(
        &self,
        target: &TargetId,
        now: i64,
        kind: PayloadKind,
    ) -> Result<Option<i64>, StoreError> {
        Ok(self
            .snapshots
            .latest(target, kind)?
            .map(|meta| now - meta.source_last_modified.unwrap_or(meta.fetched_at)))
    }
}

/// Per-payload-kind snapshot ages for one target. The freshness gate is per
/// market: each [`Category`] only observes against its own kind's age, so a
/// live commodities feed cannot arm rules whose evidence is the (dead)
/// auctions feed, and vice versa.
struct FeedAges {
    commodities: Option<i64>,
    auctions: Option<i64>,
}

impl FeedAges {
    /// `true` when the gate is enabled and the kind's feed is older than the
    /// maximum data age (`None` = no snapshot of that kind: "no data yet",
    /// not stale — observation naturally finds nothing).
    fn gates(&self, max_data_age_secs: i64, kind: PayloadKind) -> bool {
        if max_data_age_secs <= 0 {
            return false;
        }
        let age = match kind {
            PayloadKind::Commodities => self.commodities,
            PayloadKind::Auctions => self.auctions,
        };
        age.is_some_and(|age| age > max_data_age_secs)
    }
}

/// The feed payload kind a market category's evidence comes from: the
/// commodity AH dump feeds the commodity market, the per-item auction dump
/// feeds the item and pet markets.
fn feed_kind(category: Category) -> PayloadKind {
    match category {
        Category::Commodity => PayloadKind::Commodities,
        Category::Item | Category::Pet => PayloadKind::Auctions,
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

/// Median copper as the event's observed value (display-only widening).
#[allow(clippy::cast_precision_loss)]
fn median_observation(median: i64) -> f64 {
    median as f64
}

/// Days-since-last-sale as the event's observed value (display-only widening).
#[allow(clippy::cast_precision_loss)]
fn days_observation(days: i64) -> f64 {
    days as f64
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

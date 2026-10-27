//! Alert evaluation (R-alerts): runs a target's alert rules against the same
//! cached metric blocks the UI shows, records fired events, stamps cooldown
//! anchors and hands each event to the configured dispatch adapter (log,
//! Discord, Telegram — whatever the composition root wired in). Rules fire at
//! most once per cooldown window, anchored on the rule's `last_fired_at`.

use std::sync::Arc;

use crate::{
    domain::{
        AlertCondition,
        AlertEvent,
        AlertRule,
        Category,
        FloorPercentile,
        ItemRef,
        NewAlertEvent,
        NewAlertRule,
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
}

/// Evaluates alert rules and persists/dispatches the events they fire.
pub struct AlertEvaluationService {
    rules: Arc<dyn AlertRuleRepository>,
    events: Arc<dyn AlertEventRepository>,
    metrics: Arc<MetricsService>,
    sales: Arc<dyn SalesRepository>,
    dispatcher: Option<Arc<dyn AlertDispatchPort>>,
    clock: Arc<dyn ClockPort>,
}

impl AlertEvaluationService {
    /// Wires the evaluator to its ports. `dispatcher` is the configured
    /// delivery adapter; `None` (the `log` delivery) still records events —
    /// they surface in the UI and JSON API without any outbound message.
    pub fn new(
        rules: Arc<dyn AlertRuleRepository>,
        events: Arc<dyn AlertEventRepository>,
        metrics: Arc<MetricsService>,
        sales: Arc<dyn SalesRepository>,
        dispatcher: Option<Arc<dyn AlertDispatchPort>>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self { rules, events, metrics, sales, dispatcher, clock }
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
    /// metric blocks, records an event per fired rule (and stamps its
    /// cooldown anchor) and dispatches it best effort.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when rules, blocks or event writes fail;
    /// dispatch failures are counted in [`AlertRun::dispatch_failures`] and
    /// reported by the adapter, not returned here.
    pub fn evaluate(&self, target: &TargetId, cooldown_secs: i64) -> Result<AlertRun, StoreError> {
        let rules = self.rules.list(target)?;
        let now = self.clock.now();
        let mut run = AlertRun::default();
        for rule in rules {
            run.evaluated += 1;
            if rule.last_fired_at.is_some_and(|last| now - last < cooldown_secs) {
                continue;
            }
            let Some(observed) = self.observe(&rule)? else {
                continue;
            };
            let event = NewAlertEvent {
                rule_id: rule.id,
                target: rule.target.clone(),
                item: rule.item.clone(),
                condition: rule.condition,
                observed: observed.value,
                message: observed.message,
                fired_at: now,
            };
            let event_id = self.events.record(&event)?;
            self.rules.mark_fired(rule.id, now)?;
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
        }
        Ok(run)
    }

    /// Observes one rule's condition across the rule's item's market
    /// categories: each category's block / sale rate is checked in canonical
    /// order and the first breach fires, its message naming the category.
    /// `None` = no category breached (or none had enough data to decide).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the underlying reads fail.
    fn observe(&self, rule: &AlertRule) -> Result<Option<Observation>, StoreError> {
        for category in Category::ALL {
            if let Some(observation) = self.observe_category(rule, category)? {
                return Ok(Some(observation));
            }
        }
        Ok(None)
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
        }
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

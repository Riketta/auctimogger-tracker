//! Poll-loop scheduler: a plain thread that runs ingestion cycles per target
//! on the configured cadence, then keeps the operational side effects that
//! belong to ingestion flowing: raw-listing retention (`prune_raw_before`),
//! feed publication (the files `/feed.lua` serves), and the prepared
//! opportunity/overview scans (`OpportunityService::refresh` — at startup,
//! after every successful cycle, and when a scan outlives its TTL, so
//! requests serve prepared data with no store reads). Doing all of this here
//! means a plain `serve` is self-contained — no cron, no exporter sidecar.
//!
//! D13: no async in or around the core; everything below is blocking and
//! runs on this dedicated thread.

use std::{
    any::Any,
    collections::HashMap,
    panic::{
        self,
        AssertUnwindSafe,
    },
    sync::{
        Arc,
        Mutex,
        PoisonError,
    },
    thread,
    time::{
        Duration,
        Instant,
    },
};

use amtracker_core::{
    domain::{
        utc_day,
        FeedFormat,
        PayloadKind,
        TargetId,
    },
    services::{
        feed_export::ExportOptions,
        opportunity::SCAN_TTL_SECONDS,
    },
};

use crate::web::AppState;

/// How often the loop wakes up to check whether a cycle is due.
const TICK: Duration = Duration::from_secs(30);

/// Per-target operational state: last cycle run, last prune day, last publish.
#[derive(Default)]
struct TargetTimers {
    cycle_run: HashMap<(String, PayloadKind), Instant>,
    prune_day: HashMap<String, i64>,
    feed_publish: HashMap<String, Instant>,
}

/// Spawns the background poll loop: ingestion cycles for every configured
/// target on its configured cadence, raw-listing pruning once per UTC day per
/// target, and feed re-publication per target at most every
/// `server.feed_publish_secs`. Outcomes are emitted via `tracing` (info on
/// success, warn on failures — the warn path is what surfaces in Sentry). A
/// panicking cycle is caught and logged so one bad fetch can never silently
/// kill ingestion while the web UI keeps serving.
///
/// # Errors
/// Returns the [`std::io::Error`] from [`thread::Builder::spawn`] when the OS
/// refuses to start the scheduler thread.
pub fn spawn(state: Arc<AppState>) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new().name("amtracker-scheduler".into()).spawn(move || {
        let config = state.config.clone();
        // Prepare every configured target's opportunity scan before the first
        // request arrives; best effort — a failing scan logs a warning and the
        // target's scan simply stays cold (the request path falls back to an
        // inline compute), so `serve` never fails startup over it.
        for target in &config.targets {
            survive("startup-refresh", &target.id.0, || {
                refresh_opportunities(&state, &target.id);
            });
        }
        // The critical sections below only touch the maps, so a poisoned
        // lock can only come from a panic inside this very loop; recover
        // the guard instead of panicking on poison.
        let timers = Mutex::new(TargetTimers::default());
        loop {
            for target in &config.targets {
                for (kind, interval) in [
                    (PayloadKind::Commodities, target.poll.commodities_secs),
                    (PayloadKind::Auctions, target.poll.auctions_secs),
                ] {
                    let key = (target.id.0.clone(), kind);
                    let due = match timers
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .cycle_run
                        .get(&key)
                    {
                        Some(last) => last.elapsed().as_secs() >= interval,
                        None => true,
                    };
                    if !due {
                        continue;
                    }
                    run_cycle_logged(&state, &config, &timers, target, kind);
                    timers
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .cycle_run
                        .insert(key, Instant::now());
                }
            }
            // Refresh any prepared opportunity scan that outlived its TTL;
            // the debounce window in `refresh` makes overlap with the
            // post-cycle refresh above harmless.
            for target in &config.targets {
                let refresh_due = state
                    .opportunities
                    .snapshot_age_secs(&target.id)
                    .is_none_or(|age| age >= SCAN_TTL_SECONDS);
                if refresh_due {
                    survive("scan-refresh", &target.id.0, || {
                        refresh_opportunities(&state, &target.id);
                    });
                }
            }
            tracing::trace!(tick_secs = TICK.as_secs(), "scheduler tick");
            thread::sleep(TICK);
        }
    })
}

/// Runs one due ingest cycle under `catch_unwind`, logging the outcome and
/// recording it in the dashboard's recent-cycles ring, then triggers the
/// post-cycle side effects on success. Panics never kill the scheduler:
/// the panic hook logs the detail, the ring shows it on the dashboard, and
/// the next interval retries (the caller still stamps the run timer).
fn run_cycle_logged(
    state: &Arc<AppState>,
    config: &amtracker_core::config::TrackerConfig,
    timers: &Mutex<TargetTimers>,
    target: &amtracker_core::config::TargetConfig,
    kind: PayloadKind,
) {
    let run = panic::catch_unwind(AssertUnwindSafe(|| {
        // Manual `/api/fetch` cycles take the same lock, so two
        // observe/close_missing sequences never interleave.
        let _guard = state.cycle_lock.lock().unwrap_or_else(PoisonError::into_inner);
        state.cycle.run_cycle(target, kind)
    }));
    match run {
        Ok(Ok(report)) => {
            let outcome_label = match report.outcome {
                amtracker_core::services::cycle::CycleOutcome::Empty => {
                    tracing::warn!(
                        target = %report.target.0,
                        kind = kind.as_str(),
                        "ingest cycle returned an empty payload; \
                         fold/sighting stages were skipped"
                    );
                    "empty"
                }
                amtracker_core::services::cycle::CycleOutcome::Fetched => "fresh",
                amtracker_core::services::cycle::CycleOutcome::NotModified => "not-modified",
            };
            tracing::info!(
                target = %report.target.0,
                kind = kind.as_str(),
                outcome = ?report.outcome,
                entries = report.entries,
                dropped = report.dropped,
                folds = report.items_folded,
                sale_credits = report.sale_credits,
                "ingest cycle complete"
            );
            state.recent_cycles.push(crate::web::CycleRecord {
                at: state.clock.now(),
                target: report.target.0.clone(),
                kind: kind.as_str().to_string(),
                outcome: outcome_label.to_string(),
                entries: report.entries,
                dropped: report.dropped,
                folds: report.items_folded,
                credits: report.sale_credits,
                error: None,
            });
            survive("after-cycle", &target.id.0, || {
                after_cycle(state, config, timers, &target.id.0);
            });
        }
        Ok(Err(error)) => {
            tracing::warn!(
                target = %target.id.0,
                kind = kind.as_str(),
                error = %error,
                "ingest cycle failed"
            );
            state.recent_cycles.push(crate::web::CycleRecord {
                at: state.clock.now(),
                target: target.id.0.clone(),
                kind: kind.as_str().to_string(),
                outcome: "failed".to_string(),
                entries: 0,
                dropped: 0,
                folds: 0,
                credits: 0,
                error: Some(error.to_string()),
            });
        }
        Err(panic_payload) => {
            // The panic hook already logged the detail; this record ties the
            // panic to the target/kind. The caller's `last_run` insert still
            // runs, so the panicking cycle retries on the next interval
            // instead of hot-looping.
            let message = panic_message(&panic_payload);
            tracing::error!(
                target = %target.id.0,
                kind = kind.as_str(),
                panic = %message,
                "ingest cycle panicked; continuing"
            );
            state.recent_cycles.push(crate::web::CycleRecord {
                at: state.clock.now(),
                target: target.id.0.clone(),
                kind: kind.as_str().to_string(),
                outcome: "panicked".to_string(),
                entries: 0,
                dropped: 0,
                folds: 0,
                credits: 0,
                error: Some(message),
            });
        }
    }
}

/// Operational side effects after a successful cycle for one target:
/// once-per-UTC-day retention pruning, throttled feed publication, and a
/// refresh of the target's prepared opportunity scan (new folds/credits just
/// landed). All are best effort — a failure is logged and retried on a later
/// cycle.
fn after_cycle(
    state: &Arc<AppState>,
    config: &amtracker_core::config::TrackerConfig,
    timers: &Mutex<TargetTimers>,
    target: &str,
) {
    let now_unix = state.clock.now();
    let today = utc_day(now_unix);
    let mut timers = timers.lock().unwrap_or_else(PoisonError::into_inner);

    if timers.prune_day.get(target) != Some(&today) {
        match state.history.prune_raw_before(
            &amtracker_core::domain::TargetId(target.to_string()),
            today - i64::from(config.storage.raw_listing_days) + 1,
        ) {
            Ok(removed) => {
                timers.prune_day.insert(target.to_string(), today);
                tracing::debug!(target, removed, "raw listing retention applied");
            }
            Err(error) => {
                tracing::warn!(target, error = %error, "raw listing retention failed");
            }
        }
    }

    let publish_due = timers
        .feed_publish
        .get(target)
        .is_none_or(|last| last.elapsed().as_secs() >= config.server.feed_publish_secs);
    if publish_due {
        timers.feed_publish.insert(target.to_string(), Instant::now());
        drop(timers);
        publish_feed(state, config, target);
    }

    refresh_opportunities(state, &TargetId(target.to_string()));
}

/// Runs the after-cycle side effects for one target right now, without the
/// scheduler's once-per-day/throttle bookkeeping: retention pruning, feed
/// publication, and an opportunity scan refresh, exactly as a successful
/// scheduled cycle would trigger them.
/// Used by the manual `/api/fetch` path, whose successful cycles must show
/// up in `/feed.lua` and the ranking pages immediately instead of waiting
/// for the scheduler.
pub fn after_cycle_now(state: &Arc<AppState>, target: &str) {
    after_cycle(state, &state.config, &Mutex::new(TargetTimers::default()), target);
}

/// Runs a best-effort scheduler step under `catch_unwind`, giving the
/// operational side effects (prune, publish, scan refresh) the same
/// panic-survival guarantee ingest cycles already have: the panic hook logs
/// the detail, this keeps the thread alive and names the step.
fn survive(step: &'static str, target: &str, f: impl FnOnce()) {
    if panic::catch_unwind(AssertUnwindSafe(f)).is_err() {
        tracing::error!(step, target, "scheduler step panicked; continuing");
    }
}

/// Recomputes one target's prepared opportunity scan, best effort: success is
/// logged at debug with the items covered, failures warn and are swallowed —
/// ranking data is background hygiene, never worth failing a cycle or the
/// startup over.
fn refresh_opportunities(state: &Arc<AppState>, target: &TargetId) {
    match state.opportunities.refresh(target) {
        Ok(items) => tracing::debug!(target = %target.0, items, "opportunity scan refreshed"),
        Err(error) => {
            tracing::warn!(target = %target.0, error = %error, "opportunity scan refresh failed");
        }
    }
}

/// Builds and publishes the full feed (lua + json) for one target.
fn publish_feed(
    state: &Arc<AppState>,
    config: &amtracker_core::config::TrackerConfig,
    target: &str,
) {
    let target_id = amtracker_core::domain::TargetId(target.to_string());
    let options = ExportOptions { spans: config.metrics.spans.clone(), watchlist_only: false };
    match state.feed_export.build_feed(&target_id, &options) {
        Ok(feed) => {
            for format in [FeedFormat::Lua, FeedFormat::Json] {
                match state.renderer.render(&feed, format) {
                    Ok(bytes) => {
                        if let Err(error) = state.publisher.publish(&target_id, format, &bytes) {
                            tracing::error!(
                                target,
                                format = ?format,
                                error = %error,
                                "feed publish failed"
                            );
                        }
                    }
                    Err(error) => tracing::error!(
                        target,
                        format = ?format,
                        error = %error,
                        "feed render failed"
                    ),
                }
            }
            tracing::info!(target, items = feed.entries.len(), "feed published");
        }
        Err(error) => tracing::warn!(target, error = %error, "feed export failed"),
    }
}

/// Best-effort panic payload rendering: string payloads print their message,
/// anything else is reported generically rather than as `Any`.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

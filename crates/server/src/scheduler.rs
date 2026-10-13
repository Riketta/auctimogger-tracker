//! Poll-loop scheduler: a plain thread that runs ingestion cycles per target
//! on the configured cadence, then keeps the operational side effects that
//! belong to ingestion flowing: raw-listing retention (`prune_raw_before`)
//! and feed publication (the files `/feed.lua` serves). Doing both here means
//! a plain `serve` is self-contained — no cron, no exporter sidecar.
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
    },
    services::feed_export::ExportOptions,
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
                    match panic::catch_unwind(AssertUnwindSafe(|| {
                        state.cycle.run_cycle(target, kind)
                    })) {
                        Ok(Ok(report)) => {
                            if report.outcome
                                == amtracker_core::services::cycle::CycleOutcome::Empty
                            {
                                tracing::warn!(
                                    target = %report.target.0,
                                    kind = kind.as_str(),
                                    "ingest cycle returned an empty payload; \
                                     fold/sighting stages were skipped"
                                );
                            }
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
                            after_cycle(&state, &config, &timers, &target.id.0);
                        }
                        Ok(Err(error)) => tracing::warn!(
                            target = %target.id.0,
                            kind = kind.as_str(),
                            error = %error,
                            "ingest cycle failed"
                        ),
                        Err(panic_payload) => {
                            // The panic hook already logged the detail; this
                            // record ties the panic to the target/kind. The
                            // `last_run` insert below still runs, so the
                            // panicking cycle retries on the next interval
                            // instead of hot-looping.
                            tracing::error!(
                                target = %target.id.0,
                                kind = kind.as_str(),
                                panic = panic_message(&panic_payload),
                                "ingest cycle panicked; continuing"
                            );
                        }
                    }
                    timers
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .cycle_run
                        .insert(key, Instant::now());
                }
            }
            tracing::trace!(tick_secs = TICK.as_secs(), "scheduler tick");
            thread::sleep(TICK);
        }
    })
}

/// Operational side effects after a successful cycle for one target:
/// once-per-UTC-day retention pruning and throttled feed publication. Both
/// are best effort — a failure is logged and retried on a later cycle.
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

//! Poll-loop scheduler: a plain thread that runs ingestion cycles per target
//! on the configured cadence (D13: no async in or around the core).

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
    config::TrackerConfig,
    domain::PayloadKind,
    services::cycle::IngestCycleService,
};

/// How often the loop wakes up to check whether a cycle is due.
const TICK: Duration = Duration::from_secs(30);

/// Spawns the background poll loop that runs ingestion cycles for every
/// configured target on its configured cadence. The thread runs forever on a
/// 30-second tick and emits per-cycle outcomes via `tracing` (info on
/// success, warn on failures — the warn path is what surfaces in Sentry).
/// A panicking cycle is caught and logged so one bad fetch can never
/// silently kill ingestion while the web UI keeps serving.
///
/// # Errors
/// Returns the [`std::io::Error`] from [`thread::Builder::spawn`] when the OS
/// refuses to start the scheduler thread.
pub fn spawn(
    config: &TrackerConfig,
    cycle: Arc<IngestCycleService>,
) -> std::io::Result<thread::JoinHandle<()>> {
    let config = config.clone();
    thread::Builder::new().name("amtracker-scheduler".into()).spawn(move || {
        let last_run: Mutex<HashMap<(String, PayloadKind), Instant>> = Mutex::new(HashMap::new());
        loop {
            for target in &config.targets {
                for (kind, interval) in [
                    (PayloadKind::Commodities, target.poll.commodities_secs),
                    (PayloadKind::Auctions, target.poll.auctions_secs),
                ] {
                    let key = (target.id.0.clone(), kind);
                    // The critical sections below only touch the map, so the
                    // lock can only be poisoned by a panic inside this very
                    // loop; recover the guard instead of panicking on poison.
                    let due =
                        match last_run.lock().unwrap_or_else(PoisonError::into_inner).get(&key) {
                            Some(last) => last.elapsed().as_secs() >= interval,
                            None => true,
                        };
                    if !due {
                        continue;
                    }
                    match panic::catch_unwind(AssertUnwindSafe(|| cycle.run_cycle(target, kind))) {
                        Ok(Ok(report)) => tracing::info!(
                            target = %report.target.0,
                            kind = kind.as_str(),
                            outcome = ?report.outcome,
                            entries = report.entries,
                            dropped = report.dropped,
                            folds = report.items_folded,
                            sale_credits = report.sale_credits,
                            "ingest cycle complete"
                        ),
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
                    last_run
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(key, Instant::now());
                }
            }
            tracing::trace!(tick_secs = TICK.as_secs(), "scheduler tick");
            thread::sleep(TICK);
        }
    })
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

//! Poll-loop scheduler: a plain thread that runs ingestion cycles per target
//! on the configured cadence (D13: no async in or around the core).

use std::{
    collections::HashMap,
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
/// 30-second tick and logs per-cycle outcomes to stderr.
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
                    match cycle.run_cycle(target, kind) {
                        Ok(report) => eprintln!(
                            "[scheduler] {} {}: {:?} — {} entries (+{} dropped), {} sale credits",
                            report.target,
                            kind.as_str(),
                            report.outcome,
                            report.entries,
                            report.dropped,
                            report.sale_credits,
                        ),
                        Err(error) => {
                            eprintln!(
                                "[scheduler] {} {}: error: {error}",
                                target.id,
                                kind.as_str()
                            );
                        }
                    }
                    last_run
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(key, Instant::now());
                }
            }
            thread::sleep(TICK);
        }
    })
}

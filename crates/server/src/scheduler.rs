//! Poll-loop scheduler: a plain thread that runs ingestion cycles per target
//! on the configured cadence (D13: no async in or around the core).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use amtracker_core::config::TrackerConfig;
use amtracker_core::domain::PayloadKind;
use amtracker_core::services::cycle::IngestCycleService;

/// How often the loop wakes up to check whether a cycle is due.
const TICK: Duration = Duration::from_secs(30);

pub fn spawn(config: &TrackerConfig, cycle: Arc<IngestCycleService>) -> thread::JoinHandle<()> {
    let config = config.clone();
    thread::Builder::new()
        .name("amtracker-scheduler".into())
        .spawn(move || {
            let last_run: Mutex<HashMap<(String, PayloadKind), Instant>> =
                Mutex::new(HashMap::new());
            loop {
                for target in &config.targets {
                    for (kind, interval) in [
                        (PayloadKind::Commodities, target.poll.commodities_secs),
                        (PayloadKind::Auctions, target.poll.auctions_secs),
                    ] {
                        let key = (target.id.0.clone(), kind);
                        let due = match last_run.lock().unwrap().get(&key) {
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
                            Err(error) => eprintln!(
                                "[scheduler] {} {}: error: {error}",
                                target.id,
                                kind.as_str()
                            ),
                        }
                        last_run.lock().unwrap().insert(key, Instant::now());
                    }
                }
                thread::sleep(TICK);
            }
        })
        .expect("spawn scheduler thread")
}

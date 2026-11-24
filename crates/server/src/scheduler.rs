//! Poll-loop scheduler: a plain thread that runs ingestion cycles per target
//! on the configured cadence, while the operational side effects that belong
//! to ingestion — raw-listing retention (`prune_raw_before`), feed
//! publication (the files `/feed.lua` serves), and the prepared
//! opportunity/overview scans (`OpportunityService::refresh`) — run on a
//! companion maintenance thread fed by a deduplicated per-target queue, so a
//! slow publish or prune can never delay the next target's fetch. Name
//! resolution for every id ever folded runs on its own warmer thread (an
//! all-time listing once at boot, then only what folded since the previous
//! pass), decoupled from the ingest cadence. Requests serve prepared data
//! with no store reads; a plain `serve` stays self-contained — no cron, no
//! exporter sidecar.
//!
//! D13: no async in or around the core; everything below is blocking and
//! runs on these three dedicated threads.

use std::{
    any::Any,
    collections::{
        BTreeSet,
        HashMap,
        HashSet,
        VecDeque,
    },
    panic::{
        self,
        AssertUnwindSafe,
    },
    path::Path,
    sync::{
        atomic::{
            AtomicBool,
            Ordering,
        },
        Arc,
        Condvar,
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
    config::VacuumPolicy,
    domain::{
        utc_day,
        Category,
        FeedFormat,
        Flavor,
        ItemRef,
        PayloadKind,
        TargetId,
    },
    error::StoreError,
    services::{
        feed_export::ExportOptions,
        opportunity::SCAN_TTL_SECONDS,
    },
};

use crate::web::AppState;

/// How often the loop wakes up to check whether a cycle is due.
const TICK: Duration = Duration::from_secs(30);

/// How long an idle maintenance worker waits before waking up anyway: due
/// retention prunes depend on UTC day rolls, which enqueue nothing.
const MAINTENANCE_IDLE_TICK: Duration = Duration::from_secs(60);

/// How long the name warmer waits after a pass that had nothing to fetch./// Passes that fetched (or hit the batch cap) loop immediately, so a fresh
/// backlog drains at the pacing pace instead of one batch per tick.
const WARMER_IDLE_TICK: Duration = Duration::from_secs(30);

/// How long the warmer trusts the catalog's "needs no lookup" answer for an
/// identity before re-checking it. The only way a complete row needs the
/// source again is the 30-day enrichment window, so a day is fine-grained
/// enough for that heal while keeping the re-check load negligible.
const WARMER_REVERIFY_SECONDS: i64 = 24 * 60 * 60;

/// Known identities re-checked per pass on the round-robin ring (see
/// [`TargetWarmer`]): a handful of point reads, so re-verification never
/// needs a whole-market listing.
const WARMER_REVERIFY_PER_PASS: usize = 64;

/// Freelist fraction above which the incremental pass returns pages to the
/// filesystem. Below it, freed pages are simply reused by future writes, so
/// reclaiming would churn without shrinking the file.
const VACUUM_FREE_PAGE_RATIO: f64 = 0.2;

/// Backoff applied to a rate-limited (target, kind) when the source sent no
/// parseable `Retry-After` hint.
const DEFAULT_BACKOFF_SECS: u64 = 60;

/// Upper bound for a wire-provided `Retry-After` hint. A hostile or broken
/// source can send an absurd value (e.g. `u64::MAX`); `Instant::now() +
/// Duration::from_secs(secs)` panics on overflow, and the backoff insertion
/// site sits OUTSIDE the cycle's `catch_unwind` — an unclamped hint could
/// therefore kill the scheduler thread. One day is longer than any sane
/// rate-limit window.
const MAX_RETRY_AFTER_SECS: u64 = 24 * 60 * 60;

/// Clamps a parsed `Retry-After` hint to a safe backoff duration: no hint
/// falls back to [`DEFAULT_BACKOFF_SECS`], any hint is capped at
/// [`MAX_RETRY_AFTER_SECS`] so the `Instant + Duration` insertion below can
/// never overflow and panic the scheduler thread.
fn retry_after_secs(retry_after: Option<u64>) -> u64 {
    match retry_after {
        None => DEFAULT_BACKOFF_SECS,
        Some(secs) => secs.min(MAX_RETRY_AFTER_SECS),
    }
}

/// Store-wide online-backup hook: writes a consistent copy of the database to
/// the given path. Injected by the composition root (which owns the concrete
/// store) so the scheduler — and this crate — never needs the adapter type.
pub type BackupHook = Arc<dyn Fn(&Path) -> Result<(), StoreError> + Send + Sync>;

/// Per-target ingest-loop state: last cycle run, rate-limit backoff, and
/// which cycles are in flight. Written by the scheduler (and the manual
/// `/api/fetch` path), read by the dashboard and `/api/schedule` — shared
/// through [`crate::web::AppState::timers`].
#[derive(Default)]
pub struct TargetTimers {
    /// Last cycle run per (target, kind): the next due lands one interval
    /// later. Seeded with the boot jitter so the first due is spread across
    /// the interval.
    pub cycle_run: HashMap<(String, PayloadKind), Instant>,
    /// Until when a (target, kind) is rate-limit-backed off (skipped entirely).
    pub backoff_until: HashMap<(String, PayloadKind), Instant>,
    /// Cycles currently in flight (scheduler or manual fetch).
    pub running: HashSet<(String, PayloadKind)>,
}

/// One (target, kind)'s next-fetch schedule as the dashboard and
/// `/api/schedule` render it: when the poll becomes due, whether it is
/// parked by a rate-limit backoff, and whether a cycle is in flight right
/// now.
#[derive(Clone, Debug)]
pub struct ScheduleEntry {
    /// Target id.
    pub target: String,
    /// Payload kind polled on this schedule.
    pub kind: PayloadKind,
    /// Configured poll interval for this (target, kind).
    pub interval_secs: u64,
    /// Seconds until the cycle becomes due (`0` = due now or overdue).
    pub next_due_in_secs: u64,
    /// Seconds remaining of a rate-limit park, when parked.
    pub backoff_in_secs: Option<u64>,
    /// Whether a cycle for this (target, kind) is in flight right now.
    pub running: bool,
}

/// Snapshots the fetch schedule for every configured (target, kind): a pure
/// read over the scheduler's timers so the dashboard rows and the JSON
/// endpoint share one computation. `now` is injected for testability.
#[must_use]
pub fn schedule_view(
    targets: &[amtracker_core::config::TargetConfig],
    timers: &TargetTimers,
    now: Instant,
) -> Vec<ScheduleEntry> {
    let mut out = Vec::new();
    for target in targets {
        for (kind, interval) in [
            (PayloadKind::Commodities, target.poll.commodities_secs),
            (PayloadKind::Auctions, target.poll.auctions_secs),
        ] {
            let key = (target.id.0.clone(), kind);
            let next_due_in_secs = timers.cycle_run.get(&key).map_or(0, |last| {
                let ran = now.checked_duration_since(*last).unwrap_or_default();
                interval.saturating_sub(ran.as_secs())
            });
            let backoff_in_secs = timers
                .backoff_until
                .get(&key)
                .and_then(|until| until.checked_duration_since(now).map(|left| left.as_secs()));
            out.push(ScheduleEntry {
                target: target.id.0.clone(),
                kind,
                interval_secs: interval,
                next_due_in_secs,
                backoff_in_secs,
                running: timers.running.contains(&key),
            });
        }
    }
    out
}

/// Maintenance-worker bookkeeping: last retention-prune day per target and
/// knob (each step is independent — one failing prune must not block the
/// others' watermarks) and last feed publication per target (the exact state
/// the ingest loop used to carry).
#[derive(Default)]
struct MaintenanceState {
    prune_day: HashMap<String, i64>,
    sight_prune_day: HashMap<String, i64>,
    hour_prune_day: HashMap<String, i64>,
    feed_publish: HashMap<String, Instant>,
    /// UTC day the store-wide space-reclamation pass last ran.
    vacuum_day: Option<i64>,
}

/// Deduplicated maintenance queue: at most one pending pass per target — a
/// second cycle for the same target collapses into the still-queued entry
/// (the pass re-reads everything fresh anyway).
#[derive(Default)]
struct MaintenanceQueue {
    pending: HashSet<String>,
    queue: VecDeque<String>,
}

impl MaintenanceQueue {
    fn push(&mut self, target: String) {
        if self.pending.insert(target.clone()) {
            self.queue.push_back(target);
        }
    }

    /// Pops the next queued target, waiting up to `timeout` for one to
    /// arrive; `None` = nothing queued within the wait (idle tick).
    fn pop_timeout(mutex: &Mutex<Self>, condvar: &Condvar, timeout: Duration) -> Option<String> {
        let guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut guard, _) = condvar
            .wait_timeout_while(guard, timeout, |queue: &mut Self| queue.queue.is_empty())
            .unwrap_or_else(PoisonError::into_inner);
        guard.queue.pop_front().inspect(|target| {
            guard.pending.remove(target);
        })
    }
}

/// The queue plus its condition variable, shared between the ingest loop
/// (producer) and the maintenance worker (consumer).
type SharedMaintenanceQueue = Arc<(Mutex<MaintenanceQueue>, Condvar)>;

/// Queues one maintenance pass for a target, deduplicated per target.
fn enqueue_maintenance(queue: &SharedMaintenanceQueue, target: &str) {
    let (mutex, condvar) = &**queue;
    let mut guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
    guard.push(target.to_string());
    drop(guard);
    condvar.notify_one();
}

/// Deterministic boot jitter for one (target, kind): a stable FNV-1a hash of
/// the key scaled into `[0, interval)` so many targets never stampede the
/// source at startup — the same spread persists across restarts.
fn boot_jitter(key: &(String, PayloadKind), interval: u64) -> Duration {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.0.as_bytes().iter().chain(key.1.as_str().as_bytes()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Duration::from_secs(hash % interval.max(1))
}

/// Seeds every (target, kind)'s first-fire timer with its boot jitter and
/// logs the schedule so source-side gaps are explainable from the logs
/// alone: a target that has not fetched yet is queued, not stuck.
fn seed_first_fire(config: &amtracker_core::config::TrackerConfig, timers: &Mutex<TargetTimers>) {
    let mut timers = timers.lock().unwrap_or_else(PoisonError::into_inner);
    for target in &config.targets {
        for (kind, interval) in [
            (PayloadKind::Commodities, target.poll.commodities_secs),
            (PayloadKind::Auctions, target.poll.auctions_secs),
        ] {
            let key = (target.id.0.clone(), kind);
            let offset = boot_jitter(&key, interval);
            // Stamp "last run" so the first due lands after `offset`:
            // elapsed is (interval - offset) now and grows. The checked
            // subtraction cannot underflow (offset < interval), but a
            // fallback of "due now" stays correct anyway.
            let seeded = (Instant::now() + offset)
                .checked_sub(Duration::from_secs(interval))
                .unwrap_or_else(Instant::now);
            timers.cycle_run.insert(key.clone(), seeded);
            timers.backoff_until.remove(&key);
            tracing::info!(
                target = %target.id.0,
                kind = kind.as_str(),
                first_fetch_in_secs = offset.as_secs(),
                "first fetch scheduled"
            );
        }
    }
}

/// Spawns the companion maintenance worker thread and returns its queue.
/// The handle is dropped (the worker lives for the process like the
/// scheduler itself; a failure to spawn fails `serve` startup).
fn spawn_maintenance_worker(state: &Arc<AppState>) -> std::io::Result<SharedMaintenanceQueue> {
    let queue: SharedMaintenanceQueue =
        Arc::new((Mutex::new(MaintenanceQueue::default()), Condvar::new()));
    let worker_state = state.clone();
    let worker_queue = queue.clone();
    thread::Builder::new().name("amtracker-maintenance".into()).spawn(move || {
        maintenance_loop(&worker_state, &worker_queue);
    })?;
    Ok(queue)
}

/// Spawns the background poll loop: ingestion cycles for every configured
/// target on its configured cadence, prepared-scan TTL refreshes, and — when
/// `storage.backup_dir` is configured with a positive `backup_secs` — a
/// store-wide online backup through `hook` (retention-pruned to
/// `storage.backup_keep`). Successful cycles hand their side effects (feed
/// publication, retention pruning, scan refresh, alert evaluation) to the
/// maintenance worker instead of running them inline. Outcomes are emitted
/// via `tracing` (info on success, warn on failures — the warn path is what
/// surfaces in Sentry). A panicking cycle is caught and logged so one bad
/// fetch can never silently kill ingestion while the web UI keeps serving.
///
/// `backup` is the composition root's store-backed writer; `None` simply
/// disables the backup branch (the CLI never schedules backups).
///
/// # Errors
///
/// Returns the [`std::io::Error`] from [`thread::Builder::spawn`] when the OS
/// refuses to start one of the threads.
pub fn spawn(
    state: Arc<AppState>,
    backup: Option<BackupHook>,
) -> std::io::Result<thread::JoinHandle<()>> {
    // Backfill first, and latched before any other thread exists: retention
    // prunes must not delete the raw rows the backfill is folding from, and
    // `run_prune_if_due` defers while the latch is held. Everything else the
    // maintenance worker owns (feed publication, alerts, scan refresh) runs
    // right away — only the prunes wait the backfill out. The handle is
    // dropped on purpose: the pass runs detached, like the warmer.
    spawn_item_hour_backfill(&state, HourBackfillLatch::acquire())?;
    let maintenance = spawn_maintenance_worker(&state)?;
    spawn_name_warmer(&state)?;
    thread::Builder::new().name("amtracker-scheduler".into()).spawn(move || {
        let config = state.config.clone();
        // Prepare every configured target's opportunity scan before the first
        // request arrives; best effort — a failing scan logs a warning and the
        // target's scan simply stays cold (the request path falls back to an
        // inline compute), so `serve` never fails startup over it.
        for target in &config.targets {
            for category in Category::ALL {
                survive("startup-refresh", &target.id.0, || {
                    refresh_opportunities(&state, &target.id, category);
                });
            }
        }
        // The critical sections below only touch the maps, so a poisoned
        // lock can only come from a panic inside this very loop; recover
        // the guard instead of panicking on poison. The board is shared
        // with the web layer, which renders the fetch schedule from it.
        // Stagger the first fire of every (target, kind) across its own
        // interval: without this, startup fires all targets back-to-back.
        seed_first_fire(&config, &state.timers);
        let mut last_backup: Option<Instant> = None;
        loop {
            for target in &config.targets {
                for (kind, interval) in [
                    (PayloadKind::Commodities, target.poll.commodities_secs),
                    (PayloadKind::Auctions, target.poll.auctions_secs),
                ] {
                    let key = (target.id.0.clone(), kind);
                    {
                        let mut locked =
                            state.timers.lock().unwrap_or_else(PoisonError::into_inner);
                        if locked
                            .backoff_until
                            .get(&key)
                            .is_some_and(|until| Instant::now() < *until)
                        {
                            tracing::debug!(
                                target = %target.id.0,
                                kind = kind.as_str(),
                                "cycle skipped: rate-limit backoff active"
                            );
                            continue;
                        }
                        let due = match locked.cycle_run.get(&key) {
                            Some(last) => last.elapsed().as_secs() >= interval,
                            None => true,
                        };
                        if !due {
                            continue;
                        }
                        locked.running.insert(key.clone());
                    }
                    run_cycle_logged(&state, &state.timers, &maintenance, target, kind);
                    {
                        let mut locked =
                            state.timers.lock().unwrap_or_else(PoisonError::into_inner);
                        locked.running.remove(&key);
                        locked.cycle_run.insert(key, Instant::now());
                    }
                }
            }
            // Refresh any prepared opportunity scan that outlived its TTL;
            // the debounce window in `refresh` makes overlap with the
            // maintenance pass above harmless. Scans are per category, so
            // each category refreshes on its own cadence.
            for target in &config.targets {
                for category in Category::ALL {
                    let refresh_due = state
                        .opportunities
                        .snapshot_age_secs(&target.id, category)
                        .is_none_or(|age| age >= SCAN_TTL_SECONDS);
                    if refresh_due {
                        survive("scan-refresh", &target.id.0, || {
                            refresh_opportunities(&state, &target.id, category);
                        });
                    }
                }
            }
            // Store-wide online backup on its own cadence (disabled unless
            // both `backup_dir` and a positive `backup_secs` are set).
            if let Some(hook) = &backup {
                if config.storage.backup_dir.is_some() && config.storage.backup_secs > 0 {
                    let due = last_backup
                        .is_none_or(|last| last.elapsed().as_secs() >= config.storage.backup_secs);
                    if due {
                        run_backup_logged(&state, &config, hook);
                        last_backup = Some(Instant::now());
                    }
                }
            }
            tracing::trace!(tick_secs = TICK.as_secs(), "scheduler tick");
            thread::sleep(TICK);
        }
    })
}

/// Runs one scheduled online backup under `catch_unwind`: writes
/// `amtracker-backup-<UTC stamp>.sqlite3` into `storage.backup_dir`, then
/// prunes the oldest backup files down to `storage.backup_keep`. Failures are
/// logged and retried on the next cadence tick.
fn run_backup_logged(
    state: &Arc<AppState>,
    config: &amtracker_core::config::TrackerConfig,
    hook: &BackupHook,
) {
    survive("backup", "-", || {
        let Some(dir) = &config.storage.backup_dir else {
            return;
        };
        std::fs::create_dir_all(dir)
            .map_err(
                |e| tracing::warn!(error = %e, dir = %dir.display(), "backup dir create failed"),
            )
            .ok();
        let stamp = time::OffsetDateTime::from_unix_timestamp(state.clock.now())
            .ok()
            .and_then(|ts| {
                ts.format(&time::macros::format_description!(
                    "[year][month][day]T[hour][minute][second]Z"
                ))
                .ok()
            })
            .unwrap_or_else(|| state.clock.now().to_string());
        let mut path = dir.join(format!("amtracker-backup-{stamp}.sqlite3"));
        if path.exists() {
            // Same-second collision: disambiguate with the pid.
            path = dir.join(format!("amtracker-backup-{stamp}-{}.sqlite3", std::process::id()));
        }
        match hook(&path) {
            Ok(()) => {
                let bytes = std::fs::metadata(&path).map_or(0, |m| m.len());
                tracing::info!(path = %path.display(), bytes, "scheduled backup written");
                prune_backups(dir, config.storage.backup_keep);
            }
            Err(error) => {
                tracing::warn!(error = %error, path = %path.display(), "scheduled backup failed");
            }
        }
    });
}

/// Keeps the newest `keep` files named `amtracker-backup-*.sqlite3` in `dir`
/// (names sort chronologically); anything older is removed. Best effort:
/// listing or unlink failures are logged, never fatal.
fn prune_backups(dir: &Path, keep: u32) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
                name.starts_with("amtracker-backup-") && name.ends_with(".sqlite3")
            })
        })
        .collect();
    backups.sort();
    // `usize::from(u32)` does not exist (lossy on 16-bit targets); the
    // TryFrom conversion is total on everything we support.
    let keep = usize::try_from(u64::from(keep)).unwrap_or(usize::MAX);
    if backups.len() <= keep {
        return;
    }
    let stale = backups.len() - keep;
    for path in backups.into_iter().take(stale) {
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::debug!(path = %path.display(), "old backup pruned"),
            Err(error) => {
                tracing::warn!(error = %error, path = %path.display(), "backup prune failed");
            }
        }
    }
}

/// Runs one due ingest cycle under `catch_unwind`, logging the outcome and
/// recording it in the dashboard's recent-cycles ring, then queues the
/// post-cycle side effects on the maintenance worker on success. Panics never
/// kill the scheduler: the panic hook logs the detail, the ring shows it on
/// the dashboard, and the next interval retries (the caller still stamps the
/// run timer).
fn run_cycle_logged(
    state: &Arc<AppState>,
    timers: &Mutex<TargetTimers>,
    maintenance: &SharedMaintenanceQueue,
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
            enqueue_maintenance(maintenance, &target.id.0);
        }
        Ok(Err(error)) => {
            // Rate limiting is transient and self-describing: park the
            // (target, kind) until the hinted moment (or a conservative
            // default) instead of retrying on the regular interval.
            if let amtracker_core::error::CycleError::Source(
                amtracker_core::error::SourceError::RateLimited { retry_after },
            ) = &error
            {
                let secs = retry_after_secs(*retry_after);
                timers.lock().unwrap_or_else(PoisonError::into_inner).backoff_until.insert(
                    (target.id.0.clone(), kind),
                    Instant::now() + Duration::from_secs(secs),
                );
                tracing::info!(
                    target = %target.id.0,
                    kind = kind.as_str(),
                    parked_for_secs = secs,
                    "rate limited; cycle parked until backoff expiry"
                );
            }
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

/// The maintenance worker: drains per-target maintenance requests (retention
/// pruning, feed publication, opportunity refresh, alert evaluation) off the
/// ingest loop, and on idle ticks sweeps every target for due prunes so UTC
/// day rolls never depend on a cycle happening to finish. A pass is best
/// effort end to end — a panicking step is caught and the worker lives on.
fn maintenance_loop(state: &Arc<AppState>, queue: &SharedMaintenanceQueue) {
    let config = state.config.clone();
    let mut maintenance = MaintenanceState::default();
    loop {
        if let Some(target) =
            MaintenanceQueue::pop_timeout(&queue.0, &queue.1, MAINTENANCE_IDLE_TICK)
        {
            survive("maintenance-pass", &target, || {
                run_pass(state, &config, &mut maintenance, &target);
            });
        }
        for target in &config.targets {
            survive("maintenance-sweep", &target.id.0, || {
                run_prune_if_due(state, &config, &mut maintenance, &target.id.0);
            });
        }
        // Store-wide, once per UTC day, after the prunes that create the
        // free pages it reclaims.
        survive("vacuum", "-", || {
            run_vacuum_if_due(state, &config, &mut maintenance);
        });
    }
}

/// Operational side effects after a successful cycle for one target.
/// Alerts run FIRST: a fresh dump may carry a listing that is gone minutes
/// later, and a sniper alert is worth nothing queued behind a whole-market
/// feed build and three scan refreshes — it only needs the cycle's own
/// writes (snapshot floors, day folds, sightings), which have already
/// landed. Then once-per-UTC-day retention pruning, throttled feed
/// publication, and a refresh of the target's prepared opportunity scan
/// (new folds/credits just landed). All are best effort — a failure is
/// logged and retried on a later pass. (Name warm-up is not among them: it
/// runs on its own dedicated warmer thread, decoupled from the ingest
/// cadence.)
fn run_pass(
    state: &Arc<AppState>,
    config: &amtracker_core::config::TrackerConfig,
    maintenance: &mut MaintenanceState,
    target: &str,
) {
    evaluate_alerts(state, target);

    run_prune_if_due(state, config, maintenance, target);

    let publish_due = maintenance
        .feed_publish
        .get(target)
        .is_none_or(|last| last.elapsed().as_secs() >= config.server.feed_publish_secs);
    if publish_due {
        maintenance.feed_publish.insert(target.to_string(), Instant::now());
        publish_feed(state, config, target);
    }

    // New folds/credits just landed for this cycle's payload kind, but
    // categories are refreshed wholesale (cheap, debounced per category).
    for category in Category::ALL {
        refresh_opportunities(state, &TargetId(target.to_string()), category);
    }
}

/// Spawns the dedicated background name-warmer thread. `warm_batch == 0`
/// disables it entirely (logged, not spawned). A spawn failure fails `serve`
/// startup, like the other threads.
fn spawn_name_warmer(state: &Arc<AppState>) -> std::io::Result<()> {
    if state.config.catalog.warm_batch == 0 {
        tracing::info!("background name warmer disabled ([catalog].warm_batch = 0)");
        return Ok(());
    }
    let worker = state.clone();
    thread::Builder::new().name("amtracker-warmer".into()).spawn(move || {
        warmer_loop(&worker);
    })?;
    Ok(())
}

/// `true` while the one-shot hour-fold backfill runs. Retention prunes must
/// not delete the raw rows a backfill is folding from (a day is deleted in
/// batches, so a racing rebuild would fold it from a partial raw set), so
/// `run_prune_if_due` — the one prune entry point behind both the
/// maintenance worker and the manual `/api/fetch` path — defers and retries
/// later while it is held. `spawn` acquires it before the backfill thread
/// (or any prune caller) exists, so there is no window in which a prune can
/// run under an unlatched backfill; the guard rides into the thread and
/// clears it when the pass ends.
static HOUR_BACKFILL_RUNNING: AtomicBool = AtomicBool::new(false);

/// RAII clear for [`HOUR_BACKFILL_RUNNING`] — dropping (including unwinding
/// from a panic) releases the latch, so a panicking pass can never disable
/// retention pruning forever.
struct HourBackfillLatch;

impl HourBackfillLatch {
    fn acquire() -> Self {
        HOUR_BACKFILL_RUNNING.store(true, Ordering::Release);
        HourBackfillLatch
    }
}

impl Drop for HourBackfillLatch {
    fn drop(&mut self) {
        HOUR_BACKFILL_RUNNING.store(false, Ordering::Release);
    }
}

/// One-shot boot migration: rebuild the intraday hour fold from whatever raw
/// window still exists, so a deployment that just gained the fold (or a
/// backfill that was interrupted) starts the when-to-buy hour view with the
/// raw window's history instead of an empty chart. The pass is idempotent
/// and exact: hours already holding cells are skipped, and every rebuilt hour
/// is one atomic statement over its complete raw set — so re-running on
/// every boot is safe and steady-state boots pay almost nothing. Discovery is
/// bounded to the rebuildable window (raw rows ∩ fold retention): snapshot
/// rows live forever, and every hour beyond that window would otherwise be
/// re-examined on every boot. It runs on its own thread because a large raw
/// window can take minutes, and startup must not wait for it; `latch` is the
/// guard `spawn` acquired, so retention prunes stay deferred for the pass.
fn spawn_item_hour_backfill(
    state: &Arc<AppState>,
    latch: HourBackfillLatch,
) -> std::io::Result<thread::JoinHandle<()>> {
    let worker = state.clone();
    thread::Builder::new().name("amtracker-hour-backfill".into()).spawn(move || {
        let _latch = latch;
        let since_day =
            utc_day(worker.clock.now()) - worker.config.storage.fold_rebuild_window_days() + 1;
        let started = std::time::Instant::now();
        tracing::info!(since_day, "item hour fold backfill started (reconciled hours are skipped)");
        match worker.history.backfill_item_hours(since_day) {
            Ok(cells) => {
                tracing::info!(
                    cells,
                    elapsed_secs = started.elapsed().as_secs(),
                    "item hour fold reconciled with the raw window"
                );
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "item hour fold backfill failed (rerun `backfill-item-hours` to retry)"
                );
            }
        }
    })
}

/// The warmer thread's loop: run one [`NameWarmer::pass`], then either loop
/// immediately (the pass fetched, or stopped on the batch cap with work
/// left — a fresh backlog drains continuously at the pacing pace) or sleep
/// [`WARMER_IDLE_TICK`] (nothing actionable — steady state). A panicking
/// pass is caught and logged; the thread lives on.
fn warmer_loop(state: &Arc<AppState>) {
    let config = state.config.clone();
    tracing::info!(
        batch = config.catalog.warm_batch,
        pace_ms = config.catalog.warm_pace_ms,
        "background name warmer started (all-time listing once, then incremental)"
    );
    let mut warmer = NameWarmer::new();
    loop {
        let pass = panic::catch_unwind(AssertUnwindSafe(|| warmer.pass(state))).unwrap_or_else(
            |payload| {
                tracing::error!(
                    panic = %panic_message(&payload),
                    "name warmer pass panicked; continuing"
                );
                WarmPassStats::default()
            },
        );
        if pass.fetched > 0 {
            tracing::info!(
                fetched = pass.fetched,
                deferred = pass.deferred,
                candidates = pass.candidates,
                "name warmer pass"
            );
        } else if pass.deferred > 0 {
            tracing::debug!(
                deferred = pass.deferred,
                candidates = pass.candidates,
                "name warmer pass: every actionable id is deferred by the negative/error cache"
            );
        } else if pass.candidates > 0 {
            tracing::debug!(
                candidates = pass.candidates,
                "name warmer pass: every id is already named"
            );
        }
        if pass.fetched == 0 && !pass.more_work {
            thread::sleep(WARMER_IDLE_TICK);
        }
    }
}

/// What one warmer pass did, for logging and tests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WarmPassStats {
    /// Ids examined against the catalog this pass: newly listed, re-checked
    /// on the ring, and carried over from earlier passes.
    pub candidates: usize,
    /// Ids resolved from the source this pass (budget-limited by
    /// `[catalog].warm_batch`).
    pub fetched: u32,
    /// Ids skipped because the negative/error cache still defers them.
    pub deferred: u32,
    /// The batch cap stopped the pass with actionable ids left.
    pub more_work: bool,
}

/// The catalog identity a name lookup is about: an item id, or a pet
/// SPECIES — level/quality/breed never matter to the catalog, so every
/// variant of a species collapses onto one key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum NameKey {
    Item(i64),
    Species(i64),
}

impl NameKey {
    fn of(item: &ItemRef) -> Self {
        match *item {
            ItemRef::Item { item_id } => Self::Item(item_id),
            ItemRef::Pet { species, .. } => Self::Species(species),
        }
    }
}

/// Whether a lookup of `key` would hit the source right now: the id is
/// unseen, or its stored row is past the enrichment window while still
/// missing fields.
fn needs_lookup(state: &AppState, flavor: &Flavor, key: NameKey) -> Result<bool, StoreError> {
    match key {
        NameKey::Item(item_id) => state.catalog.needs_enrichment(flavor, item_id),
        NameKey::Species(species) => state.catalog.known_pet(flavor, species).map(|known| !known),
    }
}

/// Whether the catalog recently answered "missing" or failed for `key`.
fn is_deferred(state: &AppState, flavor: &Flavor, key: NameKey) -> bool {
    match key {
        NameKey::Item(item_id) => state.catalog.deferred(flavor, item_id),
        NameKey::Species(species) => state.catalog.deferred_pet(flavor, species),
    }
}

/// Resolves `key` from the source. `Ok(true)` = the catalog holds it now;
/// `Ok(false)` = the source has no record (remembered negatively).
fn lookup(state: &AppState, flavor: &Flavor, key: NameKey) -> Result<bool, StoreError> {
    match key {
        NameKey::Item(item_id) => {
            state.catalog.ensure(flavor, item_id).map(|record| record.is_some())
        }
        NameKey::Species(species) => {
            state.catalog.ensure_pet(flavor, species).map(|record| record.is_some())
        }
    }
}

/// One identity the catalog last confirmed as needing no source lookup.
struct Verified {
    key: NameKey,
    at: i64,
}

/// Per-target warmer state, carried across passes on the warmer thread.
#[derive(Default)]
struct TargetWarmer {
    /// First day the next listing reads from: `0` on the first pass after
    /// boot (all-time), then `today` — only identities with a fold since the
    /// previous pass are listed again, never the whole history.
    since_day: i64,
    /// Every identity the catalog ever answered for, as a round-robin ring:
    /// each pass visits a few slots and re-checks the stale ones, so a row
    /// that only later qualifies for enrichment (the 30-day window) still
    /// gets its heal without rescanning the folds.
    verified: Vec<Verified>,
    /// `NameKey` → slot in `verified`.
    index: HashMap<NameKey, usize>,
    /// Next ring slot the sweep visits.
    cursor: usize,
    /// Ids that still need a source lookup — unseen, negatively cached,
    /// failed, or cut off by the budget — carried from pass to pass until
    /// resolved.
    pending: BTreeSet<NameKey>,
}

impl TargetWarmer {
    /// Whether the catalog confirmed `key` recently enough to skip it.
    fn is_fresh(&self, key: &NameKey, now: i64) -> bool {
        self.index.get(key).is_some_and(|&slot| {
            self.verified
                .get(slot)
                .is_some_and(|entry| now.saturating_sub(entry.at) <= WARMER_REVERIFY_SECONDS)
        })
    }

    /// Stamps `key` as confirmed at `now`, adding it to the ring when new.
    fn confirm(&mut self, key: NameKey, now: i64) {
        if let Some(&slot) = self.index.get(&key) {
            if let Some(entry) = self.verified.get_mut(slot) {
                entry.at = now;
            }
        } else {
            self.index.insert(key, self.verified.len());
            self.verified.push(Verified { key, at: now });
        }
    }

    /// Advances the sweep by up to [`WARMER_REVERIFY_PER_PASS`] slots (never
    /// more than one lap) and returns the identities whose confirmation is
    /// too old to trust.
    fn due(&mut self, now: i64) -> Vec<NameKey> {
        let len = self.verified.len();
        let mut due = Vec::new();
        for _ in 0..len.min(WARMER_REVERIFY_PER_PASS) {
            self.cursor = (self.cursor + 1) % len;
            if let Some(entry) = self.verified.get(self.cursor) {
                if now.saturating_sub(entry.at) > WARMER_REVERIFY_SECONDS {
                    due.push(entry.key);
                }
            }
        }
        due
    }
}

/// How one target's walk ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Walk {
    /// The pending set was walked to its end (or up to the budget cap).
    Complete,
    /// A source lookup failed: every target would pay the same outage, so
    /// the pass stops here.
    Paused,
}

/// Everything one warmer pass shares across its targets.
struct PassRun {
    stats: WarmPassStats,
    now: i64,
    today: i64,
    budget: u32,
    pace: Duration,
}

/// The background name warmer's state, owned by its thread: one
/// [`TargetWarmer`] per configured target.
#[derive(Default)]
pub struct NameWarmer {
    targets: HashMap<String, TargetWarmer>,
}

impl NameWarmer {
    /// An empty warmer: its first pass lists every target all-time.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One background name-warmer pass over every configured target. The
    /// first pass after boot lists all-time — every identity that ever
    /// produced a fold goes through the catalog — and moves a per-target day
    /// cursor to today; later passes list only identities with a fold since
    /// the previous pass (an index range over the newest day), re-check a
    /// slice of the known-identity ring, and walk the ids still pending from
    /// earlier passes. Best effort: a source outage just pauses resolution
    /// until a later pass (the error cache defers the failing id briefly).
    ///
    /// The budget (`[catalog].warm_batch`) caps *source lookups* per pass,
    /// not examined ids: ids beyond it stay pending and keep their place, so
    /// an unnamed id never starves behind named ones. A listing read failure
    /// leaves that target's cursor untouched and skips it for the pass (the
    /// other targets still run; the next pass retries).
    pub fn pass(&mut self, state: &Arc<AppState>) -> WarmPassStats {
        let config = &state.config;
        let budget = config.catalog.warm_batch;
        if budget == 0 {
            // The warmer thread is not spawned in this mode; guard the direct
            // (test/manual) call path too.
            return WarmPassStats::default();
        }
        let now = state.clock.now();
        let mut run = PassRun {
            stats: WarmPassStats::default(),
            now,
            today: utc_day(now),
            budget,
            pace: Duration::from_millis(config.catalog.warm_pace_ms),
        };
        for target in &config.targets {
            let warmer = self.targets.entry(target.id.0.clone()).or_default();
            if warm_target(state, warmer, target, &mut run) == Walk::Paused {
                break;
            }
        }
        run.stats
    }
}

/// One pass for one target: list the identities with folds since the cursor,
/// re-check a ring slice, then walk every id still needing a lookup under the
/// source budget. Returns early (cursor untouched) when the listing fails, so
/// the next pass retries the same range. `pending` only shrinks once the
/// walk is over, so a panic mid-walk loses no work.
fn warm_target(
    state: &Arc<AppState>,
    warmer: &mut TargetWarmer,
    target: &amtracker_core::config::TargetConfig,
    run: &mut PassRun,
) -> Walk {
    let flavor = &target.flavor;
    let all_time = warmer.since_day == 0;
    let mut listed: Vec<NameKey> = Vec::new();
    for category in Category::ALL {
        match state.history.items_with_data(&target.id, warmer.since_day, category) {
            // The same id can have both commodity and item-AH folds; the
            // pending set dedupes. Recently confirmed ids cost nothing here.
            Ok(list) => listed.extend(list.iter().filter_map(|item| {
                let key = NameKey::of(item);
                (!warmer.is_fresh(&key, run.now)).then_some(key)
            })),
            Err(error) => {
                tracing::warn!(
                    target = %target.id.0,
                    error = %error,
                    "name warmer listing failed; cursor unchanged"
                );
                return Walk::Complete;
            }
        }
    }
    warmer.since_day = run.today;

    // Ring slice: stale confirmations go back to pending. Stamping them now
    // keeps the ring from re-checking an id while pending still owns it.
    for key in warmer.due(run.now) {
        warmer.confirm(key, run.now);
        warmer.pending.insert(key);
    }
    warmer.pending.extend(listed);

    let queue: Vec<NameKey> = warmer.pending.iter().copied().collect();
    let mut resolved = Vec::new();
    let mut unreadable = 0usize;
    let mut walk = Walk::Complete;
    for key in queue {
        run.stats.candidates += 1;
        // Ids remembered as missing or recently failed are skipped outright:
        // re-asking would burn the budget and the pacing sleep without new
        // information, and both windows expire, so they get their turn again.
        if is_deferred(state, flavor, key) {
            run.stats.deferred += 1;
            continue;
        }
        // Only ids whose lookup would hit the source cost a request. An
        // unreadable catalog leaves the id pending; one line per target
        // below covers a store failing wholesale.
        let Ok(needed) = needs_lookup(state, flavor, key) else {
            unreadable += 1;
            continue;
        };
        if !needed {
            warmer.confirm(key, run.now);
            resolved.push(key);
            continue;
        }
        if run.stats.fetched >= run.budget {
            run.stats.more_work = true;
            break;
        }
        match lookup(state, flavor, key) {
            Ok(true) => {
                run.stats.fetched += 1;
                thread::sleep(run.pace);
                warmer.confirm(key, run.now);
                resolved.push(key);
            }
            Ok(false) => {
                // Negatively cached now: it stays pending and reads as
                // deferred until that window lapses.
                run.stats.fetched += 1;
                thread::sleep(run.pace);
            }
            Err(error) => {
                tracing::warn!(
                    target = %target.id.0,
                    error = %error,
                    "name warmer lookup failed; pausing until the next pass"
                );
                walk = Walk::Paused;
                break;
            }
        }
    }
    for key in resolved {
        warmer.pending.remove(&key);
    }
    if unreadable > 0 {
        tracing::warn!(
            target = %target.id.0,
            ids = unreadable,
            "name warmer could not read the catalog; those ids stay pending"
        );
    }
    if all_time {
        tracing::info!(
            target = %target.id.0,
            identities = warmer.verified.len(),
            pending = warmer.pending.len(),
            "name warmer: all-time pass complete; listing incrementally from today"
        );
    }
    walk
}

/// Applies retention pruning when the target has not been pruned today yet.
/// Each knob is an independent step with its own watermark: a failing raw
/// prune must not silently disable the sight and hour-fold prunes (the fold
/// keeps growing at ingest), and a failed step retries on the next sweep.
fn run_prune_if_due(
    state: &Arc<AppState>,
    config: &amtracker_core::config::TrackerConfig,
    maintenance: &mut MaintenanceState,
    target: &str,
) {
    if HOUR_BACKFILL_RUNNING.load(Ordering::Acquire) {
        // The boot backfill is folding the raw window; pruning under it could
        // rebuild a day from a half-deleted raw set. Deferred, not failed —
        // the next sweep (or the next fetch's side effects) retries.
        tracing::debug!(target, "retention deferred: hour-fold backfill running");
        return;
    }
    let today = utc_day(state.clock.now());
    let target_id = TargetId(target.to_string());
    // Raw listings: the working set behind the folds and the snapshot viewer.
    if maintenance.prune_day.get(target) != Some(&today) {
        match state
            .history
            .prune_raw_before(&target_id, today - i64::from(config.storage.raw_listing_days) + 1)
        {
            Ok(removed) => {
                maintenance.prune_day.insert(target.to_string(), today);
                tracing::debug!(target, removed, "raw listing retention applied");
            }
            Err(error) => tracing::warn!(target, error = %error, "raw listing retention failed"),
        }
    }
    // Closed sightings age out on their own knob (0 = keep forever, the
    // historical default): their aggregates already live in the sales
    // ledger, so only the reopen-reversal horizon is lost.
    let sight_retention = i64::from(config.storage.sight_retention_days);
    if sight_retention > 0 && maintenance.sight_prune_day.get(target) != Some(&today) {
        match state.history.prune_sights_before(&target_id, today - sight_retention + 1) {
            Ok(removed) => {
                maintenance.sight_prune_day.insert(target.to_string(), today);
                tracing::debug!(target, removed, "sight retention applied");
            }
            Err(error) => tracing::warn!(target, error = %error, "sight retention failed"),
        }
    }
    // The intraday hour fold ages out on its own knob too (0 = keep
    // forever): it must merely stay at/above the largest when-to-buy
    // intraday preset the UI should serve.
    let hour_retention = i64::from(config.storage.item_hour_retention_days);
    if hour_retention > 0 && maintenance.hour_prune_day.get(target) != Some(&today) {
        match state.history.prune_item_hours_before(&target_id, today - hour_retention + 1) {
            Ok(removed) => {
                maintenance.hour_prune_day.insert(target.to_string(), today);
                tracing::debug!(target, removed, "item hour retention applied");
            }
            Err(error) => tracing::warn!(target, error = %error, "item hour retention failed"),
        }
    }
}

/// Store-wide space reclamation, at most once per UTC day and only after
/// the prune sweep (which creates the free pages it reclaims). With
/// `incremental`, a legacy never-shrink store is converted once (full
/// `VACUUM`, logged loudly), then freed pages are returned when the
/// freelist crosses the fragmentation threshold. With `full`, a complete
/// `VACUUM` runs daily. Every failure warns and waits for the next day.
fn run_vacuum_if_due(
    state: &Arc<AppState>,
    config: &amtracker_core::config::TrackerConfig,
    maintenance: &mut MaintenanceState,
) {
    if config.storage.vacuum == VacuumPolicy::Off {
        return;
    }
    let today = utc_day(state.clock.now());
    if maintenance.vacuum_day == Some(today) {
        return;
    }
    maintenance.vacuum_day = Some(today);
    let store = &state.maintenance;
    match config.storage.vacuum {
        VacuumPolicy::Off => {}
        VacuumPolicy::Full => {
            let Ok(before) = store.space_stats() else {
                tracing::warn!("store space stats failed; skipping vacuum");
                return;
            };
            let started = Instant::now();
            match store.full_vacuum() {
                Ok(()) => {
                    let after = store.space_stats().ok();
                    tracing::info!(
                        duration_secs = started.elapsed().as_secs(),
                        before_bytes = before.total_bytes(),
                        after_bytes = after.as_ref().map_or(
                            before.total_bytes(),
                            amtracker_core::ports::SpaceStats::total_bytes
                        ),
                        "full vacuum complete"
                    );
                }
                Err(error) => {
                    tracing::warn!(error = %error, "full vacuum failed");
                }
            }
        }
        VacuumPolicy::Incremental => {
            // Legacy conversion: a 0 header ignores the pragma until a full
            // VACUUM applies it; this runs exactly once and can take a
            // while on a large store.
            match store.auto_vacuum_mode() {
                Ok(0) => {
                    tracing::info!(
                        "one-time store conversion: full VACUUM to enable incremental \
                         auto-vacuum (blocks writes for its duration)"
                    );
                    match store.enable_incremental_auto_vacuum() {
                        Ok(()) => {
                            if let Ok(after) = store.space_stats() {
                                tracing::info!(
                                    total_bytes = after.total_bytes(),
                                    "store converted to incremental auto-vacuum"
                                );
                            }
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "auto-vacuum conversion failed");
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(error = %error, "auto_vacuum read failed"),
            }
            match store.space_stats() {
                Ok(space) => {
                    if space.free_ratio() >= VACUUM_FREE_PAGE_RATIO {
                        match store.incremental_vacuum() {
                            Ok(()) => tracing::info!(
                                freed_bytes = space.free_bytes(),
                                freed_ratio = %format!("{:.0}%", 100.0 * space.free_ratio()),
                                "incremental vacuum returned freed pages"
                            ),
                            Err(error) => {
                                tracing::warn!(error = %error, "incremental vacuum failed");
                            }
                        }
                    } else {
                        tracing::debug!(
                            total_bytes = space.total_bytes(),
                            free_ratio = %format!("{:.1}%", 100.0 * space.free_ratio()),
                            "store space stats"
                        );
                    }
                }
                Err(error) => tracing::warn!(error = %error, "store space stats failed"),
            }
        }
    }
    if let Err(error) = store.checkpoint_wal() {
        tracing::debug!(error = %error, "WAL checkpoint failed");
    }
}

/// Runs one alert-evaluation pass for a target (post-cycle and post-manual-
/// fetch): disabled configs are a no-op; fired alerts are counted and logged
/// here since the core never logs. Best effort — an evaluation failure warns
/// and waits for the next cycle.
fn evaluate_alerts(state: &Arc<AppState>, target: &str) {
    if !state.config.alerts.enabled {
        return;
    }
    let cooldown = i64::try_from(state.config.alerts.cooldown_secs).unwrap_or(i64::MAX);
    match state.alerts.evaluate(&TargetId(target.to_string()), cooldown) {
        Ok(run) if run.fired > 0 => tracing::info!(
            target,
            evaluated = run.evaluated,
            fired = run.fired,
            dispatched = run.dispatched,
            failures = run.dispatch_failures,
            skipped_stale = run.skipped_stale,
            "alerts evaluated"
        ),
        Ok(run) if run.skipped_stale > 0 => tracing::warn!(
            target,
            evaluated = run.evaluated,
            skipped_stale = run.skipped_stale,
            "alerts skipped: feed data older than [alerts].max_data_age_secs"
        ),
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(target, error = %error, "alert evaluation failed");
        }
    }
}

/// Best-effort panic payload rendering: string payloads print their message,
/// anything else is reported generically rather than `Any`.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Runs the after-cycle side effects for one target right now, without the
/// maintenance worker's once-per-day/throttle bookkeeping: alert evaluation,
/// retention pruning, feed publication, and an opportunity scan refresh,
/// exactly as a successful scheduled cycle would trigger them.
/// Used by the manual `/api/fetch` path, whose successful cycles must show
/// up in `/feed.lua` and the ranking pages immediately instead of waiting
/// for the maintenance worker. (Name resolution needs no trigger here: the
/// dedicated warmer thread picks up new ids on its own.)
pub fn after_cycle_now(state: &Arc<AppState>, target: &str) {
    let config = state.config.clone();
    run_pass(state, &config, &mut MaintenanceState::default(), target);
}

/// Runs one step under `catch_unwind`, logging a panic and continuing.
fn survive(step: &'static str, target: &str, f: impl FnOnce()) {
    if panic::catch_unwind(AssertUnwindSafe(f)).is_err() {
        tracing::error!(step, target, "scheduler step panicked; continuing");
    }
}

/// Recomputes one target's prepared opportunity scan for one category, best
/// effort: success is logged at debug with the items covered, failures warn
/// and are swallowed — ranking data is background hygiene, never worth
/// failing a cycle or the startup over.
fn refresh_opportunities(state: &Arc<AppState>, target: &TargetId, category: Category) {
    match state.opportunities.refresh(target, category) {
        Ok(items) => tracing::debug!(
            target = %target.0,
            category = category.as_str(),
            items,
            "opportunity scan refreshed"
        ),
        Err(error) => {
            tracing::warn!(
                target = %target.0,
                category = category.as_str(),
                error = %error,
                "opportunity scan refresh failed"
            );
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

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect/panic: fixture-driven, deterministic
    // inputs.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn schedule_view_reports_due_running_and_parked_entries() {
        let target =
            |id: &str, commodities: u64, auctions: u64| amtracker_core::config::TargetConfig {
                id: amtracker_core::domain::TargetId(id.into()),
                kind: amtracker_core::config::TargetKind::StaticUrls {
                    commodities_url: "http://example.test/c.json".into(),
                    auctions_url: "http://example.test/a.json".into(),
                },
                auth: None,
                poll: amtracker_core::config::PollConfig {
                    commodities_secs: commodities,
                    auctions_secs: auctions,
                },
                flavor: amtracker_core::domain::Flavor::default(),
            };
        let targets = [target("alpha", 3_600, 1_800), target("beta", 600, 300)];
        let now = std::time::Instant::now();
        let mut timers = TargetTimers::default();
        // alpha/commodities: ran 10 min ago of a 60 min interval → 50 min left.
        timers.cycle_run.insert(
            ("alpha".into(), PayloadKind::Commodities),
            now.checked_sub(std::time::Duration::from_secs(600)).expect("uptime"),
        );
        // alpha/auctions: overdue by 400 s.
        timers.cycle_run.insert(
            ("alpha".into(), PayloadKind::Auctions),
            now.checked_sub(std::time::Duration::from_secs(2_200)).expect("uptime"),
        );
        // beta/commodities: rate-limit parked for 2 min.
        timers.backoff_until.insert(
            ("beta".into(), PayloadKind::Commodities),
            now + std::time::Duration::from_secs(120),
        );
        // beta/auctions: cycle in flight.
        timers.running.insert(("beta".into(), PayloadKind::Auctions));

        let view = schedule_view(&targets, &timers, now);
        assert_eq!(view.len(), 4);
        let by_key = |target: &str, kind: PayloadKind| {
            view.iter().find(|e| e.target == target && e.kind == kind).expect("entry")
        };

        let alpha_c = by_key("alpha", PayloadKind::Commodities);
        assert_eq!(alpha_c.next_due_in_secs, 3_000);
        assert!(!alpha_c.running);
        assert!(alpha_c.backoff_in_secs.is_none());

        let alpha_a = by_key("alpha", PayloadKind::Auctions);
        assert_eq!(alpha_a.next_due_in_secs, 0, "overdue clamps to due now");

        let beta_c = by_key("beta", PayloadKind::Commodities);
        assert_eq!(beta_c.backoff_in_secs, Some(120));
        assert!(!beta_c.running);

        let beta_a = by_key("beta", PayloadKind::Auctions);
        assert!(beta_a.running);
        assert!(beta_a.backoff_in_secs.is_none());
    }

    #[test]
    fn maintenance_queue_dedupes_pending_targets() {
        let mut queue = MaintenanceQueue::default();
        queue.push("a".into());
        queue.push("b".into());
        queue.push("a".into());
        assert_eq!(queue.queue, VecDeque::from(vec!["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn maintenance_queue_pops_fifo_and_releases_the_dedup_guard() {
        let shared: SharedMaintenanceQueue =
            Arc::new((Mutex::new(MaintenanceQueue::default()), Condvar::new()));
        {
            let (mutex, _) = &*shared;
            let mut guard = mutex.lock().unwrap();
            guard.push("a".into());
            guard.push("b".into());
            guard.push("a".into());
        }
        let (mutex, condvar) = &*shared;
        assert_eq!(MaintenanceQueue::pop_timeout(mutex, condvar, Duration::ZERO), Some("a".into()));
        // Popping releases the guard: "a" may queue again immediately.
        {
            let mut guard = mutex.lock().unwrap();
            guard.push("a".into());
            assert_eq!(guard.queue.back(), Some(&"a".to_string()));
        }
        assert_eq!(MaintenanceQueue::pop_timeout(mutex, condvar, Duration::ZERO), Some("b".into()));
        assert_eq!(MaintenanceQueue::pop_timeout(mutex, condvar, Duration::ZERO), Some("a".into()));
        assert_eq!(MaintenanceQueue::pop_timeout(mutex, condvar, Duration::ZERO), None);
    }

    #[test]
    fn missing_retry_after_uses_the_default_backoff() {
        assert_eq!(retry_after_secs(None), DEFAULT_BACKOFF_SECS);
    }

    #[test]
    fn parseable_retry_after_is_honored() {
        assert_eq!(retry_after_secs(Some(30)), 30);
    }

    #[test]
    fn absurd_retry_after_is_clamped_to_one_day() {
        // `u64::MAX` would overflow `Instant + Duration` and panic the
        // scheduler thread (the insertion site is outside `catch_unwind`).
        assert_eq!(retry_after_secs(Some(u64::MAX)), MAX_RETRY_AFTER_SECS);
    }

    #[test]
    fn hour_backfill_latch_is_held_exactly_while_its_guard_lives() {
        assert!(!HOUR_BACKFILL_RUNNING.load(Ordering::Acquire), "no backfill, prunes run");
        let latch = HourBackfillLatch::acquire();
        assert!(
            HOUR_BACKFILL_RUNNING.load(Ordering::Acquire),
            "prunes defer while the backfill's guard lives"
        );
        drop(latch);
        assert!(
            !HOUR_BACKFILL_RUNNING.load(Ordering::Acquire),
            "the pass ended (or unwound): prunes resume"
        );
    }

    #[test]
    fn name_key_collapses_pet_variants_onto_the_species() {
        let young = ItemRef::Pet { species: 264, level: 1, quality: 3, breed: 11 };
        let grown = ItemRef::Pet { species: 264, level: 25, quality: 5, breed: 4 };
        assert_eq!(NameKey::of(&young), NameKey::of(&grown));
        assert_eq!(NameKey::of(&ItemRef::Item { item_id: 264 }), NameKey::Item(264));
        // Species ids are their own namespace: a cataloged item with the same
        // number says nothing about the pet.
        assert_ne!(NameKey::of(&young), NameKey::of(&ItemRef::Item { item_id: 264 }));
    }

    #[test]
    fn ring_recheck_returns_only_stale_confirmations_and_covers_every_slot() {
        let mut warmer = TargetWarmer::default();
        assert!(warmer.due(0).is_empty(), "an empty ring has nothing due");

        let confirmed_at = 100;
        for id in 1..=3 {
            warmer.confirm(NameKey::Item(id), confirmed_at);
        }
        assert!(warmer.due(confirmed_at).is_empty(), "fresh confirmations are not due");
        assert!(warmer.is_fresh(&NameKey::Item(1), confirmed_at + WARMER_REVERIFY_SECONDS));
        assert!(
            !warmer.is_fresh(&NameKey::Item(1), confirmed_at + WARMER_REVERIFY_SECONDS + 1),
            "one second past the window the confirmation no longer counts"
        );

        let later = confirmed_at + WARMER_REVERIFY_SECONDS + 1;
        let mut due = warmer.due(later);
        due.sort_unstable();
        assert_eq!(
            due,
            vec![NameKey::Item(1), NameKey::Item(2), NameKey::Item(3)],
            "a lap re-checks every known identity"
        );

        // Re-confirming one leaves only the others due on the next lap.
        warmer.confirm(NameKey::Item(2), later);
        let mut due = warmer.due(later);
        due.sort_unstable();
        assert_eq!(due, vec![NameKey::Item(1), NameKey::Item(3)]);
    }

    #[test]
    fn confirming_a_known_identity_restamps_its_slot_instead_of_growing_the_ring() {
        let mut warmer = TargetWarmer::default();
        warmer.confirm(NameKey::Species(7), 10);
        warmer.confirm(NameKey::Species(7), 20);
        assert_eq!(warmer.verified.len(), 1);
        assert_eq!(warmer.index.len(), 1);
        assert!(warmer.is_fresh(&NameKey::Species(7), 20 + WARMER_REVERIFY_SECONDS));
        assert!(!warmer.is_fresh(&NameKey::Species(8), 20), "unknown ids are never fresh");
    }
}

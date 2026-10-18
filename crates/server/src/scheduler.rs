//! Poll-loop scheduler: a plain thread that runs ingestion cycles per target
//! on the configured cadence, while the operational side effects that belong
//! to ingestion — raw-listing retention (`prune_raw_before`), feed
//! publication (the files `/feed.lua` serves), and the prepared
//! opportunity/overview scans (`OpportunityService::refresh`) — run on a
//! companion maintenance thread fed by a deduplicated per-target queue, so a
//! slow publish or prune can never delay the next target's fetch. Requests
//! serve prepared data with no store reads; a plain `serve` stays
//! self-contained — no cron, no exporter sidecar.
//!
//! D13: no async in or around the core; everything below is blocking and
//! runs on these two dedicated threads.

use std::{
    any::Any,
    collections::{
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
    domain::{
        utc_day,
        FeedFormat,
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

/// Backoff applied to a rate-limited (target, kind) when the source sent no
/// parseable `Retry-After` hint.
const DEFAULT_BACKOFF_SECS: u64 = 60;

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

/// Maintenance-worker bookkeeping: last retention-prune day and last feed
/// publication per target (the exact state the ingest loop used to carry).
#[derive(Default)]
struct MaintenanceState {
    prune_day: HashMap<String, i64>,
    feed_publish: HashMap<String, Instant>,
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
    let maintenance = spawn_maintenance_worker(&state)?;
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
            // maintenance pass above harmless.
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
                let secs = retry_after.unwrap_or(DEFAULT_BACKOFF_SECS);
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
    }
}

/// Operational side effects after a successful cycle for one target:
/// once-per-UTC-day retention pruning, throttled feed publication, and a
/// refresh of the target's prepared opportunity scan (new folds/credits just
/// landed). All are best effort — a failure is logged and retried on a later
/// pass.
fn run_pass(
    state: &Arc<AppState>,
    config: &amtracker_core::config::TrackerConfig,
    maintenance: &mut MaintenanceState,
    target: &str,
) {
    run_prune_if_due(state, config, maintenance, target);

    let publish_due = maintenance
        .feed_publish
        .get(target)
        .is_none_or(|last| last.elapsed().as_secs() >= config.server.feed_publish_secs);
    if publish_due {
        maintenance.feed_publish.insert(target.to_string(), Instant::now());
        publish_feed(state, config, target);
    }

    refresh_opportunities(state, &TargetId(target.to_string()));
    evaluate_alerts(state, target);
}

/// Applies retention pruning when the target has not been pruned today yet.
fn run_prune_if_due(
    state: &Arc<AppState>,
    config: &amtracker_core::config::TrackerConfig,
    maintenance: &mut MaintenanceState,
    target: &str,
) {
    let today = utc_day(state.clock.now());
    if maintenance.prune_day.get(target) == Some(&today) {
        return;
    }
    match state.history.prune_raw_before(
        &TargetId(target.to_string()),
        today - i64::from(config.storage.raw_listing_days) + 1,
    ) {
        Ok(removed) => {
            maintenance.prune_day.insert(target.to_string(), today);
            tracing::debug!(target, removed, "raw listing retention applied");
        }
        Err(error) => {
            tracing::warn!(target, error = %error, "raw listing retention failed");
        }
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
            "alerts evaluated"
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
/// maintenance worker's once-per-day/throttle bookkeeping: retention pruning,
/// feed publication, and an opportunity scan refresh, exactly as a successful
/// scheduled cycle would trigger them.
/// Used by the manual `/api/fetch` path, whose successful cycles must show
/// up in `/feed.lua` and the ranking pages immediately instead of waiting
/// for the maintenance worker.
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
}

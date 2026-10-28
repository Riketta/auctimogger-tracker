//! Composition root + CLI. The only place that names concrete adapter types;
//! everything else works through `amtracker_core` port traits.

use std::{
    path::{
        Path,
        PathBuf,
    },
    sync::Arc,
};

use amtracker_core::{
    config::{
        AuthConfig,
        TrackerConfig,
        VacuumPolicy,
    },
    domain::PayloadKind,
    ports::{
        FeedRenderer,
        ItemInfoPort,
        SpaceMaintenance,
    },
    services::{
        chain::ChainedItemInfo,
        cycle::IngestCycleService,
        feed_export::FeedExportService,
        metrics::MetricsService,
    },
};
use amtracker_infra::{
    blizzard_item_info::BlizzardItemInfo,
    blizzard_source::BlizzardSource,
    clock::SystemClock,
    feed::{
        FeedCodec,
        FileFeedPublisher,
    },
    store::{
        AutoVacuum,
        SqliteStore,
    },
    wowhead_item_info::WowheadItemInfo,
};
use amtracker_server::{
    config_file::{
        load_config,
        LoadedConfig,
    },
    web::AppState,
};
use clap::{
    Parser,
    Subcommand,
};

mod observability;

#[derive(Parser)]
#[command(
    name = "auctimogger-tracker",
    version,
    about = "Standalone market memory for the AuctiMogger ecosystem"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Parse and validate a config file, then print a summary.
    ConfigCheck { path: PathBuf },
    /// Write a consistent online backup of the database, then exit.
    Backup {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Run one ingestion cycle now (all targets, or `--target` only).
    Fetch {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        target: Option<String>,
    },
    /// Run the poll loop + web UI.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Print the analytics block for one item/span/category (debug aid).
    Metrics {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        target: String,
        #[arg(long)]
        item: i64,
        #[arg(long, default_value = "d1")]
        span: String,
        #[arg(long, default_value = "item")]
        category: String,
    },
    /// Export the tracker feed for a target (lua + json).
    ExportFeed {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value = "feeds")]
        out: PathBuf,
        /// Export only the target's watchlist items.
        #[arg(long, default_value_t = false)]
        slim: bool,
    },
    /// Compact the database file now: rebuild it without the retained free
    /// pages and truncate the WAL. Safe to attempt against a live `serve`,
    /// but it blocks all writers for its duration — stop `serve` for large
    /// stores. Fails with `SQLITE_BUSY` when another process is mid-write.
    Vacuum {
        #[arg(long)]
        config: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Commands::ConfigCheck { path } => run_config_check(&path),
        Commands::Backup { config, out } => run_backup(&config, &out),
        Commands::Fetch { config, target } => {
            with_observability(&config, |loaded| run_fetch(&loaded.tracker, target.as_deref()))
        }
        Commands::Serve { config } => {
            with_observability(&config, |loaded| run_serve(&loaded.tracker))
        }
        Commands::Metrics { config, target, item, span, category } => {
            with_observability(&config, |loaded| {
                run_metrics(&loaded.tracker, &target, item, &span, &category)
            })
        }
        Commands::ExportFeed { config, target, out, slim } => {
            with_observability(&config, |loaded| {
                run_export_feed(&loaded.tracker, target.as_deref(), &out, slim)
            })
        }
        Commands::Vacuum { config } => {
            with_observability(&config, |loaded| run_vacuum(&loaded.tracker))
        }
    };
    exit(code);
}

/// Loads the config, installs logging/Sentry, runs `command`, and returns
/// its exit code. This frame — not the commands — owns the exit: dropping
/// the guards is what flushes the non-blocking file-log worker, and
/// destructors never run on `process::exit`, so the drop must happen here,
/// before the explicit exit.
fn with_observability(path: &Path, command: impl FnOnce(&LoadedConfig) -> i32) -> i32 {
    let loaded = match load_config(path) {
        Ok(loaded) => loaded,
        // Pre-init: no guards exist yet; the explicit flush only covers the
        // parse error's own reporting path.
        Err(error) => observability::exit_with_flush(1, &format!("error: {error}")),
    };
    let guards = observability::init(&loaded.observability);
    tracing::debug!(config = %path.display(), "configuration loaded");
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "auctimogger-tracker starting");
    let code = command(&loaded);
    drop(guards);
    code
}

/// Single process exit: flush queued Sentry events, then exit. File-log
/// buffers are already drained by `with_observability` dropping the guards.
fn exit(code: i32) -> ! {
    observability::flush();
    std::process::exit(code);
}

fn run_config_check(path: &Path) -> i32 {
    let loaded = match load_config(path) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let config = &loaded.tracker;
    println!(
        "config ok: {} target(s), catalog source={}",
        config.targets.len(),
        config.catalog.source
    );
    for target in &config.targets {
        println!(
            "  {} : {}, flavor={}, poll commodities={}s auctions={}s",
            target.id,
            target.describe(),
            target.flavor,
            target.poll.commodities_secs,
            target.poll.auctions_secs
        );
    }
    0
}

/// Writes one consistent online backup of the database (`VACUUM INTO`), then
/// exits: safe to run against a live `serve`, since it reads the store like
/// any other client.
///
/// Like `config-check`, this one-shot command installs no tracing
/// subscriber, so `SqliteStore::open`'s open/migration records are not
/// emitted here; failures still print to stderr.
fn run_backup(path: &Path, out: &Path) -> i32 {
    let loaded = match load_config(path) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let clock: Arc<dyn amtracker_core::ports::ClockPort> = Arc::new(SystemClock);
    let store = match open_store(&loaded.tracker, clock) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    match store.backup_to(out) {
        Ok(()) => {
            println!("backup written: {}", out.display());
            0
        }
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

/// Manual store compaction: stats, one full `VACUUM` (or, on a legacy
/// non-auto-vacuum file under the `incremental` policy, the header
/// conversion which is itself a full `VACUUM`), then a WAL truncate.
/// Safe to attempt against a live `serve` — it fails with `SQLITE_BUSY`
/// instead of corrupting — but the exclusive rebuild blocks writers, so
/// stop `serve` for large stores.
fn run_vacuum(config: &amtracker_core::config::TrackerConfig) -> i32 {
    let clock: Arc<dyn amtracker_core::ports::ClockPort> = Arc::new(SystemClock);
    let store = match open_store(config, clock) {
        Ok(store) => store,
        Err(error) => {
            tracing::error!(error = %error, "store open failed");
            eprintln!("error: {error}");
            return 1;
        }
    };
    let before = match store.space_stats() {
        Ok(stats) => stats,
        Err(error) => {
            eprintln!("error: cannot read store stats: {error}");
            return 1;
        }
    };
    let converted = config.storage.vacuum == VacuumPolicy::Incremental
        && store.auto_vacuum_mode().is_ok_and(|mode| mode == 0);
    let result = if converted {
        println!(
            "converting {} to incremental auto-vacuum (full VACUUM rebuild)...",
            config.storage.database
        );
        store.enable_incremental_auto_vacuum()
    } else {
        store.full_vacuum()
    };
    if let Err(error) = result {
        eprintln!("error: vacuum failed: {error}");
        return 1;
    }
    if let Err(error) = store.checkpoint_wal() {
        eprintln!("warning: WAL checkpoint failed: {error}");
    }
    let after = store.space_stats().unwrap_or(before);
    println!(
        "vacuum complete: {} -> {} bytes ({:.1}% reclaimed, {} free pages remain)",
        before.total_bytes(),
        after.total_bytes(),
        percent_of(before.total_bytes().saturating_sub(after.total_bytes()), before.total_bytes()),
        after.freelist_count
    );
    0
}

/// Display percentage of `part` relative to `total`.
#[allow(clippy::cast_precision_loss)] // byte counts far below 2^53: exact
#[must_use]
fn percent_of(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        100.0 * part as f64 / total as f64
    }
}

fn run_serve(config: &amtracker_core::config::TrackerConfig) -> i32 {
    // Fail fast BEFORE any state is created: an open mutating API on a
    // non-loopback bind, or missing ingestion credentials, must abort in
    // milliseconds with a message instead of serving a broken instance.
    if let Err(message) = amtracker_server::startup::validate_serve_startup(config) {
        tracing::error!("{message}");
        eprintln!("error: {message}");
        return 1;
    }
    // validate_serve_startup already resolved this once; resolving again is
    // cheap and keeps `serve`'s signature address-typed.
    let bind = match amtracker_server::startup::resolve_bind(&config.server.bind) {
        Ok(bind) => bind,
        Err(message) => {
            tracing::error!("{message}");
            eprintln!("error: {message}");
            return 1;
        }
    };
    let clock: Arc<dyn amtracker_core::ports::ClockPort> = Arc::new(SystemClock);
    let store = match open_store(config, clock.clone()) {
        Ok(store) => store,
        Err(error) => {
            tracing::error!(error = %error, "store open failed");
            eprintln!("error: {error}");
            return 1;
        }
    };
    // One-line startup banner: docker logs / journald should describe the
    // running instance without cross-referencing the config file.
    let targets = config.targets.iter().map(|t| t.id.as_str()).collect::<Vec<_>>().join(",");
    tracing::info!(
        store = %config.storage.database,
        targets = %targets,
        catalog = %config.catalog.source,
        feed_dir = %config.storage.feed_dir,
        raw_retention_days = config.storage.raw_listing_days,
        sight_retention_days = config.storage.sight_retention_days,
        alerts = %alert_delivery_summary(config),
        backup = %backup_summary(config),
        "serve configuration"
    );
    let item_info = build_item_info(config);
    let metrics =
        Arc::new(MetricsService::new(store.clone(), store.clone(), store.clone(), clock.clone()));
    let state = build_app_state(config, &clock, &store, &item_info, metrics);

    // Store-backed online-backup writer for the scheduler; the hook keeps the
    // server crate free of the concrete adapter type.
    let backup_hook: amtracker_server::scheduler::BackupHook = {
        let store = store.clone();
        Arc::new(move |path| store.backup_to(path))
    };
    if let Err(error) = amtracker_server::scheduler::spawn(state.clone(), Some(backup_hook)) {
        tracing::error!(error = %error, "scheduler thread failed to start");
        eprintln!("error: scheduler thread failed to start: {error}");
        return 1;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(error = %error, "tokio runtime build failed");
            eprintln!("error: tokio runtime build failed: {error}");
            return 1;
        }
    };
    tracing::info!(bind = %bind, "listening");
    if let Err(error) = runtime.block_on(amtracker_server::web::serve(state, bind)) {
        tracing::error!(error = %error, "server error");
        eprintln!("error: server error: {error}");
        return 1;
    }
    0
}

/// Assembles the item/pet name-resolution chain from `[catalog].source`:
/// `"wowhead"` runs the credential-free adapter alone, `"blizzard"` the
/// authenticated one alone, and `"chain"` (the default) merges Wowhead and
/// Blizzard item data, Wowhead first: a Wowhead hit answers with icon and
/// vendor price, and the Blizzard leg tops up the fields it cannot see
/// (class, subclass, slot, item level). The Blizzard leg is omitted entirely
/// (with one log line) when no Blizzard credentials are configured, so the
/// chain never burns a doomed request per lookup.
fn build_item_info(config: &TrackerConfig) -> Arc<dyn ItemInfoPort> {
    match config.catalog.source.as_str() {
        "wowhead" => Arc::new(WowheadItemInfo::from_config(config)) as Arc<dyn ItemInfoPort>,
        "blizzard" => Arc::new(BlizzardItemInfo::from_config(config)) as Arc<dyn ItemInfoPort>,
        _ => {
            let mut legs: Vec<Arc<dyn ItemInfoPort>> =
                vec![Arc::new(WowheadItemInfo::from_config(config))];
            if blizzard_credentials_present(config) {
                legs.push(Arc::new(BlizzardItemInfo::from_config(config)));
            } else {
                tracing::info!("no Blizzard credentials configured; name chain runs wowhead-only");
            }
            Arc::new(ChainedItemInfo::new(legs))
        }
    }
}

/// Whether the Blizzard item API is actually reachable with the configured
/// credentials: the shared [`TrackerConfig::blizzard_auth`] selection (the
/// same one the adapter chain uses) must name environment variables that
/// are set and non-empty right now.
fn blizzard_credentials_present(config: &TrackerConfig) -> bool {
    let env_set = |name: &str| std::env::var(name).is_ok_and(|value| !value.trim().is_empty());
    match config.blizzard_auth() {
        AuthConfig::Bearer { env } => env_set(&env),
        AuthConfig::ClientCredentials { client_id_env, client_secret_env } => {
            env_set(&client_id_env) && env_set(&client_secret_env)
        }
    }
}

/// Builds the alert dispatcher for the configured delivery; `None` is the
/// `log` delivery (events stay recorded, no outbound messages). Dispatchers
/// read their env values on every dispatch, so rotated URLs and tokens apply
/// without a restart; config loading has already guaranteed the env-var
/// NAMES are present (`validate_alerts_delivery` refuses otherwise).
fn alert_dispatcher(
    config: &amtracker_core::config::TrackerConfig,
) -> Option<Arc<dyn amtracker_core::ports::AlertDispatchPort>> {
    use amtracker_core::config::AlertDelivery;
    match config.alerts.delivery_kind() {
        AlertDelivery::Log => None,
        AlertDelivery::Discord => {
            Some(Arc::new(amtracker_infra::alert_discord::DiscordAlertDispatcher::new(
                config.alerts.webhook_env.as_deref().unwrap_or_default(),
            )))
        }
        AlertDelivery::Telegram => {
            Some(Arc::new(amtracker_infra::alert_telegram::TelegramAlertDispatcher::new(
                config.alerts.token_env.as_deref().unwrap_or_default(),
                config.alerts.chat_id_env.as_deref().unwrap_or_default(),
            )))
        }
    }
}

/// Assembles the shared application state for `serve`: every port wiring
/// lives here so `run_serve` stays a readable startup sequence.
fn build_app_state(
    config: &amtracker_core::config::TrackerConfig,
    clock: &Arc<dyn amtracker_core::ports::ClockPort>,
    store: &Arc<SqliteStore>,
    item_info: &Arc<dyn ItemInfoPort>,
    metrics: Arc<MetricsService>,
) -> Arc<AppState> {
    Arc::new(AppState {
        config: config.clone(),
        clock: clock.clone(),
        health: Arc::new(amtracker_core::services::health::HealthService::new(
            store.clone(),
            config.clone(),
            clock.clone(),
        )),
        search: Arc::new(amtracker_core::services::search::SearchService::new(store.clone())),
        browse: Arc::new(amtracker_core::services::snapshot_browse::SnapshotBrowseService::new(
            store.clone(),
        )),
        catalog: Arc::new(amtracker_core::services::catalog::CatalogService::new(
            store.clone(),
            item_info.clone(),
            clock.clone(),
        )),
        metrics: metrics.clone(),
        opportunities: Arc::new(amtracker_core::services::opportunity::OpportunityService::new(
            store.clone(),
            store.clone(),
            clock.clone(),
            config,
        )),
        watchlist: Arc::new(amtracker_core::services::watchlist::WatchlistService::new(
            store.clone(),
            clock.clone(),
        )),
        watchlist_report: Arc::new(
            amtracker_core::services::watchlist_report::WatchlistReportService::new(
                store.clone(),
                metrics.clone(),
                store.clone(),
                store.clone(),
                clock.clone(),
            ),
        ),
        alerts: Arc::new(amtracker_core::services::alert::AlertEvaluationService::new(
            store.clone(),
            store.clone(),
            metrics.clone(),
            store.clone(),
            store.clone(),
            alert_dispatcher(config),
            clock.clone(),
            i64::try_from(config.alerts.max_data_age_secs).unwrap_or(i64::MAX),
        )),
        feed_export: Arc::new(FeedExportService::new(
            store.clone(),
            store.clone(),
            store.clone(),
            metrics,
            clock.clone(),
        )),
        publisher: Arc::new(FileFeedPublisher::new(PathBuf::from(&config.storage.feed_dir))),
        renderer: Arc::new(FeedCodec),
        cycle: Arc::new(IngestCycleService::new(
            Arc::new(BlizzardSource::new()),
            store.clone(),
            store.clone(),
            store.clone(),
            store.clone(),
            clock.clone(),
        )),
        history: store.clone(),
        snapshots: store.clone(),
        // Storage-file space accounting + reclamation (scheduler pass and
        // the `vacuum` command).
        maintenance: store.clone(),
        // One lock serializes scheduled and manual (`/api/fetch`) cycles.
        cycle_lock: Arc::new(std::sync::Mutex::new(())),
        // Dashboard-visible ingest history (bounded ring).
        recent_cycles: Arc::new(amtracker_server::web::RecentCycles::new(24)),
        // Scheduler-owned fetch timers, rendered by the dashboard's
        // "next fetch" rows and /api/schedule.
        timers: Arc::new(std::sync::Mutex::new(
            amtracker_server::scheduler::TargetTimers::default(),
        )),
    })
}

/// Opens the SQLite store (shared by every command that touches state). The
/// configured vacuum policy decides the fresh-file auto-vacuum header; on
/// existing files the pragma is inert until the maintenance conversion or a
/// manual `vacuum` run.
fn open_store(
    config: &amtracker_core::config::TrackerConfig,
    clock: Arc<dyn amtracker_core::ports::ClockPort>,
) -> Result<Arc<SqliteStore>, String> {
    let auto_vacuum = match config.storage.vacuum {
        VacuumPolicy::Incremental => AutoVacuum::Incremental,
        _ => AutoVacuum::Off,
    };
    SqliteStore::open(
        std::path::Path::new(&config.storage.database),
        std::path::Path::new(&config.storage.archive_dir),
        clock,
        auto_vacuum,
    )
    .map(Arc::new)
    .map_err(|error| error.to_string())
}

/// The alert delivery actually in effect, for the startup banner. Config
/// loading guarantees a named transport has its env-var names present
/// (`validate_alerts_delivery` refuses otherwise).
fn alert_delivery_summary(config: &amtracker_core::config::TrackerConfig) -> String {
    if !config.alerts.enabled {
        return "disabled".to_owned();
    }
    match config.alerts.delivery_kind() {
        amtracker_core::config::AlertDelivery::Log => "log".to_owned(),
        amtracker_core::config::AlertDelivery::Discord => format!(
            "discord webhook_env={}",
            config.alerts.webhook_env.as_deref().unwrap_or_default()
        ),
        amtracker_core::config::AlertDelivery::Telegram => format!(
            "telegram token_env={} chat_id_env={}",
            config.alerts.token_env.as_deref().unwrap_or_default(),
            config.alerts.chat_id_env.as_deref().unwrap_or_default()
        ),
    }
}

/// The scheduled-backup setting for the banner (mirrors the scheduler's
/// gate: both `backup_dir` and a positive `backup_secs`).
fn backup_summary(config: &amtracker_core::config::TrackerConfig) -> String {
    match config.storage.backup_dir.as_deref() {
        Some(dir) if config.storage.backup_secs > 0 => format!(
            "every {}s keep {} -> {}",
            config.storage.backup_secs,
            config.storage.backup_keep,
            dir.display()
        ),
        _ => "disabled".to_owned(),
    }
}

fn run_export_feed(
    config: &amtracker_core::config::TrackerConfig,
    only_target: Option<&str>,
    out_dir: &std::path::Path,
    slim: bool,
) -> i32 {
    use amtracker_core::ports::FeedPublisher;

    let clock = Arc::new(SystemClock);
    let store = match open_store(config, clock.clone()) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let metrics =
        Arc::new(MetricsService::new(store.clone(), store.clone(), store.clone(), clock.clone()));
    let export = FeedExportService::new(store.clone(), store.clone(), store, metrics, clock);
    let publisher = FileFeedPublisher::new(out_dir.to_path_buf());
    let codec = FeedCodec;

    let mut failures = 0u32;
    for target in &config.targets {
        if let Some(wanted) = only_target {
            if target.id.as_str() != wanted {
                continue;
            }
        }
        let options = amtracker_core::services::feed_export::ExportOptions {
            spans: config.metrics.spans.clone(),
            watchlist_only: slim,
        };
        match export.build_feed(&target.id, &options) {
            Ok(feed) => {
                for format in [
                    amtracker_core::domain::FeedFormat::Lua,
                    amtracker_core::domain::FeedFormat::Json,
                ] {
                    match codec.render(&feed, format) {
                        Ok(bytes) => {
                            if let Err(error) = publisher.publish(&target.id, format, &bytes) {
                                failures += 1;
                                tracing::error!(
                                    target = %target.id.0,
                                    format = ?format,
                                    error = %error,
                                    "feed publish failed"
                                );
                            }
                        }
                        Err(error) => {
                            failures += 1;
                            tracing::error!(
                                target = %target.id.0,
                                format = ?format,
                                error = %error,
                                "feed render failed"
                            );
                        }
                    }
                }
                tracing::info!(
                    target = %target.id.0,
                    items = feed.entries.len(),
                    generated_at = feed.generated_at,
                    "feed exported"
                );
            }
            Err(error) => {
                failures += 1;
                tracing::error!(target = %target.id.0, error = %error, "feed export failed");
            }
        }
    }
    tracing::info!(out = %out_dir.display(), "feeds written");
    if failures > 0 {
        eprintln!("error: {failures} export(s) failed");
        return 1;
    }
    0
}

fn run_metrics(
    config: &amtracker_core::config::TrackerConfig,
    target_id: &str,
    item_id: i64,
    span_name: &str,
    category_name: &str,
) -> i32 {
    let Some(span) = amtracker_core::domain::Span::parse(span_name) else {
        eprintln!("error: unknown span '{span_name}' (expected d1|d7|d14|d30)");
        return 1;
    };
    let Some(category) = amtracker_core::domain::Category::parse(category_name) else {
        eprintln!("error: unknown category '{category_name}' (expected item|commodity|pet)");
        return 1;
    };
    if !config.metrics.spans.contains(&span) {
        eprintln!("error: span {} is not enabled in the config", span.name());
        return 1;
    }
    let Some(target) = config.targets.iter().find(|t| t.id.as_str() == target_id) else {
        eprintln!("error: unknown target '{target_id}'");
        return 1;
    };
    let clock = Arc::new(SystemClock);
    let store = match open_store(config, clock.clone()) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let metrics = amtracker_core::services::metrics::MetricsService::new(
        store.clone(),
        store.clone(),
        store.clone(),
        clock,
    );
    let item = amtracker_core::domain::ItemRef::Item { item_id };
    match metrics.block(&target.id, &item, category, span) {
        Ok(Some(block)) => {
            println!(
                "{} {} {} span {} : n={} median={} min={} p05={} p25={} p75={} p95={} max={} mad={} vwap={} qty={} sale_rate={}",
                target_id,
                item_id,
                category.as_str(),
                span.name(),
                block.n,
                block.median,
                block.min,
                block.p05,
                block.p25,
                block.p75,
                block.p95,
                block.max,
                block.mad,
                block.vwap,
                block.qty_sum,
                block
                    .sale_rate
                    .map_or_else(|| "n/a".into(), |rate| format!("{rate:.3}")),
            );
            0
        }
        Ok(None) => {
            eprintln!("no data for item {item_id} in the {} window", span.name());
            1
        }
        Err(error) => {
            tracing::error!(error = %error, "metrics query failed");
            1
        }
    }
}

/// Runs one ingestion cycle per target/kind and exits. Analysis-only by
/// design: unlike the serve loop (and `/api/fetch`), no feed is published,
/// no retention prune runs, and no ranking scan is refreshed — the next
/// `serve` cycle picks all of that up. Documented in the README.
fn run_fetch(config: &amtracker_core::config::TrackerConfig, only_target: Option<&str>) -> i32 {
    let clock: Arc<dyn amtracker_core::ports::ClockPort> = Arc::new(SystemClock);
    let store = match open_store(config, clock.clone()) {
        Ok(store) => store,
        Err(error) => {
            tracing::error!(error = %error, "store open failed");
            eprintln!("error: {error}");
            return 1;
        }
    };
    let cycle = IngestCycleService::new(
        Arc::new(BlizzardSource::new()),
        store.clone(),
        store.clone(),
        store.clone(),
        store,
        clock,
    );

    let mut failures = 0u32;
    for target in &config.targets {
        if let Some(wanted) = only_target {
            if target.id.as_str() != wanted {
                continue;
            }
        }
        for kind in [PayloadKind::Commodities, PayloadKind::Auctions] {
            match cycle.run_cycle(target, kind) {
                Ok(report) => tracing::info!(
                    target = %report.target.0,
                    kind = kind.as_str(),
                    outcome = ?report.outcome,
                    entries = report.entries,
                    dropped = report.dropped,
                    folds = report.items_folded,
                    snapshot = ?report.snapshot_id,
                    "ingest cycle complete"
                ),
                Err(error) => {
                    failures += 1;
                    tracing::error!(
                        target = %target.id.0,
                        kind = kind.as_str(),
                        error = %error,
                        "ingest cycle failed"
                    );
                }
            }
        }
    }
    if failures > 0 {
        eprintln!("error: {failures} cycle(s) failed");
        return 1;
    }
    0
}

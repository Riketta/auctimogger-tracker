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
    domain::PayloadKind,
    ports::FeedRenderer,
    services::{
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
    store::SqliteStore,
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
    /// Print the analytics block for one item/span (debug aid).
    Metrics {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        target: String,
        #[arg(long)]
        item: i64,
        #[arg(long, default_value = "d1")]
        span: String,
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
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Commands::ConfigCheck { path } => run_config_check(&path),
        Commands::Fetch { config, target } => {
            with_observability(&config, |loaded| run_fetch(&loaded.tracker, target.as_deref()))
        }
        Commands::Serve { config } => {
            with_observability(&config, |loaded| run_serve(&loaded.tracker))
        }
        Commands::Metrics { config, target, item, span } => {
            with_observability(&config, |loaded| run_metrics(&loaded.tracker, &target, item, &span))
        }
        Commands::ExportFeed { config, target, out, slim } => {
            with_observability(&config, |loaded| {
                run_export_feed(&loaded.tracker, target.as_deref(), &out, slim)
            })
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
    println!("config ok: {} target(s)", config.targets.len());
    for target in &config.targets {
        println!(
            "  {} : {}, poll commodities={}s auctions={}s",
            target.id,
            target.describe(),
            target.poll.commodities_secs,
            target.poll.auctions_secs
        );
    }
    0
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
    let clock: Arc<dyn amtracker_core::ports::ClockPort> = Arc::new(SystemClock);
    let store = match open_store(config, clock.clone()) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let item_info = Arc::new(BlizzardItemInfo::from_config(config));
    let metrics =
        Arc::new(MetricsService::new(store.clone(), store.clone(), store.clone(), clock.clone()));

    let state = Arc::new(AppState {
        config: config.clone(),
        clock: clock.clone(),
        health: Arc::new(amtracker_core::services::health::HealthService::new(
            store.clone(),
            config.clone(),
            clock.clone(),
        )),
        search: Arc::new(amtracker_core::services::search::SearchService::new(store.clone())),
        catalog: Arc::new(amtracker_core::services::catalog::CatalogService::new(
            store.clone(),
            item_info.clone(),
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
        feed_export: Arc::new(FeedExportService::new(
            store.clone(),
            store.clone(),
            store.clone(),
            metrics,
            clock.clone(),
        )),
        publisher: Arc::new(FileFeedPublisher::new(PathBuf::from("feeds"))),
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
        item_info,
        // One lock serializes scheduled and manual (`/api/fetch`) cycles.
        cycle_lock: Arc::new(std::sync::Mutex::new(())),
        // Dashboard-visible ingest history (bounded ring).
        recent_cycles: Arc::new(amtracker_server::web::RecentCycles::new(24)),
    });

    if let Err(error) = amtracker_server::scheduler::spawn(state.clone()) {
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
    tracing::info!(bind = %config.server.bind, "listening");
    if let Err(error) =
        runtime.block_on(amtracker_server::web::serve(state, config.server.bind.clone()))
    {
        tracing::error!(error = %error, "server error");
        eprintln!("error: server error: {error}");
        return 1;
    }
    0
}

/// Opens the SQLite store (shared by every command that touches state).
fn open_store(
    config: &amtracker_core::config::TrackerConfig,
    clock: Arc<dyn amtracker_core::ports::ClockPort>,
) -> Result<Arc<SqliteStore>, String> {
    SqliteStore::open(
        std::path::Path::new(&config.storage.database),
        std::path::Path::new(&config.storage.archive_dir),
        clock,
    )
    .map(Arc::new)
    .map_err(|error| error.to_string())
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
) -> i32 {
    let Some(span) = amtracker_core::domain::Span::parse(span_name) else {
        eprintln!("error: unknown span '{span_name}' (expected d1|d7|d14|d30)");
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
    match metrics.block(&target.id, &item, span) {
        Ok(Some(block)) => {
            println!(
                "{} {} span {} : n={} median={} min={} p05={} p25={} p75={} p95={} max={} mad={} vwap={} qty={} sale_rate={}",
                target_id,
                item_id,
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

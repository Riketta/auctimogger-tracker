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
    match cli.command {
        Commands::ConfigCheck { path } => run_config_check(&path),
        Commands::Fetch { config, target } => {
            with_observability(&config, |loaded| run_fetch(&loaded.tracker, target.as_deref()));
        }
        Commands::Serve { config } => {
            with_observability(&config, |loaded| run_serve(&loaded.tracker));
        }
        Commands::Metrics { config, target, item, span } => {
            with_observability(&config, |loaded| {
                run_metrics(&loaded.tracker, &target, item, &span);
            });
        }
        Commands::ExportFeed { config, target, out, slim } => {
            with_observability(&config, |loaded| {
                run_export_feed(&loaded.tracker, target.as_deref(), &out, slim);
            });
        }
    }
}

/// Loads the config, installs logging/Sentry, and runs `command` with the
/// loaded pair. The observability guards live in this frame; the command
/// diverges (`-> !`), so explicit flushes on the exit paths (see
/// [`observability::exit_with_flush`]) are what carry queued events out.
/// Loads the config, installs logging/Sentry, and runs `command` with the
/// loaded pair. The observability guards live in this frame: when the
/// command returns (it normally exits the process itself via
/// [`observability::exit_with_flush`]), dropping them flushes queued events.
fn with_observability(path: &Path, command: impl FnOnce(&LoadedConfig)) {
    let loaded = match load_config(path) {
        Ok(loaded) => loaded,
        Err(error) => observability::exit_with_flush(1, &format!("error: {error}")),
    };
    let _guards = observability::init(&loaded.observability);
    tracing::debug!(config = %path.display(), "configuration loaded");
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "auctimogger-tracker starting");
    command(&loaded);
}

fn run_config_check(path: &Path) -> ! {
    let loaded = match load_config(path) {
        Ok(loaded) => loaded,
        Err(error) => observability::exit_with_flush(1, &format!("error: {error}")),
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
    observability::exit_with_flush(0, "");
}

fn run_serve(config: &amtracker_core::config::TrackerConfig) -> ! {
    let store = match open_store(config) {
        Ok(store) => store,
        Err(error) => observability::exit_with_flush(1, &format!("error: {error}")),
    };
    let clock = Arc::new(SystemClock);
    let item_info = Arc::new(BlizzardItemInfo::from_config(config));
    let metrics =
        Arc::new(MetricsService::new(store.clone(), store.clone(), store.clone(), clock.clone()));

    let state = Arc::new(AppState {
        config: config.clone(),
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
        item_info,
    });

    // The open-without-token fallback is documented developer-mode behavior,
    // but an operator who configured `token_env` and forgot the variable must
    // still see one loud line at startup.
    if let Some(token_env) = &config.server.token_env {
        if std::env::var(token_env).is_err() {
            tracing::warn!(
                env = %token_env,
                "bearer token env unset - mutating API endpoints are OPEN"
            );
        }
    }

    if let Err(error) = amtracker_server::scheduler::spawn(config, state.cycle.clone()) {
        tracing::error!(error = %error, "scheduler thread failed to start");
        observability::exit_with_flush(1, "");
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(error = %error, "tokio runtime build failed");
            observability::exit_with_flush(1, "");
        }
    };
    tracing::info!(bind = %config.server.bind, "listening");
    if let Err(error) =
        runtime.block_on(amtracker_server::web::serve(state, config.server.bind.clone()))
    {
        tracing::error!(error = %error, "server error");
        observability::exit_with_flush(1, "");
    }
    observability::exit_with_flush(0, "");
}

/// Opens the SQLite store (shared by every command that touches state).
fn open_store(config: &amtracker_core::config::TrackerConfig) -> Result<Arc<SqliteStore>, String> {
    SqliteStore::open(
        std::path::Path::new(&config.storage.database),
        std::path::Path::new(&config.storage.archive_dir),
    )
    .map(Arc::new)
    .map_err(|error| error.to_string())
}

fn run_export_feed(
    config: &amtracker_core::config::TrackerConfig,
    only_target: Option<&str>,
    out_dir: &std::path::Path,
    slim: bool,
) -> ! {
    use amtracker_core::ports::FeedPublisher;

    let store = match open_store(config) {
        Ok(store) => store,
        Err(error) => observability::exit_with_flush(1, &format!("error: {error}")),
    };
    let clock = Arc::new(SystemClock);
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
        observability::exit_with_flush(1, &format!("{failures} export(s) failed"));
    }
    observability::exit_with_flush(0, "");
}

fn run_metrics(
    config: &amtracker_core::config::TrackerConfig,
    target_id: &str,
    item_id: i64,
    span_name: &str,
) -> ! {
    let Some(span) = amtracker_core::domain::Span::parse(span_name) else {
        observability::exit_with_flush(
            1,
            &format!("error: unknown span '{span_name}' (expected d1|d7|d14|d30)"),
        );
    };
    if !config.metrics.spans.contains(&span) {
        observability::exit_with_flush(
            1,
            &format!("error: span {} is not enabled in the config", span.name()),
        );
    }
    let Some(target) = config.targets.iter().find(|t| t.id.as_str() == target_id) else {
        observability::exit_with_flush(1, &format!("error: unknown target '{target_id}'"));
    };
    let store = match open_store(config) {
        Ok(store) => store,
        Err(error) => observability::exit_with_flush(1, &format!("error: {error}")),
    };
    let metrics = amtracker_core::services::metrics::MetricsService::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(SystemClock),
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
            observability::exit_with_flush(0, "");
        }
        Ok(None) => {
            observability::exit_with_flush(
                1,
                &format!("no data for item {item_id} in the {} window", span.name()),
            );
        }
        Err(error) => {
            tracing::error!(error = %error, "metrics query failed");
            observability::exit_with_flush(1, "");
        }
    }
}

fn run_fetch(config: &amtracker_core::config::TrackerConfig, only_target: Option<&str>) -> ! {
    let store = match open_store(config) {
        Ok(store) => store,
        Err(error) => observability::exit_with_flush(1, &format!("error: {error}")),
    };
    let cycle = IngestCycleService::new(
        Arc::new(BlizzardSource::new()),
        store.clone(),
        store.clone(),
        store.clone(),
        store,
        Arc::new(SystemClock),
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
        observability::exit_with_flush(1, &format!("{failures} cycle(s) failed"));
    }
    observability::exit_with_flush(0, "");
}

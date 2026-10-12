//! Composition root + CLI. The only place that names concrete adapter types;
//! everything else works through `amtracker-core` port traits.

use std::path::PathBuf;
use std::sync::Arc;

use amtracker_core::domain::PayloadKind;
use amtracker_core::services::cycle::IngestCycleService;
use amtracker_infra::blizzard_source::BlizzardSource;
use amtracker_infra::clock::SystemClock;
use amtracker_infra::store::SqliteStore;
use amtracker_server::config_file::load_config;
use clap::{Parser, Subcommand};

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
    /// Run the poll loop + web UI (lands in R5).
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
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::ConfigCheck { path } => {
            let config = match load_config(&path) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(1);
                }
            };
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
        }
        Commands::Fetch { config, target } => {
            let config = match load_config(&config) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(1);
                }
            };
            run_fetch(&config, target.as_deref());
        }
        Commands::Serve { config } => {
            let config = match load_config(&config) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(1);
                }
            };
            eprintln!(
                "not implemented yet: web UI + poll loop land in R5 ({} target(s) validated, see PLAN.md)",
                config.targets.len()
            );
            std::process::exit(2);
        }
        Commands::Metrics {
            config,
            target,
            item,
            span,
        } => {
            let config = match load_config(&config) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(1);
                }
            };
            run_metrics(&config, &target, item, &span);
        }
    }
}

fn run_metrics(
    config: &amtracker_core::config::TrackerConfig,
    target_id: &str,
    item_id: i64,
    span_name: &str,
) -> ! {
    let Some(span) = amtracker_core::domain::Span::parse(span_name) else {
        eprintln!("error: unknown span '{span_name}' (expected d1|d7|d14|d30)");
        std::process::exit(1);
    };
    if !config.metrics.spans.contains(&span) {
        eprintln!("error: span {} is not enabled in the config", span.name());
        std::process::exit(1);
    }
    let Some(target) = config.targets.iter().find(|t| t.id.as_str() == target_id) else {
        eprintln!("error: unknown target '{target_id}'");
        std::process::exit(1);
    };
    let store = match SqliteStore::open(
        std::path::Path::new(&config.storage.database),
        std::path::Path::new(&config.storage.archive_dir),
    ) {
        Ok(store) => Arc::new(store),
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
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
                    .map(|rate| format!("{rate:.3}"))
                    .unwrap_or_else(|| "n/a".into()),
            );
            std::process::exit(0);
        }
        Ok(None) => {
            eprintln!("no data for item {item_id} in the {} window", span.name());
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}

fn run_fetch(config: &amtracker_core::config::TrackerConfig, only_target: Option<&str>) -> ! {
    let store = match SqliteStore::open(
        std::path::Path::new(&config.storage.database),
        std::path::Path::new(&config.storage.archive_dir),
    ) {
        Ok(store) => Arc::new(store),
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
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
                Ok(report) => println!(
                    "[{}] {}: {:?} — {} entries (+{} dropped), {} item-day folds{}",
                    report.target,
                    kind.as_str(),
                    report.outcome,
                    report.entries,
                    report.dropped,
                    report.items_folded,
                    report
                        .snapshot_id
                        .map(|id| format!(", snapshot #{id}"))
                        .unwrap_or_default(),
                ),
                Err(error) => {
                    failures += 1;
                    eprintln!("[{}] {}: error: {error}", target.id, kind.as_str());
                }
            }
        }
    }
    if failures > 0 {
        eprintln!("{failures} cycle(s) failed");
        std::process::exit(1);
    }
    std::process::exit(0);
}

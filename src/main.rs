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

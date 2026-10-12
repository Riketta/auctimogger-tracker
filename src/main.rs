//! Composition root + CLI. The only place that names concrete adapter types;
//! everything else works through `amtracker-core` port traits.

use std::path::PathBuf;

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
    /// Run one ingestion cycle now (lands in R1).
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
            let _ = target;
            eprintln!(
                "not implemented yet: ingestion lands in R1 ({} target(s) validated, see PLAN.md)",
                config.targets.len()
            );
            std::process::exit(2);
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

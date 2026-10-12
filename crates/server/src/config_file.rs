//! TOML config loading — an ACL over the file format. Serde types here mirror
//! the file exactly; the mapping output is the core's plain config model.

use std::path::Path;

use amtracker_core::config::{
    AuthConfig, MetricsConfig, OpportunitiesConfig, PollConfig, ScoreWeights, ServerConfig,
    StorageConfig, TargetConfig, TargetKind, TrackerConfig,
};
use amtracker_core::domain::Span;
use amtracker_core::error::ConfigError;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigLoadError {
    #[error("cannot read config file: {0}")]
    Io(String),
    #[error("cannot parse config TOML: {0}")]
    Toml(String),
    #[error("{0}")]
    Validation(#[from] ConfigError),
}

// ------------------------------------------------------------- file shapes

#[derive(Deserialize)]
pub struct FileConfig {
    pub server: FileServer,
    pub storage: FileStorage,
    pub metrics: FileMetrics,
    pub opportunities: FileOpportunities,
    pub targets: Vec<FileTarget>,
}

#[derive(Deserialize)]
pub struct FileServer {
    pub bind: String,
    #[serde(default)]
    pub token_env: Option<String>,
}

#[derive(Deserialize)]
pub struct FileStorage {
    pub database: String,
    #[serde(default = "default_raw_days")]
    pub raw_listing_days: u32,
    pub archive_dir: String,
}

fn default_raw_days() -> u32 {
    7
}

#[derive(Deserialize)]
pub struct FileMetrics {
    pub spans: Vec<String>,
}

#[derive(Deserialize)]
pub struct FileOpportunities {
    pub min_samples: u32,
    pub weights: FileWeights,
}

#[derive(Deserialize)]
pub struct FileWeights {
    pub discount: f64,
    pub sale: f64,
    pub churn: f64,
    pub stability: f64,
}

#[derive(Deserialize)]
pub struct FilePoll {
    pub commodities_secs: u64,
    pub auctions_secs: u64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum FileAuth {
    Bearer { env: String },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FileTargetKind {
    BlizzardConnectedRealm {
        region: String,
        connected_realm_id: i64,
    },
    StaticUrls {
        commodities_url: String,
        auctions_url: String,
    },
}

#[derive(Deserialize)]
pub struct FileTarget {
    pub id: String,
    #[serde(flatten)]
    pub kind: FileTargetKind,
    #[serde(default)]
    pub auth: Option<FileAuth>,
    pub poll: FilePoll,
}

// ------------------------------------------------------------- mapping

impl TryFrom<FileConfig> for TrackerConfig {
    type Error = ConfigError;

    fn try_from(file: FileConfig) -> Result<Self, ConfigError> {
        let mut targets = Vec::with_capacity(file.targets.len());
        for target in file.targets {
            let kind = match target.kind {
                FileTargetKind::BlizzardConnectedRealm {
                    region,
                    connected_realm_id,
                } => TargetKind::BlizzardConnectedRealm {
                    region,
                    connected_realm_id,
                },
                FileTargetKind::StaticUrls {
                    commodities_url,
                    auctions_url,
                } => TargetKind::StaticUrls {
                    commodities_url,
                    auctions_url,
                },
            };
            let auth = target
                .auth
                .map(|auth| match auth {
                    FileAuth::Bearer { env } => AuthConfig { env },
                })
                .or_else(|| match &kind {
                    // Blizzard targets default to the shared token variable.
                    TargetKind::BlizzardConnectedRealm { .. } => Some(AuthConfig {
                        env: "AMT_TOKEN".to_string(),
                    }),
                    TargetKind::StaticUrls { .. } => None,
                });
            targets.push(TargetConfig {
                id: amtracker_core::domain::TargetId(target.id),
                kind,
                auth,
                poll: PollConfig {
                    commodities_secs: target.poll.commodities_secs,
                    auctions_secs: target.poll.auctions_secs,
                },
            });
        }
        let mut spans = Vec::with_capacity(file.metrics.spans.len());
        for name in &file.metrics.spans {
            let span = Span::parse(name).ok_or_else(|| {
                ConfigError(format!("unknown span '{name}' (expected d1|d7|d14|d30)"))
            })?;
            spans.push(span);
        }
        Ok(TrackerConfig {
            server: ServerConfig {
                bind: file.server.bind,
                token_env: file.server.token_env,
            },
            storage: StorageConfig {
                database: file.storage.database,
                raw_listing_days: file.storage.raw_listing_days,
                archive_dir: file.storage.archive_dir,
            },
            metrics: MetricsConfig { spans },
            opportunities: OpportunitiesConfig {
                min_samples: file.opportunities.min_samples,
                weights: ScoreWeights {
                    discount: file.opportunities.weights.discount,
                    sale: file.opportunities.weights.sale,
                    churn: file.opportunities.weights.churn,
                    stability: file.opportunities.weights.stability,
                },
            },
            targets,
        })
    }
}

/// Parses a config from string form (tests + file loader share this path).
pub fn parse_config_str(text: &str) -> Result<TrackerConfig, ConfigLoadError> {
    let file: FileConfig =
        toml::from_str(text).map_err(|e| ConfigLoadError::Toml(e.to_string()))?;
    let config = TrackerConfig::try_from(file)?;
    config.validate().map_err(ConfigLoadError::Validation)?;
    Ok(config)
}

/// Loads and validates a TOML config file.
pub fn load_config(path: &Path) -> Result<TrackerConfig, ConfigLoadError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ConfigLoadError::Io(format!("{}: {e}", path.display())))?;
    parse_config_str(&text)
}

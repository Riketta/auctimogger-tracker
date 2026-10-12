//! TOML config loading — an ACL over the file format. Serde types here mirror
//! the file exactly; the mapping output is the core's plain config model.

use std::path::Path;

use amtracker_core::{
    config::{
        AuthConfig,
        MetricsConfig,
        OpportunitiesConfig,
        PollConfig,
        ScoreWeights,
        ServerConfig,
        StorageConfig,
        TargetConfig,
        TargetKind,
        TrackerConfig,
    },
    domain::Span,
    error::ConfigError,
};
use serde::Deserialize;

/// Failure modes when turning a config file into the core [`TrackerConfig`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigLoadError {
    /// The file could not be read from disk (missing, permission, encoding).
    #[error("cannot read config file: {0}")]
    Io(String),
    /// The file exists but its contents are not valid TOML.
    #[error("cannot parse config TOML: {0}")]
    Toml(String),
    /// The file parsed but violated a structural rule of the core model.
    #[error("{0}")]
    Validation(#[from] ConfigError),
}

// ------------------------------------------------------------- file shapes

/// Root shape of the config file exactly as it appears on disk; the
/// [`TryFrom`] mapping below turns this into the core [`TrackerConfig`].
#[derive(Deserialize)]
pub struct FileConfig {
    /// `[server]` section: bind address and optional bearer-token env var.
    pub server: FileServer,
    /// `[storage]` section: database, archive directory and retention.
    pub storage: FileStorage,
    /// `[metrics]` section: the analysis spans to precompute.
    pub metrics: FileMetrics,
    /// `[opportunities]` section: ranking knobs for the opportunities page.
    pub opportunities: FileOpportunities,
    /// `[[targets]]` array: one entry per polled market source.
    pub targets: Vec<FileTarget>,
}

/// File shape of the `[server]` section.
#[derive(Deserialize)]
pub struct FileServer {
    /// Socket address the web UI binds to, such as `127.0.0.1:8080`.
    pub bind: String,
    /// Env var holding the bearer token; when absent, mutating endpoints
    /// stay open (the documented developer-mode behavior).
    #[serde(default)]
    pub token_env: Option<String>,
}

/// File shape of the `[storage]` section.
#[derive(Deserialize)]
pub struct FileStorage {
    /// Path to the SQLite database file.
    pub database: String,
    /// Days raw listings are kept before pruning; defaults to 7.
    #[serde(default = "default_raw_days")]
    pub raw_listing_days: u32,
    /// Directory where raw snapshot payloads are archived.
    pub archive_dir: String,
}

fn default_raw_days() -> u32 {
    7
}

/// File shape of the `[metrics]` section.
#[derive(Deserialize)]
pub struct FileMetrics {
    /// Span names to precompute; each must parse as `d1|d7|d14|d30`.
    pub spans: Vec<String>,
}

/// File shape of the `[opportunities]` section.
#[derive(Deserialize)]
pub struct FileOpportunities {
    /// Sample count below which a signal is flagged as low-confidence.
    pub min_samples: u32,
    /// Relative weights of the score components.
    pub weights: FileWeights,
}

/// File shape of the score-weight table under `[opportunities]`.
#[derive(Deserialize)]
pub struct FileWeights {
    /// Weight of the discount-below-median component.
    pub discount: f64,
    /// Weight of the sale-rate component.
    pub sale: f64,
    /// Weight of the price-churn component.
    pub churn: f64,
    /// Weight of the price-stability component.
    pub stability: f64,
}

/// File shape of a target's `poll` table: per-payload cadence in seconds.
#[derive(Deserialize)]
pub struct FilePoll {
    /// Seconds between two commodities-API poll cycles.
    pub commodities_secs: u64,
    /// Seconds between two auctions-API poll cycles.
    pub auctions_secs: u64,
}

/// File shape of a target's optional `auth` table.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum FileAuth {
    /// Read the bearer token from an environment variable.
    Bearer {
        /// Name of the env var holding the token.
        env: String,
    },
}

/// File shape of the flattened `kind` discriminator on a target.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FileTargetKind {
    /// Live Blizzard commodities/auctions endpoints for one connected realm.
    BlizzardConnectedRealm {
        /// Region slug, such as `us` or `eu`.
        region: String,
        /// Blizzard connected-realm ID.
        connected_realm_id: i64,
    },
    /// Fixed JSON endpoints, as served by private-server emulators.
    StaticUrls {
        /// URL of the commodities JSON dump.
        commodities_url: String,
        /// URL of the auctions JSON dump.
        auctions_url: String,
    },
}

/// File shape of one `[[targets]]` entry.
#[derive(Deserialize)]
pub struct FileTarget {
    /// Unique short identifier used in URLs, feeds and logs.
    pub id: String,
    /// Flattened `kind` table selecting the source type.
    #[serde(flatten)]
    pub kind: FileTargetKind,
    /// Auth override; Blizzard targets default to the `AMT_TOKEN` env var.
    #[serde(default)]
    pub auth: Option<FileAuth>,
    /// Poll cadence per payload kind.
    pub poll: FilePoll,
}

// ------------------------------------------------------------- mapping

impl TryFrom<FileConfig> for TrackerConfig {
    type Error = ConfigError;

    /// Maps the file-shaped model onto the core config, applying the Blizzard
    /// default auth env (`AMT_TOKEN`) and parsing the configured span names.
    ///
    /// # Errors
    /// Returns [`ConfigError`] when a configured span name is not one of
    /// `d1|d7|d14|d30`.
    fn try_from(file: FileConfig) -> Result<Self, ConfigError> {
        let mut targets = Vec::with_capacity(file.targets.len());
        for target in file.targets {
            let kind = match target.kind {
                FileTargetKind::BlizzardConnectedRealm { region, connected_realm_id } => {
                    TargetKind::BlizzardConnectedRealm { region, connected_realm_id }
                }
                FileTargetKind::StaticUrls { commodities_url, auctions_url } => {
                    TargetKind::StaticUrls { commodities_url, auctions_url }
                }
            };
            let auth = target
                .auth
                .map(|auth| match auth {
                    FileAuth::Bearer { env } => AuthConfig { env },
                })
                .or_else(|| match &kind {
                    // Blizzard targets default to the shared token variable.
                    TargetKind::BlizzardConnectedRealm { .. } => {
                        Some(AuthConfig { env: "AMT_TOKEN".to_string() })
                    }
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
            server: ServerConfig { bind: file.server.bind, token_env: file.server.token_env },
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

/// Parses and validates a config from string form (tests + file loader share
/// this path).
///
/// # Errors
/// Returns [`ConfigLoadError::Toml`] when `text` is not valid TOML and
/// [`ConfigLoadError::Validation`] when the parsed model fails validation.
pub fn parse_config_str(text: &str) -> Result<TrackerConfig, ConfigLoadError> {
    let file: FileConfig =
        toml::from_str(text).map_err(|e| ConfigLoadError::Toml(e.to_string()))?;
    let config = TrackerConfig::try_from(file)?;
    config.validate().map_err(ConfigLoadError::Validation)?;
    Ok(config)
}

/// Loads and validates a TOML config file from disk.
///
/// # Errors
/// Returns [`ConfigLoadError::Io`] when the file cannot be read,
/// [`ConfigLoadError::Toml`] when its contents are not valid TOML, and
/// [`ConfigLoadError::Validation`] when the parsed model fails validation.
pub fn load_config(path: &Path) -> Result<TrackerConfig, ConfigLoadError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ConfigLoadError::Io(format!("{}: {e}", path.display())))?;
    parse_config_str(&text)
}

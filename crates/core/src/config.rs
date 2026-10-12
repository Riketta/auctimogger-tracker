//! Configuration model (plain structs). Parsing the TOML file format is a
//! driving-adapter concern (`amtracker-server`); the core only validates and
//! consumes the mapped result.

use crate::domain::{Span, TargetId};
use crate::error::ConfigError;

#[derive(Clone, Debug, PartialEq)]
pub struct TrackerConfig {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub metrics: MetricsConfig,
    pub opportunities: OpportunitiesConfig,
    pub targets: Vec<TargetConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    pub bind: String,
    /// Environment variable holding the API token; `None` = open (localhost).
    pub token_env: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageConfig {
    pub database: String,
    /// Raw listing working set in days; snapshot metadata live forever.
    pub raw_listing_days: u32,
    pub archive_dir: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetricsConfig {
    pub spans: Vec<Span>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpportunitiesConfig {
    pub min_samples: u32,
    pub weights: ScoreWeights,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScoreWeights {
    pub discount: f64,
    pub sale: f64,
    pub churn: f64,
    pub stability: f64,
}

/// How to reach one market. `BlizzardConnectedRealm` builds standard URLs from
/// region + realm id; `StaticUrls` accepts any conforming endpoint directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetKind {
    BlizzardConnectedRealm {
        region: String,
        connected_realm_id: i64,
    },
    StaticUrls {
        commodities_url: String,
        auctions_url: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthConfig {
    /// Environment variable holding the bearer token.
    pub env: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollConfig {
    pub commodities_secs: u64,
    pub auctions_secs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetConfig {
    pub id: TargetId,
    pub kind: TargetKind,
    pub auth: Option<AuthConfig>,
    pub poll: PollConfig,
}

impl TargetConfig {
    /// Human-readable kind summary for logs/UI.
    pub fn describe(&self) -> String {
        match &self.kind {
            TargetKind::BlizzardConnectedRealm {
                region,
                connected_realm_id,
            } => {
                format!("blizzard-connected-realm (region {region}, realm {connected_realm_id})")
            }
            TargetKind::StaticUrls { .. } => "static-urls".to_string(),
        }
    }
}

impl TrackerConfig {
    /// Structural validation: ids unique, polls positive, spans known and
    /// non-empty, score weights sane, URL/auth references non-empty.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.targets.is_empty() {
            return Err(ConfigError(
                "at least one [[targets]] entry is required".into(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for target in &self.targets {
            if target.id.0.trim().is_empty() {
                return Err(ConfigError("target id must not be empty".into()));
            }
            if !seen.insert(target.id.clone()) {
                return Err(ConfigError(format!("duplicate target id '{}'", target.id)));
            }
            if target.poll.commodities_secs == 0 || target.poll.auctions_secs == 0 {
                return Err(ConfigError(format!(
                    "target '{}' poll intervals must be positive",
                    target.id
                )));
            }
            if let Some(auth) = &target.auth {
                if auth.env.trim().is_empty() {
                    return Err(ConfigError(format!(
                        "target '{}' auth env must not be empty",
                        target.id
                    )));
                }
            }
            match &target.kind {
                TargetKind::BlizzardConnectedRealm {
                    region,
                    connected_realm_id,
                } => {
                    if region.trim().is_empty() {
                        return Err(ConfigError(format!(
                            "target '{}' region must not be empty",
                            target.id
                        )));
                    }
                    if *connected_realm_id <= 0 {
                        return Err(ConfigError(format!(
                            "target '{}' connected_realm_id must be positive",
                            target.id
                        )));
                    }
                }
                TargetKind::StaticUrls {
                    commodities_url,
                    auctions_url,
                } => {
                    for (name, url) in [
                        ("commodities_url", commodities_url),
                        ("auctions_url", auctions_url),
                    ] {
                        if url.trim().is_empty() {
                            return Err(ConfigError(format!(
                                "target '{}' {name} must not be empty",
                                target.id
                            )));
                        }
                    }
                }
            }
        }
        if self.metrics.spans.is_empty() {
            return Err(ConfigError("[metrics].spans must not be empty".into()));
        }
        let weights = &self.opportunities.weights;
        for (name, value) in [
            ("discount", weights.discount),
            ("sale", weights.sale),
            ("churn", weights.churn),
            ("stability", weights.stability),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(ConfigError(format!(
                    "[opportunities].weights.{name} must be a finite non-negative number"
                )));
            }
        }
        let sum = weights.discount + weights.sale + weights.churn + weights.stability;
        if sum <= 0.0 {
            return Err(ConfigError(
                "[opportunities].weights must sum to a positive number".into(),
            ));
        }
        if self.opportunities.min_samples == 0 {
            return Err(ConfigError(
                "[opportunities].min_samples must be positive".into(),
            ));
        }
        Ok(())
    }
}

//! Configuration model (plain structs). Parsing the TOML file format is a
//! driving-adapter concern (`amtracker-server`); the core only validates and
//! consumes the mapped result.

use crate::{
    domain::{
        Span,
        TargetId,
    },
    error::ConfigError,
};

/// The fully-mapped tracker configuration: server, storage, analytics and
/// per-target source settings.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackerConfig {
    /// HTTP server settings.
    pub server: ServerConfig,
    /// Persistence settings.
    pub storage: StorageConfig,
    /// Which metric spans to expose.
    pub metrics: MetricsConfig,
    /// Opportunity scoring settings.
    pub opportunities: OpportunitiesConfig,
    /// One entry per configured market source.
    pub targets: Vec<TargetConfig>,
}

/// HTTP server settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    /// Socket address the server binds to (e.g. `127.0.0.1:8833`).
    pub bind: String,
    /// Environment variable holding the API token; `None` = open (localhost).
    pub token_env: Option<String>,
    /// Explicit opt-out for the open-mutating-API guard: when set, `serve`
    /// starts even with an unset/empty token on a non-loopback bind (the
    /// default refuses, because that combination exposes watchlist writes
    /// and `/api/fetch` to the network).
    pub allow_open: bool,
    /// Minimum seconds between feed re-publications per target (the
    /// scheduler publishes after successful cycles, throttled by this).
    pub feed_publish_secs: u64,
}

/// Persistence settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageConfig {
    /// Path of the backing database file.
    pub database: String,
    /// Raw listing working set in days; snapshot metadata live forever.
    pub raw_listing_days: u32,
    /// Directory raw payload archives are written to.
    pub archive_dir: String,
}

/// Analytics exposure settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetricsConfig {
    /// The metric spans to compute and expose (non-empty after validation).
    pub spans: Vec<Span>,
}

/// Opportunity scoring settings.
#[derive(Clone, Debug, PartialEq)]
pub struct OpportunitiesConfig {
    /// Minimum unique-listing evidence before an item may rank.
    pub min_samples: u32,
    /// Relative weights of the four score components.
    pub weights: ScoreWeights,
}

/// Weights of the opportunity score components. Only the ratio matters for
/// ranking; the absolute scale is free.
#[derive(Clone, Debug, PartialEq)]
pub struct ScoreWeights {
    /// Weight of the discount component (`1 - floor_now / median7`).
    pub discount: f64,
    /// Weight of the sell-through (reliability) component.
    pub sale: f64,
    /// Weight of the churn (listing turnover) component.
    pub churn: f64,
    /// Weight of the stability (low MAD vs median) component.
    pub stability: f64,
}

/// How to reach one market. `BlizzardConnectedRealm` builds standard URLs from
/// region + realm id; `StaticUrls` accepts any conforming endpoint directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetKind {
    /// Standard Blizzard community-API endpoints for a connected realm.
    BlizzardConnectedRealm {
        /// API region slug (e.g. `us`, `eu`).
        region: String,
        /// The connected realm's id in the Blizzard API.
        connected_realm_id: i64,
    },
    /// Explicit conforming endpoint URLs (self-hosted proxies, caches).
    StaticUrls {
        /// URL of the commodity AH endpoint.
        commodities_url: String,
        /// URL of the item AH endpoint.
        auctions_url: String,
    },
}

/// API authentication for one target. `Bearer` suits endpoints wanting a
/// static token (self-hosted proxies); `ClientCredentials` runs the
/// `Battle.net` `OAuth2` client-credentials flow, with the adapter fetching and
/// refreshing bearer tokens itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthConfig {
    /// A static bearer token read from an environment variable on every
    /// request.
    Bearer {
        /// Environment variable holding the bearer token.
        env: String,
    },
    /// `Battle.net` `OAuth2` client credentials: the client id/secret are read
    /// from the named environment variables and exchanged for bearer tokens
    /// that are cached until shortly before expiry.
    ClientCredentials {
        /// Environment variable holding the OAuth client id.
        client_id_env: String,
        /// Environment variable holding the OAuth client secret.
        client_secret_env: String,
    },
}

impl AuthConfig {
    /// The default credentials for Blizzard endpoints: OAuth client
    /// credentials read from `BLIZZARD_CLIENT_ID` / `BLIZZARD_CLIENT_SECRET`.
    #[must_use]
    pub fn default_blizzard() -> Self {
        Self::ClientCredentials {
            client_id_env: "BLIZZARD_CLIENT_ID".into(),
            client_secret_env: "BLIZZARD_CLIENT_SECRET".into(),
        }
    }
}

/// Poll intervals (seconds) for the two payload kinds of one target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollConfig {
    /// Seconds between commodity AH polls (must be positive).
    pub commodities_secs: u64,
    /// Seconds between item AH polls (must be positive).
    pub auctions_secs: u64,
}

/// One configured market source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetConfig {
    /// Unique slug identifying the target (see `TrackerConfig::validate`).
    pub id: TargetId,
    /// How the source endpoints are reached.
    pub kind: TargetKind,
    /// API authentication, when the source requires a token. Blizzard
    /// targets default to [`AuthConfig::default_blizzard`] when absent.
    pub auth: Option<AuthConfig>,
    /// Poll intervals for the two payload kinds.
    pub poll: PollConfig,
}

impl TargetConfig {
    /// Human-readable kind summary for logs/UI.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.kind {
            TargetKind::BlizzardConnectedRealm { region, connected_realm_id } => {
                format!("blizzard-connected-realm (region {region}, realm {connected_realm_id})")
            }
            TargetKind::StaticUrls { .. } => "static-urls".to_string(),
        }
    }
}

impl TrackerConfig {
    /// The configured target with the given id, if any.
    #[must_use]
    pub fn find_target(&self, id: &str) -> Option<&TargetConfig> {
        self.targets.iter().find(|target| target.id.as_str() == id)
    }

    /// Structural validation: ids unique, polls positive, spans known and
    /// non-empty, score weights sane, URL/auth references non-empty.
    ///
    /// # Errors
    ///
    /// Returns a `ConfigError` when no `[[targets]]` entries exist, a target
    /// id is empty or duplicated, a poll interval is zero, an auth env name
    /// or source URL/region is blank, `connected_realm_id` is not positive,
    /// `[metrics].spans` is empty, a score weight is negative/non-finite or
    /// the weights sum to zero, or `[opportunities].min_samples` is zero.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.targets.is_empty() {
            return Err(ConfigError("at least one [[targets]] entry is required".into()));
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
                let empty = match auth {
                    AuthConfig::Bearer { env } if env.trim().is_empty() => {
                        Some("auth env".to_string())
                    }
                    AuthConfig::ClientCredentials { client_id_env, client_secret_env }
                        if client_id_env.trim().is_empty()
                            || client_secret_env.trim().is_empty() =>
                    {
                        Some("auth client_id_env/client_secret_env".to_string())
                    }
                    _ => None,
                };
                if let Some(names) = empty {
                    return Err(ConfigError(format!(
                        "target '{}' {names} must not be empty",
                        target.id
                    )));
                }
            }
            match &target.kind {
                TargetKind::BlizzardConnectedRealm { region, connected_realm_id } => {
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
                TargetKind::StaticUrls { commodities_url, auctions_url } => {
                    for (name, url) in
                        [("commodities_url", commodities_url), ("auctions_url", auctions_url)]
                    {
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
            return Err(ConfigError("[opportunities].min_samples must be positive".into()));
        }
        Ok(())
    }
}

//! Configuration model (plain structs). Parsing the TOML file format is a
//! driving-adapter concern (`amtracker-server`); the core only validates and
//! consumes the mapped result.

use std::path::PathBuf;

use crate::{
    domain::{
        Flavor,
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
    /// Alert evaluation and delivery settings.
    pub alerts: AlertsConfig,
    /// Item/pet name resolution settings (source chain, locale, pacing).
    pub catalog: CatalogConfig,
    /// One entry per configured market source.
    pub targets: Vec<TargetConfig>,
}

/// HTTP server settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    /// Socket address the server binds to (e.g. `127.0.0.1:8833`, or
    /// `localhost:8833` — the hostname is pinned to `127.0.0.1`; other
    /// hostnames are refused at startup).
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

/// Store-file space reclamation policy. Retention prunes free database
/// pages internally, but SQLite keeps the file at its high-water mark until
/// something returns them to the filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VacuumPolicy {
    /// Fresh databases get `PRAGMA auto_vacuum = INCREMENTAL`; after each
    /// prune pass the maintenance worker returns freed pages once the
    /// freelist crosses a fragmentation threshold. Legacy stores are
    /// converted with a one-time full `VACUUM` on the first pass.
    Incremental,
    /// A full `VACUUM` runs once per UTC day after the prune pass:
    /// maximal shrink, but it needs free disk roughly the size of the
    /// database and blocks writes for its duration.
    Full,
    /// Historical behavior: the file never shrinks.
    Off,
}

/// Persistence settings: database location, retention, payload archives and
/// optional scheduled online backups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageConfig {
    /// Path of the backing database file.
    pub database: String,
    /// Raw listing working set in days; snapshot metadata live forever.
    pub raw_listing_days: u32,
    /// Closed sell-through sightings older than this many days are pruned
    /// (their aggregates already live in the sales ledger); `0` keeps them
    /// forever — the historical default.
    pub sight_retention_days: u32,
    /// Directory raw payload archives are written to.
    pub archive_dir: String,
    /// Directory feed.lua / feed.json are published to (serve mode).
    pub feed_dir: String,
    /// Directory for scheduled online backups: when set together with a
    /// positive `backup_secs`, `serve` writes rotated
    /// `amtracker-backup-*.sqlite3` copies here; `None` disables them.
    pub backup_dir: Option<PathBuf>,
    /// Minimum seconds between scheduled online backups; `0` disables them.
    pub backup_secs: u64,
    /// How many recent backup files to keep (the oldest are pruned after
    /// each backup).
    pub backup_keep: u32,
    /// How freed database pages are returned to the filesystem after
    /// retention prunes (see [`VacuumPolicy`]).
    pub vacuum: VacuumPolicy,
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

/// Item/pet name resolution settings: which source chain resolves unknown
/// ids, in which language, and how eagerly (inline render budget + paced
/// background warmer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogConfig {
    /// Resolution source: `"chain"` (Wowhead first, Blizzard fallback),
    /// `"wowhead"` (only the credential-free Wowhead adapter) or
    /// `"blizzard"` (only the authenticated Blizzard API adapter).
    pub source: String,
    /// Locale tag for resolved names (e.g. `"enUS"`), in the game's
    /// locale-code spelling; adapters map it to their wire formats.
    pub locale: String,
    /// How many not-yet-named ids one table render may resolve inline,
    /// synchronously (the rest keep their id fallback labels until the next
    /// view). `0` disables inline resolution entirely.
    pub render_resolve_max: u32,
    /// Source lookups one background warmer pass may make. The warmer is a
    /// dedicated thread that continuously resolves every id ever seen in a
    /// fold (all-time, all targets/categories, paced, see `warm_pace_ms`).
    /// `0` disables the warmer thread entirely.
    pub warm_batch: u32,
    /// Milliseconds the background warmer sleeps between two source lookups
    /// (politeness pacing for the credential-free adapters).
    pub warm_pace_ms: u64,
    /// Explicit Wowhead base override (e.g. a mirror or proxy), used for
    /// every flavor; `None` selects the per-flavor default bases.
    pub wowhead_base: Option<String>,
}

impl Default for CatalogConfig {
    /// The defaults: chained resolution, English names, a bounded inline
    /// render budget and a polite background warmer.
    fn default() -> Self {
        Self {
            source: "chain".into(),
            locale: "enUS".into(),
            render_resolve_max: 8,
            warm_batch: 25,
            warm_pace_ms: 500,
            wowhead_base: None,
        }
    }
}

/// Alert evaluation and delivery settings. Rules are per-target and live in
/// the store; this section decides whether (and how often) they are evaluated
/// and where fired alerts go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertsConfig {
    /// Master switch: `false` skips evaluation entirely (rules and history
    /// stay browsable in the UI/API).
    pub enabled: bool,
    /// A fired rule re-fires at most once per this many seconds.
    pub cooldown_secs: u64,
    /// Delivery adapter: `"log"` (record only — events surface in the UI/API
    /// and the scheduler logs them), `"discord"` (webhook message) or
    /// `"telegram"` (bot API message).
    pub delivery: String,
    /// Environment variable holding the Discord webhook URL
    /// (`delivery = "discord"`; required for that delivery).
    pub webhook_env: Option<String>,
    /// Environment variable holding the Telegram bot token
    /// (`delivery = "telegram"`; required for that delivery).
    pub token_env: Option<String>,
    /// Environment variable holding the Telegram chat id
    /// (`delivery = "telegram"`; required for that delivery).
    pub chat_id_env: Option<String>,
    /// Freshness gate: rules are skipped (and counted as skipped-stale) while
    /// the target's newest feed snapshot is older than this many seconds, so
    /// a silently dead feed can never fire alerts. `0` disables the gate.
    pub max_data_age_secs: u64,
}

impl AlertsConfig {
    /// The delivery enum the configured string maps to.
    #[must_use]
    pub fn delivery_kind(&self) -> AlertDelivery {
        match self.delivery.as_str() {
            "discord" => AlertDelivery::Discord,
            "telegram" => AlertDelivery::Telegram,
            _ => AlertDelivery::Log,
        }
    }
}

/// Delivery adapters the config string can select.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertDelivery {
    /// Record events only; no outbound messages.
    Log,
    /// Post each fired alert to a Discord webhook.
    Discord,
    /// Send each fired alert through the Telegram bot API.
    Telegram,
}

/// Weights of the opportunity score components. Only the ratio matters for
/// ranking; the absolute scale is free.
#[derive(Clone, Debug, PartialEq)]
pub struct ScoreWeights {
    /// Weight of the discount component (`1 - p05(newest day) / median7`; the
    /// robust floor keeps one outlier listing from dominating the score).
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
    /// Which game flavor this target's items resolve against for catalog
    /// naming (defaults to retail for targets that omit it).
    pub flavor: Flavor,
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

/// Upper bound for every poll-style interval — the per-target poll seconds
/// and `[server].feed_publish_secs`: 30 days (`2_592_000` s). The scheduler
/// seeds first-fire timers as `Instant::now() + jitter` with the jitter drawn
/// from `[0, interval)`, so an unbounded interval could overflow that
/// addition and panic at startup.
const MAX_INTERVAL_SECS: u64 = 30 * 24 * 60 * 60;

/// Whether `id` is a valid target id: `[a-z0-9][a-z0-9._-]*`. Target ids are
/// used as filesystem path segments (`feeds/<id>/`, raw archives), so the
/// charset is restricted to lowercase ASCII alphanumerics plus `.`, `_`, `-`
/// after a leading alphanumeric — this excludes `/`, `\`, whitespace and
/// control characters, the leading-character rule rejects `.`/`..` as ids,
/// and the `..` contains-check rejects interior empty segments (`a..b`).
/// Plain char checks, no regex dependency.
fn is_valid_target_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else { return false };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    if !chars
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-'))
    {
        return false;
    }
    !id.contains("..")
}

/// Target id rules that do not need cross-target state: non-empty and
/// restricted to `[a-z0-9][a-z0-9._-]*` (ids become filesystem path
/// segments — see [`is_valid_target_id`]).
///
/// # Errors
///
/// Returns a `ConfigError` for the first violated rule.
fn validate_target_identity(target: &TargetConfig) -> Result<(), ConfigError> {
    if target.id.0.trim().is_empty() {
        return Err(ConfigError("target id must not be empty".into()));
    }
    if !is_valid_target_id(target.id.as_str()) {
        return Err(ConfigError(format!(
            "target id '{}' must match [a-z0-9][a-z0-9._-]* (lowercase ASCII \
             alphanumerics, then also '.', '_', '-'; no path separators or '..')",
            target.id
        )));
    }
    Ok(())
}

/// Per-target poll checks: both intervals positive and under the 30-day cap
/// ([`MAX_INTERVAL_SECS`]).
///
/// # Errors
///
/// Returns a `ConfigError` for the first violated rule.
fn validate_target_poll(target: &TargetConfig) -> Result<(), ConfigError> {
    if target.poll.commodities_secs == 0 || target.poll.auctions_secs == 0 {
        return Err(ConfigError(format!("target '{}' poll intervals must be positive", target.id)));
    }
    for (name, secs) in [
        ("poll.commodities_secs", target.poll.commodities_secs),
        ("poll.auctions_secs", target.poll.auctions_secs),
    ] {
        if secs > MAX_INTERVAL_SECS {
            return Err(ConfigError(format!(
                "target '{}' {name} must not exceed {MAX_INTERVAL_SECS} (30 days)",
                target.id
            )));
        }
    }
    Ok(())
}

impl TrackerConfig {
    /// The configured target with the given id, if any.
    #[must_use]
    pub fn find_target(&self, id: &str) -> Option<&TargetConfig> {
        self.targets.iter().find(|target| target.id.as_str() == id)
    }

    /// Structural validation: ids unique, polls positive, spans known and
    /// non-empty, score weights sane, URL/auth references non-empty, catalog
    /// source/locale/flavors sane.
    ///
    /// # Errors
    ///
    /// Returns a `ConfigError` when no `[[targets]]` entries exist, a target
    /// id is empty, not `[a-z0-9][a-z0-9._-]*`, or duplicated, a poll
    /// interval is zero or exceeds the 30-day cap (`[server].
    /// feed_publish_secs` included), an auth env name
    /// or source URL/region is blank, `connected_realm_id` is not positive,
    /// `[metrics].spans` is empty, a score weight is negative/non-finite or
    /// the weights sum to zero, `[opportunities].min_samples` is zero, the
    /// `[catalog]` `source/locale/wowhead_base` are invalid, or a target
    /// flavor is empty.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.targets.is_empty() {
            return Err(ConfigError("at least one [[targets]] entry is required".into()));
        }
        let mut seen = std::collections::BTreeSet::new();
        for target in &self.targets {
            validate_target_identity(target)?;
            if !seen.insert(target.id.clone()) {
                return Err(ConfigError(format!("duplicate target id '{}'", target.id)));
            }
            validate_target_poll(target)?;
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
        if self.server.feed_publish_secs > MAX_INTERVAL_SECS {
            return Err(ConfigError(format!(
                "[server].feed_publish_secs must not exceed {MAX_INTERVAL_SECS} (30 days)"
            )));
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
        self.validate_catalog()?;
        if !matches!(self.alerts.delivery.as_str(), "log" | "discord" | "telegram") {
            return Err(ConfigError(format!(
                "[alerts].delivery must be one of \"log\", \"discord\", \"telegram\" (got '{}')",
                self.alerts.delivery
            )));
        }
        self.validate_alerts_delivery()
    }

    /// The locale tags `[catalog].locale` accepts, normalized (lowercased,
    /// separator-stripped): the game's `xxXX` spellings and the wire `xx_XX`
    /// spellings normalize onto the same tag. Each tag has a Wowhead database
    /// prefix and a Blizzard API wire form; anything else would 404 every
    /// lookup and negative-cache real item names.
    const KNOWN_LOCALE_TAGS: [&str; 12] = [
        "enus", "engb", "dede", "eses", "esmx", "frfr", "itit", "ptbr", "ruru", "kokr", "zhcn",
        "zhtw",
    ];

    /// The credential config for Blizzard item/community endpoints: the
    /// first Blizzard-connected target's auth, or the default
    /// client-credential env names when no such target exists. Shared by the
    /// source adapters and the binary's credential probe so the two can
    /// never disagree about which credentials the name chain would use.
    #[must_use]
    pub fn blizzard_auth(&self) -> AuthConfig {
        self.targets
            .iter()
            .find_map(|target| match &target.kind {
                TargetKind::BlizzardConnectedRealm { .. } => Some(target.auth.clone()),
                TargetKind::StaticUrls { .. } => None,
            })
            .flatten()
            .unwrap_or_else(AuthConfig::default_blizzard)
    }

    /// Catalog-specific checks: the source picker names a known adapter, the
    /// locale tag is a supported game locale and the optional Wowhead base
    /// is non-empty, and every target carries a non-empty game flavor.
    ///
    /// # Errors
    ///
    /// Returns a `ConfigError` for the first violated rule.
    fn validate_catalog(&self) -> Result<(), ConfigError> {
        if !matches!(self.catalog.source.as_str(), "chain" | "wowhead" | "blizzard") {
            return Err(ConfigError(format!(
                "[catalog].source must be one of \"chain\", \"wowhead\", \"blizzard\" (got '{}')",
                self.catalog.source
            )));
        }
        let tag: String = self
            .catalog
            .locale
            .chars()
            .filter(|ch| *ch != '_')
            .collect::<String>()
            .to_ascii_lowercase();
        if !Self::KNOWN_LOCALE_TAGS.contains(&tag.as_str()) {
            return Err(ConfigError(format!(
                "[catalog].locale must be a supported game locale tag such as \"enUS\" or \
                 \"deDE\" (got '{}')",
                self.catalog.locale
            )));
        }
        if let Some(base) = &self.catalog.wowhead_base {
            if base.trim().is_empty() {
                return Err(ConfigError("[catalog].wowhead_base must not be empty".into()));
            }
        }
        for target in &self.targets {
            if target.flavor.as_str().trim().is_empty() {
                return Err(ConfigError(format!(
                    "target '{}' flavor must not be empty",
                    target.id
                )));
            }
        }
        Ok(())
    }

    /// Delivery-specific `[alerts]` checks: the selected adapter's credential
    /// env var names must be present and non-empty.
    fn validate_alerts_delivery(&self) -> Result<(), ConfigError> {
        let missing = |env: &Option<String>| env.as_deref().is_none_or(|v| v.trim().is_empty());
        if self.alerts.delivery == "discord" && missing(&self.alerts.webhook_env) {
            return Err(ConfigError(
                "[alerts].webhook_env must name an environment variable when delivery = \"discord\""
                    .into(),
            ));
        }
        if self.alerts.delivery == "telegram"
            && (missing(&self.alerts.token_env) || missing(&self.alerts.chat_id_env))
        {
            return Err(ConfigError(
                "[alerts].token_env and chat_id_env must name environment variables when delivery = \"telegram\""
                    .into(),
            ));
        }
        Ok(())
    }
}

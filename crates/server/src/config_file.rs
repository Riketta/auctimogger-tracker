//! TOML config loading — an ACL over the file format. Serde types here mirror
//! the file exactly; the mapping output is the core's plain config model.

use std::{
    collections::HashMap,
    path::{
        Path,
        PathBuf,
    },
};

use amtracker_core::{
    config::{
        AlertsConfig,
        AuthConfig,
        CatalogConfig,
        CraftsConfig,
        MetricsConfig,
        OpportunitiesConfig,
        PollConfig,
        ScoreWeights,
        ServerConfig,
        StorageConfig,
        TargetConfig,
        TargetKind,
        TrackerConfig,
        VacuumPolicy,
    },
    domain::{
        ItemRef,
        Reagent,
        Recipe,
        Span,
    },
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
    /// The file parsed, but contains keys the model does not know.
    #[error("unknown config keys: {0}")]
    UnknownKeys(String),
    /// The file parsed but violated a structural rule of the core model.
    #[error("{0}")]
    Validation(#[from] ConfigError),
}

// ------------------------------------------------------------- file shapes

/// Root shape of the config file exactly as it appears on disk; the
/// mapping below turns this into the loaded-config pair for the binary.
#[derive(Deserialize)]
pub struct FileConfig {
    /// Louder stdout defaults for the tracker's own crates; `RUST_LOG`
    /// overrides this entirely. Defaults to `false`.
    #[serde(default)]
    pub debug: bool,
    /// `[server]` section: bind address and optional bearer-token env var.
    pub server: FileServer,
    /// `[storage]` section: database, archive directory and retention.
    pub storage: FileStorage,
    /// `[metrics]` section: the analysis spans to precompute.
    pub metrics: FileMetrics,
    /// `[opportunities]` section: ranking knobs for the opportunities page.
    pub opportunities: FileOpportunities,
    /// `[alerts]` section: evaluation switch, cooldown and delivery. Absent =
    /// enabled with `log` delivery (events surface in the UI/API only).
    #[serde(default)]
    pub alerts: FileAlerts,
    /// `[crafts]` section: the manual recipe book and the crafting view's
    /// knobs. Absent = no recipes, so the crafting views stay empty.
    #[serde(default)]
    pub crafts: FileCrafts,
    /// `[catalog]` section: item/pet name resolution knobs. Absent = chained
    /// Wowhead→Blizzard resolution with English names and polite pacing.
    #[serde(default)]
    pub catalog: FileCatalog,
    /// `[[targets]]` array: one entry per polled market source.
    pub targets: Vec<FileTarget>,
    /// Optional `[logging]` section: persistent file logging. Absent = stdout
    /// (and Sentry) only.
    #[serde(default)]
    pub logging: Option<FileLogging>,
    /// Optional `[sentry]` section: Sentry-protocol error reporting (the
    /// hosted Sentry service or self-hosted `GlitchTip`). Absent or empty
    /// `dsn` = disabled.
    #[serde(default)]
    pub sentry: Option<FileSentry>,
}

/// File shape of the optional `[logging]` section (startup-only: the tracing
/// subscriber is installed once at boot, so changes require a restart).
#[derive(Deserialize)]
pub struct FileLogging {
    /// Directory for the log files; created at boot. Failure aborts startup:
    /// an operator-declared local sink must not silently degrade to stdout.
    pub dir: String,
    /// `"daily"` (default) writes one file per day; `"never"` appends to a
    /// single file (pair with an external rotator).
    #[serde(default)]
    pub rotation: FileRotation,
    /// Independent filter directives for the file layer (`EnvFilter` syntax,
    /// used verbatim; invalid directives abort startup). `RUST_LOG` never
    /// affects the file. Default: the tracker's own crates at debug.
    #[serde(default)]
    pub level: Option<String>,
}

/// File-log rotation policy (`rotation` in the `[logging]` section).
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileRotation {
    /// One file per day (`auctimogger-tracker.log.2026-10-12`).
    #[default]
    Daily,
    /// A single append-only file (`auctimogger-tracker.log`).
    Never,
}

/// File shape of the optional `[sentry]` section.
#[derive(Deserialize)]
pub struct FileSentry {
    /// Sentry-protocol DSN (the hosted Sentry service or self-hosted
    /// `GlitchTip`); the
    /// address to report to is encoded in the DSN host. Empty = disabled.
    pub dsn: String,
    /// Environment label attached to reported events (e.g. `production`).
    #[serde(default)]
    pub environment: Option<String>,
    /// Performance-transaction sample rate (0.0..=1.0). Absent = error-only
    /// reporting; out-of-range values disable performance monitoring with a
    /// warning instead of aborting.
    #[serde(default)]
    pub traces_sample_rate: Option<f32>,
}

/// Composition-root observability settings mapped from the file: what the
/// binary's logging/Sentry bootstrap consumes. Plain model, no serde - the
/// file shapes above are the ACL.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservabilitySettings {
    /// Louder stdout defaults for the tracker's own crates.
    pub debug: bool,
    /// Optional persistent file logging.
    pub logging: Option<LoggingSettings>,
    /// Optional Sentry-protocol reporting.
    pub sentry: Option<SentrySettings>,
}

/// Mapped `[logging]` settings.
#[derive(Debug, Clone, PartialEq)]
pub struct LoggingSettings {
    /// Directory for the log files.
    pub dir: String,
    /// Rotation policy.
    pub rotation: Rotation,
    /// Independent `EnvFilter` directives for the file layer.
    pub level: Option<String>,
}

/// Runtime rotation policy (file layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// One file per day.
    Daily,
    /// A single append-only file.
    Never,
}

/// Mapped `[sentry]` settings.
#[derive(Debug, Clone, PartialEq)]
pub struct SentrySettings {
    /// Sentry-protocol DSN.
    pub dsn: String,
    /// Environment label for reported events.
    pub environment: Option<String>,
    /// Performance-transaction sample rate.
    pub traces_sample_rate: Option<f32>,
}

/// A config file loaded and validated: the core model plus the
/// composition-root observability settings that the binary consumes.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// The core tracker model (validated).
    pub tracker: TrackerConfig,
    /// Logging/Sentry bootstrap settings.
    pub observability: ObservabilitySettings,
}

/// File shape of the `[server]` section.
#[derive(Deserialize)]
pub struct FileServer {
    /// Socket address the web UI binds to, such as `127.0.0.1:8080`.
    pub bind: String,
    /// Env var holding the bearer token; when absent (or empty), mutating
    /// endpoints stay open (the documented developer-mode behavior).
    #[serde(default)]
    pub token_env: Option<String>,
    /// Explicit opt-out for the open-mutating-API guard: with `true`, `serve`
    /// starts even with an unset/empty token on a non-loopback bind (the
    /// default refuses). Defaults to `false`.
    #[serde(default)]
    pub allow_open: bool,
    /// Minimum seconds between feed re-publications per target; defaults
    /// to 300. The scheduler publishes after successful cycles, throttled
    /// by this.
    #[serde(default = "default_feed_publish_secs")]
    pub feed_publish_secs: u64,
}

fn default_feed_publish_secs() -> u64 {
    300
}

/// File shape of the `[storage]` section.
#[derive(Deserialize)]
pub struct FileStorage {
    /// Path to the SQLite database file.
    pub database: String,
    /// Days raw listings are kept before pruning; defaults to 7.
    #[serde(default = "default_raw_days")]
    pub raw_listing_days: u32,
    /// Days closed sell-through sightings are kept before pruning;
    /// defaults to 0 (kept forever).
    #[serde(default)]
    pub sight_retention_days: u32,
    /// Days intraday hour-fold cells are kept before pruning; defaults to
    /// 45 (covers the when-to-buy view's 30-day horizon with margin), 0 =
    /// forever.
    #[serde(default = "default_item_hour_retention_days")]
    pub item_hour_retention_days: u32,
    /// Directory where raw snapshot payloads are archived.
    pub archive_dir: String,
    /// Directory feed.lua / feed.json are published to (serve mode);
    /// defaults to CWD-relative `./feeds` (the historical behavior).
    #[serde(default = "default_feed_dir")]
    pub feed_dir: String,
    /// Directory for scheduled online backups; absent = they stay off.
    #[serde(default)]
    pub backup_dir: Option<PathBuf>,
    /// Minimum seconds between scheduled online backups; defaults to 0
    /// (disabled).
    #[serde(default = "default_backup_secs")]
    pub backup_secs: u64,
    /// How many recent backup files to keep (oldest pruned after each
    /// backup); defaults to 7.
    #[serde(default = "default_backup_keep")]
    pub backup_keep: u32,
    /// Freed-space reclamation after retention prunes:
    /// `incremental` (default), `full`, or `off`.
    #[serde(default = "default_vacuum")]
    pub vacuum: String,
}

fn default_raw_days() -> u32 {
    7
}

/// Default `[storage].item_hour_retention_days`: 45 days covers the
/// when-to-buy view's 30-day intraday preset with margin.
fn default_item_hour_retention_days() -> u32 {
    45
}

fn default_feed_dir() -> String {
    "feeds".to_string()
}

fn default_backup_secs() -> u64 {
    0
}

fn default_backup_keep() -> u32 {
    7
}

fn default_vacuum() -> String {
    "incremental".to_string()
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

/// File model of the `[alerts]` section.
#[derive(Deserialize, Clone, Debug)]
pub struct FileAlerts {
    /// Master switch; defaults to `true`.
    #[serde(default = "default_alerts_enabled")]
    pub enabled: bool,
    /// Re-fire cooldown per rule, seconds; defaults to 6 hours.
    #[serde(default = "default_alert_cooldown")]
    pub cooldown_secs: u64,
    /// `"log"` (default), `"discord"` or `"telegram"`.
    #[serde(default = "default_alert_delivery")]
    pub delivery: String,
    /// Discord webhook URL env var name (`discord` delivery).
    #[serde(default)]
    pub webhook_env: Option<String>,
    /// Telegram bot token env var name (`telegram` delivery).
    #[serde(default)]
    pub token_env: Option<String>,
    /// Telegram chat id env var name (`telegram` delivery).
    #[serde(default)]
    pub chat_id_env: Option<String>,
    /// Freshness gate, seconds; rules skip while the target's newest feed is
    /// older. `0` disables. Defaults to 48 hours.
    #[serde(default = "default_alert_max_data_age")]
    pub max_data_age_secs: u64,
}

impl Default for FileAlerts {
    fn default() -> Self {
        Self {
            enabled: default_alerts_enabled(),
            cooldown_secs: default_alert_cooldown(),
            delivery: default_alert_delivery(),
            webhook_env: None,
            token_env: None,
            chat_id_env: None,
            max_data_age_secs: default_alert_max_data_age(),
        }
    }
}

fn default_alerts_enabled() -> bool {
    true
}

fn default_alert_cooldown() -> u64 {
    21_600
}

fn default_alert_delivery() -> String {
    "log".into()
}

fn default_alert_max_data_age() -> u64 {
    48 * 60 * 60
}

/// File shape of the optional `[crafts]` section.
#[derive(Deserialize)]
pub struct FileCrafts {
    /// Path of the manual recipe file (TOML, one `[[recipe]]` per crafted
    /// item; see `recipes.example.toml`). Relative paths resolve against the
    /// working directory like the storage paths. Absent = no recipes.
    pub recipes: Option<String>,
    /// Crafted items below this 14-day sale rate are not ranked as efficient
    /// crafts. Defaults to `0.2`.
    #[serde(default = "default_min_sale_rate")]
    pub min_sale_rate: f64,
    /// Auction-house cut applied to the output value, in percent. Defaults to
    /// `5.0`.
    #[serde(default = "default_ah_cut_percent")]
    pub ah_cut_percent: f64,
}

impl Default for FileCrafts {
    /// An absent section: no recipes and the documented thresholds.
    fn default() -> Self {
        Self {
            recipes: None,
            min_sale_rate: default_min_sale_rate(),
            ah_cut_percent: default_ah_cut_percent(),
        }
    }
}

fn default_min_sale_rate() -> f64 {
    0.2
}

fn default_ah_cut_percent() -> f64 {
    5.0
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
    /// Read a static bearer token from an environment variable.
    Bearer {
        /// Name of the env var holding the token.
        env: String,
    },
    /// `Battle.net` `OAuth2` client credentials; the adapter fetches and
    /// refreshes bearer tokens itself.
    ClientCredentials {
        /// Name of the env var holding the OAuth client id.
        client_id_env: String,
        /// Name of the env var holding the OAuth client secret.
        client_secret_env: String,
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
    /// Blizzard "Forever" API (undocumented, realmless): one faction
    /// auction house of one ruleset.
    BlizzardForever {
        /// Region slug (`us`, `eu`, `kr`, `tw`; CN has no Forever API).
        region: String,
        /// Blizzard ruleset ID (the realm replacement).
        ruleset_id: i64,
        /// Auction house ID within the ruleset (from the auctions index;
        /// use the `forever-rulesets` command to list them).
        house_id: i64,
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
    /// Auth override; Blizzard targets default to OAuth client credentials
    /// (`BLIZZARD_CLIENT_ID` / `BLIZZARD_CLIENT_SECRET`).
    #[serde(default)]
    pub auth: Option<FileAuth>,
    /// Poll cadence per payload kind.
    pub poll: FilePoll,
    /// Game flavor this target's items resolve against for catalog naming
    /// (`retail`, `classic-era`, `classic`, `mop-classic`, or a custom key
    /// for retail-compatible private servers). Absent = `retail`.
    #[serde(default)]
    pub flavor: Option<String>,
}

/// File shape of the optional `[catalog]` section: which chain resolves
/// unknown item/pet ids, in which language, and how eagerly.
#[derive(Deserialize, Default)]
pub struct FileCatalog {
    /// `"chain"` (Wowhead first, Blizzard fallback), `"wowhead"` or
    /// `"blizzard"`. Defaults to `"chain"`.
    #[serde(default)]
    pub source: Option<String>,
    /// Locale tag for resolved names (e.g. `"enUS"`). Defaults to `"enUS"`.
    #[serde(default)]
    pub locale: Option<String>,
    /// Inline source-lookup budget per table render for not-yet-named ids
    /// (cataloged names always render); `0` disables inline source
    /// resolution. Defaults to 8.
    #[serde(default)]
    pub render_resolve_max: Option<u32>,
    /// Source lookups one background warmer pass may make; `0` disables the
    /// warmer thread. Defaults to 25.
    #[serde(default)]
    pub warm_batch: Option<u32>,
    /// Milliseconds between two warmer lookups. Defaults to 500.
    #[serde(default)]
    pub warm_pace_ms: Option<u64>,
    /// Explicit Wowhead base override for every flavor. Absent = per-flavor
    /// defaults.
    #[serde(default)]
    pub wowhead_base: Option<String>,
}

// ------------------------------------------------------------- mapping

/// Maps the file-shaped model onto the loaded-config pair: the core
/// [`TrackerConfig`] for the services and the observability settings for
/// the binary's logging/Sentry bootstrap.
///
/// # Errors
/// Returns [`ConfigError`] when a configured span name is not one of
/// `d1|d7|d14|d30`.
fn map_loaded(file: FileConfig) -> Result<LoadedConfig, ConfigError> {
    let mut file = file;
    let debug = file.debug;
    let logging = file.logging.take();
    let sentry = file.sentry.take();
    let observability = ObservabilitySettings {
        debug,
        logging: logging.map(|logging| LoggingSettings {
            dir: logging.dir,
            rotation: match logging.rotation {
                FileRotation::Daily => Rotation::Daily,
                FileRotation::Never => Rotation::Never,
            },
            level: logging.level,
        }),
        sentry: sentry.map(|sentry| SentrySettings {
            dsn: sentry.dsn,
            environment: sentry.environment,
            traces_sample_rate: sentry.traces_sample_rate,
        }),
    };
    let tracker = build_tracker(file)?;
    Ok(LoadedConfig { tracker, observability })
}

/// Builds the runtime config from the file model, applying the Blizzard
/// default auth (OAuth client credentials) and parsing the configured span
/// names.
///
/// # Errors
///
/// Returns [`ConfigError`] when a configured span name is not one of
/// `d1|d7|d14|d30`.
#[allow(clippy::too_many_lines)] // one declarative mapping per config section
fn build_tracker(file: FileConfig) -> Result<TrackerConfig, ConfigError> {
    let mut targets = Vec::with_capacity(file.targets.len());
    for target in file.targets {
        let kind = match target.kind {
            FileTargetKind::BlizzardConnectedRealm { region, connected_realm_id } => {
                TargetKind::BlizzardConnectedRealm { region, connected_realm_id }
            }
            FileTargetKind::BlizzardForever { region, ruleset_id, house_id } => {
                TargetKind::BlizzardForever { region, ruleset_id, house_id }
            }
            FileTargetKind::StaticUrls { commodities_url, auctions_url } => {
                TargetKind::StaticUrls { commodities_url, auctions_url }
            }
        };
        let auth = target
            .auth
            .map(|auth| match auth {
                FileAuth::Bearer { env } => AuthConfig::Bearer { env },
                FileAuth::ClientCredentials { client_id_env, client_secret_env } => {
                    AuthConfig::ClientCredentials { client_id_env, client_secret_env }
                }
            })
            .or_else(|| match &kind {
                // Blizzard targets default to the OAuth client-credential
                // env names; static endpoints stay unauthenticated.
                TargetKind::BlizzardConnectedRealm { .. } | TargetKind::BlizzardForever { .. } => {
                    Some(AuthConfig::default_blizzard())
                }
                TargetKind::StaticUrls { .. } => None,
            });
        // Forever targets are named for the Forever game flavor unless the
        // config says otherwise: the catalog namespace maps 1:1.
        let default_flavor = match &kind {
            TargetKind::BlizzardForever { .. } => "forever",
            _ => amtracker_core::domain::Flavor::RETAIL,
        };
        targets.push(TargetConfig {
            id: amtracker_core::domain::TargetId(target.id),
            kind,
            auth,
            poll: PollConfig {
                commodities_secs: target.poll.commodities_secs,
                auctions_secs: target.poll.auctions_secs,
            },
            // Flavor keys are catalog namespaces: normalize case and
            // whitespace so "Retail" or "retail " cannot fork the catalog.
            flavor: amtracker_core::domain::Flavor(
                target.flavor.unwrap_or_else(|| default_flavor.into()).trim().to_ascii_lowercase(),
            ),
        });
    }
    let mut spans = Vec::with_capacity(file.metrics.spans.len());
    for name in &file.metrics.spans {
        let span = Span::parse(name).ok_or_else(|| {
            ConfigError(format!("unknown span '{name}' (expected d1|d7|d14|d30)"))
        })?;
        spans.push(span);
    }
    let vacuum = match file.storage.vacuum.as_str() {
        "incremental" => VacuumPolicy::Incremental,
        "full" => VacuumPolicy::Full,
        "off" => VacuumPolicy::Off,
        other => {
            return Err(ConfigError(format!(
                "unknown [storage].vacuum '{other}' (expected incremental|full|off)"
            )));
        }
    };
    Ok(TrackerConfig {
        server: ServerConfig {
            bind: file.server.bind,
            token_env: file.server.token_env,
            allow_open: file.server.allow_open,
            feed_publish_secs: file.server.feed_publish_secs,
        },
        storage: StorageConfig {
            database: file.storage.database,
            raw_listing_days: file.storage.raw_listing_days,
            sight_retention_days: file.storage.sight_retention_days,
            item_hour_retention_days: file.storage.item_hour_retention_days,
            archive_dir: file.storage.archive_dir,
            feed_dir: file.storage.feed_dir,
            backup_dir: file.storage.backup_dir,
            backup_secs: file.storage.backup_secs,
            backup_keep: file.storage.backup_keep,
            vacuum,
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
        alerts: AlertsConfig {
            enabled: file.alerts.enabled,
            cooldown_secs: file.alerts.cooldown_secs,
            delivery: file.alerts.delivery,
            webhook_env: file.alerts.webhook_env,
            token_env: file.alerts.token_env,
            chat_id_env: file.alerts.chat_id_env,
            max_data_age_secs: file.alerts.max_data_age_secs,
        },
        crafts: CraftsConfig {
            recipes_file: file.crafts.recipes,
            recipes: Vec::new(),
            min_sale_rate: file.crafts.min_sale_rate,
            ah_cut_percent: file.crafts.ah_cut_percent,
        },
        catalog: CatalogConfig {
            source: file.catalog.source.unwrap_or_else(|| CatalogConfig::default().source),
            locale: file.catalog.locale.unwrap_or_else(|| CatalogConfig::default().locale),
            render_resolve_max: file
                .catalog
                .render_resolve_max
                .unwrap_or(CatalogConfig::default().render_resolve_max),
            warm_batch: file.catalog.warm_batch.unwrap_or(CatalogConfig::default().warm_batch),
            warm_pace_ms: file
                .catalog
                .warm_pace_ms
                .unwrap_or(CatalogConfig::default().warm_pace_ms),
            wowhead_base: file.catalog.wowhead_base,
        },
        targets,
    })
}

/// Keys the file model accepts at the top level.
const TOP_KEYS: &[&str] = &[
    "debug",
    "server",
    "storage",
    "metrics",
    "opportunities",
    "alerts",
    "crafts",
    "catalog",
    "targets",
    "logging",
    "sentry",
];

/// Rejects keys the file model does not know: a typo like `enviroment` must
/// fail startup naming the path instead of silently disabling the setting
/// (the same reasoning as the fail-fast `[logging].level` directive check).
/// Kind-specific target keys share the target table (the `kind` enum is
/// flattened), so the union of both profiles is accepted there.
///
/// # Errors
/// Returns [`ConfigLoadError::UnknownKeys`] naming every unknown path.
// One declarative table per section: the key lists read best as a single
// ordered block.
#[allow(clippy::too_many_lines)]
fn check_unknown_keys(root: &toml::Table) -> Result<(), ConfigLoadError> {
    let mut unknown = Vec::new();
    collect_unknown(root, "", TOP_KEYS, &mut unknown);

    if let Some(server) = section(root, "server") {
        collect_unknown(
            server,
            "server",
            &["bind", "token_env", "allow_open", "feed_publish_secs"],
            &mut unknown,
        );
    }
    if let Some(storage) = section(root, "storage") {
        collect_unknown(
            storage,
            "storage",
            &[
                "database",
                "raw_listing_days",
                "sight_retention_days",
                "item_hour_retention_days",
                "archive_dir",
                "feed_dir",
                "backup_dir",
                "backup_secs",
                "backup_keep",
                "vacuum",
            ],
            &mut unknown,
        );
    }
    if let Some(metrics) = section(root, "metrics") {
        collect_unknown(metrics, "metrics", &["spans"], &mut unknown);
    }
    if let Some(opportunities) = section(root, "opportunities") {
        collect_unknown(opportunities, "opportunities", &["min_samples", "weights"], &mut unknown);
        if let Some(weights) = opportunities.get("weights").and_then(toml::Value::as_table) {
            collect_unknown(
                weights,
                "opportunities.weights",
                &["discount", "sale", "churn", "stability"],
                &mut unknown,
            );
        }
    }
    if let Some(catalog) = section(root, "catalog") {
        collect_unknown(
            catalog,
            "catalog",
            &[
                "source",
                "locale",
                "render_resolve_max",
                "warm_batch",
                "warm_pace_ms",
                "wowhead_base",
            ],
            &mut unknown,
        );
    }
    if let Some(alerts) = section(root, "alerts") {
        collect_unknown(
            alerts,
            "alerts",
            &[
                "enabled",
                "cooldown_secs",
                "delivery",
                "webhook_env",
                "token_env",
                "chat_id_env",
                "max_data_age_secs",
            ],
            &mut unknown,
        );
    }
    if let Some(crafts) = section(root, "crafts") {
        collect_unknown(
            crafts,
            "crafts",
            &["recipes", "min_sale_rate", "ah_cut_percent"],
            &mut unknown,
        );
    }
    if let Some(logging) = section(root, "logging") {
        collect_unknown(logging, "logging", &["dir", "rotation", "level"], &mut unknown);
    }
    if let Some(sentry) = section(root, "sentry") {
        collect_unknown(
            sentry,
            "sentry",
            &["dsn", "environment", "traces_sample_rate"],
            &mut unknown,
        );
    }
    if let Some(targets) = root.get("targets").and_then(toml::Value::as_array) {
        check_target_keys(targets, &mut unknown);
    }

    if unknown.is_empty() {
        Ok(())
    } else {
        Err(ConfigLoadError::UnknownKeys(unknown.join(", ")))
    }
}

/// Keys the `[[targets]]` array entries accept, including the flattened
/// `kind` variants and the nested `poll`/`auth` tables.
fn check_target_keys(targets: &[toml::Value], unknown: &mut Vec<String>) {
    for (index, target) in targets.iter().enumerate() {
        let Some(target) = target.as_table() else { continue };
        let path = format!("targets[{index}]");
        collect_unknown(
            target,
            &path,
            &[
                "id",
                "kind",
                "auth",
                "poll",
                "flavor",
                "region",
                "connected_realm_id",
                "ruleset_id",
                "house_id",
                "commodities_url",
                "auctions_url",
            ],
            unknown,
        );
        if let Some(poll) = target.get("poll").and_then(toml::Value::as_table) {
            collect_unknown(
                poll,
                &format!("{path}.poll"),
                &["commodities_secs", "auctions_secs"],
                unknown,
            );
        }
        if let Some(auth) = target.get("auth").and_then(toml::Value::as_table) {
            collect_unknown(
                auth,
                &format!("{path}.auth"),
                &["type", "env", "client_id_env", "client_secret_env"],
                unknown,
            );
        }
    }
}

/// Appends `path.key` for every table key not in `allowed` (`path` is empty
/// at the root, where keys are reported bare).
fn collect_unknown(table: &toml::Table, path: &str, allowed: &[&str], unknown: &mut Vec<String>) {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            unknown.push(if path.is_empty() { key.clone() } else { format!("{path}.{key}") });
        }
    }
}

/// The named section when it is present and a table (shape errors inside are
/// the deserializer's job, not this check's).
fn section<'a>(root: &'a toml::Table, name: &str) -> Option<&'a toml::Table> {
    root.get(name).and_then(toml::Value::as_table)
}

/// Parses and validates a config from string form (tests + file loader share
/// this path).
///
/// # Errors
/// Returns [`ConfigLoadError::Toml`] when `text` is not valid TOML,
/// [`ConfigLoadError::UnknownKeys`] when the file contains unknown keys, and
/// [`ConfigLoadError::Validation`] when the parsed model fails validation.
pub fn parse_config_str(text: &str) -> Result<LoadedConfig, ConfigLoadError> {
    let table: toml::Table =
        toml::from_str(text).map_err(|e| ConfigLoadError::Toml(e.to_string()))?;
    check_unknown_keys(&table)?;
    let file: FileConfig =
        toml::Value::Table(table).try_into().map_err(|e| ConfigLoadError::Toml(e.to_string()))?;
    let loaded = map_loaded(file)?;
    loaded.tracker.validate().map_err(ConfigLoadError::Validation)?;
    Ok(loaded)
}

/// Loads and validates a TOML config file from disk, then the recipe book it
/// points at (see [`parse_recipe_book`]).
///
/// # Errors
/// Returns [`ConfigLoadError::Io`] when either file cannot be read,
/// [`ConfigLoadError::Toml`] when their contents are not valid TOML,
/// [`ConfigLoadError::UnknownKeys`] when the config contains unknown keys,
/// and [`ConfigLoadError::Validation`] when the parsed model or a recipe
/// fails validation.
pub fn load_config(path: &Path) -> Result<LoadedConfig, ConfigLoadError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ConfigLoadError::Io(format!("{}: {e}", path.display())))?;
    let mut loaded = parse_config_str(&text)?;
    if let Some(recipes_file) = loaded.tracker.crafts.recipes_file.clone() {
        let recipes_path = Path::new(&recipes_file);
        let text = std::fs::read_to_string(recipes_path)
            .map_err(|e| ConfigLoadError::Io(format!("{}: {e}", recipes_path.display())))?;
        loaded.tracker.crafts.recipes = parse_recipe_book(&text)?;
        // The structural recipe rules ride the model's validation.
        loaded.tracker.validate().map_err(ConfigLoadError::Validation)?;
    }
    Ok(loaded)
}

/// File shape of the recipe book: `[[recipe]]` entries and nothing else, so a
/// typo fails the load instead of silently dropping a field.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRecipeBook {
    /// One entry per crafted item, in file order.
    #[serde(rename = "recipe", default)]
    recipes: Vec<FileRecipe>,
}

/// File shape of one `[[recipe]]` entry.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRecipe {
    /// Crafted item id.
    item: i64,
    /// Units one make yields; defaults to 1.
    #[serde(default = "default_makes")]
    makes: i64,
    /// Reagent item id → units per make. TOML table keys are strings, so ids
    /// arrive spelled out.
    #[serde(default)]
    reagents: HashMap<String, i64>,
}

fn default_makes() -> i64 {
    1
}

/// Parses the recipe file's TOML into core recipes. Reagents sort by id so
/// two loads of one file read identically; structural rules (positive ids
/// and quantities, no repeats) are left to `TrackerConfig::validate`, which
/// names the offending recipe.
///
/// # Errors
/// Returns [`ConfigLoadError::Toml`] for invalid TOML or an unknown key, and
/// [`ConfigLoadError::Validation`] when a reagent key is not an item id.
pub fn parse_recipe_book(text: &str) -> Result<Vec<Recipe>, ConfigLoadError> {
    let book: FileRecipeBook =
        toml::from_str(text).map_err(|e| ConfigLoadError::Toml(format!("recipe file: {e}")))?;
    book.recipes
        .into_iter()
        .map(|entry| {
            let mut reagents = Vec::with_capacity(entry.reagents.len());
            for (key, qty) in entry.reagents {
                let item_id = key.trim().parse::<i64>().map_err(|_| {
                    ConfigLoadError::Validation(ConfigError(format!(
                        "recipe for item {}: reagent '{key}' is not an item id",
                        entry.item
                    )))
                })?;
                reagents.push(Reagent { item: ItemRef::Item { item_id }, qty });
            }
            reagents.sort_by_key(|reagent| match reagent.item {
                ItemRef::Item { item_id } => item_id,
                ItemRef::Pet { species, .. } => species,
            });
            Ok(Recipe { item: ItemRef::Item { item_id: entry.item }, makes: entry.makes, reagents })
        })
        .collect()
}

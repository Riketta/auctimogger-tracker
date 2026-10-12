//! Observability bootstrap: global tracing subscriber (stdout + optional
//! file layer) and optional Sentry-protocol reporting, adapted from the
//! versa-bot reference design. Installed once at startup; changes require a
//! restart. The returned guards must outlive the whole run — dropping the
//! Sentry guard discards queued events, dropping the file worker guard
//! discards buffered log lines. [`exit_with_flush`] covers the CLI's
//! `process::exit` paths, where destructors never run.

use std::time::Duration;

use amtracker_server::config_file::{
    ObservabilitySettings,
    Rotation,
};
use sentry_tracing::EventFilter;
use tracing_subscriber::{
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
    Layer,
};

/// Fallback directives when `RUST_LOG` is unset.
const FALLBACK_INFO_FILTER: &str = "info";
/// Debug fallback: the tracker's own crates at debug, everything else info.
const FALLBACK_DEBUG_FILTER: &str =
    "info,auctimogger_tracker=debug,amtracker_infra=debug,amtracker_server=debug";
/// File-layer default (`[logging].level` absent): flight-recorder mode — the
/// tracker's own breadcrumbs at debug regardless of the stdout level.
const FILE_DEFAULT_FILTER: &str =
    "info,auctimogger_tracker=debug,amtracker_infra=debug,amtracker_server=debug";

/// Log file name prefix — `tracing-appender` appends the rotation date, so
/// daily files are `auctimogger-tracker.log.2026-10-12` and `never` yields
/// `auctimogger-tracker.log`.
const LOG_FILE_PREFIX: &str = "auctimogger-tracker.log";

/// How long [`flush`] waits for queued Sentry events to drain before the
/// CLI's `process::exit` cuts the transport off.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Guards that must outlive the whole run. The fields exist for their
/// lifetime side effects (dropping either discards queued events/buffers) —
/// they are never read.
pub struct ObservabilityGuards {
    /// Sentry SDK client guard; `None` without a configured DSN.
    pub _sentry: Option<sentry::ClientInitGuard>,
    /// Non-blocking file-log worker; `None` without a `[logging]` section.
    pub _file: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// Flushes queued Sentry events synchronously. `process::exit` skips
/// destructors, so the CLI paths call this explicitly before exiting;
/// without a configured DSN this is a no-op.
pub fn flush() {
    if let Some(client) = sentry::Hub::current().client() {
        client.flush(Some(FLUSH_TIMEOUT));
    }
}

/// Prints `message` to stderr, flushes Sentry, and exits the process with
/// `code`. The CLI's error paths use this instead of bare `process::exit`
/// so error-grade events reach the reporting endpoint.
pub fn exit_with_flush(code: i32, message: &str) -> ! {
    if !message.is_empty() {
        eprintln!("{message}");
    }
    flush();
    std::process::exit(code);
}

/// Installs the global tracing subscriber: a stdout fmt layer, an optional
/// file layer, and an optional Sentry/GlitchTip layer.
///
/// The layers filter independently: stdout follows `RUST_LOG` (with the
/// `debug` config flag as the fallback default); the file layer uses its own
/// `[logging].level` verbatim (a typo aborts startup — a directive typo must
/// not silently change what is captured); Sentry always carries info and
/// above (audit-grade records ship even when an operator quiets stdout).
///
/// Without a DSN — or with a malformed one, which is reported on stderr and
/// skipped — the app logs to stdout only. A misconfigured file layer, in
/// contrast, aborts startup: a local, verifiable directory failing to exist
/// beats silently running without the requested records. Error events carry
/// the crate release (`auctimogger-tracker@<version>`); performance
/// transactions are only sampled when `traces_sample_rate` (0.0..=1.0) is
/// configured.
///
/// Returns the guards the caller must keep alive for the whole process
/// lifetime.
///
/// # Panics
/// Two operator-declared sinks fail fast on misconfiguration: an invalid
/// `[logging].level` (a directive typo must not silently change what is
/// captured) and an uncreatable `[logging].dir` (a local, verifiable sink
/// must not silently degrade to stdout). Both abort before the subscriber
/// exists — the deliberate exception to the no-panic rule.
#[allow(clippy::panic)]
pub fn init(settings: &ObservabilitySettings) -> ObservabilityGuards {
    let stdout_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(fallback_directives(settings.debug)));
    // An explicit `[logging].level` is used verbatim; a typo must not
    // silently change what is captured, so invalid directives abort startup.
    let file_filter = EnvFilter::new(
        file_filter_directives(settings.logging.as_ref().and_then(|l| l.level.as_deref()))
            .unwrap_or_else(|error| {
                panic!("config [logging].level expected to be valid EnvFilter directives: {error}")
            }),
    );

    let mut file_guard = None;
    let mut file_layer = None;
    if let Some(logging) = &settings.logging {
        let rotation = match logging.rotation {
            Rotation::Daily => tracing_appender::rolling::Rotation::DAILY,
            Rotation::Never => tracing_appender::rolling::Rotation::NEVER,
        };
        let appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(rotation)
            .filename_prefix(LOG_FILE_PREFIX)
            .build(&logging.dir)
            .unwrap_or_else(|error| {
                panic!(
                    "config [logging].dir expected to be a creatable and writable log directory: {error}"
                )
            });
        let (writer, guard) = tracing_appender::non_blocking(appender);
        file_guard = Some(guard);
        file_layer = Some(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer)
                .with_filter(file_filter),
        );
    }

    let registry = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(stdout_filter))
        .with(file_layer);

    // A malformed DSN must not abort startup: the tracing subscriber is not
    // installed yet, so warn on stderr and continue stdout-only.
    let guard = effective_dsn(settings.sentry.as_ref().map(|s| s.dsn.as_str())).and_then(|dsn| {
        match dsn.parse() {
            Ok(dsn) => {
                // `ClientOptions` is `#[non_exhaustive]` — construct via
                // defaults and builder-style field assignment.
                let mut options = sentry::ClientOptions::default();
                options.dsn = Some(dsn);
                options.environment =
                    settings.sentry.as_ref().and_then(|s| s.environment.clone()).map(Into::into);
                options.release = sentry::release_name!();
                let configured_rate = settings.sentry.as_ref().and_then(|s| s.traces_sample_rate);
                match (usable_sample_rate(configured_rate), configured_rate) {
                    (Some(rate), _) => options = options.traces_sample_rate(rate),
                    // Reported, not fatal: an out-of-range rate turns
                    // performance monitoring off while error reporting stays on.
                    (None, Some(configured)) => eprintln!(
                        "warning: sentry.traces_sample_rate {configured} is outside 0.0..=1.0 \
                         - performance monitoring disabled"
                    ),
                    (None, None) => {}
                }
                Some(sentry::init(options))
            }
            Err(error) => {
                eprintln!("warning: invalid Sentry DSN ({error}) - Sentry reporting disabled");
                None
            }
        }
    });

    match &guard {
        Some(_) => registry
            .with(
                sentry_tracing::layer()
                    .event_filter(|metadata| match *metadata.level() {
                        // Pinned contract: error-grade events surface as Issues
                        // (the always-delivered path), warn/info ride along as
                        // log items, debug/trace never ship.
                        tracing::Level::ERROR => EventFilter::Event | EventFilter::Log,
                        tracing::Level::WARN | tracing::Level::INFO => EventFilter::Log,
                        tracing::Level::DEBUG | tracing::Level::TRACE => EventFilter::Ignore,
                    })
                    // The Sentry layer filters independently of stdout (and of
                    // RUST_LOG): audit-grade info must ship even when an
                    // operator quiets stdout.
                    .with_filter(EnvFilter::new("info")),
            )
            .init(),
        None => registry.init(),
    }

    install_panic_hook();

    ObservabilityGuards { _sentry: guard, _file: file_guard }
}

/// Routes panics through the tracing stack (the Sentry layer turns the
/// error-grade record into an Issue) while keeping the default hook's stderr
/// line: journald captures it even when no subscriber exists yet, and the
/// file/Sentry layers must never be the only witness to a crash. Installed
/// after the subscriber is up, so `init`'s own fail-fast panics keep the
/// default behavior.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let thread_name = std::thread::current().name().unwrap_or("<unnamed>").to_owned();
        let location = info.location().map_or_else(|| "unknown".to_owned(), ToString::to_string);
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_owned());
        eprintln!("thread '{thread_name}' panicked at {location}: {message}");
        tracing::error!(
            thread = %thread_name,
            location = %location,
            panic = %message,
            "panic"
        );
    }));
}

/// The file layer's filter directives: an explicit `[logging].level` is used
/// verbatim after validation, its absence picks the flight-recorder default.
/// The `Err` case is the deliberate fail-fast for invalid directives.
fn file_filter_directives(level: Option<&str>) -> Result<String, String> {
    match level {
        Some(directives) => EnvFilter::try_new(directives)
            .map(|_| directives.to_owned())
            .map_err(|err| err.to_string()),
        None => Ok(FILE_DEFAULT_FILTER.to_owned()),
    }
}

/// The stdout fallback directives for a debug flag: `RUST_LOG` wins when set,
/// otherwise the flag picks the verbosity preset.
#[must_use]
pub fn fallback_directives(debug: bool) -> &'static str {
    if debug {
        FALLBACK_DEBUG_FILTER
    } else {
        FALLBACK_INFO_FILTER
    }
}

/// The DSN Sentry would parse: trimmed, with an empty or whitespace-only
/// value treated as absent — reporting disabled without a warning, since a
/// blank DSN is a configuration, not a misconfiguration.
fn effective_dsn(dsn: Option<&str>) -> Option<&str> {
    dsn.map(str::trim).filter(|dsn| !dsn.is_empty())
}

/// The sample rate `init` may hand to the Sentry client: an unset rate stays
/// `None`, an in-range rate passes through, an out-of-range one degrades to
/// `None` (performance monitoring off, error reporting unaffected).
fn usable_sample_rate(rate: Option<f32>) -> Option<f32> {
    rate.filter(|rate| (0.0..=1.0).contains(rate))
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect: fixed synthetic inputs, not hostile
    // data (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A DSN that passes `sentry`'s parser — the valid shape of the Sentry
    /// decisions.
    const VALID_DSN: &str = "https://examplePublicKey@o0.ingest.sentry.io/0";

    #[test]
    fn explicit_file_level_is_used_verbatim() {
        assert_eq!(
            file_filter_directives(Some("info,amtracker_infra=trace")).as_deref(),
            Ok("info,amtracker_infra=trace")
        );
    }

    /// The deliberate fail-fast: invalid directives are an `Err` that `init`
    /// turns into the aborting path — a typo must not silently change what is
    /// captured.
    #[test]
    fn invalid_file_level_is_the_fail_fast_error() {
        assert!(file_filter_directives(Some("amtracker_infra=not_a_level")).is_err());
    }

    #[test]
    fn absent_file_level_picks_the_flight_recorder_default() {
        assert_eq!(file_filter_directives(None).as_deref(), Ok(FILE_DEFAULT_FILTER));
    }

    #[test]
    fn debug_flag_switches_the_stdout_fallback() {
        assert_eq!(fallback_directives(false), FALLBACK_INFO_FILTER);
        assert_eq!(fallback_directives(true), FALLBACK_DEBUG_FILTER);
    }

    #[test]
    fn absent_or_blank_dsn_disables_silently() {
        assert_eq!(effective_dsn(None), None);
        assert_eq!(effective_dsn(Some("")), None);
        assert_eq!(effective_dsn(Some("   ")), None);
    }

    #[test]
    fn dsn_is_trimmed_not_rejected() {
        let padded = format!("  {VALID_DSN}  ");
        assert_eq!(effective_dsn(Some(&padded)), Some(VALID_DSN));
    }

    /// The malformed-DSN branch: `init` warns on this parse `Err` and
    /// continues stdout-only — pinned on the pure decision that guards the
    /// enable path.
    #[test]
    fn malformed_dsn_fails_the_parse_guarding_the_enable_path() {
        assert!("wat".parse::<sentry::types::Dsn>().is_err());
        assert!(VALID_DSN.parse::<sentry::types::Dsn>().is_ok());
    }

    #[test]
    fn sample_rate_passes_only_in_range_values() {
        assert_eq!(usable_sample_rate(None), None);
        assert_eq!(usable_sample_rate(Some(0.0)), Some(0.0));
        assert_eq!(usable_sample_rate(Some(0.5)), Some(0.5));
        assert_eq!(usable_sample_rate(Some(1.0)), Some(1.0));
        // Out of range is off: reported and degraded by `init`.
        assert_eq!(usable_sample_rate(Some(-0.1)), None);
        assert_eq!(usable_sample_rate(Some(1.5)), None);
        assert_eq!(usable_sample_rate(Some(f32::NAN)), None);
    }
}

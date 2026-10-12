// Test assertions may unwrap/expect/panic: fixture-driven, deterministic inputs.
// Direct indexing is bounded by the committed example's two targets.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

//! Config loading specs: the committed example must parse into the core model
//! and malformed configs must be rejected with useful errors.

use amtracker_core::{
    config::TargetKind,
    domain::Span,
};
use amtracker_server::config_file::{
    parse_config_str,
    LoggingSettings,
    Rotation,
    SentrySettings,
};

const EXAMPLE: &str = include_str!("../../../config.example.toml");

#[test]
fn parses_the_committed_example_config() {
    let loaded = parse_config_str(EXAMPLE).expect("example config must parse and validate");
    let config = &loaded.tracker;

    assert_eq!(config.server.bind, "127.0.0.1:8080");
    assert_eq!(config.server.token_env.as_deref(), Some("AMT_API_TOKEN"));

    assert_eq!(config.storage.raw_listing_days, 7);
    assert_eq!(config.metrics.spans, vec![Span::D1, Span::D7, Span::D14, Span::D30]);
    assert_eq!(config.opportunities.min_samples, 30);
    assert_eq!(config.opportunities.weights.discount, 0.4);
    assert_eq!(config.opportunities.weights.stability, 0.1);

    assert_eq!(config.targets.len(), 2);

    let first = &config.targets[0];
    assert_eq!(first.id.as_str(), "us-4");
    match &first.kind {
        TargetKind::BlizzardConnectedRealm { region, connected_realm_id } => {
            assert_eq!(region, "us");
            assert_eq!(*connected_realm_id, 4);
        }
        other @ TargetKind::StaticUrls { .. } => {
            panic!("expected blizzard-connected-realm, got {other:?}")
        }
    }
    // Blizzard targets default to the shared token env when auth is implicit.
    let auth = first.auth.as_ref().expect("blizzard default auth");
    assert_eq!(auth.env, "AMT_TOKEN");
    assert_eq!(first.poll.commodities_secs, 3600);
    assert_eq!(first.poll.auctions_secs, 1800);

    let second = &config.targets[1];
    assert_eq!(second.id.as_str(), "wowforever-main");
    assert!(matches!(second.kind, TargetKind::StaticUrls { .. }));
    assert!(second.auth.is_none(), "static-urls stay unauthenticated unless declared");
}

#[test]
fn the_example_config_has_observability_off_by_default() {
    let loaded = parse_config_str(EXAMPLE).expect("example config must parse");
    assert!(!loaded.observability.debug);
    assert!(loaded.observability.logging.is_none(), "no [logging] section committed");
    assert!(loaded.observability.sentry.is_none(), "no [sentry] section committed");
}

#[test]
fn parses_observability_sections() {
    let text = format!(
        "debug = true\n\n{EXAMPLE}\n\
         [logging]\n\
         dir = \"logs\"\n\
         rotation = \"never\"\n\
         level = \"info,amtracker_infra=debug\"\n\
         [sentry]\n\
         dsn = \"http://your-key@localhost:9000/1\"\n\
         environment = \"production\"\n\
         traces_sample_rate = 0.01\n"
    );
    let loaded = parse_config_str(&text).expect("observability sections must parse");
    let observability = &loaded.observability;

    assert!(observability.debug);
    assert_eq!(
        observability.logging,
        Some(LoggingSettings {
            dir: "logs".to_string(),
            rotation: Rotation::Never,
            level: Some("info,amtracker_infra=debug".to_string()),
        })
    );
    assert_eq!(
        observability.sentry,
        Some(SentrySettings {
            dsn: "http://your-key@localhost:9000/1".to_string(),
            environment: Some("production".to_string()),
            traces_sample_rate: Some(0.01),
        })
    );
}

#[test]
fn unknown_rotation_is_rejected() {
    let text = format!("{EXAMPLE}\n[logging]\ndir = \"logs\"\nrotation = \"weekly\"\n");
    let error = parse_config_str(&text).expect_err("unknown rotation must fail");
    assert!(error.to_string().contains("unknown variant"), "{error}");
}

#[test]
fn rejects_unknown_spans() {
    let text =
        EXAMPLE.replace("spans = [\"d1\", \"d7\", \"d14\", \"d30\"]", "spans = [\"d1\", \"week\"]");
    let error = parse_config_str(&text).expect_err("unknown span must fail");
    assert!(error.to_string().contains("unknown span 'week'"), "{error}");
}

#[test]
fn rejects_duplicate_target_ids() {
    let text = EXAMPLE.replace("id = \"wowforever-main\"", "id = \"us-4\"");
    let error = parse_config_str(&text).expect_err("duplicate ids must fail");
    assert!(error.to_string().contains("duplicate target id 'us-4'"), "{error}");
}

#[test]
fn rejects_zero_poll_intervals() {
    let text = EXAMPLE.replace("commodities_secs = 3600", "commodities_secs = 0");
    let error = parse_config_str(&text).expect_err("zero poll must fail");
    assert!(error.to_string().contains("poll intervals must be positive"), "{error}");
}

#[test]
fn rejects_negative_weights() {
    let text = EXAMPLE.replace("sale = 0.3", "sale = -1.0");
    let error = parse_config_str(&text).expect_err("negative weight must fail");
    assert!(error.to_string().contains("non-negative"), "{error}");
}

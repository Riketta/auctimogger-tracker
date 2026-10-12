//! Config loading specs: the committed example must parse into the core model
//! and malformed configs must be rejected with useful errors.

use amtracker_core::config::TargetKind;
use amtracker_core::domain::Span;
use amtracker_server::config_file::parse_config_str;

const EXAMPLE: &str = include_str!("../../../config.example.toml");

#[test]
fn parses_the_committed_example_config() {
    let config = parse_config_str(EXAMPLE).expect("example config must parse and validate");

    assert_eq!(config.server.bind, "127.0.0.1:8080");
    assert_eq!(config.server.token_env.as_deref(), Some("AMT_API_TOKEN"));

    assert_eq!(config.storage.raw_listing_days, 7);
    assert_eq!(
        config.metrics.spans,
        vec![Span::D1, Span::D7, Span::D14, Span::D30]
    );
    assert_eq!(config.opportunities.min_samples, 30);
    assert_eq!(config.opportunities.weights.discount, 0.4);
    assert_eq!(config.opportunities.weights.stability, 0.1);

    assert_eq!(config.targets.len(), 2);

    let first = &config.targets[0];
    assert_eq!(first.id.as_str(), "us-4");
    match &first.kind {
        TargetKind::BlizzardConnectedRealm {
            region,
            connected_realm_id,
        } => {
            assert_eq!(region, "us");
            assert_eq!(*connected_realm_id, 4);
        }
        other => panic!("expected blizzard-connected-realm, got {other:?}"),
    }
    // Blizzard targets default to the shared token env when auth is implicit.
    let auth = first.auth.as_ref().expect("blizzard default auth");
    assert_eq!(auth.env, "AMT_TOKEN");
    assert_eq!(first.poll.commodities_secs, 3600);
    assert_eq!(first.poll.auctions_secs, 1800);

    let second = &config.targets[1];
    assert_eq!(second.id.as_str(), "wowforever-main");
    assert!(matches!(second.kind, TargetKind::StaticUrls { .. }));
    assert!(
        second.auth.is_none(),
        "static-urls stay unauthenticated unless declared"
    );
}

#[test]
fn rejects_unknown_spans() {
    let text = EXAMPLE.replace(
        "spans = [\"d1\", \"d7\", \"d14\", \"d30\"]",
        "spans = [\"d1\", \"week\"]",
    );
    let error = parse_config_str(&text).expect_err("unknown span must fail");
    assert!(error.to_string().contains("unknown span 'week'"), "{error}");
}

#[test]
fn rejects_duplicate_target_ids() {
    let text = EXAMPLE.replace("id = \"wowforever-main\"", "id = \"us-4\"");
    let error = parse_config_str(&text).expect_err("duplicate ids must fail");
    assert!(
        error.to_string().contains("duplicate target id 'us-4'"),
        "{error}"
    );
}

#[test]
fn rejects_zero_poll_intervals() {
    let text = EXAMPLE.replace("commodities_secs = 3600", "commodities_secs = 0");
    let error = parse_config_str(&text).expect_err("zero poll must fail");
    assert!(
        error
            .to_string()
            .contains("poll intervals must be positive"),
        "{error}"
    );
}

#[test]
fn rejects_negative_weights() {
    let text = EXAMPLE.replace("sale = 0.3", "sale = -1.0");
    let error = parse_config_str(&text).expect_err("negative weight must fail");
    assert!(error.to_string().contains("non-negative"), "{error}");
}

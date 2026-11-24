// Test assertions may unwrap/expect/panic: fixture-driven, deterministic inputs.
// Direct indexing is bounded by the committed example's two targets.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

//! Config loading specs: the committed example must parse into the core model
//! and malformed configs must be rejected with useful errors.

use amtracker_core::{
    config::TargetKind,
    domain::{
        ItemRef,
        Span,
    },
};
use amtracker_server::config_file::{
    load_config,
    parse_config_str,
    parse_recipe_book,
    ConfigLoadError,
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
    assert!(!config.server.allow_open, "the open-API opt-out defaults to off");

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
        other @ (TargetKind::BlizzardForever { .. } | TargetKind::StaticUrls { .. }) => {
            panic!("expected blizzard-connected-realm, got {other:?}")
        }
    }
    // The example pins the auth explicitly: OAuth client credentials with
    // env var NAMES (never the secrets themselves).
    let auth = first.auth.as_ref().expect("blizzard default auth");
    let amtracker_core::config::AuthConfig::ClientCredentials { client_id_env, client_secret_env } =
        auth
    else {
        panic!("expected client-credentials auth, got {auth:?}")
    };
    assert_eq!(client_id_env, "BLIZZARD_CLIENT_ID");
    assert_eq!(client_secret_env, "BLIZZARD_CLIENT_SECRET");
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

#[test]
fn unknown_top_level_key_is_rejected_by_path() {
    // Prepended: a bare key after `[[targets]]` would belong to that table.
    let text = format!("loging = true\n{EXAMPLE}");
    let error = parse_config_str(&text).expect_err("unknown top-level key must fail");
    assert!(error.to_string().contains("unknown config keys: loging"), "{error}");
}

#[test]
fn unknown_section_key_is_rejected_by_path() {
    let text = format!("{EXAMPLE}\n[sentry]\ndsn = \"\"\nenvironment2 = \"production\"\n");
    let error = parse_config_str(&text).expect_err("unknown sentry key must fail");
    assert!(error.to_string().contains("unknown config keys: sentry.environment2"), "{error}");
}

#[test]
fn unknown_target_key_is_rejected_by_path() {
    let text = format!(
        "{EXAMPLE}\n[[targets]]\nid = \"typo\"\nkind = \"static-urls\"\n\
         commidities_url = \"https://example.org/a.json\"\
         \nauctions_url = \"https://example.org/b.json\"\
         \npoll = {{ commodities_secs = 60, auctions_secs = 60 }}\n"
    );
    let error = parse_config_str(&text).expect_err("unknown target key must fail");
    assert!(
        error.to_string().contains("unknown config keys: targets[2].commidities_url"),
        "{error}"
    );
}

#[test]
fn both_kind_profiles_keys_are_accepted_in_targets() {
    // The kind enum is flattened, so either profile's keys are legal in the
    // table; the check must not reject a blizzard target for having them.
    let text = format!(
        "{EXAMPLE}\n[[targets]]\nid = \"us-5\"\nkind = \"blizzard-connected-realm\"\
         \nregion = \"eu\"\nconnected_realm_id = 5\
         \npoll = {{ commodities_secs = 60, auctions_secs = 60 }}\n"
    );
    let loaded = parse_config_str(&text).expect("known kind keys must parse");
    assert_eq!(loaded.tracker.targets.len(), 3);
}

#[test]
fn implicit_blizzard_auth_defaults_to_client_credentials() {
    let text = format!(
        "{EXAMPLE}\n[[targets]]\nid = \"us-5\"\nkind = \"blizzard-connected-realm\"\
         \nregion = \"eu\"\nconnected_realm_id = 5\
         \npoll = {{ commodities_secs = 60, auctions_secs = 60 }}\n"
    );
    let loaded = parse_config_str(&text).expect("parses");
    let auth = loaded.tracker.targets[2].auth.as_ref().expect("default auth");
    assert_eq!(auth, &amtracker_core::config::AuthConfig::default_blizzard());
}

#[test]
fn forever_target_parses_with_forever_flavor_and_default_auth() {
    let text = format!(
        "{EXAMPLE}\n[[targets]]\nid = \"forever-eu-pvp-alliance\"\
         \nkind = \"blizzard-forever\"\nregion = \"eu\"\nruleset_id = 1\nhouse_id = 113\
         \npoll = {{ commodities_secs = 3600, auctions_secs = 1800 }}\n"
    );
    let loaded = parse_config_str(&text).expect("forever target must parse");
    let last = loaded.tracker.targets.last().expect("target");
    assert_eq!(last.id.as_str(), "forever-eu-pvp-alliance");
    assert_eq!(last.flavor.as_str(), "forever", "the kind defaults its flavor");
    let auth = last.auth.as_ref().expect("default blizzard auth");
    assert_eq!(auth, &amtracker_core::config::AuthConfig::default_blizzard());
    match &last.kind {
        TargetKind::BlizzardForever { region, ruleset_id, house_id } => {
            assert_eq!(region, "eu");
            assert_eq!(*ruleset_id, 1);
            assert_eq!(*house_id, 113);
        }
        other => panic!("expected blizzard-forever, got {other:?}"),
    }
}

#[test]
fn forever_target_rejects_non_positive_ids() {
    let text = format!(
        "{EXAMPLE}\n[[targets]]\nid = \"forever-eu-pvp-alliance\"\
         \nkind = \"blizzard-forever\"\nregion = \"eu\"\nruleset_id = 0\nhouse_id = 113\
         \npoll = {{ commodities_secs = 60, auctions_secs = 60 }}\n"
    );
    let error = parse_config_str(&text).expect_err("ruleset_id 0 must fail");
    assert!(error.to_string().contains("ruleset_id must be positive"), "{error}");
}

#[test]
fn forever_unknown_target_key_is_rejected() {
    let text = format!(
        "{EXAMPLE}\n[[targets]]\nid = \"forever-eu-pvp-alliance\"\
         \nkind = \"blizzard-forever\"\nregion = \"eu\"\nruleset_id = 1\nhouse_ids = 113\
         \npoll = {{ commodities_secs = 60, auctions_secs = 60 }}\n"
    );
    let error = parse_config_str(&text).expect_err("house_ids typo must fail");
    assert!(error.to_string().contains("targets[2].house_ids"), "{error}");
}

#[test]
fn explicit_bearer_auth_still_parses() {
    let text = EXAMPLE.replace(
        "auth = { type = \"client-credentials\", client_id_env = \"BLIZZARD_CLIENT_ID\", client_secret_env = \"BLIZZARD_CLIENT_SECRET\" }",
        "auth = { type = \"bearer\", env = \"AMT_TOKEN\" }",
    );
    let loaded = parse_config_str(&text).expect("bearer auth must still parse");
    let auth = loaded.tracker.targets[0].auth.as_ref().expect("bearer auth");
    let amtracker_core::config::AuthConfig::Bearer { env } = auth else {
        panic!("expected bearer auth, got {auth:?}")
    };
    assert_eq!(env, "AMT_TOKEN");
}

#[test]
fn allow_open_opt_out_parses_and_defaults_off() {
    // Absent key → false (the guard is on by default).
    let loaded = parse_config_str(EXAMPLE).expect("example parses");
    assert!(!loaded.tracker.server.allow_open);

    // Explicit true → true (and the key is a known [server] key).
    let text = EXAMPLE.replace(
        "token_env = \"AMT_API_TOKEN\"",
        "token_env = \"AMT_API_TOKEN\"\nallow_open = true",
    );
    let loaded = parse_config_str(&text).expect("allow_open = true must parse");
    assert!(loaded.tracker.server.allow_open);

    // A typo'd variant is rejected by path, like every unknown key.
    let text = EXAMPLE.replace(
        "token_env = \"AMT_API_TOKEN\"",
        "token_env = \"AMT_API_TOKEN\"\nallow_openn = true",
    );
    let error = parse_config_str(&text).expect_err("typo'd allow_open must fail");
    assert!(error.to_string().contains("server.allow_openn"), "{error}");
}

#[test]
fn unknown_auth_key_is_rejected_by_path() {
    let text = EXAMPLE.replace(
        "client_secret_env = \"BLIZZARD_CLIENT_SECRET\" }",
        "client_secret = \"BLIZZARD_CLIENT_SECRET\" }",
    );
    let error = parse_config_str(&text).expect_err("unknown auth key must fail");
    assert!(error.to_string().contains("targets[0].auth.client_secret"), "{error}");
}

#[test]
fn blank_credential_env_names_are_rejected() {
    let text = EXAMPLE
        .replace("client_secret_env = \"BLIZZARD_CLIENT_SECRET\"", "client_secret_env = \"  \"");
    let error = parse_config_str(&text).expect_err("blank env name must fail");
    assert!(error.to_string().contains("must not be empty"), "{error}");
}

// --------------------------------------------------------------- recipe book

#[test]
fn parses_the_recipe_book_with_reagents_in_id_order() {
    let book = "[[recipe]]\n\
                item = 13446\n\
                [recipe.reagents]\n\
                2449 = 1\n\
                765 = 2\n\
                \n\
                [[recipe]]\n\
                item = 999\n\
                makes = 5\n\
                reagents = { 765 = 1 }\n";
    let recipes = parse_recipe_book(book).expect("recipe book must parse");
    assert_eq!(recipes.len(), 2);
    assert_eq!(recipes[0].item, ItemRef::Item { item_id: 13446 });
    assert_eq!(recipes[0].makes, 1, "makes defaults to 1");
    let reagents: Vec<(i64, i64)> = recipes[0]
        .reagents
        .iter()
        .map(|reagent| match reagent.item {
            ItemRef::Item { item_id } => (item_id, reagent.qty),
            ItemRef::Pet { .. } => panic!("recipes only deal in items"),
        })
        .collect();
    assert_eq!(reagents, [(765, 2), (2449, 1)], "sorted by id, not file order");
    assert_eq!(recipes[1].item, ItemRef::Item { item_id: 999 });
    assert_eq!(recipes[1].makes, 5);
    assert_eq!(parse_recipe_book("").expect("an empty book is a valid book"), Vec::new());
}

#[test]
fn rejects_malformed_recipe_books() {
    let error = parse_recipe_book("[[recipe]]\nitem = 1\nquanity = 2\n")
        .expect_err("a typo in a key must not be ignored");
    assert!(matches!(error, ConfigLoadError::Toml(_)), "{error:?}");

    let error = parse_recipe_book("[[recipe]]\nitem = 1\n[recipe.reagents]\nherb = 2\n")
        .expect_err("reagent keys are item ids");
    match error {
        ConfigLoadError::Validation(message) => {
            assert!(message.0.contains("reagent 'herb' is not an item id"), "{message}");
        }
        other => panic!("expected a validation error, got {other:?}"),
    }

    assert!(parse_recipe_book("[[recipe]]\nmakes = 1\n").is_err(), "item is required");
}

#[test]
fn the_crafts_section_maps_its_knobs() {
    let text = format!(
        "{EXAMPLE}\n[crafts]\nrecipes = \"recipes.toml\"\nmin_sale_rate = 0.3\nah_cut_percent = 3\n"
    );
    let loaded = parse_config_str(&text).expect("the crafts section must parse");
    assert_eq!(loaded.tracker.crafts.recipes_file.as_deref(), Some("recipes.toml"));
    assert_eq!(loaded.tracker.crafts.min_sale_rate, 0.3);
    assert_eq!(loaded.tracker.crafts.ah_cut_percent, 3.0);
    assert!(loaded.tracker.crafts.recipes.is_empty(), "the string loader reads no files");

    let error = parse_config_str(&format!("{EXAMPLE}\n[crafts]\nrecipe = \"x\"\n"))
        .expect_err("unknown crafts key");
    assert!(matches!(error, ConfigLoadError::UnknownKeys(_)), "{error:?}");
    let error = parse_config_str(&format!("{EXAMPLE}\n[crafts]\nmin_sale_rate = 1.5\n"))
        .expect_err("out-of-range floor");
    assert!(matches!(error, ConfigLoadError::Validation(_)), "{error:?}");

    // Defaults apply when the section is absent.
    let plain = parse_config_str(EXAMPLE).expect("example parses");
    assert_eq!(plain.tracker.crafts.min_sale_rate, 0.2);
    assert_eq!(plain.tracker.crafts.ah_cut_percent, 5.0);
    assert!(plain.tracker.crafts.recipes_file.is_none());
}

/// A TOML basic string for a filesystem path (backslashes escaped).
fn toml_path(path: &std::path::Path) -> String {
    let text = path.to_str().expect("utf8 temp path");
    format!("\"{}\"", text.replace('\\', "\\\\"))
}

#[test]
fn load_config_reads_and_validates_the_recipe_file() {
    let dir = std::env::temp_dir().join(format!(
        "amtracker-config-recipes-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let recipes_path = dir.join("recipes.toml");
    std::fs::write(&recipes_path, "[[recipe]]\nitem = 1\nreagents = { 2 = 1, 3 = 2 }\n")
        .expect("write recipes");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!("{EXAMPLE}\n[crafts]\nrecipes = {}\n", toml_path(&recipes_path)),
    )
    .expect("write config");

    let loaded = load_config(&config_path).expect("config and recipes load");
    assert_eq!(loaded.tracker.crafts.recipes.len(), 1);
    assert_eq!(loaded.tracker.crafts.recipes[0].reagents.len(), 2);

    // A malformed book fails the load naming the recipe.
    std::fs::write(&recipes_path, "[[recipe]]\nitem = 1\n[recipe.reagents]\n2 = 0\n")
        .expect("rewrite recipes");
    match load_config(&config_path).expect_err("a zero quantity must fail") {
        ConfigLoadError::Validation(message) => {
            assert!(message.0.contains("recipe for item 1"), "{message}");
        }
        other => panic!("expected a validation error, got {other:?}"),
    }

    // A missing recipe file is an IO error naming the path.
    std::fs::write(
        &config_path,
        format!("{EXAMPLE}\n[crafts]\nrecipes = {}\n", toml_path(&dir.join("missing.toml"))),
    )
    .expect("rewrite config");
    assert!(matches!(load_config(&config_path), Err(ConfigLoadError::Io(_))));
    std::fs::remove_dir_all(&dir).ok();
}

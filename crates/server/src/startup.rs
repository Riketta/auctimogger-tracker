//! `serve` startup safety checks: refuse to open the mutating API to the
//! network by accident, refuse to run ingestion without credentials, and
//! surface placeholder/example configuration loudly. All checks run before
//! the store, scheduler or listener are created — a misconfigured deploy
//! must fail in milliseconds with a message, not serve an open API.

use amtracker_core::config::{
    AuthConfig,
    TargetKind,
    TrackerConfig,
};

/// The literal dev token the demo compose file defaults to. It is a REAL
/// string (an empty value would leave the API open), but it must never reach
/// production: the server announces it at startup and the compose comment
/// says "replace me".
pub const DEV_INSECURE_TOKEN: &str = "dev-insecure-token-change-me";

/// How the mutating API ends up configured for this run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenPosture {
    /// A real token is configured: mutations require the bearer.
    Protected,
    /// Open because the bind address is loopback-only (developer mode).
    OpenLoopback,
    /// Open on a non-loopback bind because `[server].allow_open = true`.
    OpenExplicit,
    /// Protected, but the token is the well-known compose default.
    DevDefaultToken,
}

/// The startup decision for the mutating-API posture, pure in its inputs so
/// the matrix is testable without env manipulation:
/// - a configured, non-empty token protects the API (warning only when it is
///   the well-known dev default);
/// - an unset/empty token stays open on loopback binds (documented dev
///   mode) but refuses to start on any other bind unless
///   `[server].allow_open = true` explicitly opts out.
///
/// # Errors
///
/// Returns a startup-refusing message when the API would be open on a
/// non-loopback bind without the explicit opt-out.
pub fn open_api_decision(
    token: Option<&str>,
    bind_loopback: bool,
    allow_open: bool,
) -> Result<TokenPosture, String> {
    match token {
        Some(value) if value == DEV_INSECURE_TOKEN => Ok(TokenPosture::DevDefaultToken),
        Some(_) => Ok(TokenPosture::Protected),
        None => {
            if bind_loopback {
                Ok(TokenPosture::OpenLoopback)
            } else if allow_open {
                Ok(TokenPosture::OpenExplicit)
            } else {
                Err("refusing to start `serve`: the mutating API would be OPEN on a \
                     non-loopback bind (token env unset or empty). Set the token, bind a \
                     loopback address, or set `[server].allow_open = true` to accept the risk"
                    .into())
            }
        }
    }
}

/// Whether a bind string provably listens on loopback only. Unparseable
/// binds cannot be proven loopback and count as exposed (conservative).
#[must_use]
pub fn bind_is_loopback(bind: &str) -> bool {
    match bind.parse::<std::net::SocketAddr>() {
        Ok(addr) => addr.ip().is_loopback(),
        Err(_) => false,
    }
}

/// Whether an env var name resolves to a non-blank value.
fn env_present(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
}

/// Runs every startup check. Warnings are logged (they reach the file log
/// and Sentry); refusals return an error the caller turns into a fatal exit.
///
/// # Errors
///
/// Returns the refusal message when the mutating API would be open on a
/// non-loopback bind without `[server].allow_open`, or when a configured
/// Blizzard target's OAuth credential env vars are missing/empty.
pub fn validate_serve_startup(config: &TrackerConfig) -> Result<(), String> {
    // 1. Ingestion credentials must resolve NOW, not on the first cycle: a
    //    deploy with an expired secret would otherwise serve a UI that
    //    never ingests ("feed ingestion unavailable").
    let mut missing = Vec::new();
    for target in &config.targets {
        if let Some(AuthConfig::ClientCredentials { client_id_env, client_secret_env }) =
            &target.auth
        {
            for (what, env) in [("client id", client_id_env), ("client secret", client_secret_env)]
            {
                if !env_present(env) {
                    missing.push(format!(
                        "target '{}': environment variable {env} ({what}) is unset or empty",
                        target.id
                    ));
                }
            }
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "refusing to start `serve`: configured blizzard ingestion credentials are \
             missing:\n  {}",
            missing.join("\n  ")
        ));
    }

    // 2. Placeholder URLs: the committed demo config points at example.org;
    //    a deploy that forgot to edit it would poll forever without data.
    for target in &config.targets {
        if let TargetKind::StaticUrls { commodities_url, auctions_url } = &target.kind {
            for (name, url) in
                [("commodities_url", commodities_url), ("auctions_url", auctions_url)]
            {
                if url.contains("example.org") {
                    tracing::warn!(
                        target = %target.id,
                        field = name,
                        "static-urls target still points at the example.org placeholder; \
                         replace it with a real endpoint"
                    );
                }
            }
        }
    }

    // 3. Mutating-API posture (see `open_api_decision`).
    let token = config
        .server
        .token_env
        .as_ref()
        .and_then(|env| std::env::var(env).ok())
        .filter(|value| !value.trim().is_empty());
    let loopback = bind_is_loopback(&config.server.bind);
    match open_api_decision(token.as_deref(), loopback, config.server.allow_open) {
        Ok(TokenPosture::Protected) => {}
        Ok(posture) => match posture {
            TokenPosture::OpenLoopback => {
                if let Some(env) = &config.server.token_env {
                    tracing::warn!(
                        env = %env,
                        "bearer token env unset or empty - mutating API endpoints are OPEN \
                         (loopback bind)"
                    );
                } else {
                    tracing::warn!(
                        "no token_env configured - mutating API endpoints are OPEN \
                         (loopback bind)"
                    );
                }
            }
            TokenPosture::OpenExplicit => {
                tracing::warn!(
                    bind = %config.server.bind,
                    "[server].allow_open is set - mutating API endpoints are OPEN on a \
                     NON-LOOPBACK bind; anyone who can reach this port can mutate the \
                     watchlist and trigger fetches"
                );
            }
            TokenPosture::DevDefaultToken => {
                tracing::warn!(
                    "the API token is the well-known compose default \
                     ('dev-insecure-token-change-me') - REPLACE ME before exposing this \
                     instance beyond localhost"
                );
            }
            TokenPosture::Protected => {}
        },
        Err(message) => return Err(message),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect/panic: fixture-driven, deterministic
    // inputs (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_real_token_protects_any_bind() {
        assert_eq!(open_api_decision(Some("s3cret"), true, false), Ok(TokenPosture::Protected));
        assert_eq!(open_api_decision(Some("s3cret"), false, false), Ok(TokenPosture::Protected));
        assert_eq!(open_api_decision(Some("s3cret"), false, true), Ok(TokenPosture::Protected));
    }

    #[test]
    fn the_dev_default_token_warns_but_does_not_refuse() {
        assert_eq!(
            open_api_decision(Some(DEV_INSECURE_TOKEN), false, false),
            Ok(TokenPosture::DevDefaultToken)
        );
    }

    #[test]
    fn an_unset_token_stays_open_only_on_loopback() {
        assert_eq!(open_api_decision(None, true, false), Ok(TokenPosture::OpenLoopback));
        assert_eq!(open_api_decision(None, true, true), Ok(TokenPosture::OpenLoopback));
    }

    #[test]
    fn an_open_api_on_a_public_bind_needs_the_explicit_opt_out() {
        let error = open_api_decision(None, false, false).expect_err("must refuse");
        assert!(error.contains("allow_open"), "{error}");
        assert_eq!(
            open_api_decision(None, false, true),
            Ok(TokenPosture::OpenExplicit),
            "explicit opt-out allowed"
        );
    }

    #[test]
    fn loopback_detection_covers_the_documented_bind_shapes() {
        assert!(bind_is_loopback("127.0.0.1:8080"));
        assert!(bind_is_loopback("127.0.0.1:0"));
        assert!(bind_is_loopback("[::1]:8080"));
        assert!(!bind_is_loopback("0.0.0.0:8080"));
        assert!(!bind_is_loopback("[::]:8080"));
        assert!(!bind_is_loopback("192.168.1.10:8080"));
        // Unparseable binds cannot be proven loopback: exposed.
        assert!(!bind_is_loopback("localhost:8080"));
        assert!(!bind_is_loopback("not an address"));
    }
}

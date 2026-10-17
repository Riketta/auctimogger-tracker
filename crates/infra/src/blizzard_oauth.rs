//! `Battle.net` `OAuth2` client-credentials token source (driven adapter):
//! exchanges a client id/secret for bearer tokens, caches them in memory and
//! refreshes them ahead of expiry. Used by both Blizzard adapters; the core
//! never sees tokens.

use std::{
    sync::Mutex,
    time::{
        Duration,
        Instant,
    },
};

use amtracker_core::error::SourceError;
use base64::Engine as _;
use serde::Deserialize;

/// A cached token is treated as stale this long before its reported expiry,
/// so in-flight requests never ride on an about-to-expire token.
const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// Upper bound for an accepted token lifetime (30 days). Real Blizzard
/// answers are ~24h; the cap also keeps `Instant + Duration` math
/// overflow-free no matter what a hostile endpoint reports.
const MAX_TOKEN_LIFETIME_SECS: u64 = 30 * 24 * 3_600;

/// Read-cap for one token-endpoint answer (real answers are ~1 KB).
const MAX_TOKEN_BODY_BYTES: u64 = 1024 * 1024;

/// The lifetime accepted into the cache: a hostile `expires_in` is clamped
/// so the `Instant` arithmetic below can never overflow.
fn clamped_lifetime(expires_in: u64) -> u64 {
    expires_in.min(MAX_TOKEN_LIFETIME_SECS)
}

/// `Battle.net` `OAuth2` client-credentials token source for one (endpoint,
/// credentials) pair. `token()` returns a cached access token while it is
/// fresh and fetches a new one otherwise; [`invalidate`](Self::invalidate)
/// drops the cache after a 401 so the next call refetches.
pub struct OAuthTokenSource {
    agent: ureq::Agent,
    endpoint: String,
    client_id_env: String,
    client_secret_env: String,
    cache: Mutex<Option<CachedToken>>,
}

/// A cached token answer: the bearer value and when it stops being usable
/// (expiry minus the refresh margin).
#[derive(Clone)]
struct CachedToken {
    access_token: String,
    /// When the token stops being usable (expiry minus the refresh margin).
    expires_at: Instant,
}

impl OAuthTokenSource {
    /// Source for the regional `Battle.net` OAuth endpoint (`region` set) or
    /// the global one (`None`). Id and secret are read from the named
    /// environment variables on every fetch, so rotated credentials are
    /// picked up without a restart.
    #[must_use]
    pub fn new(
        agent: ureq::Agent,
        region: Option<&str>,
        client_id_env: &str,
        client_secret_env: &str,
    ) -> Self {
        Self {
            agent,
            endpoint: token_endpoint(region),
            client_id_env: client_id_env.to_owned(),
            client_secret_env: client_secret_env.to_owned(),
            cache: Mutex::new(None),
        }
    }

    /// A fresh access token, from the cache when possible. The fetch happens
    /// under the cache lock, so concurrent callers share one request instead
    /// of stampeding the token endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::Config`] when a credential env var is missing
    /// and [`SourceError::Http`]/[`SourceError::Parse`] when the token
    /// endpoint fails or answers something unexpected.
    pub fn token(&self) -> Result<String, SourceError> {
        let mut cache = self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.as_ref() {
            if !needs_refresh(Instant::now(), cached.expires_at) {
                return Ok(cached.access_token.clone());
            }
        }
        let fetched = self.fetch_token()?;
        *cache = Some(fetched.clone());
        Ok(fetched.access_token)
    }

    /// Drops the cached token: the next [`token`](Self::token) call fetches
    /// afresh. Called after a 401, when a cached token was rejected before
    /// its time (revocation, clock skew).
    pub fn invalidate(&self) {
        *self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    fn fetch_token(&self) -> Result<CachedToken, SourceError> {
        let client_id = credential(&self.client_id_env, "client id")?;
        let client_secret = credential(&self.client_secret_env, "client secret")?;
        let response = self
            .agent
            .post(&self.endpoint)
            .set("Authorization", &basic_auth(&client_id, &client_secret))
            .send_form(&[("grant_type", "client_credentials")])
            .map_err(|e| SourceError::Http(format!("token endpoint: {e}")))?;
        let status = response.status();
        if status != 200 {
            return Err(SourceError::Http(format!("token endpoint status {status}")));
        }
        let bytes = crate::blizzard_source::read_capped(
            response.into_reader(),
            MAX_TOKEN_BODY_BYTES,
            "token body",
        )?;
        let (access_token, expires_in) = parse_token_response(&bytes)?;
        Ok(CachedToken {
            access_token,
            expires_at: Instant::now() + Duration::from_secs(clamped_lifetime(expires_in)),
        })
    }
}

/// The token endpoint for a Blizzard API region, or the global host when the
/// region is unknown (static-urls targets may still need Blizzard tokens).
#[must_use]
pub fn token_endpoint(region: Option<&str>) -> String {
    match region {
        Some(region) => format!("https://{region}.oauth.battle.net/token"),
        None => "https://oauth.battle.net/token".to_string(),
    }
}

/// Reads one credential env var; blank values are treated as missing (a
/// leftover empty `.env` line must not reach the token endpoint).
fn credential(env: &str, what: &str) -> Result<String, SourceError> {
    match std::env::var(env) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        Ok(_) => {
            Err(SourceError::Config(format!("environment variable {env} is empty (OAuth {what})")))
        }
        Err(_) => {
            Err(SourceError::Config(format!("missing environment variable {env} (OAuth {what})")))
        }
    }
}

/// Reads the static-bearer token env var; resolved per request so rotated
/// tokens are picked up without a restart.
pub(crate) fn bearer_token(env: &str) -> Result<String, SourceError> {
    std::env::var(env).map_err(|_| {
        SourceError::Config(format!("missing environment variable {env} (bearer token)"))
    })
}

/// Wire shape of the token endpoint answer.
#[derive(Deserialize)]
struct WireToken {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Decodes a token answer into `(access_token, expires_in)`. A token without
/// a known lifetime cannot be refreshed responsibly, so a missing
/// `expires_in` is an error, not a guess.
fn parse_token_response(bytes: &[u8]) -> Result<(String, u64), SourceError> {
    let wire: WireToken = serde_json::from_slice(bytes)
        .map_err(|e| SourceError::Parse(format!("token payload: {e}")))?;
    let expires_in = wire.expires_in.ok_or_else(|| {
        SourceError::Parse(
            "token payload has no expires_in; refusing an unrefreshable token".into(),
        )
    })?;
    Ok((wire.access_token, expires_in))
}

/// RFC 7617 `Basic` scheme for the client-credentials exchange (ureq has no
/// request-level auth helper). `Battle.net` ids/secrets are ASCII.
fn basic_auth(client_id: &str, client_secret: &str) -> String {
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{client_secret}"));
    format!("Basic {encoded}")
}

/// The freshness decision: within [`REFRESH_MARGIN`] of expiry the token is
/// considered stale so the swap happens before any request fails. A margin
/// beyond the platform's `Instant` range reads as stale (always refresh).
fn needs_refresh(now: Instant, expires_at: Instant) -> bool {
    now.checked_add(REFRESH_MARGIN).is_none_or(|threshold| threshold >= expires_at)
}

// ------------------------------------------------- 401 refresh-retry helper

/// Token-provisioning surface the 401-retry helper needs. Implemented by the
/// live [`OAuthTokenSource`] and by test stubs, so the retry logic is
/// unit-testable without HTTP.
pub trait RefreshableCredential {
    /// A bearer token, fetched fresh when the cache is empty or stale.
    ///
    /// # Errors
    ///
    /// Propagates credential/endpoint failures as [`SourceError`].
    fn token(&self) -> Result<String, SourceError>;
    /// Drops any cached token so the next [`token`](Self::token) refetches.
    fn invalidate(&self);
}

impl RefreshableCredential for OAuthTokenSource {
    fn token(&self) -> Result<String, SourceError> {
        // The inherent method takes precedence in normal call position; the
        // explicit path disambiguates inside this trait impl.
        OAuthTokenSource::token(self)
    }

    fn invalidate(&self) {
        OAuthTokenSource::invalidate(self);
    }
}

/// What a request authenticates with. `ureq` 2.x reports HTTP error statuses
/// as `Err(Error::Status(..))`, so "call and check the response status" never
/// sees real 401s — this trait is how the fetch path learns which rejected
/// tokens can be refreshed.
pub trait RequestAuth {
    /// The bearer token for the first attempt; `None` = no auth header.
    ///
    /// # Errors
    ///
    /// Propagates credential resolution failures as [`SourceError`].
    fn initial_token(&self) -> Result<Option<String>, SourceError>;
    /// The refresh seam when a 401 may trigger one invalidate+retry cycle.
    fn refresh_source(&self) -> Option<&dyn RefreshableCredential>;
}

/// Failure modes of one send attempt, split so the retry helper recognizes a
/// refreshable 401 without knowing HTTP details.
pub enum CallFailure {
    /// The endpoint answered 401: the presented credential was rejected.
    Unauthorized,
    /// Anything else is terminal (transport failure or non-401 status).
    Terminal(SourceError),
}

/// Performs an authorized call with the documented 401 semantics: `send`
/// receives the bearer token (`None` = no Authorization header). When the
/// first attempt fails with a 401 and the credential has a refresh source,
/// the cached token is dropped, a fresh one is fetched, and the request is
/// retried exactly once. A second 401 — or a 401 on a credential without a
/// refresh source (static bearer, anonymous) — surfaces as a terminal
/// [`SourceError::Http`] carrying the status.
///
/// # Errors
///
/// Returns the first [`CallFailure::Terminal`] as-is, credential resolution
/// failures from [`RequestAuth::initial_token`]/refresh, and
/// [`SourceError::Http`] for unrecoverable 401s.
pub fn call_refreshing_on_401<T, F>(auth: &dyn RequestAuth, mut send: F) -> Result<T, SourceError>
where
    F: FnMut(Option<&str>) -> Result<T, CallFailure>,
{
    let token = auth.initial_token()?;
    match send(token.as_deref()) {
        Ok(value) => Ok(value),
        Err(CallFailure::Terminal(error)) => Err(error),
        Err(CallFailure::Unauthorized) => {
            let Some(source) = auth.refresh_source() else {
                return Err(SourceError::Http(
                    "source answered 401 unauthorized (static bearer or unauthenticated \
                     request - nothing to refresh)"
                        .into(),
                ));
            };
            // A cached token can be rejected before its expiry (revoked,
            // clock skew): drop it and retry once with a freshly fetched one.
            tracing::debug!("oauth token rejected; refreshing and retrying once");
            source.invalidate();
            let fresh = source.token()?;
            match send(Some(fresh.as_str())) {
                Ok(value) => Ok(value),
                Err(CallFailure::Terminal(error)) => Err(error),
                Err(CallFailure::Unauthorized) => Err(SourceError::Http(
                    "source still answers 401 unauthorized after one OAuth token refresh".into(),
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect: fixture-driven, deterministic inputs
    // (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn regional_targets_use_the_regional_oauth_host() {
        assert_eq!(token_endpoint(Some("eu")), "https://eu.oauth.battle.net/token");
    }

    #[test]
    fn regionless_targets_use_the_global_oauth_host() {
        assert_eq!(token_endpoint(None), "https://oauth.battle.net/token");
    }

    #[test]
    fn token_answer_parses_into_token_and_lifetime() {
        let payload = br#"{"access_token":"EUabcd1234","token_type":"bearer","expires_in":29999}"#;
        let (token, expires_in) = parse_token_response(payload).expect("parses");
        assert_eq!(token, "EUabcd1234");
        assert_eq!(expires_in, 29_999);
    }

    #[test]
    fn token_answer_without_the_token_is_an_error() {
        let payload = br#"{"token_type":"bearer","expires_in":29999}"#;
        let error = parse_token_response(payload).expect_err("no access_token");
        assert!(error.to_string().contains("token payload"), "{error}");
    }

    #[test]
    fn unrefreshable_token_answers_are_rejected() {
        let payload = br#"{"access_token":"EUabcd1234"}"#;
        let error = parse_token_response(payload).expect_err("no expires_in");
        assert!(error.to_string().contains("expires_in"), "{error}");
    }

    #[test]
    fn non_token_json_is_a_parse_error() {
        let error = parse_token_response(br"<html>").expect_err("html is not a token");
        assert!(error.to_string().contains("token payload"), "{error}");
    }

    #[test]
    fn basic_auth_matches_the_rfc_scheme() {
        // RFC 7617 example: base64("Aladdin:open sesame").
        assert_eq!(basic_auth("Aladdin", "open sesame"), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    }

    #[test]
    fn refresh_kicks_in_inside_the_margin_only() {
        let now = Instant::now();
        // Well inside the lifetime: fresh.
        assert!(!needs_refresh(now, now + Duration::from_secs(3_600)));
        // Exactly at the margin boundary: stale (swap before any request).
        assert!(needs_refresh(now, now + REFRESH_MARGIN));
        // Already expired: stale.
        assert!(needs_refresh(now, now));
        // Far-future expiry (the clamp ceiling): fresh.
        assert!(!needs_refresh(now, now + Duration::from_secs(MAX_TOKEN_LIFETIME_SECS)));
    }

    #[test]
    fn hostile_lifetimes_are_clamped_for_the_instant_math() {
        // The clamp keeps `Instant::now() + lifetime` panic-free for any
        // `expires_in` a broken or hostile endpoint reports.
        assert_eq!(clamped_lifetime(u64::MAX), MAX_TOKEN_LIFETIME_SECS);
        assert_eq!(clamped_lifetime(29_999), 29_999);
        let now = Instant::now();
        assert!(!needs_refresh(now, now + Duration::from_secs(clamped_lifetime(u64::MAX))));
    }

    // ---------------------------------------------- 401 refresh-retry helper

    /// Credential stub: hands out scripted tokens and counts invalidations.
    struct StubCredential {
        tokens: Mutex<Vec<&'static str>>,
        invalidations: Mutex<usize>,
    }

    impl StubCredential {
        fn new(tokens: &[&'static str]) -> Self {
            Self { tokens: Mutex::new(tokens.to_vec()), invalidations: Mutex::new(0) }
        }

        fn invalidations(&self) -> usize {
            *self.invalidations.lock().unwrap()
        }
    }

    impl RefreshableCredential for StubCredential {
        fn token(&self) -> Result<String, SourceError> {
            self.tokens
                .lock()
                .unwrap()
                .pop()
                .map(str::to_owned)
                .ok_or_else(|| SourceError::Config("stub token list exhausted".into()))
        }

        fn invalidate(&self) {
            *self.invalidations.lock().unwrap() += 1;
        }
    }

    /// Auth stub over a [`StubCredential`].
    struct StubAuth {
        credential: StubCredential,
        with_refresh: bool,
    }

    impl RequestAuth for StubAuth {
        fn initial_token(&self) -> Result<Option<String>, SourceError> {
            if self.with_refresh {
                self.credential.token().map(Some)
            } else {
                // Static-bearer shape: token present, no refresh source.
                Ok(Some("static-bearer".into()))
            }
        }

        fn refresh_source(&self) -> Option<&dyn RefreshableCredential> {
            if self.with_refresh {
                Some(&self.credential)
            } else {
                None
            }
        }
    }

    /// A send stub that answers 401 for the given tokens, succeeds otherwise,
    /// and records the tokens it was called with.
    fn rejecting_send<'a>(
        reject: &'a [&'static str],
    ) -> impl FnMut(Option<&str>) -> Result<&'static str, CallFailure> + 'a {
        let seen = std::cell::RefCell::new(Vec::new());
        move |token: Option<&str>| {
            seen.borrow_mut().push(token.map(str::to_owned));
            let rejected = token.is_some_and(|t| reject.contains(&t));
            if rejected {
                Err(CallFailure::Unauthorized)
            } else {
                Ok("payload")
            }
        }
    }

    #[test]
    fn a_401_invalidates_and_retries_once_with_a_fresh_token() {
        let auth =
            StubAuth { credential: StubCredential::new(&["fresh", "stale"]), with_refresh: true };
        let send = rejecting_send(&["stale"]);
        let result = call_refreshing_on_401(&auth, send).expect("retry succeeds");
        assert_eq!(result, "payload");
        assert_eq!(auth.credential.invalidations(), 1, "exactly one invalidation");
    }

    #[test]
    fn a_second_401_surfaces_as_a_terminal_error() {
        let auth =
            StubAuth { credential: StubCredential::new(&["fresh-2", "stale"]), with_refresh: true };
        let error = call_refreshing_on_401(&auth, rejecting_send(&["stale", "fresh-2"]))
            .expect_err("both attempts rejected");
        assert!(error.to_string().contains("401"), "{error}");
        assert_eq!(auth.credential.invalidations(), 1, "only one refresh cycle");
    }

    #[test]
    fn a_401_without_a_refresh_source_is_terminal() {
        let auth = StubAuth { credential: StubCredential::new(&[]), with_refresh: false };
        let error = call_refreshing_on_401(&auth, rejecting_send(&["static-bearer"]))
            .expect_err("static bearer cannot refresh");
        assert!(error.to_string().contains("nothing to refresh"), "{error}");
    }

    #[test]
    fn terminal_failures_pass_through_without_a_retry() {
        let auth = StubAuth { credential: StubCredential::new(&["unused"]), with_refresh: true };
        let result: Result<&'static str, SourceError> = call_refreshing_on_401(&auth, |_| {
            Err(CallFailure::Terminal(SourceError::Http("status 503 from source".into())))
        });
        let error = result.expect_err("terminal failure");
        assert!(error.to_string().contains("503"), "{error}");
        assert_eq!(auth.credential.invalidations(), 0, "no refresh on non-401");
    }

    #[test]
    fn a_first_attempt_success_never_touches_the_refresh_path() {
        let auth = StubAuth { credential: StubCredential::new(&["good"]), with_refresh: true };
        let result = call_refreshing_on_401(&auth, |token| match token {
            Some("good") => Ok("payload"),
            other => panic!("unexpected token {other:?}"),
        });
        assert_eq!(result.expect("succeeds"), "payload");
        assert_eq!(auth.credential.invalidations(), 0);
    }
}

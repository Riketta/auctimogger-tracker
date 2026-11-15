//! Blizzard "Forever" API discovery: rulesets and their auction houses.
//! Read-only aid for choosing `ruleset_id`/`house_id` in target configs —
//! the Forever API is undocumented, realmless, and has already changed its
//! routing once (the auctions index requires a trailing slash), so this
//! module keeps every discovery URL in one place, next to the fetch
//! adapter's URLs in [`crate::blizzard_source`].

use std::{
    io::Read as _,
    time::Duration,
};

use amtracker_core::{
    config::AuthConfig,
    error::SourceError,
};
use serde::Deserialize;

use crate::blizzard_oauth::OAuthTokenSource;

/// Production-ish timeouts (same posture as the fetch adapter).
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
}

/// One ruleset: the Forever replacement for a realm.
#[derive(Clone, Debug)]
pub struct ForeverRuleset {
    /// Ruleset id (stable, referenced from target configs).
    pub id: i64,
    /// Display name (e.g. ``PvP``), when the payload carries one.
    pub name: Option<String>,
    /// Up/health indicator, when the payload carries one.
    pub status: Option<String>,
}

/// One faction auction house within a ruleset.
#[derive(Clone, Debug)]
pub struct ForeverHouse {
    /// House id (stable, referenced from target configs).
    pub id: i64,
    /// Display name (e.g. "Alliance Auction House").
    pub name: Option<String>,
    /// Wire key (slug-style), when the payload carries one.
    pub key: Option<String>,
}

/// The bearer-token seam for the discovery calls: client credentials from
/// the target config (self-refreshing), or a static env bearer.
enum DiscoveryAuth {
    OAuth(std::sync::Arc<OAuthTokenSource>),
    BearerEnv(String),
}

impl DiscoveryAuth {
    fn from_config(region: &str, auth: &AuthConfig) -> Self {
        match auth {
            AuthConfig::ClientCredentials { client_id_env, client_secret_env } => {
                Self::OAuth(std::sync::Arc::new(OAuthTokenSource::new(
                    agent(),
                    Some(region),
                    client_id_env,
                    client_secret_env,
                )))
            }
            AuthConfig::Bearer { env } => Self::BearerEnv(env.clone()),
        }
    }

    fn token(&self) -> Result<String, SourceError> {
        match self {
            Self::OAuth(source) => source.token(),
            Self::BearerEnv(env) => std::env::var(env).map_err(|_| {
                SourceError::Config(format!("missing environment variable {env} (bearer token)"))
            }),
        }
    }
}

/// One authorized GET returning the raw body (Discovery payloads are tiny).
fn authorized_get(auth: &DiscoveryAuth, url: &str) -> Result<Vec<u8>, SourceError> {
    let token = auth.token()?;
    let request = {
        let mut request = agent().get(url);
        request = request.set("Authorization", &format!("Bearer {token}"));
        request
    };
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(status, _response)) => {
            // Terminal for a one-shot discovery call: no cached token to
            // invalidate beyond what `token()` just fetched. The body is
            // omitted: these error documents carry nothing actionable.
            return Err(SourceError::Http(format!("unexpected status {status} from {url}")));
        }
        Err(error) => return Err(SourceError::Http(error.to_string())),
    };
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(16 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| SourceError::Http(format!("read discovery payload: {e}")))?;
    Ok(bytes)
}

/// Lenient parse helpers: the ruleset index shape is not published, so the
/// decoder accepts both a top-level array and an object carrying the array
/// under a known key, and tolerates missing `name`/`status` fields.
fn decode_list<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    keys: &[&str],
    what: &str,
) -> Result<Vec<T>, SourceError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| SourceError::Parse(format!("{what} payload rejected: {e}")))?;
    let list = match &value {
        serde_json::Value::Array(items) => Some(items.clone()),
        serde_json::Value::Object(map) => {
            keys.iter().find_map(|key| map.get(*key)).and_then(|found| found.as_array()).cloned()
        }
        _ => None,
    };
    let list = list.ok_or_else(|| {
        SourceError::Parse(format!("{what} payload has no {} array", keys.join("/")))
    })?;
    serde_json::from_value::<Vec<T>>(serde_json::Value::Array(list))
        .map_err(|e| SourceError::Parse(format!("{what} row rejected: {e}")))
}

#[derive(Debug, Deserialize)]
struct WireRuleset {
    id: i64,
    #[serde(default, deserialize_with = "lenient_string")]
    name: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireHouse {
    id: i64,
    #[serde(default, deserialize_with = "lenient_string")]
    name: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    key: Option<String>,
}

/// Decodes the string-ish fields of the unpublished Forever payloads
/// leniently: a plain string passes through; a localized map
/// (`{"en_US": …}`) or a Blizzard key object (`{"href": …}`) yields the
/// `href`/`en_US` entry, falling back to the first string value; `null`
/// yields `None`. Anything else is real shape drift and fails the row
/// loudly instead of printing a wrong table.
fn lenient_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None => Ok(None),
        Some(serde_json::Value::String(text)) => Ok(Some(text)),
        Some(serde_json::Value::Object(map)) => {
            let picked = ["href", "en_US"]
                .iter()
                .find_map(|preferred| map.get(*preferred))
                .or_else(|| map.values().find(|value| value.is_string()))
                .and_then(|value| value.as_str());
            match picked {
                Some(text) => Ok(Some(text.to_owned())),
                None => Err(D::Error::custom("string field object carried no string value")),
            }
        }
        // `Value::unexpected` is private in serde_json, so classify here.
        // Null cannot reach this arm: `Option<Value>` maps it to `None` above.
        Some(other) => {
            let found = match &other {
                serde_json::Value::Bool(_) => "a boolean",
                serde_json::Value::Number(_) => "a number",
                serde_json::Value::Array(_) => "an array",
                serde_json::Value::Null
                | serde_json::Value::String(_)
                | serde_json::Value::Object(_) => unreachable!("handled above"),
            };
            Err(D::Error::custom(format!("expected a string or object, found {found}")))
        }
    }
}

/// All rulesets of one region (`/data/wow/ruleset/index`).
///
/// # Errors
///
/// Returns [`SourceError`] on transport/auth failures and on payload shapes
/// this decoder cannot read (the index shape is unpublished; drift fails
/// loudly here instead of printing a wrong table).
pub fn fetch_rulesets(region: &str, auth: &AuthConfig) -> Result<Vec<ForeverRuleset>, SourceError> {
    let credential = DiscoveryAuth::from_config(region, auth);
    let url = format!(
        "https://{region}.api.blizzard.com/data/wow/ruleset/index?namespace=dynamic-forever-{region}&locale=en_US"
    );
    let bytes = authorized_get(&credential, &url)?;
    Ok(decode_list::<WireRuleset>(&bytes, &["rulesets", "results", "data"], "ruleset index")?
        .into_iter()
        .map(|ruleset| ForeverRuleset {
            id: ruleset.id,
            name: ruleset.name,
            status: ruleset.status,
        })
        .collect())
}

/// A ruleset's auction houses. The index URL carries a trailing slash on
/// purpose: the live API routes `/auctions` (no slash) to nothing while
/// `/auctions/` serves the index — the documented quirk, kept explicit.
///
/// # Errors
///
/// Returns [`SourceError`] on transport/auth failures and unreadable
/// payload shapes.
pub fn fetch_auction_houses(
    region: &str,
    ruleset_id: i64,
    auth: &AuthConfig,
) -> Result<Vec<ForeverHouse>, SourceError> {
    let credential = DiscoveryAuth::from_config(region, auth);
    let url = format!(
        "https://{region}.api.blizzard.com/data/wow/ruleset/{ruleset_id}/auctions/?namespace=dynamic-forever-{region}&locale=en_US"
    );
    let bytes = authorized_get(&credential, &url)?;
    Ok(decode_list::<WireHouse>(&bytes, &["auctions"], "auction-house index")?
        .into_iter()
        .map(|house| ForeverHouse { id: house.id, name: house.name, key: house.key })
        .collect())
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect: fixture-driven, deterministic inputs
    // (the blizzard_item_info precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn key_objects_and_localized_name_maps_decode() {
        // The live `/ruleset/{id}/auctions/` shape: Blizzard key objects and
        // localized name maps, which the plain-`String` decoder rejected
        // with "invalid type: map, expected a string".
        let payload = br#"{"auctions": [
            {"id": 2, "name": {"en_US": "Horde Auction House"}, "key": {"href": "https://eu.api.blizzard.com/data/wow/ruleset/1/auctions/2?namespace=dynamic-forever-eu"}},
            {"id": 1, "name": "Alliance Auction House"}
        ]}"#;
        let houses = decode_list::<WireHouse>(payload, &["auctions"], "auction-house index")
            .unwrap()
            .into_iter()
            .map(|house| ForeverHouse { id: house.id, name: house.name, key: house.key })
            .collect::<Vec<_>>();
        assert_eq!(houses.len(), 2);
        assert_eq!(houses[0].id, 2);
        assert_eq!(houses[0].name.as_deref(), Some("Horde Auction House"));
        assert!(houses[0].key.as_deref().is_some_and(|key| key.starts_with("https://")));
        assert_eq!(houses[1].name.as_deref(), Some("Alliance Auction House"));
        assert_eq!(houses[1].key, None);
    }

    #[test]
    fn scalar_shape_drift_still_fails_loudly() {
        let payload = br#"{"auctions": [{"id": 1, "name": 42}]}"#;
        let error = decode_list::<WireHouse>(payload, &["auctions"], "auction-house index")
            .expect_err("a numeric name is drift, not a decodable row");
        assert!(error.to_string().contains("row rejected"));
    }
}

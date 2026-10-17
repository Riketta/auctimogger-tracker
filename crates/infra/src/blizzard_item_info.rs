//! Blizzard item-information adapter (`ItemInfoPort`): resolves item names,
//! quality and class for the search catalog. Data comes from the static
//! game-data namespace and is cached permanently by the catalog repository.

use std::{
    fmt,
    time::Duration,
};

use amtracker_core::{
    config::{
        AuthConfig,
        TargetKind,
        TrackerConfig,
    },
    domain::NameRecord,
    error::SourceError,
    ports::ItemInfoPort,
};
use serde::{
    de::{
        Error,
        IgnoredAny,
        MapAccess,
        Visitor,
    },
    Deserialize,
    Deserializer,
};

use crate::blizzard_oauth::{
    bearer_token,
    call_refreshing_on_401,
    CallFailure,
    OAuthTokenSource,
    RefreshableCredential,
    RequestAuth,
};

/// Read-cap for one item-information payload (real answers are a few KB).
const MAX_ITEM_INFO_BYTES: u64 = 1024 * 1024;

/// How the item API is authenticated.
enum Credential {
    /// Static bearer token read from this env var on every request.
    BearerEnv(String),
    /// Self-refreshing OAuth client-credentials token source.
    OAuth(std::sync::Arc<OAuthTokenSource>),
}

/// Request-auth view over [`BlizzardItemInfo`]'s credential (see
/// [`RequestAuth`]): the OAuth flavor is refreshable, the env bearer is not.
enum ItemAuth<'a> {
    BearerEnv(&'a str),
    OAuth(&'a OAuthTokenSource),
}

impl RequestAuth for ItemAuth<'_> {
    fn initial_token(&self) -> Result<Option<String>, SourceError> {
        Ok(Some(match self {
            Self::BearerEnv(env) => bearer_token(env)?,
            Self::OAuth(source) => source.token()?,
        }))
    }

    fn refresh_source(&self) -> Option<&dyn RefreshableCredential> {
        match self {
            Self::OAuth(source) => Some(*source),
            Self::BearerEnv(_) => None,
        }
    }
}

/// Blizzard item API (`data/wow/item/{id}`, static namespace).
pub struct BlizzardItemInfo {
    agent: ureq::Agent,
    region: String,
    locale: String,
    credential: Credential,
}

impl BlizzardItemInfo {
    /// Derives region and credentials from the config's first Blizzard
    /// target; falls back to `us` with the default Blizzard client-credential
    /// env names.
    #[must_use]
    pub fn from_config(config: &TrackerConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build();
        let (region, auth) = config
            .targets
            .iter()
            .find_map(|target| match &target.kind {
                TargetKind::BlizzardConnectedRealm { region, .. } => {
                    Some((region.clone(), target.auth.clone()))
                }
                TargetKind::StaticUrls { .. } => None,
            })
            .unwrap_or_else(|| ("us".into(), None));
        let credential = match auth.unwrap_or_else(AuthConfig::default_blizzard) {
            AuthConfig::Bearer { env } => Credential::BearerEnv(env),
            AuthConfig::ClientCredentials { client_id_env, client_secret_env } => {
                Credential::OAuth(std::sync::Arc::new(OAuthTokenSource::new(
                    agent.clone(),
                    Some(&region),
                    &client_id_env,
                    &client_secret_env,
                )))
            }
        };
        Self { agent, region, locale: "en_US".into(), credential }
    }

    fn item_url(&self, item_id: i64) -> String {
        format!(
            "https://{}.api.blizzard.com/data/wow/item/{}?namespace=static-{}&locale={}",
            self.region, item_id, self.region, self.locale
        )
    }

    /// One authorized GET through the 401 refresh-retry helper: a 401 on the
    /// OAuth credential drops the cached token and retries once with a fresh
    /// one; anything else surfaces as a terminal [`SourceError`].
    fn authorized_get(&self, url: &str) -> Result<ureq::Response, SourceError> {
        let auth = match &self.credential {
            Credential::BearerEnv(env) => ItemAuth::BearerEnv(env),
            Credential::OAuth(source) => ItemAuth::OAuth(source),
        };
        call_refreshing_on_401(&auth, |token| {
            let mut request = self.agent.get(url);
            if let Some(token) = token {
                request = request.set("Authorization", &format!("Bearer {token}"));
            }
            match request.call() {
                // 401 is the refreshable case; every other answer — success
                // or >=400 — flows to the caller so its terminal status
                // handling (404 = unknown item, anything else = error)
                // applies uniformly.
                Err(ureq::Error::Status(401, _)) => Err(CallFailure::Unauthorized),
                Ok(response) | Err(ureq::Error::Status(_, response)) => Ok(response),
                Err(error) => Err(CallFailure::Terminal(SourceError::Http(error.to_string()))),
            }
        })
    }
}

/// A localized string on the wire. The single-item endpoint returns plain
/// strings for the requested locale (`"name": "Linen Cloth"`), while
/// search-style payloads return locale-keyed maps (`"en_US"`). Both decode
/// into `en_us` so either shape works.
#[derive(Debug, Default)]
struct WireLocalized {
    en_us: Option<String>,
}

impl<'de> Deserialize<'de> for WireLocalized {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct WireLocalizedVisitor;

        impl<'de> Visitor<'de> for WireLocalizedVisitor {
            type Value = WireLocalized;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a plain string or a locale-keyed map")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: Error,
            {
                Ok(WireLocalized { en_us: Some(value.to_owned()) })
            }

            fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut decoded = WireLocalized::default();
                while let Some(key) = access.next_key::<String>()? {
                    if key == "en_US" {
                        decoded.en_us = access.next_value::<Option<String>>()?;
                    } else {
                        access.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(decoded)
            }
        }

        deserializer.deserialize_any(WireLocalizedVisitor)
    }
}

#[derive(Deserialize)]
struct WireItem {
    #[serde(default)]
    name: Option<WireLocalized>,
    #[serde(default)]
    quality: Option<WireNamed>,
    #[serde(default)]
    item_class: Option<WireNamed>,
}

#[derive(Deserialize)]
struct WireNamed {
    #[serde(default)]
    name: Option<WireLocalized>,
}

impl ItemInfoPort for BlizzardItemInfo {
    fn describe(&self, item_id: i64) -> Result<Option<NameRecord>, SourceError> {
        let response = self.authorized_get(&self.item_url(item_id))?;
        let status = response.status();
        if status == 404 {
            return Ok(None);
        }
        if status != 200 {
            return Err(SourceError::Http(format!("item api status {status}")));
        }
        let bytes = crate::blizzard_source::read_capped(
            response.into_reader(),
            MAX_ITEM_INFO_BYTES,
            "item body",
        )?;
        parse_item(item_id, &bytes)
    }
}

/// Decodes one item-API payload into a [`NameRecord`]. Kept separate from the
/// HTTP call so the wire format is unit-testable against real payload shapes.
fn parse_item(item_id: i64, bytes: &[u8]) -> Result<Option<NameRecord>, SourceError> {
    let wire: WireItem = serde_json::from_slice(bytes)
        .map_err(|e| SourceError::Parse(format!("item payload: {e}")))?;
    let name = wire
        .name
        .and_then(|n| n.en_us)
        .ok_or_else(|| SourceError::Parse(format!("item {item_id} has no name")))?;
    Ok(Some(NameRecord {
        item_id,
        name,
        item_class: wire.item_class.and_then(|c| c.name.and_then(|n| n.en_us)),
        quality: wire.quality.and_then(|q| q.name.and_then(|n| n.en_us)),
    }))
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect: fixture-driven, deterministic inputs
    // (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn plain_string_names_from_the_item_api_decode() {
        // Real shape of `GET /data/wow/item/{id}?locale=en_US`: localized
        // fields are plain strings for the requested locale. This exact shape
        // used to fail with "invalid type: string ..., expected struct
        // WireLocalized" and 500 every uncached item page.
        let payload = br#"{
            "_links": {"self": {"href": "https://us.api.blizzard.com/data/wow/item/2589?namespace=static-us"}},
            "id": 2589,
            "name": "Linen Cloth",
            "quality": {"type": "COMMON", "name": "Common"},
            "item_class": {"key": {"href": "https://us.api.blizzard.com/data/wow/item-class/9"}, "name": "Cloth", "id": 9},
            "inventory_type": {"type": "NON_EQUIP", "name": "Generic"}
        }"#;
        let record = parse_item(2589, payload).expect("parses").expect("named");
        assert_eq!(record.name, "Linen Cloth");
        assert_eq!(record.quality.as_deref(), Some("Common"));
        assert_eq!(record.item_class.as_deref(), Some("Cloth"));
    }

    #[test]
    fn locale_map_names_still_decode() {
        // Search-endpoint shape: locale-keyed maps with extra locales.
        let payload = br#"{
            "id": 2589,
            "name": {"en_US": "Linen Cloth", "es_MX": "Tela de lino"},
            "quality": {"name": {"en_US": "Common"}},
            "item_class": {"name": {"en_US": "Cloth"}}
        }"#;
        let record = parse_item(2589, payload).expect("parses").expect("named");
        assert_eq!(record.name, "Linen Cloth");
        assert_eq!(record.quality.as_deref(), Some("Common"));
        assert_eq!(record.item_class.as_deref(), Some("Cloth"));
    }

    #[test]
    fn missing_name_is_a_parse_error() {
        let payload = br#"{"id": 2589}"#;
        let error = parse_item(2589, payload).expect_err("no name");
        assert!(error.to_string().contains("has no name"));
    }

    #[test]
    fn unexpected_name_type_is_a_parse_error() {
        let payload = br#"{"id": 2589, "name": 42}"#;
        let error = parse_item(2589, payload).expect_err("name is a number");
        assert!(error.to_string().contains("item payload"));
    }
}

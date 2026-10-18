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
    domain::{
        Flavor,
        NameRecord,
        PetNameRecord,
    },
    error::SourceError,
    ports::ItemInfoPort,
};
use serde::{
    de::{
        Error,
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
    /// Derives region from the config's first Blizzard target and the
    /// credentials from [`TrackerConfig::blizzard_auth`] (shared with the
    /// binary's credential probe, so the chain and the boot log can never
    /// disagree about which credentials exist); falls back to `us` with the
    /// default Blizzard client-credential env names. The name locale comes
    /// from the `[catalog]` config.
    #[must_use]
    pub fn from_config(config: &TrackerConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build();
        let region = config
            .targets
            .iter()
            .find_map(|target| match &target.kind {
                TargetKind::BlizzardConnectedRealm { region, .. } => Some(region.clone()),
                TargetKind::StaticUrls { .. } => None,
            })
            .unwrap_or_else(|| "us".into());
        let credential = match config.blizzard_auth() {
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
        Self { agent, region, locale: blizzard_locale(&config.catalog.locale), credential }
    }

    fn item_url(&self, flavor: &Flavor, item_id: i64) -> String {
        format!(
            "https://{}.api.blizzard.com/data/wow/item/{}?namespace={}&locale={}",
            self.region,
            item_id,
            self.flavor_namespace(flavor),
            self.locale
        )
    }

    fn pet_url(&self, flavor: &Flavor, species_id: i64) -> String {
        format!(
            "https://{}.api.blizzard.com/data/wow/pet/{}?namespace={}&locale={}",
            self.region,
            species_id,
            self.flavor_namespace(flavor),
            self.locale
        )
    }

    /// The Blizzard static namespace for one game flavor. Only flavors with
    /// known namespace slugs map off retail; unknown flavors (private
    /// servers with retail-compatible ids, beta keys) resolve through the
    /// retail namespace. The classic slugs follow Blizzard's namespace
    /// naming; they should be verified against the live API the first time a
    /// classic flavor is actually deployed.
    fn flavor_namespace(&self, flavor: &Flavor) -> String {
        match flavor.as_str() {
            "classic-era" => format!("static-classic-era-{}", self.region),
            "classic" => format!("static-classic-{}", self.region),
            "mop-classic" => format!("static-mop-classic-{}", self.region),
            _ => format!("static-{}", self.region),
        }
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
/// into a `(locale, name)` pair list so the requested locale can be picked
/// at parse time instead of hard-coding the English key.
#[derive(Debug, Default)]
struct WireLocalized {
    /// `(locale key, name)` pairs in payload order (usually one).
    names: Vec<(String, String)>,
}

impl WireLocalized {
    /// The name for the requested locale tag (either spelling), falling back
    /// to the English wire key and then to whatever the payload carries.
    fn pick(&self, locale: &str) -> Option<&str> {
        let wanted: String =
            locale.chars().filter(|ch| *ch != '_').collect::<String>().to_ascii_lowercase();
        let key_of = |key: &str| {
            key.chars().filter(|ch| *ch != '_').collect::<String>().to_ascii_lowercase()
        };
        self.names
            .iter()
            .find(|(key, _)| key_of(key) == wanted)
            .or_else(|| self.names.iter().find(|(key, _)| key_of(key) == "enus"))
            .map(|(_, name)| name.as_str())
    }
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
                Ok(WireLocalized { names: vec![("en_US".into(), value.to_owned())] })
            }

            fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut decoded = WireLocalized::default();
                while let Some(key) = access.next_key::<String>()? {
                    if let Some(name) = access.next_value::<Option<String>>()? {
                        decoded.names.push((key, name));
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
    #[serde(default)]
    item_subclass: Option<WireNamed>,
    #[serde(default)]
    inventory_type: Option<WireNamed>,
    #[serde(default)]
    sell_price: Option<i64>,
}

#[derive(Deserialize)]
struct WirePet {
    #[serde(default)]
    name: Option<WireLocalized>,
}

#[derive(Deserialize)]
struct WireNamed {
    #[serde(default)]
    name: Option<WireLocalized>,
}

impl ItemInfoPort for BlizzardItemInfo {
    fn describe(&self, flavor: &Flavor, item_id: i64) -> Result<Option<NameRecord>, SourceError> {
        let response = self.authorized_get(&self.item_url(flavor, item_id))?;
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
        parse_item(item_id, &self.locale, &bytes)
    }

    fn describe_pet(
        &self,
        flavor: &Flavor,
        species_id: i64,
    ) -> Result<Option<PetNameRecord>, SourceError> {
        let response = self.authorized_get(&self.pet_url(flavor, species_id))?;
        let status = response.status();
        if status == 404 {
            return Ok(None);
        }
        if status != 200 {
            return Err(SourceError::Http(format!("pet api status {status}")));
        }
        let bytes = crate::blizzard_source::read_capped(
            response.into_reader(),
            MAX_ITEM_INFO_BYTES,
            "pet body",
        )?;
        parse_pet(species_id, &self.locale, &bytes)
    }
}

/// Decodes one item-API payload into a [`NameRecord`]. Kept separate from the
/// HTTP call so the wire format is unit-testable against real payload shapes.
fn parse_item(item_id: i64, locale: &str, bytes: &[u8]) -> Result<Option<NameRecord>, SourceError> {
    let wire: WireItem = serde_json::from_slice(bytes)
        .map_err(|e| SourceError::Parse(format!("item payload: {e}")))?;
    let localized = |wire_name: Option<WireLocalized>| -> Option<String> {
        wire_name.and_then(|n| n.pick(locale).map(str::to_owned))
    };
    let name = localized(wire.name)
        .ok_or_else(|| SourceError::Parse(format!("item {item_id} has no name")))?;
    Ok(Some(NameRecord {
        item_id,
        name,
        item_class: localized(wire.item_class.and_then(|c| c.name)),
        item_subclass: localized(wire.item_subclass.and_then(|c| c.name)),
        inventory_slot: localized(wire.inventory_type.and_then(|t| t.name)),
        quality: localized(wire.quality.and_then(|q| q.name)),
        // Blizzard keeps icons behind a second `/media` call; the chain's
        // Wowhead leg carries them in the first response, so this leg stays
        // one-request and leaves icons empty.
        icon: None,
        sell_price: wire.sell_price,
        fetched_at: 0,
    }))
}

/// Decodes one pet-API payload into a [`PetNameRecord`] (same wire name
/// shapes as the item endpoint: plain string or locale-keyed map).
fn parse_pet(
    species_id: i64,
    locale: &str,
    bytes: &[u8],
) -> Result<Option<PetNameRecord>, SourceError> {
    let wire: WirePet = serde_json::from_slice(bytes)
        .map_err(|e| SourceError::Parse(format!("pet payload: {e}")))?;
    let name = wire
        .name
        .and_then(|n| n.pick(locale).map(str::to_owned))
        .ok_or_else(|| SourceError::Parse(format!("pet {species_id} has no name")))?;
    Ok(Some(PetNameRecord { species_id, name }))
}

/// Maps a config locale tag to the Blizzard API's `xx_XX` wire spelling.
/// Both spellings (the game's `enUS` and the wire's `en_US`) normalize onto
/// one form, so the wire spelling cannot double-separator into `en__US`.
fn blizzard_locale(tag: &str) -> String {
    let compact: String =
        tag.chars().filter(|ch| *ch != '_').collect::<String>().to_ascii_lowercase();
    let mut chars = compact.chars();
    let lang: String = chars.by_ref().take(2).collect();
    let region: String = chars.collect();
    if region.is_empty() {
        compact
    } else {
        format!("{lang}_{}", region.to_ascii_uppercase())
    }
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
            "item_subclass": {"key": {"href": "https://us.api.blizzard.com/data/wow/item-subclass/9/0"}, "name": "Cloth", "id": 0},
            "inventory_type": {"type": "NON_EQUIP", "name": "Generic"},
            "purchase_price": 5,
            "sell_price": 1
        }"#;
        let record = parse_item(2589, "en_US", payload).expect("parses").expect("named");
        assert_eq!(record.name, "Linen Cloth");
        assert_eq!(record.quality.as_deref(), Some("Common"));
        assert_eq!(record.item_class.as_deref(), Some("Cloth"));
        assert_eq!(record.item_subclass.as_deref(), Some("Cloth"));
        assert_eq!(record.inventory_slot.as_deref(), Some("Generic"));
        assert_eq!(record.sell_price, Some(1));
        assert!(!record.is_sparse(), "blizzard records carry enrichment");
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
        let record = parse_item(2589, "en_US", payload).expect("parses").expect("named");
        assert_eq!(record.name, "Linen Cloth");
        assert_eq!(record.quality.as_deref(), Some("Common"));
        assert_eq!(record.item_class.as_deref(), Some("Cloth"));
    }

    #[test]
    fn locale_map_names_pick_the_requested_locale() {
        // Search-endpoint shape under a non-English locale: the configured
        // locale key wins over the English one (this used to only read
        // en_US, so a de_DE deployment silently failed to find a name).
        let payload = br#"{
            "id": 2589,
            "name": {"en_US": "Linen Cloth", "de_DE": "Leinenstoff"}
        }"#;
        let record = parse_item(2589, "de_DE", payload).expect("parses").expect("named");
        assert_eq!(record.name, "Leinenstoff");
        // Either locale spelling picks the same key.
        let record = parse_item(2589, "deDE", payload).expect("parses").expect("named");
        assert_eq!(record.name, "Leinenstoff");
        // An absent locale key falls back to the English name.
        let record = parse_item(2589, "ru_RU", payload).expect("parses").expect("named");
        assert_eq!(record.name, "Linen Cloth");
    }

    #[test]
    fn missing_name_is_a_parse_error() {
        let payload = br#"{"id": 2589}"#;
        let error = parse_item(2589, "en_US", payload).expect_err("no name");
        assert!(error.to_string().contains("has no name"));
    }

    #[test]
    fn unexpected_name_type_is_a_parse_error() {
        let payload = br#"{"id": 2589, "name": 42}"#;
        let error = parse_item(2589, "en_US", payload).expect_err("name is a number");
        assert!(error.to_string().contains("item payload"));
    }

    #[test]
    fn plain_string_pet_names_decode() {
        // Real shape of `GET /data/wow/pet/{species}?locale=en_US`.
        let payload = br#"{
            "_links": {"self": {"href": "https://us.api.blizzard.com/data/wow/pet/264?namespace=static-us"}},
            "id": 264,
            "name": "Mechanical Squirrel Box",
            "battle_pet_type": {"type": "MECHANICAL", "name": {"en_US": "Mechanical"}},
            "creature": {"key": {"href": "https://us.api.blizzard.com/data/wow/creature/6227"}, "name": "Mechanical Squirrel", "id": 6227}
        }"#;
        let record = parse_pet(264, "en_US", payload).expect("parses").expect("named");
        assert_eq!(record.species_id, 264);
        assert_eq!(record.name, "Mechanical Squirrel Box");
    }

    #[test]
    fn locale_map_pet_names_still_decode() {
        let payload = br#"{"id": 264, "name": {"en_US": "Mechanical Squirrel Box"}}"#;
        let record = parse_pet(264, "en_US", payload).expect("parses").expect("named");
        assert_eq!(record.name, "Mechanical Squirrel Box");
    }

    #[test]
    fn pet_missing_name_is_a_parse_error() {
        let payload = br#"{"id": 264}"#;
        let error = parse_pet(264, "en_US", payload).expect_err("no name");
        assert!(error.to_string().contains("has no name"));
    }

    #[test]
    fn locale_tags_map_to_blizzard_spelling() {
        assert_eq!(blizzard_locale("enUS"), "en_US");
        assert_eq!(blizzard_locale("deDE"), "de_DE");
        assert_eq!(blizzard_locale("ruRU"), "ru_RU");
        // The wire spelling normalizes instead of double-separating.
        assert_eq!(blizzard_locale("en_US"), "en_US");
        assert_eq!(blizzard_locale("ru_RU"), "ru_RU");
        assert_eq!(blizzard_locale("es"), "es");
    }
}

//! Blizzard item-information adapter (`ItemInfoPort`): resolves item names,
//! quality and class for the search catalog. Data comes from the static
//! game-data namespace and is cached permanently by the catalog repository.

use amtracker_core::{
    config::{
        TargetKind,
        TrackerConfig,
    },
    domain::NameRecord,
    error::SourceError,
    ports::ItemInfoPort,
};
use serde::Deserialize;

/// Blizzard item API (`data/wow/item/{id}`, static namespace).
pub struct BlizzardItemInfo {
    agent: ureq::Agent,
    region: String,
    locale: String,
    token_env: String,
}

impl BlizzardItemInfo {
    /// Derives region/locale/token from the config's first Blizzard target;
    /// falls back to `us` + `AMT_TOKEN`.
    #[must_use]
    pub fn from_config(config: &TrackerConfig) -> Self {
        let (region, token_env) = config
            .targets
            .iter()
            .find_map(|target| match &target.kind {
                TargetKind::BlizzardConnectedRealm { region, .. } => Some((
                    region.clone(),
                    target
                        .auth
                        .as_ref()
                        .map_or_else(|| "AMT_TOKEN".into(), |auth| auth.env.clone()),
                )),
                TargetKind::StaticUrls { .. } => None,
            })
            .unwrap_or_else(|| ("us".into(), "AMT_TOKEN".into()));
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .timeout(Duration::from_secs(60))
                .build(),
            region,
            locale: "en_US".into(),
            token_env,
        }
    }

    fn item_url(&self, item_id: i64) -> String {
        format!(
            "https://{}.api.blizzard.com/data/wow/item/{}?namespace=static-{}&locale={}",
            self.region, item_id, self.region, self.locale
        )
    }
}

#[derive(Deserialize)]
struct WireLocalized {
    #[serde(rename = "en_US")]
    en_us: Option<String>,
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
        let token = std::env::var(&self.token_env).map_err(|_| {
            SourceError::Config(format!(
                "missing environment variable {} (bearer token)",
                self.token_env
            ))
        })?;
        let response = self
            .agent
            .get(&self.item_url(item_id))
            .set("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(|e| SourceError::Http(e.to_string()))?;
        let status = response.status();
        if status == 404 {
            return Ok(None);
        }
        if status != 200 {
            return Err(SourceError::Http(format!("item api status {status}")));
        }
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|e| SourceError::Http(format!("read body: {e}")))?;
        let wire: WireItem = serde_json::from_slice(&bytes)
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
}

use std::{
    io::Read,
    time::Duration,
};

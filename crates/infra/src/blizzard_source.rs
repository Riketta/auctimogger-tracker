//! Blizzard-format auction source adapter (`AuctionSourcePort`).
//!
//! This is the anti-corruption layer over the wire format: URL profiles,
//! bearer auth via environment, conditional GET (`If-Modified-Since`), strict
//! payload parsing, and the degenerate-row rules all live HERE — the core
//! never sees vendor shapes.
//!
//! Wire facts verified against live dumps (2026-10-12):
//! - both endpoints use the list key `"auctions"`;
//! - commodities carry `unit_price` (per unit), realm auctions carry `buyout`
//!   (stack total) and optional `bid`;
//! - battle pets list as the cage item (82800) with explicit `pet_*` fields
//!   and/or modifier type 6 = species; some cage rows lack species entirely
//!   and must be dropped;
//! - payloads can be truncated mid-transfer → strict parse or reject.

use std::io::Read;
use std::path::Path;
use std::time::{Duration, SystemTime};

use amtracker_core::config::{TargetConfig, TargetKind};
use amtracker_core::domain::{
    FetchOutcome, FetchedPayload, ItemRef, ListingKind, NormalizedListing, PayloadKind,
    TimeLeftBand,
};
use amtracker_core::error::SourceError;
use amtracker_core::ports::AuctionSourcePort;
use serde::Deserialize;

/// Pet cage item id: listings of this item are battle pets keyed by species.
const PET_CAGE_ITEM_ID: i64 = 82_800;
/// Item modifier type carrying the battle pet species on cage rows.
const MODIFIER_PET_SPECIES: i64 = 6;

pub struct BlizzardSource {
    agent: ureq::Agent,
}

impl Default for BlizzardSource {
    fn default() -> Self {
        Self::new()
    }
}

impl BlizzardSource {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build();
        Self { agent }
    }

    /// Build a feed fixture from a JSON file on disk (local testing against
    /// real dumps without the network).
    pub fn from_file(
        path: &Path,
        kind: PayloadKind,
        source_last_modified: Option<i64>,
    ) -> Result<FetchOutcome, SourceError> {
        let bytes = std::fs::read(path)
            .map_err(|e| SourceError::Http(format!("{}: {e}", path.display())))?;
        let (listings, dropped_count) = parse_payload(kind, &bytes)?;
        Ok(FetchOutcome::Payload(FetchedPayload {
            kind,
            source_last_modified,
            listings,
            dropped_count,
            raw: bytes,
        }))
    }

    fn build_url(&self, target: &TargetConfig, kind: PayloadKind) -> Result<String, SourceError> {
        match (&target.kind, kind) {
            (
                TargetKind::BlizzardConnectedRealm {
                    region,
                    connected_realm_id: _,
                },
                PayloadKind::Commodities,
            ) => Ok(format!(
                "https://{region}.api.blizzard.com/data/wow/auctions/commodities?namespace=dynamic-{region}&locale=en_US"
            )),
            (
                TargetKind::BlizzardConnectedRealm {
                    region,
                    connected_realm_id,
                },
                PayloadKind::Auctions,
            ) => Ok(format!(
                "https://{region}.api.blizzard.com/data/wow/connected-realm/{connected_realm_id}/auctions?namespace=dynamic-{region}&locale=en_US"
            )),
            (TargetKind::StaticUrls { commodities_url, auctions_url }, kind) => Ok(match kind {
                PayloadKind::Commodities => commodities_url.clone(),
                PayloadKind::Auctions => auctions_url.clone(),
            }),
        }
    }

    fn bearer_token(&self, target: &TargetConfig) -> Result<Option<String>, SourceError> {
        match &target.auth {
            None => Ok(None),
            Some(auth) => std::env::var(&auth.env).map(Some).map_err(|_| {
                SourceError::Config(format!(
                    "missing environment variable {} (bearer token)",
                    auth.env
                ))
            }),
        }
    }
}

impl AuctionSourcePort for BlizzardSource {
    fn fetch(
        &self,
        target: &TargetConfig,
        kind: PayloadKind,
        if_modified_since: Option<i64>,
    ) -> Result<FetchOutcome, SourceError> {
        let url = self.build_url(target, kind)?;
        let mut request = self.agent.get(&url);
        if let Some(token) = self.bearer_token(target)? {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        if let Some(ims) = if_modified_since {
            let date = httpdate::fmt_http_date(
                SystemTime::UNIX_EPOCH + Duration::from_secs(ims.max(0) as u64),
            );
            request = request.set("If-Modified-Since", &date);
        }

        let response = request
            .call()
            .map_err(|e| SourceError::Http(e.to_string()))?;
        let status = response.status();
        if status == 304 {
            return Ok(FetchOutcome::NotModified);
        }
        if status != 200 {
            return Err(SourceError::Http(format!(
                "unexpected status {status} from {url}"
            )));
        }
        let source_last_modified = response
            .header("last-modified")
            .and_then(|value| httpdate::parse_http_date(value).ok())
            .map(|t| {
                t.duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            });
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|e| SourceError::Http(format!("read body: {e}")))?;
        let (listings, dropped_count) = parse_payload(kind, &bytes)?;
        Ok(FetchOutcome::Payload(FetchedPayload {
            kind,
            source_last_modified,
            listings,
            dropped_count,
            raw: bytes,
        }))
    }
}

// ------------------------------------------------------------------ wire ACL

#[derive(Deserialize)]
struct WirePayload {
    #[serde(default)]
    auctions: Vec<WireAuction>,
}

#[derive(Deserialize)]
struct WireAuction {
    id: i64,
    item: WireItem,
    #[serde(default)]
    quantity: Option<i64>,
    #[serde(default)]
    unit_price: Option<i64>,
    #[serde(default)]
    buyout: Option<i64>,
    #[serde(default)]
    bid: Option<i64>,
    #[serde(default)]
    time_left: Option<String>,
}

#[derive(Deserialize)]
struct WireItem {
    id: i64,
    #[serde(default)]
    context: Option<i64>,
    #[serde(default)]
    bonus_lists: Vec<i64>,
    #[serde(default)]
    modifiers: Vec<WireModifier>,
    #[serde(default)]
    pet_species_id: Option<i64>,
    #[serde(default)]
    pet_level: Option<i64>,
    #[serde(default)]
    pet_quality_id: Option<i64>,
    #[serde(default)]
    pet_breed_id: Option<i64>,
}

#[derive(Deserialize)]
struct WireModifier {
    #[serde(rename = "type")]
    kind: i64,
    value: i64,
}

/// Strict whole-payload parse: a structurally invalid document fails the
/// snapshot (fetcher refuses truncated/spliced payloads). Per-row soft rules
/// (degenerate cage rows, non-positive prices) drop and count the row only.
/// Public: integration tests and tooling replay real dumps through it.
pub fn parse_payload(
    kind: PayloadKind,
    bytes: &[u8],
) -> Result<(Vec<NormalizedListing>, u64), SourceError> {
    let payload: WirePayload = serde_json::from_slice(bytes)
        .map_err(|e| SourceError::Parse(format!("{kind:?} payload rejected: {e}")))?;
    let mut listings = Vec::with_capacity(payload.auctions.len());
    let mut dropped = 0u64;
    for auction in payload.auctions {
        match map_auction(kind, auction) {
            Ok(Some(listing)) => listings.push(listing),
            Ok(None) => dropped += 1,
            Err(()) => dropped += 1,
        }
    }
    Ok((listings, dropped))
}

/// ACL rules for one row. `Ok(None)` = drop (degenerate), `Err` = drop.
fn map_auction(kind: PayloadKind, a: WireAuction) -> Result<Option<NormalizedListing>, ()> {
    let quantity = a.quantity.unwrap_or(0);
    if quantity <= 0 {
        return Ok(None);
    }

    let species = a
        .item
        .pet_species_id
        .or_else(|| {
            a.item
                .modifiers
                .iter()
                .find(|m| m.kind == MODIFIER_PET_SPECIES)
                .map(|m| m.value)
        })
        .filter(|s| *s > 0);

    let item_ref = match species {
        Some(species) => ItemRef::Pet {
            species,
            level: a.item.pet_level.unwrap_or(0).clamp(0, i32::MAX as i64) as i32,
            quality: a.item.pet_quality_id.unwrap_or(0).clamp(0, i32::MAX as i64) as i32,
            breed: a.item.pet_breed_id.unwrap_or(0).clamp(0, i32::MAX as i64) as i32,
        },
        None if a.item.id == PET_CAGE_ITEM_ID => {
            return Ok(None); // cage row without species: degenerate, drop
        }
        None => ItemRef::Item { item_id: a.item.id },
    };

    let unit_price = match (kind, a.unit_price, a.buyout) {
        (PayloadKind::Commodities, Some(price), _) if price > 0 => price, // already per unit
        (PayloadKind::Auctions, _, Some(buyout)) if buyout > 0 => buyout / quantity,
        _ => return Ok(None), // missing or non-positive price (incl. bid-only rows)
    };

    let bid = match kind {
        PayloadKind::Auctions => a.bid.filter(|bid| *bid > 0),
        PayloadKind::Commodities => None,
    };

    Ok(Some(NormalizedListing {
        auction_id: a.id,
        item_ref,
        kind: match kind {
            PayloadKind::Commodities => ListingKind::Commodity,
            PayloadKind::Auctions => ListingKind::Item,
        },
        quantity,
        unit_price,
        bid,
        time_left: TimeLeftBand::parse(a.time_left.as_deref().unwrap_or("SHORT")),
        context: a.item.context,
        bonus_lists: a.item.bonus_lists,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUCTIONS_SAMPLE: &str = include_str!("../../../fixtures/auctions_sample.json");
    const TRUNCATED: &str = include_str!("../../../fixtures/commodities_truncated.json");

    #[test]
    fn sample_fixture_parses_with_real_rows() {
        let (listings, dropped) = parse_payload(PayloadKind::Auctions, AUCTIONS_SAMPLE.as_bytes())
            .expect("sample must parse");
        assert_eq!(dropped, 0, "real sample rows are all well-formed");
        assert_eq!(listings.len(), 60);
        // auction ids must survive as identity
        assert!(listings.iter().all(|l| l.auction_id > 0));
        // items carry per-unit price derived from buyout (qty is 1 on this realm)
        assert!(listings.iter().all(|l| l.unit_price > 0));
    }

    #[test]
    fn truncated_payload_is_rejected() {
        let error = parse_payload(PayloadKind::Commodities, TRUNCATED.as_bytes())
            .expect_err("truncated payload must fail");
        assert!(matches!(error, SourceError::Parse(_)));
    }

    #[test]
    fn spliced_payload_is_rejected() {
        // Reproduces the corruption observed in a real download: one entry cut
        // off mid-string and spliced into the next.
        let spliced = r#"{"auctions":[{"id":1,"item":{"id":241304},"quantity":19,"unit_price":780000,"time_left":","unit_price":19900,"time_left":"LONG"}]}"#;
        let error =
            parse_payload(PayloadKind::Commodities, spliced.as_bytes()).expect_err("must fail");
        assert!(matches!(error, SourceError::Parse(_)));
    }

    #[test]
    fn pet_rows_map_to_species_identity_and_degenerates_drop() {
        let payload = r#"{"auctions":[
            {"id":1,"item":{"id":82800,"pet_species_id":2546,"pet_level":1,"pet_quality_id":3,"pet_breed_id":11},"quantity":1,"buyout":116780000,"time_left":"MEDIUM"},
            {"id":2,"item":{"id":82800,"modifiers":[{"type":6,"value":81665}]},"quantity":1,"buyout":100,"time_left":"LONG"},
            {"id":3,"item":{"id":82800},"quantity":1,"buyout":100,"time_left":"LONG"},
            {"id":4,"item":{"id":82800,"modifiers":[]},"quantity":1,"buyout":100,"time_left":"LONG"}
        ]}"#;
        let (listings, dropped) =
            parse_payload(PayloadKind::Auctions, payload.as_bytes()).expect("parses");
        assert_eq!(dropped, 2, "species-less cage rows drop");
        assert_eq!(listings.len(), 2);
        assert_eq!(
            listings[0].item_ref,
            ItemRef::Pet {
                species: 2546,
                level: 1,
                quality: 3,
                breed: 11
            }
        );
        assert_eq!(
            listings[1].item_ref,
            ItemRef::Pet {
                species: 81665,
                level: 0,
                quality: 0,
                breed: 0
            }
        );
    }

    #[test]
    fn per_unit_price_rules_differ_per_payload_kind() {
        let commodities = r#"{"auctions":[{"id":1,"item":{"id":89112},"quantity":16,"unit_price":170000,"time_left":"SHORT"}]}"#;
        let auctions = r#"{"auctions":[{"id":1,"item":{"id":863},"quantity":4,"buyout":800000,"time_left":"SHORT","bid":700000}]}"#;

        let (commodity_rows, _) =
            parse_payload(PayloadKind::Commodities, commodities.as_bytes()).expect("ok");
        assert_eq!(
            commodity_rows[0].unit_price, 170000,
            "commodities are already per unit"
        );
        assert_eq!(commodity_rows[0].bid, None);

        let (auction_rows, _) =
            parse_payload(PayloadKind::Auctions, auctions.as_bytes()).expect("ok");
        assert_eq!(
            auction_rows[0].unit_price, 200000,
            "buyout divides by quantity"
        );
        assert_eq!(auction_rows[0].bid, Some(700000));
    }

    #[test]
    fn degenerate_prices_and_quantities_drop() {
        let payload = r#"{"auctions":[
            {"id":1,"item":{"id":1},"quantity":0,"unit_price":100,"time_left":"SHORT"},
            {"id":2,"item":{"id":1},"quantity":1,"unit_price":0,"time_left":"SHORT"},
            {"id":3,"item":{"id":1},"quantity":1,"time_left":"SHORT"}
        ]}"#;
        let (listings, dropped) =
            parse_payload(PayloadKind::Commodities, payload.as_bytes()).expect("parses");
        assert!(listings.is_empty());
        assert_eq!(dropped, 3);
    }

    #[test]
    fn url_profiles_build_per_target_kind() {
        let source = BlizzardSource::new();
        let blizzard = target(TargetKind::BlizzardConnectedRealm {
            region: "eu".into(),
            connected_realm_id: 609,
        });
        assert_eq!(
            source.build_url(&blizzard, PayloadKind::Commodities).unwrap(),
            "https://eu.api.blizzard.com/data/wow/auctions/commodities?namespace=dynamic-eu&locale=en_US"
        );
        assert_eq!(
            source.build_url(&blizzard, PayloadKind::Auctions).unwrap(),
            "https://eu.api.blizzard.com/data/wow/connected-realm/609/auctions?namespace=dynamic-eu&locale=en_US"
        );

        let custom = target(TargetKind::StaticUrls {
            commodities_url: "https://wf.example/c.json".into(),
            auctions_url: "https://wf.example/a.json".into(),
        });
        assert_eq!(
            source.build_url(&custom, PayloadKind::Auctions).unwrap(),
            "https://wf.example/a.json"
        );
    }

    fn target(kind: TargetKind) -> TargetConfig {
        TargetConfig {
            id: amtracker_core::domain::TargetId("test".into()),
            kind,
            auth: None,
            poll: amtracker_core::config::PollConfig {
                commodities_secs: 1,
                auctions_secs: 1,
            },
        }
    }
}

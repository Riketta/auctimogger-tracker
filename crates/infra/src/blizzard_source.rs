//! Blizzard-format auction source adapter (`AuctionSourcePort`).
//!
//! This is the anti-corruption layer over the wire format: URL profiles,
//! auth (static bearer from env, or self-refreshing OAuth client
//! credentials), conditional GET (`If-Modified-Since`), strict payload
//! parsing, and the degenerate-row rules all live HERE — the core never sees
//! vendor shapes.
//!
//! Wire facts verified against live dumps (2026-10-12):
//! - both endpoints use the list key `"auctions"`;
//! - commodities carry `unit_price` (per unit), realm auctions carry `buyout`
//!   (stack total) and optional `bid`;
//! - battle pets list as the cage item (82800) with explicit `pet_*` fields
//!   and/or modifier type 6 = species; some cage rows lack species entirely
//!   and must be dropped;
//! - payloads can be truncated mid-transfer → strict parse or reject.

use std::{
    collections::{
        BTreeMap,
        HashMap,
    },
    io::Read,
    path::Path,
    sync::{
        Arc,
        Mutex,
    },
    time::{
        Duration,
        SystemTime,
    },
};

use amtracker_core::{
    config::{
        AuthConfig,
        TargetConfig,
        TargetKind,
    },
    domain::{
        FetchOutcome,
        FetchedPayload,
        ItemRef,
        ListingKind,
        NormalizedListing,
        PayloadKind,
        TargetId,
        TimeLeftBand,
    },
    error::SourceError,
    ports::AuctionSourcePort,
};
use serde::Deserialize;

use crate::blizzard_oauth::OAuthTokenSource;

/// Pet cage item id: listings of this item are battle pets keyed by species.
const PET_CAGE_ITEM_ID: i64 = 82_800;
/// Item modifier type carrying the battle pet species on cage rows.
const MODIFIER_PET_SPECIES: i64 = 6;

/// Blizzard auction source: authenticated conditional GET against the
/// Blizzard auction endpoints with strict whole-payload parsing.
///
/// The adapter never panics on wire data: malformed documents surface as
/// [`SourceError::Parse`], degenerate rows are dropped and counted.
pub struct BlizzardSource {
    agent: ureq::Agent,
    /// OAuth token sources per target, built lazily on first use from the
    /// target's auth config (client credentials name env vars, the region
    /// names the token endpoint host).
    tokens: Mutex<HashMap<TargetId, Arc<OAuthTokenSource>>>,
}

impl Default for BlizzardSource {
    fn default() -> Self {
        Self::new()
    }
}

impl BlizzardSource {
    /// Source with production timeouts (15 s connect, 600 s total).
    #[must_use]
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build();
        Self { agent, tokens: Mutex::new(HashMap::new()) }
    }

    /// Build a feed fixture from a JSON file on disk (local testing against
    /// real dumps without the network).
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::Http`] when the file cannot be read and
    /// [`SourceError::Parse`] when the payload fails strict parsing.
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
}

/// Reads the static-bearer token env var; resolved per request so rotated
/// tokens are picked up without a restart.
fn bearer_token(env: &str) -> Result<String, SourceError> {
    std::env::var(env).map_err(|_| {
        SourceError::Config(format!("missing environment variable {env} (bearer token)"))
    })
}

fn build_url(target: &TargetConfig, kind: PayloadKind) -> String {
    match (&target.kind, kind) {
        (
            TargetKind::BlizzardConnectedRealm {
                region,
                connected_realm_id: _,
            },
            PayloadKind::Commodities,
        ) => format!(
            "https://{region}.api.blizzard.com/data/wow/auctions/commodities?namespace=dynamic-{region}&locale=en_US"
        ),
        (
            TargetKind::BlizzardConnectedRealm {
                region,
                connected_realm_id,
            },
            PayloadKind::Auctions,
        ) => format!(
            "https://{region}.api.blizzard.com/data/wow/connected-realm/{connected_realm_id}/auctions?namespace=dynamic-{region}&locale=en_US"
        ),
        (TargetKind::StaticUrls { commodities_url, auctions_url }, kind) => match kind {
            PayloadKind::Commodities => commodities_url.clone(),
            PayloadKind::Auctions => auctions_url.clone(),
        },
    }
}

impl BlizzardSource {
    /// Per-target OAuth token source, built on first use: the region names
    /// the regional token endpoint host (static-urls fall back to the
    /// global one).
    fn oauth_source(
        &self,
        target: &TargetConfig,
        client_id_env: &str,
        client_secret_env: &str,
    ) -> Arc<OAuthTokenSource> {
        let mut tokens = self.tokens.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        tokens
            .entry(target.id.clone())
            .or_insert_with(|| {
                let region = match &target.kind {
                    TargetKind::BlizzardConnectedRealm { region, .. } => Some(region.as_str()),
                    TargetKind::StaticUrls { .. } => None,
                };
                Arc::new(OAuthTokenSource::new(
                    self.agent.clone(),
                    region,
                    client_id_env,
                    client_secret_env,
                ))
            })
            .clone()
    }

    /// Builds and sends one authenticated conditional-GET request; returns
    /// the response plus the OAuth source when the request rode on one (the
    /// caller needs it for the 401 refresh-retry).
    fn send_request(
        &self,
        target: &TargetConfig,
        kind: PayloadKind,
        if_modified_since: Option<i64>,
    ) -> Result<(ureq::Response, String, Option<Arc<OAuthTokenSource>>), SourceError> {
        let url = build_url(target, kind);
        let mut request = self.agent.get(&url);
        let mut oauth = None;
        match &target.auth {
            None => {}
            Some(AuthConfig::Bearer { env }) => {
                request = request.set("Authorization", &format!("Bearer {}", bearer_token(env)?));
            }
            Some(AuthConfig::ClientCredentials { client_id_env, client_secret_env }) => {
                let source = self.oauth_source(target, client_id_env, client_secret_env);
                request = request.set("Authorization", &format!("Bearer {}", source.token()?));
                oauth = Some(source);
            }
        }
        if let Some(ims) = if_modified_since {
            // `ims.max(0)` is non-negative, so the conversion is total.
            let date = httpdate::fmt_http_date(
                SystemTime::UNIX_EPOCH
                    + Duration::from_secs(u64::try_from(ims.max(0)).unwrap_or(0)),
            );
            request = request.set("If-Modified-Since", &date);
        }
        let response = request.call().map_err(|e| SourceError::Http(e.to_string()))?;
        Ok((response, url, oauth))
    }
}

/// Terminal status handling + strict payload parse for one response.
fn read_payload(
    response: ureq::Response,
    kind: PayloadKind,
    url: &str,
) -> Result<FetchOutcome, SourceError> {
    let status = response.status();
    if status == 304 {
        tracing::debug!(url = %url, "source returned not-modified");
        return Ok(FetchOutcome::NotModified);
    }
    if status != 200 {
        let retry_after = response.header("retry-after").map(str::to_owned);
        tracing::warn!(
            status = status,
            url = %url,
            retry_after = ?retry_after,
            "unexpected source status"
        );
        return Err(SourceError::Http(match retry_after {
            Some(after) => {
                format!("unexpected status {status} from {url} (retry-after: {after})")
            }
            None => format!("unexpected status {status} from {url}"),
        }));
    }
    let source_last_modified = response
        .header("last-modified")
        .and_then(|value| httpdate::parse_http_date(value).ok())
        .map(|t| {
            // `u64` unix seconds only exceed `i64` far beyond any real
            // date, so the conversion saturates; pre-epoch dates read 0.
            t.duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        });
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| SourceError::Http(format!("read body: {e}")))?;
    let (listings, dropped_count) = parse_payload(kind, &bytes)?;
    tracing::info!(
        kind = ?kind,
        bytes = bytes.len(),
        listings = listings.len(),
        dropped = dropped_count,
        "payload fetched"
    );
    Ok(FetchOutcome::Payload(FetchedPayload {
        kind,
        source_last_modified,
        listings,
        dropped_count,
        raw: bytes,
    }))
}

impl AuctionSourcePort for BlizzardSource {
    fn fetch(
        &self,
        target: &TargetConfig,
        kind: PayloadKind,
        if_modified_since: Option<i64>,
    ) -> Result<FetchOutcome, SourceError> {
        let (response, url, oauth) = self.send_request(target, kind, if_modified_since)?;
        // A cached OAuth token can be rejected before its expiry (revoked,
        // clock skew): drop it and retry once with a freshly fetched one.
        // Static bearer and unauthenticated requests take the plain path.
        if response.status() == 401 {
            if let Some(source) = &oauth {
                tracing::debug!(url = %url, "oauth token rejected; refreshing and retrying once");
                source.invalidate();
                let (response, url, _) = self.send_request(target, kind, if_modified_since)?;
                return read_payload(response, kind, &url);
            }
        }
        read_payload(response, kind, &url)
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
///
/// # Errors
///
/// Returns [`SourceError::Parse`] when `bytes` is not a complete, structurally
/// valid JSON document of the expected wire shape (truncated or spliced
/// payloads are rejected wholesale rather than partially mapped).
pub fn parse_payload(
    kind: PayloadKind,
    bytes: &[u8],
) -> Result<(Vec<NormalizedListing>, u64), SourceError> {
    let payload: WirePayload = serde_json::from_slice(bytes)
        .map_err(|e| SourceError::Parse(format!("{kind:?} payload rejected: {e}")))?;
    let mut listings = Vec::with_capacity(payload.auctions.len());
    let mut dropped = 0u64;
    let mut by_reason = BTreeMap::<&'static str, u64>::new();
    for auction in payload.auctions {
        match map_auction(kind, auction) {
            Ok(listing) => listings.push(listing),
            Err(reason) => {
                dropped += 1;
                *by_reason.entry(reason.as_str()).or_default() += 1;
            }
        }
    }
    if dropped > 0 {
        // The aggregate rides on the fetch info log; this breakdown answers
        // "why" without recompiling.
        tracing::debug!(kind = ?kind, dropped, reasons = ?by_reason, "degenerate rows dropped");
    }
    Ok((listings, dropped))
}

/// Why a degenerate row was dropped. Diagnostics only: drops are expected
/// (bid-only rows, species-less cage rows) and counted, never fatal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropReason {
    /// `quantity` missing or non-positive.
    NonPositiveQuantity,
    /// Pet-cage row with no species field or modifier.
    CageWithoutSpecies,
    /// No usable per-unit/stack price (including bid-only rows).
    MissingPrice,
}

impl DropReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::NonPositiveQuantity => "non-positive-quantity",
            Self::CageWithoutSpecies => "cage-without-species",
            Self::MissingPrice => "missing-price",
        }
    }
}

/// Narrow a wire pet field to its domain range. The value is clamped into
/// `0..=i32::MAX`, so the cast below is exact (never wraps or truncates).
#[allow(clippy::cast_possible_truncation)]
fn pet_field(value: Option<i64>) -> i32 {
    value.unwrap_or(0).clamp(0, i64::from(i32::MAX)) as i32
}

/// ACL rules for one row. `Err(reason)` = degenerate row, drop and count it.
fn map_auction(kind: PayloadKind, a: WireAuction) -> Result<NormalizedListing, DropReason> {
    let quantity = a.quantity.unwrap_or(0);
    if quantity <= 0 {
        return Err(DropReason::NonPositiveQuantity);
    }

    let species = a
        .item
        .pet_species_id
        .or_else(|| {
            a.item.modifiers.iter().find(|m| m.kind == MODIFIER_PET_SPECIES).map(|m| m.value)
        })
        .filter(|s| *s > 0);

    let item_ref = match species {
        Some(species) => ItemRef::Pet {
            species,
            level: pet_field(a.item.pet_level),
            quality: pet_field(a.item.pet_quality_id),
            breed: pet_field(a.item.pet_breed_id),
        },
        None if a.item.id == PET_CAGE_ITEM_ID => {
            return Err(DropReason::CageWithoutSpecies); // degenerate, drop
        }
        None => ItemRef::Item { item_id: a.item.id },
    };

    let unit_price = match (kind, a.unit_price, a.buyout) {
        (PayloadKind::Commodities, Some(price), _) if price > 0 => price, // already per unit
        (PayloadKind::Auctions, _, Some(buyout)) if buyout > 0 => buyout / quantity,
        _ => return Err(DropReason::MissingPrice), // incl. bid-only rows
    };

    let bid = match kind {
        PayloadKind::Auctions => a.bid.filter(|bid| *bid > 0),
        PayloadKind::Commodities => None,
    };

    Ok(NormalizedListing {
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
    })
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap: fixture-driven, deterministic inputs.
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::assert_is_empty
    )]

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
            ItemRef::Pet { species: 2546, level: 1, quality: 3, breed: 11 }
        );
        assert_eq!(
            listings[1].item_ref,
            ItemRef::Pet { species: 81665, level: 0, quality: 0, breed: 0 }
        );
    }

    #[test]
    fn per_unit_price_rules_differ_per_payload_kind() {
        let commodities = r#"{"auctions":[{"id":1,"item":{"id":89112},"quantity":16,"unit_price":170000,"time_left":"SHORT"}]}"#;
        let auctions = r#"{"auctions":[{"id":1,"item":{"id":863},"quantity":4,"buyout":800000,"time_left":"SHORT","bid":700000}]}"#;

        let (commodity_rows, _) =
            parse_payload(PayloadKind::Commodities, commodities.as_bytes()).expect("ok");
        assert_eq!(commodity_rows[0].unit_price, 170_000, "commodities are already per unit");
        assert_eq!(commodity_rows[0].bid, None);

        let (auction_rows, _) =
            parse_payload(PayloadKind::Auctions, auctions.as_bytes()).expect("ok");
        assert_eq!(auction_rows[0].unit_price, 200_000, "buyout divides by quantity");
        assert_eq!(auction_rows[0].bid, Some(700_000));
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
        let blizzard = target(TargetKind::BlizzardConnectedRealm {
            region: "eu".into(),
            connected_realm_id: 609,
        });
        assert_eq!(
            build_url(&blizzard, PayloadKind::Commodities),
            "https://eu.api.blizzard.com/data/wow/auctions/commodities?namespace=dynamic-eu&locale=en_US"
        );
        assert_eq!(
            build_url(&blizzard, PayloadKind::Auctions),
            "https://eu.api.blizzard.com/data/wow/connected-realm/609/auctions?namespace=dynamic-eu&locale=en_US"
        );

        let custom = target(TargetKind::StaticUrls {
            commodities_url: "https://wf.example/c.json".into(),
            auctions_url: "https://wf.example/a.json".into(),
        });
        assert_eq!(build_url(&custom, PayloadKind::Auctions), "https://wf.example/a.json");
    }

    #[test]
    fn missing_oauth_credentials_fail_fast_before_the_request() {
        // Unique env names: if these were ever exported the fetch would
        // continue to a real HTTP call, so the test preconditions on absence.
        assert!(std::env::var("AMT_TEST_MISSING_CLIENT_ID").is_err());
        assert!(std::env::var("AMT_TEST_MISSING_CLIENT_SECRET").is_err());
        let mut target = target(TargetKind::BlizzardConnectedRealm {
            region: "eu".into(),
            connected_realm_id: 609,
        });
        target.auth = Some(AuthConfig::ClientCredentials {
            client_id_env: "AMT_TEST_MISSING_CLIENT_ID".into(),
            client_secret_env: "AMT_TEST_MISSING_CLIENT_SECRET".into(),
        });
        let error = BlizzardSource::new()
            .fetch(&target, PayloadKind::Commodities, None)
            .expect_err("missing credentials must fail fast");
        assert!(error.to_string().contains("AMT_TEST_MISSING_CLIENT_ID"), "{error}");
    }

    fn target(kind: TargetKind) -> TargetConfig {
        TargetConfig {
            id: amtracker_core::domain::TargetId("test".into()),
            kind,
            auth: None,
            poll: amtracker_core::config::PollConfig { commodities_secs: 1, auctions_secs: 1 },
        }
    }
}

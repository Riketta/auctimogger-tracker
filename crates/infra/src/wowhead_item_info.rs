//! Wowhead item-information adapter (`ItemInfoPort`): credential-free name,
//! quality, icon and vendor-price resolution from Wowhead's tooltip data
//! endpoints. The default base (`nether.wowhead.com`) is Wowhead's anonymous
//! data API — the public site's tooltip route sits behind an aggressive CDN
//! bot filter, while the data subdomain answers plain JSON.
//!
//! Per flavor, the flavor's path prefix selects the game version's database
//! (`retail`, `forever`, classic flavors); custom flavors with
//! retail-compatible ids fall through to retail, which is exactly the private
//! server case. Resolved records are cached permanently by the catalog
//! repository, so each id costs one request ever.

use std::time::Duration;

use amtracker_core::{
    config::TrackerConfig,
    domain::{
        Flavor,
        NameRecord,
        PetNameRecord,
    },
    error::SourceError,
    ports::ItemInfoPort,
};
use serde::Deserialize;

use crate::blizzard_source::read_capped;

/// Default Wowhead data API base: the anonymous `nether` subdomain. The
/// public site (`www`) blocks non-browser traffic at the CDN edge.
const DEFAULT_BASE: &str = "https://nether.wowhead.com";

/// The browser-ish User-Agent the endpoints expect; a bare client UA gets
/// edge-filtered on some deployments.
const USER_AGENT: &str = "Mozilla/5.0 (compatible; auctimogger-tracker item-name resolver)";

/// Read-cap for one tooltip payload (real answers are a few KB).
const MAX_TOOLTIP_BYTES: u64 = 1024 * 1024;

/// How the config's game-flavor keys map onto Wowhead's database path
/// prefixes. Only the flavors the tracker ships are mapped; unknown flavors
/// (private servers with retail-compatible ids) resolve through retail.
fn flavor_prefix(flavor: &Flavor) -> &'static str {
    match flavor.as_str() {
        "forever" => "/forever",
        "classic-era" => "/classic-era",
        "classic" => "/classic",
        "cata-classic" => "/cata-classic",
        "mop-classic" => "/mop-classic",
        _ => "",
    }
}

/// Maps the config's locale tag (`enUS`, the game's spelling) onto Wowhead's
/// locale path prefix (`/ru`, `/cn`, ...). English stays unprefixed.
fn locale_prefix(locale: &str) -> String {
    let lowered = locale.to_ascii_lowercase();
    match lowered.as_str() {
        "" | "enus" | "engb" => String::new(),
        "zhcn" => "/cn".into(),
        "zhtw" => "/tw".into(),
        "esmx" => "/mx".into(),
        "ptbr" => "/pt".into(),
        other => {
            format!("/{}", other.get(..2).unwrap_or(other))
        }
    }
}

/// The Wowhead item-information source.
pub struct WowheadItemInfo {
    agent: ureq::Agent,
    base: String,
    locale: String,
}

impl WowheadItemInfo {
    /// Builds the source; the optional `[catalog].wowhead_base` override
    /// replaces the default data-API base wholesale, and `[catalog].locale`
    /// picks the name language.
    #[must_use]
    pub fn from_config(config: &TrackerConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .user_agent(USER_AGENT)
            .build();
        Self {
            agent,
            base: config.catalog.wowhead_base.clone().unwrap_or_else(|| DEFAULT_BASE.into()),
            locale: config.catalog.locale.clone(),
        }
    }

    fn tooltip_url(&self, flavor: &Flavor, kind: &str, id: i64) -> String {
        format!(
            "{base}{flavor}{locale}/tooltip/{kind}/{id}",
            base = self.base,
            flavor = flavor_prefix(flavor),
            locale = locale_prefix(&self.locale)
        )
    }

    /// One tooltip GET: 404 (`{"error": ...}` or a missing entity) is the
    /// "does not exist there" answer; anything else non-200 is transport
    /// trouble the chain may fall through on.
    fn get_json(&self, url: &str) -> Result<Option<Vec<u8>>, SourceError> {
        let response = match self.agent.get(url).call() {
            Ok(response) => response,
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(error) => return Err(SourceError::Http(error.to_string())),
        };
        if response.status() != 200 {
            return Err(SourceError::Http(format!("wowhead tooltip status {}", response.status())));
        }
        let bytes = read_capped(response.into_reader(), MAX_TOOLTIP_BYTES, "wowhead tooltip body")?;
        Ok(Some(bytes))
    }
}

impl ItemInfoPort for WowheadItemInfo {
    fn describe(&self, flavor: &Flavor, item_id: i64) -> Result<Option<NameRecord>, SourceError> {
        let url = self.tooltip_url(flavor, "item", item_id);
        let Some(bytes) = self.get_json(&url)? else {
            return Ok(None);
        };
        parse_item(item_id, &bytes)
    }

    fn describe_pet(
        &self,
        flavor: &Flavor,
        species_id: i64,
    ) -> Result<Option<PetNameRecord>, SourceError> {
        let url = self.tooltip_url(flavor, "battle-pet", species_id);
        let Some(bytes) = self.get_json(&url)? else {
            return Ok(None);
        };
        parse_pet(species_id, &bytes)
    }
}

/// The tooltip JSON wire shape: only the fields the tracker consumes; the
/// rendered `tooltip` HTML rides along as the vendor-price source.
#[derive(Deserialize)]
struct WireTooltip {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    quality: Option<i64>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    tooltip: Option<String>,
}

/// The battle-pet tooltip wire shape (the endpoint redirects to the pet's
/// creature page, whose JSON carries the species display name).
#[derive(Deserialize)]
struct WirePetTooltip {
    #[serde(default)]
    name: Option<String>,
}

/// Wowhead quality integers → the tracker's quality labels.
fn quality_name(quality: Option<i64>) -> Option<String> {
    match quality? {
        0 => Some("poor"),
        1 => Some("common"),
        2 => Some("uncommon"),
        3 => Some("rare"),
        4 => Some("epic"),
        5 => Some("legendary"),
        6 => Some("artifact"),
        7 => Some("heirloom"),
        _ => None,
    }
    .map(str::to_string)
}

/// Decodes one item tooltip payload. A payload without a name (Wowhead's
/// error shape rides on a 200 sometimes) answers "not found" rather than a
/// parse error, so junk ids land in the negative cache instead of erroring
/// forever.
fn parse_item(item_id: i64, bytes: &[u8]) -> Result<Option<NameRecord>, SourceError> {
    let wire: WireTooltip = serde_json::from_slice(bytes)
        .map_err(|e| SourceError::Parse(format!("wowhead item payload: {e}")))?;
    let Some(name) = wire.name else {
        return Ok(None);
    };
    let sell_price = wire.tooltip.as_deref().and_then(sell_price_from_tooltip);
    Ok(Some(NameRecord {
        item_id,
        name: decode_entities(&name),
        item_class: None,
        // The tooltip JSON carries no class/subclass/slot; the chain's
        // Blizzard leg fills those when it answers first elsewhere.
        item_subclass: None,
        inventory_slot: None,
        quality: quality_name(wire.quality),
        icon: wire.icon,
        sell_price,
        fetched_at: 0,
    }))
}

/// Decodes one battle-pet tooltip payload (same missing-name policy).
fn parse_pet(species_id: i64, bytes: &[u8]) -> Result<Option<PetNameRecord>, SourceError> {
    let wire: WirePetTooltip = serde_json::from_slice(bytes)
        .map_err(|e| SourceError::Parse(format!("wowhead pet payload: {e}")))?;
    let Some(name) = wire.name else {
        return Ok(None);
    };
    Ok(Some(PetNameRecord { species_id, name: decode_entities(&name) }))
}

/// Extracts the vendor sell price (copper total) from the rendered tooltip
/// HTML: the `whtt-sellprice` block sums its `moneygold`/`moneysilver`/
/// `moneycopper` counters. Anything unexpected reads as "no price recorded".
fn sell_price_from_tooltip(html: &str) -> Option<i64> {
    let start = html.find("whtt-sellprice")?;
    let rest = html.get(start..).unwrap_or("");
    let end = rest.find("</div>").unwrap_or(rest.len());
    let segment = rest.get(..end).unwrap_or("");
    let count = |class: &str| money_counter(segment, class);
    let total = count("moneygold") * 10_000 + count("moneysilver") * 100 + count("moneycopper");
    (total > 0).then_some(total)
}

/// Sums every `<span class="{class}">N</span>` counter in the segment.
fn money_counter(segment: &str, class: &str) -> i64 {
    let marker = format!("{class}\">");
    let mut total = 0i64;
    let mut from = 0usize;
    while let Some(rel) = segment.get(from..).and_then(|rest| rest.find(&marker)) {
        let after = segment.get(from + rel + marker.len()..).unwrap_or("");
        let digits: usize = after.chars().take_while(char::is_ascii_digit).count();
        let parsed = after.get(..digits).and_then(|d| d.parse::<i64>().ok()).unwrap_or(0);
        total = total.saturating_add(parsed);
        from += rel + marker.len() + digits;
    }
    total
}

/// Decodes the handful of HTML entities Wowhead leaves inside display names
/// (`&amp;`, numeric references, ...). Unknown entities pass through — the
/// renderer escapes on output anyway.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(rest.get(..amp).unwrap_or(""));
        let after = rest.get(amp..).unwrap_or("");
        let (replacement, consumed) = match entity(after) {
            Some((replacement, consumed)) => (replacement, consumed),
            None => ('&', 1),
        };
        out.push(replacement);
        rest = after.get(consumed..).unwrap_or("");
    }
    out.push_str(rest);
    out
}

/// The replacement for the entity at the start of `text`, with its byte
/// length; `None` when the `&` is not an entity at all.
fn entity(text: &str) -> Option<(char, usize)> {
    const NAMED: &[(&str, char)] = &[
        ("&amp;", '&'),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&quot;", '"'),
        ("&apos;", '\''),
        ("&nbsp;", ' '),
    ];
    for (pattern, replacement) in NAMED {
        if text.starts_with(pattern) {
            return Some((*replacement, pattern.len()));
        }
    }
    // Numeric references: `&#39;` / `&#x27;` up to the semicolon (the body
    // starts after `&`).
    let semi = text.find(';')?;
    let body = text.get(2..semi)?;
    let code = if let Some(hex) = body.strip_prefix('x').or_else(|| body.strip_prefix('X')) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        body.parse::<u32>().ok()
    };
    let ch = char::from_u32(code?).filter(|c| !c.is_control())?;
    Some((ch, semi + 1))
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect/panic: fixture-driven, deterministic
    // inputs (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn retail_tooltip_payload_decodes() {
        // Real shape of GET nether.wowhead.com/tooltip/item/2589 (2026-10).
        let payload = br#"{"name":"Linen Cloth","quality":1,"icon":"inv_fabric_linen_01","tooltip":"<table><tr><td><!--nstart--><b class=\"q1\">Linen Cloth</b><!--nend--></td></tr></table><table><tr><td><div class=\"whtt-sellprice\">Sell Price: <span class=\"moneycopper\">13</span></div></td></tr></table>","spells":[]}"#;
        let record = parse_item(2589, payload).expect("parses").expect("named");
        assert_eq!(record.item_id, 2589);
        assert_eq!(record.name, "Linen Cloth");
        assert_eq!(record.quality.as_deref(), Some("common"));
        assert_eq!(record.icon.as_deref(), Some("inv_fabric_linen_01"));
        assert_eq!(record.sell_price, Some(13));
        assert!(!record.is_sparse(), "wowhead records carry icon enrichment");
    }

    #[test]
    fn gold_silver_copper_counters_sum_to_copper() {
        let html = "<div class=\"whtt-sellprice\">Sell Price: \
                    <span class=\"moneygold\">2</span> \
                    <span class=\"moneysilver\">34</span> \
                    <span class=\"moneycopper\">56</span></div>";
        assert_eq!(sell_price_from_tooltip(html), Some(2 * 10_000 + 34 * 100 + 56));
    }

    #[test]
    fn tooltip_without_a_sell_price_block_yields_none() {
        assert_eq!(sell_price_from_tooltip("<table><td>no price here</td></table>"), None);
        assert_eq!(sell_price_from_tooltip("<div class=\"whtt-sellprice\">none</div>"), None);
    }

    #[test]
    fn error_payload_without_a_name_reads_as_missing() {
        let payload = br#"{"error":"Entity not found."}"#;
        assert!(parse_item(999, payload).expect("parses").is_none());
    }

    #[test]
    fn malformed_payload_is_a_parse_error() {
        assert!(parse_item(1, b"not json").is_err());
    }

    #[test]
    fn entity_names_decode() {
        assert_eq!(decode_entities("Marauder&#39;s Pike"), "Marauder's Pike");
        assert_eq!(decode_entities("A &amp; B"), "A & B");
        assert_eq!(decode_entities("plain"), "plain");
        assert_eq!(decode_entities("lone & amp"), "lone & amp");
    }

    #[test]
    fn pet_tooltip_payload_decodes() {
        // Real shape of GET nether.wowhead.com/tooltip/battle-pet/264
        // (redirects to the creature tooltip, JSON carries the species name).
        let payload = br#"{"name":"Crawling Claw","tooltip":"<table>...</table>"}"#;
        let record = parse_pet(264, payload).expect("parses").expect("named");
        assert_eq!(record.species_id, 264);
        assert_eq!(record.name, "Crawling Claw");
    }

    #[test]
    fn flavor_and_locale_map_onto_path_prefixes() {
        assert_eq!(flavor_prefix(&Flavor("retail".into())), "");
        assert_eq!(flavor_prefix(&Flavor("forever".into())), "/forever");
        assert_eq!(flavor_prefix(&Flavor("mop-classic".into())), "/mop-classic");
        assert_eq!(flavor_prefix(&Flavor("some-emu".into())), "", "custom flavors ride retail");
        assert_eq!(locale_prefix("enUS"), "");
        assert_eq!(locale_prefix("ruRU"), "/ru");
        assert_eq!(locale_prefix("zhCN"), "/cn");
        assert_eq!(locale_prefix("ptBR"), "/pt");
    }
}

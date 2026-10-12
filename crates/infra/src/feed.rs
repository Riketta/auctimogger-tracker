//! Feed rendering (tracker feed v2: Lua + JSON) and publication to the feed
//! directory served to the in-game sub-addon.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use amtracker_core::domain::{
    FeedEntry, FeedFormat, FeedSnapshot, ItemRef, MetricBlock, PublishedFeed, Span, TargetId,
};
use amtracker_core::error::{RenderError, StoreError};
use amtracker_core::ports::FeedPublisher;

/// Renders feed snapshots into the two contract serializations.
pub struct FeedCodec;

impl amtracker_core::ports::FeedRenderer for FeedCodec {
    fn render(&self, feed: &FeedSnapshot, format: FeedFormat) -> Result<Vec<u8>, RenderError> {
        match format {
            FeedFormat::Lua => render_lua(feed),
            FeedFormat::Json => render_json(feed),
        }
    }
}

fn metric_fields_lua(block: &MetricBlock) -> String {
    let mut out = format!(
        "n = {}, min = {}, p05 = {}, p25 = {}, median = {}, p75 = {}, p95 = {}, max = {}, mad = {}, vwap = {}, qtySum = {}, listingCount = {}",
        block.n, block.min, block.p05, block.p25, block.median, block.p75, block.p95, block.max, block.mad, block.vwap, block.qty_sum, block.listing_count
    );
    if let Some(rate) = block.sale_rate {
        out.push_str(&format!(", saleRate = {rate:.4}"));
    }
    out
}

fn metrics_lua(metrics: &BTreeMap<Span, MetricBlock>) -> String {
    let spans: Vec<String> = metrics
        .iter()
        .map(|(span, block)| format!("{} = {{ {} }}", span.name(), metric_fields_lua(block)))
        .collect();
    format!("metrics = {{ {} }}", spans.join(", "))
}

fn entry_lua(entry: &FeedEntry) -> String {
    let identity = match &entry.item {
        ItemRef::Item { item_id } => format!("itemID = {item_id}"),
        ItemRef::Pet {
            species,
            level,
            quality,
            breed,
        } => format!(
            "kind = \"pet\", speciesID = {species}, level = {level}, qualityID = {quality}, breedID = {breed}"
        ),
    };
    format!("{{ {identity}, {} }}", metrics_lua(&entry.metrics))
}

fn render_lua(feed: &FeedSnapshot) -> Result<Vec<u8>, RenderError> {
    let items: Vec<String> = feed
        .entries
        .iter()
        .filter(|entry| !entry.metrics.is_empty()) // empty entries would fail the addon's strict ingestor
        .map(entry_lua)
        .collect();
    let lua = format!(
        "AuctiMoggerTrackerFeed = {{\n  version = 2,\n  generatedAt = {},\n  items = {{\n    {}\n  }},\n}}\n",
        feed.generated_at,
        items.join(",\n    ")
    );
    Ok(lua.into_bytes())
}

// --------------------------------------------------------------- JSON side

fn metric_json(block: &MetricBlock) -> serde_json::Value {
    serde_json::json!({
        "n": block.n,
        "min": block.min,
        "p05": block.p05,
        "p25": block.p25,
        "median": block.median,
        "p75": block.p75,
        "p95": block.p95,
        "max": block.max,
        "mad": block.mad,
        "vwap": block.vwap,
        "qtySum": block.qty_sum,
        "listingCount": block.listing_count,
        "saleRate": block.sale_rate,
    })
}

fn entry_json(entry: &FeedEntry) -> serde_json::Value {
    let mut metrics = serde_json::Map::new();
    for (span, block) in &entry.metrics {
        metrics.insert(span.name().to_string(), metric_json(block));
    }
    let (identity, base): (serde_json::Value, serde_json::Value) = match &entry.item {
        ItemRef::Item { item_id } => (
            serde_json::json!({}),
            serde_json::json!({ "itemID": item_id }),
        ),
        ItemRef::Pet {
            species,
            level,
            quality,
            breed,
        } => (
            serde_json::json!({ "kind": "pet" }),
            serde_json::json!({
                "speciesID": species,
                "level": level,
                "qualityID": quality,
                "breedID": breed,
            }),
        ),
    };
    let mut object = serde_json::Map::new();
    if let Some(kind) = identity.as_object() {
        for (key, value) in kind {
            object.insert(key.clone(), value.clone());
        }
    }
    for (key, value) in base.as_object().expect("base is an object") {
        object.insert(key.clone(), value.clone());
    }
    object.insert("metrics".into(), serde_json::Value::Object(metrics));
    serde_json::Value::Object(object)
}

fn render_json(feed: &FeedSnapshot) -> Result<Vec<u8>, RenderError> {
    let items: Vec<serde_json::Value> = feed
        .entries
        .iter()
        .filter(|entry| !entry.metrics.is_empty())
        .map(entry_json)
        .collect();
    let value = serde_json::json!({
        "version": 2,
        "generatedAt": feed.generated_at,
        "items": items,
    });
    serde_json::to_vec(&value).map_err(|e| RenderError(format!("json render: {e}")))
}

// --------------------------------------------------------------- publisher

/// Writes rendered feeds to `<dir>/<target>/feed.<ext>` atomically and keeps
/// the last-publication info for the web layer.
pub struct FileFeedPublisher {
    dir: PathBuf,
    last: Mutex<std::collections::HashMap<(String, FeedFormat), PublishedFeed>>,
}

impl FileFeedPublisher {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            last: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }
}

fn extension(format: FeedFormat) -> &'static str {
    match format {
        FeedFormat::Lua => "lua",
        FeedFormat::Json => "json",
    }
}

impl FeedPublisher for FileFeedPublisher {
    fn publish(
        &self,
        target: &TargetId,
        format: FeedFormat,
        bytes: &[u8],
    ) -> Result<(), StoreError> {
        let dir = self.dir.join(target.as_str());
        std::fs::create_dir_all(&dir).map_err(|e| StoreError::Backend(format!("feed dir: {e}")))?;
        let final_path = dir.join(format!("feed.{}", extension(format)));
        let tmp = dir.join(format!(".feed.{}.tmp", extension(format)));
        std::fs::write(&tmp, bytes).map_err(|e| StoreError::Backend(format!("feed write: {e}")))?;
        std::fs::rename(&tmp, &final_path)
            .map_err(|e| StoreError::Backend(format!("feed rename: {e}")))?;
        let at = now_unix();
        let size = bytes.len() as u64;
        self.last
            .lock()
            .unwrap()
            .insert((target.0.clone(), format), PublishedFeed { at, size });
        Ok(())
    }

    fn last_published(
        &self,
        target: &TargetId,
        format: FeedFormat,
    ) -> Result<Option<PublishedFeed>, StoreError> {
        Ok(self
            .last
            .lock()
            .unwrap()
            .get(&(target.0.clone(), format))
            .copied())
    }

    fn read(&self, target: &TargetId, format: FeedFormat) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self
            .dir
            .join(target.as_str())
            .join(format!("feed.{}", extension(format)));
        std::fs::read(&path).map(Some).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(StoreError::Backend(format!("feed read: {e}")))
            }
        })
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use amtracker_core::ports::FeedRenderer;

    fn sample_feed() -> FeedSnapshot {
        let block = MetricBlock {
            n: 5,
            min: 100,
            p05: 100,
            p25: 200,
            median: 300,
            p75: 400,
            p95: 500,
            max: 500,
            mad: 100,
            vwap: 366,
            qty_sum: 150,
            listing_count: 5,
            sale_rate: Some(0.25),
        };
        let mut metrics = BTreeMap::new();
        metrics.insert(Span::D1, block.clone());
        let mut pet_metrics = BTreeMap::new();
        pet_metrics.insert(Span::D1, block);
        FeedSnapshot {
            target: TargetId("us-4".into()),
            generated_at: 1_760_000_000,
            entries: vec![
                FeedEntry {
                    item: ItemRef::Item { item_id: 194784 },
                    metrics,
                },
                FeedEntry {
                    item: ItemRef::Pet {
                        species: 2546,
                        level: 1,
                        quality: 3,
                        breed: 11,
                    },
                    metrics: pet_metrics,
                },
            ],
        }
    }

    #[test]
    fn lua_render_matches_the_contract_shape() {
        let mut feed = sample_feed();
        // entries with no metrics are omitted upstream; drop here to test the
        // renderer against well-formed input only
        feed.entries.truncate(1);
        let lua = FeedCodec.render(&feed, FeedFormat::Lua).expect("render");
        let text = String::from_utf8(lua).expect("utf-8");
        let expected = "AuctiMoggerTrackerFeed = {\n  \
            version = 2,\n  generatedAt = 1760000000,\n  items = {\n    \
            { itemID = 194784, metrics = { d1 = { n = 5, min = 100, p05 = 100, p25 = 200, median = 300, p75 = 400, p95 = 500, max = 500, mad = 100, vwap = 366, qtySum = 150, listingCount = 5, saleRate = 0.2500 } } }\n  },\n}\n";
        assert_eq!(text, expected);
    }

    #[test]
    fn pet_entries_render_the_contract_identity() {
        let mut feed = sample_feed();
        feed.entries.truncate(2);
        let d1_block = feed.entries[0].metrics[&Span::D1].clone();
        feed.entries[1].metrics.insert(
            Span::D7,
            MetricBlock {
                sale_rate: None,
                ..d1_block
            },
        );
        let lua = String::from_utf8(FeedCodec.render(&feed, FeedFormat::Lua).expect("render"))
            .expect("utf-8");
        assert!(
            lua.contains(
                "kind = \"pet\", speciesID = 2546, level = 1, qualityID = 3, breedID = 11"
            ),
            "{lua}"
        );
        assert!(lua.contains("d7 = { n = 5"), "{lua}");
    }

    #[test]
    fn json_render_round_trips_through_serde() {
        let feed = sample_feed();
        let bytes = FeedCodec.render(&feed, FeedFormat::Json).expect("render");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("valid json");
        assert_eq!(value["version"], 2);
        assert_eq!(value["generatedAt"], 1_760_000_000);
        assert_eq!(value["items"][0]["itemID"], 194784);
        assert_eq!(value["items"][0]["metrics"]["d1"]["median"], 300);
        assert_eq!(value["items"][0]["metrics"]["d1"]["saleRate"], 0.25);
        assert_eq!(value["items"][1]["kind"], "pet");
        assert_eq!(value["items"][1]["speciesID"], 2546);
    }
}

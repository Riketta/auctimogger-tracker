//! Feed export (tracker feed v2): gathers per-item span metrics and shapes
//! them into the frozen cross-addon contract (see the README's
//! "Feed contract (v2)" section). Rendering and publication are outbound
//! ports; this service owns the selection, freshness and grain rules.
//!
//! Grain rule: the addon collapses every pet variant onto its species (the
//! cage item key), so pet entries are exported per SPECIES — all variants'
//! day-folds merge into one metric population and their sale rates pool.
//! Exporting per-variant entries would make the in-game store silently
//! keep only whichever variant ingested last.

use std::{
    collections::BTreeMap,
    sync::Arc,
};

use crate::{
    domain::{
        utc_day,
        Category,
        FeedEntry,
        FeedSnapshot,
        ItemRef,
        MetricBlock,
        PayloadKind,
        Span,
        TargetId,
    },
    error::StoreError,
    ports::{
        ClockPort,
        PriceHistoryRepository,
        SnapshotRepository,
        WatchlistRepository,
    },
    services::metrics::MetricsService,
};

/// Feed build options.
#[derive(Clone, Debug)]
pub struct ExportOptions {
    /// The spans to include in every entry's metric map.
    pub spans: Vec<Span>,
    /// Slim mode: export only the target's watchlist items.
    pub watchlist_only: bool,
}

/// Builds feed snapshots from cached metrics and history.
pub struct FeedExportService {
    snapshots: Arc<dyn SnapshotRepository>,
    history: Arc<dyn PriceHistoryRepository>,
    watchlist: Arc<dyn WatchlistRepository>,
    metrics: Arc<MetricsService>,
    clock: Arc<dyn ClockPort>,
}

impl FeedExportService {
    /// Wires the service to its ports. `metrics` is shared with the query
    /// side so export hits the same cache.
    pub fn new(
        snapshots: Arc<dyn SnapshotRepository>,
        history: Arc<dyn PriceHistoryRepository>,
        watchlist: Arc<dyn WatchlistRepository>,
        metrics: Arc<MetricsService>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self { snapshots, history, watchlist, metrics, clock }
    }

    /// Builds the feed snapshot for a target. `generatedAt` is the newest
    /// source `Last-Modified` across the target's payload kinds (falling back
    /// to publish time when unknown), so the addon's 24-hour freshness gate
    /// reflects actual data age, not export time.
    ///
    /// Entries are per (category, feed grain): an item trading in both
    /// auction houses exports twice, once per category, each on its own
    /// evidence. All metric blocks are computed through the bulk paths — one
    /// cache / fold / sale-rate pass per span chunk per category — so a
    /// whole-market publish costs a handful of chunked queries instead of
    /// per-item-per-day round trips.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when snapshot metadata, the watchlist
    /// or a metric block cannot be read.
    pub fn build_feed(
        &self,
        target: &TargetId,
        options: &ExportOptions,
    ) -> Result<FeedSnapshot, StoreError> {
        let generated_at = self.freshness(target)?;
        let today = utc_day(self.clock.now());
        let window_days = options.spans.iter().map(Span::days).max().unwrap_or(1);
        let watchlist_items: Option<Vec<ItemRef>> = if options.watchlist_only {
            Some(self.watchlist.list(target)?.into_iter().map(|entry| entry.item).collect())
        } else {
            None
        };

        let mut entries = Vec::new();
        for category in Category::ALL {
            let items: Vec<ItemRef> = match &watchlist_items {
                Some(items) => items.clone(),
                None => self.history.items_with_data(target, today - window_days + 1, category)?,
            };
            if items.is_empty() {
                continue;
            }
            // Group pet variants by species so the exported feed matches the
            // addon's species-level grain; plain items stay per-id. The
            // grouping runs per category: the merged (species-population)
            // path pools only variants of the same category.
            let groups = group_by_feed_grain(items);

            // Split by computation path, preserving group order: single-member
            // groups go through the cached per-item bulk path, multi-member
            // populations through the merged bulk path (folds live under the
            // original variant identity, not the canonical grain).
            let mut singles: Vec<ItemRef> = Vec::new();
            let mut populations: Vec<Vec<ItemRef>> = Vec::new();
            for members in groups.values() {
                match members.first() {
                    Some(only) if members.len() == 1 => singles.push(only.clone()),
                    _ => populations.push(members.clone()),
                }
            }

            // Per-span block matrices, row-aligned with `singles` / `populations`.
            let single_blocks = options
                .spans
                .iter()
                .map(|span| self.metrics.blocks(target, &singles, category, *span))
                .collect::<Result<Vec<_>, _>>()?;
            let population_refs: Vec<&[ItemRef]> = populations.iter().map(Vec::as_slice).collect();
            let merged_blocks = options
                .spans
                .iter()
                .map(|span| self.metrics.blocks_merged(target, &population_refs, category, *span))
                .collect::<Result<Vec<_>, _>>()?;

            // Stitch: walk the groups in the same order the split ran, pulling
            // each group's rows off the matching cursor. The cursors can only
            // under-run when `spans` is empty (no rows at all), which yields an
            // empty row and therefore entry-less output — the degenerate config.
            let empty_row: Vec<Option<MetricBlock>> = Vec::new();
            let mut single_rows = single_blocks.iter();
            let mut merged_rows = merged_blocks.iter();
            for members in groups.into_values() {
                let first = members
                    .first()
                    .ok_or_else(|| StoreError::Backend("feed group lost its members".into()))?;
                let rows = if members.len() == 1 {
                    single_rows.next().unwrap_or(&empty_row)
                } else {
                    merged_rows.next().unwrap_or(&empty_row)
                };
                let mut metrics = BTreeMap::new();
                for (span, block) in options.spans.iter().zip(rows) {
                    if let Some(block) = block {
                        metrics.insert(*span, block.clone());
                    }
                }
                // Entries without any valid span would be rejected by the
                // addon's strict ingestor — omit them.
                if !metrics.is_empty() {
                    entries.push(FeedEntry { item: canonical_item(first), category, metrics });
                }
            }
        }
        entries.sort_by(|a, b| a.item.cmp(&b.item).then_with(|| a.category.cmp(&b.category)));
        Ok(FeedSnapshot { target: target.clone(), generated_at, entries })
    }

    fn freshness(&self, target: &TargetId) -> Result<i64, StoreError> {
        let mut newest = 0i64;
        for kind in [PayloadKind::Commodities, PayloadKind::Auctions] {
            if let Some(meta) = self.snapshots.latest(target, kind)? {
                if let Some(lm) = meta.source_last_modified {
                    newest = newest.max(lm);
                }
            }
        }
        if newest == 0 {
            Ok(self.clock.now())
        } else {
            Ok(newest)
        }
    }
}

/// Groups flat identities by feed grain: plain items per id, all pet
/// variants of a species together (the addon resolves every pet entry onto
/// the cage-species key).
fn group_by_feed_grain(items: Vec<ItemRef>) -> BTreeMap<PetGroupKey, Vec<ItemRef>> {
    let mut groups: BTreeMap<PetGroupKey, Vec<ItemRef>> = BTreeMap::new();
    for item in items {
        let key = match &item {
            ItemRef::Item { item_id } => PetGroupKey::Item(*item_id),
            ItemRef::Pet { species, .. } => PetGroupKey::PetSpecies(*species),
        };
        groups.entry(key).or_default().push(item);
    }
    groups
}

/// Feed-grain grouping key: plain items per id, all pet variants of a species
/// together (the addon resolves every pet entry onto the cage-species key).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PetGroupKey {
    Item(i64),
    PetSpecies(i64),
}

/// The identity a feed entry carries for a group. Pet groups export the
/// species-level tuple (level/quality/breed zeroed — the addon ignores them
/// and JSON consumers should see the canonical grain).
fn canonical_item(item: &ItemRef) -> ItemRef {
    match *item {
        ItemRef::Item { item_id } => ItemRef::Item { item_id },
        ItemRef::Pet { species, .. } => ItemRef::Pet { species, level: 0, quality: 0, breed: 0 },
    }
}

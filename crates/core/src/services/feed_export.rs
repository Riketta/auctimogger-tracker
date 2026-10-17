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
        FeedEntry,
        FeedSnapshot,
        ItemRef,
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

        let items: Vec<ItemRef> = if options.watchlist_only {
            self.watchlist.list(target)?.into_iter().map(|entry| entry.item).collect()
        } else {
            self.history.items_with_data(target, today - window_days + 1)?
        };

        // Group pet variants by species so the exported feed matches the
        // addon's species-level grain; plain items stay per-id.
        let mut groups: BTreeMap<PetGroupKey, Vec<ItemRef>> = BTreeMap::new();
        for item in items {
            let key = match &item {
                ItemRef::Item { item_id } => PetGroupKey::Item(*item_id),
                ItemRef::Pet { species, .. } => PetGroupKey::PetSpecies(*species),
            };
            groups.entry(key).or_default().push(item);
        }

        let mut entries = Vec::new();
        for (_, members) in groups {
            // Groups are built by pushing at least one member, so indexing is
            // in bounds by construction; a `.get()` fallback would produce an
            // entry-less feed row instead of failing loudly.
            #[allow(clippy::indexing_slicing)]
            let (identity, single) = (canonical_item(&members[0]), members.len() == 1);
            let mut metrics = BTreeMap::new();
            for span in &options.spans {
                // Singles keep the per-item cached path (folds live under the
                // original variant identity, not the canonical grain); merged
                // populations go through block_merged.
                #[allow(clippy::indexing_slicing)]
                let block = if single {
                    self.metrics.block(target, &members[0], *span)?
                } else {
                    self.metrics.block_merged(target, &members, *span)?
                };
                if let Some(block) = block {
                    metrics.insert(*span, block);
                }
            }
            // Entries without any valid span would be rejected by the addon's
            // strict ingestor — omit them.
            if !metrics.is_empty() {
                entries.push(FeedEntry { item: identity, metrics });
            }
        }
        entries.sort_by(|a, b| a.item.cmp(&b.item));
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

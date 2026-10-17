//! Item catalog maintenance: resolve names on demand (cache-aside over the
//! item-info port) so search and the UI have human-readable labels.

use std::{
    collections::{
        HashMap,
        VecDeque,
    },
    sync::{
        Arc,
        Mutex,
    },
    time::Instant,
};

use crate::{
    domain::{
        BrowseCursor,
        CatalogCounts,
        NameRecord,
        PetNameRecord,
        SearchHit,
    },
    error::StoreError,
    ports::{
        ItemCatalogRepository,
        ItemInfoPort,
    },
};

/// How long a "not found at the item API" answer is remembered. Without this,
/// every dashboard/opportunity render of a deleted or never-existing id (emu
/// dumps carry plenty) re-hits the external item API.
const NEGATIVE_TTL_SECONDS: u64 = 600;

/// How many "not found" ids are remembered at once. Negative ids are junk
/// ids, and junk comes in bursts; the FIFO bound keeps one burst from
/// growing memory while preserving every unrelated entry.
const NEGATIVE_CACHE_CAP: usize = 10_000;

/// Bounded map of "known junk" ids, remembered for [`NEGATIVE_TTL_SECONDS`].
/// Eviction is FIFO, not LRU: all entries share one TTL, so insertion order
/// is as good a victim rule as recency, and FIFO keeps eviction O(1).
/// (The previous full `clear()` on overflow let a single junk-id burst
/// re-trigger an external-API fetch storm for every other remembered id.)
struct NegativeCache {
    /// `id -> (remembered at, insertion sequence)` — the sequence lets the
    /// eviction queue tell superseded entries apart from live ones.
    entries: HashMap<i64, (Instant, u64)>,
    /// Insertion order for FIFO eviction: `(id, sequence it was pushed with)`.
    order: VecDeque<(i64, u64)>,
    /// Monotonic insertion counter backing the sequence tags.
    seq: u64,
    /// Entry ceiling; overshoot is shed oldest-first.
    cap: usize,
}

impl NegativeCache {
    fn new(cap: usize) -> Self {
        Self { entries: HashMap::new(), order: VecDeque::new(), seq: 0, cap }
    }

    /// Whether the key is remembered and still fresh. An expired entry is
    /// dropped on sight; its stale queue entry becomes eviction litter that
    /// [`Self::remember`] skips over.
    fn contains_fresh(&mut self, key: i64) -> bool {
        match self.entries.get(&key) {
            Some((at, _)) if at.elapsed().as_secs() < NEGATIVE_TTL_SECONDS => true,
            _ => {
                self.entries.remove(&key);
                false
            }
        }
    }

    /// Remembers `key` until the TTL expires, evicting oldest-first once over
    /// capacity. Re-remembering an id supersedes its old queue entry, which
    /// the eviction loop then skips as litter.
    fn remember(&mut self, key: i64) {
        self.seq = self.seq.wrapping_add(1);
        self.entries.insert(key, (Instant::now(), self.seq));
        self.order.push_back((key, self.seq));
        while self.entries.len() > self.cap {
            match self.order.pop_front() {
                // Only a queue entry matching the live map entry evicts;
                // superseded or expired ids are just queue litter.
                Some((id, seq)) if self.entries.get(&id).map(|(_, s)| *s) == Some(seq) => {
                    self.entries.remove(&id);
                }
                Some(_) => {}
                None => break,
            }
        }
    }
}

/// Resolves and caches human-readable item/pet names for the catalog.
pub struct CatalogService {
    catalog: Arc<dyn ItemCatalogRepository>,
    info: Arc<dyn ItemInfoPort>,
    negative: Mutex<NegativeCache>,
    negative_pets: Mutex<NegativeCache>,
}

impl CatalogService {
    /// Wires the service to the catalog store and the item-info port (the
    /// read-through source on cache misses).
    pub fn new(catalog: Arc<dyn ItemCatalogRepository>, info: Arc<dyn ItemInfoPort>) -> Self {
        Self {
            catalog,
            info,
            negative: Mutex::new(NegativeCache::new(NEGATIVE_CACHE_CAP)),
            negative_pets: Mutex::new(NegativeCache::new(NEGATIVE_CACHE_CAP)),
        }
    }

    /// Returns the catalog record, fetching it from the item API on first
    /// sight. A missing API record is remembered negatively for a bounded
    /// window (temporary item-API failures must not stick forever, but every
    /// render re-querying the API is worse).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog read/write fails or
    /// the item API lookup fails.
    pub fn ensure(&self, item_id: i64) -> Result<Option<NameRecord>, StoreError> {
        if let Some(record) = self.catalog.by_id(item_id)? {
            return Ok(Some(record));
        }
        if Self::negatively_cached(&self.negative, item_id) {
            return Ok(None);
        }
        if let Some(record) =
            self.info.describe(item_id).map_err(|e| StoreError::Backend(e.to_string()))?
        {
            self.catalog.upsert(std::slice::from_ref(&record))?;
            return Ok(Some(record));
        }
        Self::remember_negative(&self.negative, item_id);
        Ok(None)
    }

    /// Returns the pet-species catalog record, fetching it from the pet API
    /// on first sight (same negative-cache policy as items).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog read/write fails or
    /// the pet API lookup fails.
    pub fn ensure_pet(&self, species_id: i64) -> Result<Option<PetNameRecord>, StoreError> {
        if let Some(record) = self.catalog.pet_by_species(species_id)? {
            return Ok(Some(record));
        }
        if Self::negatively_cached(&self.negative_pets, species_id) {
            return Ok(None);
        }
        if let Some(record) =
            self.info.describe_pet(species_id).map_err(|e| StoreError::Backend(e.to_string()))?
        {
            self.catalog.upsert_pets(std::slice::from_ref(&record))?;
            return Ok(Some(record));
        }
        Self::remember_negative(&self.negative_pets, species_id);
        Ok(None)
    }

    /// Locks a negative cache and checks freshness. A poisoned mutex (a
    /// panic while holding it) degrades to "cache missed" instead of failing
    /// the request.
    fn negatively_cached(cache: &Mutex<NegativeCache>, key: i64) -> bool {
        cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains_fresh(key)
    }

    /// Locks a negative cache and remembers the id. Poisoning degrades to
    /// "cache missed" on the next check, never to a failed request.
    fn remember_negative(cache: &Mutex<NegativeCache>, key: i64) {
        cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remember(key);
    }

    /// Substring search over known names, items and pets (completions).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, StoreError> {
        self.catalog.search(query, limit)
    }

    /// Alphabetical catalog page (keyset-paginated, see the port).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    pub fn browse(
        &self,
        after: Option<&BrowseCursor>,
        limit: usize,
    ) -> Result<(Vec<SearchHit>, bool), StoreError> {
        self.catalog.browse(after, limit)
    }

    /// Named-record counts per kind (dashboard KPIs).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    pub fn counts(&self) -> Result<CatalogCounts, StoreError> {
        self.catalog.counts()
    }
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect/panic: fixture-driven, deterministic
    // inputs (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn eviction_drops_the_oldest_and_keeps_the_rest() {
        let mut cache = NegativeCache::new(3);
        for id in 1..=5 {
            cache.remember(id);
        }
        assert!(!cache.contains_fresh(1), "oldest is evicted first");
        assert!(!cache.contains_fresh(2));
        assert!(cache.contains_fresh(3));
        assert!(cache.contains_fresh(4));
        assert!(cache.contains_fresh(5), "the newest id survives");
    }

    /// Re-remembering an id leaves stale queue litter behind; the eviction
    /// loop must skip it and evict the genuinely oldest entry instead.
    #[test]
    fn reremembering_supersedes_the_queue_entry_not_the_entry() {
        let mut cache = NegativeCache::new(3);
        cache.remember(1);
        cache.remember(2);
        cache.remember(3);
        cache.remember(2); // supersede 2's queue entry
        cache.remember(4); // over cap: evicts 1, 2 stays live
        assert!(!cache.contains_fresh(1));
        assert!(cache.contains_fresh(2), "the re-remembered id stays live");
        assert!(cache.contains_fresh(3));
        assert!(cache.contains_fresh(4));

        cache.remember(5); // over cap: 2's old queue entry is litter, 3 evicts
        assert!(!cache.contains_fresh(3));
        assert!(cache.contains_fresh(2));
        assert!(cache.contains_fresh(4));
        assert!(cache.contains_fresh(5));
    }
}

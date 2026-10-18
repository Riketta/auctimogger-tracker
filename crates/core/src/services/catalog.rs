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
        Flavor,
        NameRecord,
        PetNameRecord,
        SearchHit,
    },
    error::StoreError,
    ports::{
        ClockPort,
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

/// How long a sparse catalog row (stored before the enriched columns existed,
/// carrying name/quality only) is kept without re-fetching it for the richer
/// fields. Long: the enrichment is cosmetic, and the point is to upgrade old
/// rows eventually without ever mass-hitting the external source.
const SPARSE_REFRESH_SECONDS: i64 = 30 * 24 * 3600;

/// Bounded map of "known junk" ids, remembered for [`NEGATIVE_TTL_SECONDS`].
/// Eviction is FIFO, not LRU: all entries share one TTL, so insertion order
/// is as good a victim rule as recency, and FIFO keeps eviction O(1).
/// (The previous full `clear()` on overflow let a single junk-id burst
/// re-trigger an external-API fetch storm for every other remembered id.)
/// Keys are `(flavor, id)` pairs: junk status is per flavor, since the same
/// numeric id may exist in one game version but not another.
struct NegativeCache {
    /// `(flavor, id) -> (remembered at, insertion sequence)` — the sequence
    /// lets the eviction queue tell superseded entries apart from live ones.
    entries: HashMap<(String, i64), (Instant, u64)>,
    /// Insertion order for FIFO eviction: `(key, sequence it was pushed with)`.
    order: VecDeque<((String, i64), u64)>,
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
    fn contains_fresh(&mut self, key: &(String, i64)) -> bool {
        match self.entries.get(key) {
            Some((at, _)) if at.elapsed().as_secs() < NEGATIVE_TTL_SECONDS => true,
            _ => {
                self.entries.remove(key);
                false
            }
        }
    }

    /// Remembers `key` until the TTL expires, evicting oldest-first once over
    /// capacity. Re-remembering an id supersedes its old queue entry, which
    /// the eviction loop then skips as litter.
    fn remember(&mut self, key: (String, i64)) {
        self.seq = self.seq.wrapping_add(1);
        self.entries.insert(key.clone(), (Instant::now(), self.seq));
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

/// Resolves and caches human-readable item/pet names for the catalog,
/// per flavor.
pub struct CatalogService {
    catalog: Arc<dyn ItemCatalogRepository>,
    info: Arc<dyn ItemInfoPort>,
    clock: Arc<dyn ClockPort>,
    negative: Mutex<NegativeCache>,
    negative_pets: Mutex<NegativeCache>,
}

impl CatalogService {
    /// Wires the service to the catalog store, the item-info port (the
    /// read-through source on cache misses) and the clock (sparse-row
    /// enrichment windows).
    pub fn new(
        catalog: Arc<dyn ItemCatalogRepository>,
        info: Arc<dyn ItemInfoPort>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self {
            catalog,
            info,
            clock,
            negative: Mutex::new(NegativeCache::new(NEGATIVE_CACHE_CAP)),
            negative_pets: Mutex::new(NegativeCache::new(NEGATIVE_CACHE_CAP)),
        }
    }

    /// Returns the catalog record for `(flavor, item_id)`, fetching it from
    /// the item API on first sight. A missing API record is remembered
    /// negatively for a bounded window (temporary item-API failures must not
    /// stick forever, but every render re-querying the API is worse). Sparse
    /// rows past the enrichment window are re-fetched to gain the newer
    /// fields.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog read/write fails or
    /// the item API lookup fails.
    pub fn ensure(&self, flavor: &Flavor, item_id: i64) -> Result<Option<NameRecord>, StoreError> {
        if let Some(record) = self.catalog.by_id(flavor, item_id)? {
            let stale = record.is_sparse()
                && self.clock.now().saturating_sub(record.fetched_at) > SPARSE_REFRESH_SECONDS;
            if !stale {
                return Ok(Some(record));
            }
        }
        let key = (flavor.as_str().to_owned(), item_id);
        if Self::negatively_cached(&self.negative, &key) {
            return Ok(None);
        }
        match self.info.describe(flavor, item_id) {
            Ok(Some(record)) => {
                self.catalog.upsert(flavor, std::slice::from_ref(&record))?;
                Ok(Some(record))
            }
            Ok(None) => {
                Self::remember_negative(&self.negative, key);
                Ok(None)
            }
            Err(error) => Err(StoreError::Backend(error.to_string())),
        }
    }

    /// Whether the catalog already holds `(flavor, item_id)` — the cheap
    /// pre-check the background warmer uses to pace only real fetches.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    pub fn known(&self, flavor: &Flavor, item_id: i64) -> Result<bool, StoreError> {
        self.catalog.by_id(flavor, item_id).map(|record| record.is_some())
    }

    /// Returns the pet-species catalog record for `(flavor, species_id)`,
    /// fetching it from the pet API on first sight (same negative-cache and
    /// error policy as items).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog read/write fails or
    /// the pet API lookup fails.
    pub fn ensure_pet(
        &self,
        flavor: &Flavor,
        species_id: i64,
    ) -> Result<Option<PetNameRecord>, StoreError> {
        if let Some(record) = self.catalog.pet_by_species(flavor, species_id)? {
            return Ok(Some(record));
        }
        let key = (flavor.as_str().to_owned(), species_id);
        if Self::negatively_cached(&self.negative_pets, &key) {
            return Ok(None);
        }
        match self.info.describe_pet(flavor, species_id) {
            Ok(Some(record)) => {
                self.catalog.upsert_pets(flavor, std::slice::from_ref(&record))?;
                Ok(Some(record))
            }
            Ok(None) => {
                Self::remember_negative(&self.negative_pets, key);
                Ok(None)
            }
            Err(error) => Err(StoreError::Backend(error.to_string())),
        }
    }

    /// Locks a negative cache and checks freshness. A poisoned mutex (a
    /// panic while holding it) degrades to "cache missed" instead of failing
    /// the request.
    fn negatively_cached(cache: &Mutex<NegativeCache>, key: &(String, i64)) -> bool {
        cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains_fresh(key)
    }

    /// Locks a negative cache and remembers the id. Poisoning degrades to
    /// "cache missed" on the next check, never to a failed request.
    fn remember_negative(cache: &Mutex<NegativeCache>, key: (String, i64)) {
        cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remember(key);
    }

    /// Substring search over known names, items and pets (completions).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        self.catalog.search(query, limit, offset)
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
        let key = |v: i64| ("retail".to_string(), v);
        let mut cache = NegativeCache::new(3);
        for id in 1..=5 {
            cache.remember(key(id));
        }
        assert!(!cache.contains_fresh(&key(1)), "oldest is evicted first");
        assert!(!cache.contains_fresh(&key(2)));
        assert!(cache.contains_fresh(&key(3)));
        assert!(cache.contains_fresh(&key(4)));
        assert!(cache.contains_fresh(&key(5)), "the newest id survives");
    }

    /// Re-remembering an id leaves stale queue litter behind; the eviction
    /// loop must skip it and evict the genuinely oldest entry instead.
    #[test]
    fn reremembering_supersedes_the_queue_entry_not_the_entry() {
        let key = |v: i64| ("retail".to_string(), v);
        let mut cache = NegativeCache::new(3);
        cache.remember(key(1));
        cache.remember(key(2));
        cache.remember(key(3));
        cache.remember(key(2)); // supersede 2's queue entry
        cache.remember(key(4)); // over cap: evicts 1, 2 stays live
        assert!(!cache.contains_fresh(&key(1)));
        assert!(cache.contains_fresh(&key(2)), "the re-remembered id stays live");
        assert!(cache.contains_fresh(&key(3)));
        assert!(cache.contains_fresh(&key(4)));

        cache.remember(key(5)); // over cap: 2's old queue entry is litter, 3 evicts
        assert!(!cache.contains_fresh(&key(3)));
        assert!(cache.contains_fresh(&key(2)));
        assert!(cache.contains_fresh(&key(4)));
        assert!(cache.contains_fresh(&key(5)));
    }

    /// Negative status is per flavor: the same numeric id may exist in one
    /// game version but not another, so keys do not collide across flavors.
    #[test]
    fn negative_keys_are_scoped_per_flavor() {
        let mut cache = NegativeCache::new(10);
        cache.remember(("retail".to_string(), 42));
        assert!(cache.contains_fresh(&("retail".to_string(), 42)));
        assert!(!cache.contains_fresh(&("forever".to_string(), 42)), "other flavor misses");
    }
}

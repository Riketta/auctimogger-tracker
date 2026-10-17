//! Item catalog maintenance: resolve names on demand (cache-aside over the
//! item-info port) so search and the UI have human-readable labels.

use std::{
    collections::HashMap,
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

/// Resolves and caches human-readable item/pet names for the catalog.
pub struct CatalogService {
    catalog: Arc<dyn ItemCatalogRepository>,
    info: Arc<dyn ItemInfoPort>,
    negative: Mutex<HashMap<i64, Instant>>,
    negative_pets: Mutex<HashMap<i64, Instant>>,
}

impl CatalogService {
    /// Wires the service to the catalog store and the item-info port (the
    /// read-through source on cache misses).
    pub fn new(catalog: Arc<dyn ItemCatalogRepository>, info: Arc<dyn ItemInfoPort>) -> Self {
        Self {
            catalog,
            info,
            negative: Mutex::new(HashMap::new()),
            negative_pets: Mutex::new(HashMap::new()),
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

    /// Locks a negative cache. A poisoned mutex (a panic while holding it)
    /// degrades to "cache missed" instead of failing the request.
    fn negatively_cached(cache: &Mutex<HashMap<i64, Instant>>, key: i64) -> bool {
        let mut cache = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match cache.get(&key) {
            Some(at) if at.elapsed().as_secs() < NEGATIVE_TTL_SECONDS => true,
            _ => {
                cache.remove(&key);
                false
            }
        }
    }

    fn remember_negative(cache: &Mutex<HashMap<i64, Instant>>, key: i64) {
        let mut cache = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(key, Instant::now());
        // Bound the map: negative ids are junk ids, and junk comes in bursts.
        if cache.len() > 10_000 {
            cache.clear();
        }
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

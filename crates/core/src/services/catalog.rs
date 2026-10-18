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

/// How long a FAILED source lookup is remembered — much shorter than the
/// negative TTL, because an outage answer means "try again soon", not "does
/// not exist". Inside the window, renders degrade to raw-id labels
/// immediately instead of re-paying the source timeout and the background
/// warm-up skips the id entirely; once it lapses the id gets a fresh
/// attempt. Without this, a source outage made every cold table render
/// block on the full transport timeout, forever.
const ERROR_TTL_SECONDS: u64 = 60;

/// How long a sparse catalog row (stored before the enriched columns existed,
/// carrying name/quality only) is kept without re-fetching it for the richer
/// fields. Long: the enrichment is cosmetic, and the point is to upgrade old
/// rows eventually without ever mass-hitting the external source.
const SPARSE_REFRESH_SECONDS: i64 = 30 * 24 * 3600;

/// Bounded map of remembered ids — "known junk" for the negative caches,
/// recent source failures for the error caches — each remembered for its
/// cache's TTL. Eviction is FIFO, not LRU: all entries share one TTL, so
/// insertion order is as good a victim rule as recency, and FIFO keeps
/// eviction O(1). (The previous full `clear()` on overflow let a single
/// junk-id burst re-trigger an external-API fetch storm for every other
/// remembered id.) Keys are `(flavor, id)` pairs: junk status is per
/// flavor, since the same numeric id may exist in one game version but not
/// another.
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
    /// How long an entry stays fresh (negative and error caches differ).
    ttl_secs: u64,
}

impl NegativeCache {
    fn new(cap: usize, ttl_secs: u64) -> Self {
        Self { entries: HashMap::new(), order: VecDeque::new(), seq: 0, cap, ttl_secs }
    }

    /// Whether the key is remembered and still fresh. An expired entry is
    /// dropped on sight; its stale queue entry becomes eviction litter that
    /// [`Self::remember`] skips over.
    fn contains_fresh(&mut self, key: &(String, i64)) -> bool {
        match self.entries.get(key) {
            Some((at, _)) if at.elapsed().as_secs() < self.ttl_secs => true,
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
/// per flavor. Answers come from the catalog store first; misses go to the
/// item-info port once and are then remembered — positive rows forever,
/// "missing" answers for the negative TTL, and source failures for a short
/// error TTL so an outage degrades to fast raw-label renders instead of
/// re-paying the transport timeout on every view.
pub struct CatalogService {
    catalog: Arc<dyn ItemCatalogRepository>,
    info: Arc<dyn ItemInfoPort>,
    clock: Arc<dyn ClockPort>,
    negative: Mutex<NegativeCache>,
    negative_pets: Mutex<NegativeCache>,
    errors: Mutex<NegativeCache>,
    errors_pets: Mutex<NegativeCache>,
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
            negative: Mutex::new(NegativeCache::new(NEGATIVE_CACHE_CAP, NEGATIVE_TTL_SECONDS)),
            negative_pets: Mutex::new(NegativeCache::new(NEGATIVE_CACHE_CAP, NEGATIVE_TTL_SECONDS)),
            errors: Mutex::new(NegativeCache::new(NEGATIVE_CACHE_CAP, ERROR_TTL_SECONDS)),
            errors_pets: Mutex::new(NegativeCache::new(NEGATIVE_CACHE_CAP, ERROR_TTL_SECONDS)),
        }
    }

    /// Returns the catalog record for `(flavor, item_id)`, fetching it from
    /// the item API on first sight. A missing API record is remembered
    /// negatively for a bounded window (temporary item-API failures must not
    /// stick forever, but every render re-querying the API is worse), and a
    /// failed API call is remembered for a shorter error window so outages
    /// degrade to raw labels instead of timing out every render. Sparse rows
    /// past the enrichment window are re-fetched to gain the newer fields —
    /// but a stored name always beats an empty or failed refresh: "cached
    /// forever" must not silently demote to a raw label.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog read/write fails or
    /// the item API lookup fails outside the error window.
    pub fn ensure(&self, flavor: &Flavor, item_id: i64) -> Result<Option<NameRecord>, StoreError> {
        let cached = self.catalog.by_id(flavor, item_id)?;
        if let Some(record) = &cached {
            let stale = record.is_sparse()
                && self.clock.now().saturating_sub(record.fetched_at) > SPARSE_REFRESH_SECONDS;
            if !stale {
                return Ok(Some(record.clone()));
            }
        }
        let key = (flavor.as_str().to_owned(), item_id);
        // A remembered miss or recent failure answers instantly; a stale
        // sparse row still beats both — its stored name is real even when
        // the source can no longer confirm it.
        if Self::remembered(&self.negative, &key) || Self::remembered(&self.errors, &key) {
            return Ok(cached);
        }
        match self.info.describe(flavor, item_id) {
            Ok(Some(record)) => {
                self.catalog.upsert(flavor, std::slice::from_ref(&record))?;
                Ok(Some(record))
            }
            Ok(None) => {
                Self::remember(&self.negative, key);
                Ok(cached)
            }
            Err(error) => {
                Self::remember(&self.errors, key);
                cached.map_or_else(|| Err(StoreError::Backend(error.to_string())), |r| Ok(Some(r)))
            }
        }
    }

    /// Whether the catalog already holds `(flavor, item_id)` in the ITEM
    /// table — the cheap pre-check the background warmer uses for items.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    pub fn known(&self, flavor: &Flavor, item_id: i64) -> Result<bool, StoreError> {
        self.catalog.by_id(flavor, item_id).map(|record| record.is_some())
    }

    /// Whether the catalog already holds `(flavor, species_id)` in the PET
    /// table. Species ids are their own namespace — a cataloged item with
    /// the same numeric id says nothing about a pet, so the warmer must ask
    /// this, not [`Self::known`].
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog cannot be read.
    pub fn known_pet(&self, flavor: &Flavor, species_id: i64) -> Result<bool, StoreError> {
        self.catalog.pet_by_species(flavor, species_id).map(|record| record.is_some())
    }

    /// Whether an item lookup recently answered "missing" or failed: the
    /// background warmer skips such ids (no budget, no pacing sleep), since
    /// re-asking now would burn idle time without new information. Both
    /// windows expire, so the id gets its turn again later.
    #[must_use]
    pub fn deferred(&self, flavor: &Flavor, item_id: i64) -> bool {
        let key = (flavor.as_str().to_owned(), item_id);
        Self::remembered(&self.negative, &key) || Self::remembered(&self.errors, &key)
    }

    /// Pet-table counterpart of [`Self::deferred`].
    #[must_use]
    pub fn deferred_pet(&self, flavor: &Flavor, species_id: i64) -> bool {
        let key = (flavor.as_str().to_owned(), species_id);
        Self::remembered(&self.negative_pets, &key) || Self::remembered(&self.errors_pets, &key)
    }

    /// Returns the pet-species catalog record for `(flavor, species_id)`,
    /// fetching it from the pet API on first sight (same negative-cache and
    /// error-window policy as items).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` when the catalog read/write fails or
    /// the pet API lookup fails outside the error window.
    pub fn ensure_pet(
        &self,
        flavor: &Flavor,
        species_id: i64,
    ) -> Result<Option<PetNameRecord>, StoreError> {
        let cached = self.catalog.pet_by_species(flavor, species_id)?;
        if cached.is_some() {
            return Ok(cached);
        }
        let key = (flavor.as_str().to_owned(), species_id);
        if Self::remembered(&self.negative_pets, &key) || Self::remembered(&self.errors_pets, &key)
        {
            return Ok(cached);
        }
        match self.info.describe_pet(flavor, species_id) {
            Ok(Some(record)) => {
                self.catalog.upsert_pets(flavor, std::slice::from_ref(&record))?;
                Ok(Some(record))
            }
            Ok(None) => {
                Self::remember(&self.negative_pets, key);
                Ok(cached)
            }
            Err(error) => {
                Self::remember(&self.errors_pets, key);
                Err(StoreError::Backend(error.to_string()))
            }
        }
    }

    /// Locks one of the service's caches and checks freshness. A poisoned
    /// mutex (a panic while holding it) degrades to "cache missed" instead
    /// of failing the request.
    fn remembered(cache: &Mutex<NegativeCache>, key: &(String, i64)) -> bool {
        cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains_fresh(key)
    }

    /// Locks one of the service's caches and remembers the id. Poisoning
    /// degrades to "cache missed" on the next check, never to a failed
    /// request.
    fn remember(cache: &Mutex<NegativeCache>, key: (String, i64)) {
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

    use std::sync::atomic::{
        AtomicI64,
        AtomicUsize,
        Ordering,
    };

    use super::*;
    use crate::error::SourceError;

    #[test]
    fn eviction_drops_the_oldest_and_keeps_the_rest() {
        let key = |v: i64| ("retail".to_string(), v);
        let mut cache = NegativeCache::new(3, NEGATIVE_TTL_SECONDS);
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
        let mut cache = NegativeCache::new(3, NEGATIVE_TTL_SECONDS);
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
        let mut cache = NegativeCache::new(10, NEGATIVE_TTL_SECONDS);
        cache.remember(("retail".to_string(), 42));
        assert!(cache.contains_fresh(&("retail".to_string(), 42)));
        assert!(!cache.contains_fresh(&("forever".to_string(), 42)), "other flavor misses");
    }

    // ------------------------------------------------------------ fakes

    /// In-memory catalog tables: items and pets keyed per flavor, so tests
    /// can pre-seed rows the service must respect.
    struct FakeCatalog {
        items: Mutex<HashMap<(String, i64), NameRecord>>,
        pets: Mutex<HashMap<(String, i64), PetNameRecord>>,
    }

    impl FakeCatalog {
        fn new() -> Self {
            Self { items: Mutex::new(HashMap::new()), pets: Mutex::new(HashMap::new()) }
        }

        fn seed_item(&self, flavor: &str, record: NameRecord) {
            self.items
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((flavor.to_string(), record.item_id), record);
        }
    }

    impl ItemCatalogRepository for FakeCatalog {
        fn upsert(&self, flavor: &Flavor, records: &[NameRecord]) -> Result<(), StoreError> {
            let mut items = self.items.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            for record in records {
                items.insert((flavor.as_str().to_string(), record.item_id), record.clone());
            }
            Ok(())
        }

        fn upsert_pets(
            &self,
            flavor: &Flavor,
            records: &[PetNameRecord],
        ) -> Result<(), StoreError> {
            let mut pets = self.pets.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            for record in records {
                pets.insert((flavor.as_str().to_string(), record.species_id), record.clone());
            }
            Ok(())
        }

        fn search(
            &self,
            _query: &str,
            _limit: usize,
            _offset: usize,
        ) -> Result<Vec<SearchHit>, StoreError> {
            Ok(Vec::new())
        }

        fn by_id(&self, flavor: &Flavor, item_id: i64) -> Result<Option<NameRecord>, StoreError> {
            Ok(self
                .items
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&(flavor.as_str().to_string(), item_id))
                .cloned())
        }

        fn pet_by_species(
            &self,
            flavor: &Flavor,
            species_id: i64,
        ) -> Result<Option<PetNameRecord>, StoreError> {
            Ok(self
                .pets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&(flavor.as_str().to_string(), species_id))
                .cloned())
        }

        fn browse(
            &self,
            _after: Option<&BrowseCursor>,
            _limit: usize,
        ) -> Result<(Vec<SearchHit>, bool), StoreError> {
            Ok((Vec::new(), false))
        }

        fn counts(&self) -> Result<CatalogCounts, StoreError> {
            Ok(CatalogCounts {
                items: self.items.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
                    as u64,
                pets: self.pets.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
                    as u64,
            })
        }
    }

    /// Scripted per-kind source answers with a call counter.
    struct FakeInfo {
        item: FakeAnswer,
        pet: FakeAnswer,
        calls: AtomicUsize,
    }

    enum FakeAnswer {
        Named,
        Missing,
        Down,
    }

    impl FakeInfo {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ItemInfoPort for FakeInfo {
        fn describe(
            &self,
            _flavor: &Flavor,
            item_id: i64,
        ) -> Result<Option<NameRecord>, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.item {
                FakeAnswer::Named => Ok(Some(NameRecord {
                    item_id,
                    name: "Fresh Name".into(),
                    item_class: Some("Misc".into()),
                    item_subclass: None,
                    inventory_slot: None,
                    quality: Some("common".into()),
                    icon: None,
                    sell_price: None,
                    fetched_at: 0,
                })),
                FakeAnswer::Missing => Ok(None),
                FakeAnswer::Down => Err(SourceError::Http("down in test".into())),
            }
        }

        fn describe_pet(
            &self,
            _flavor: &Flavor,
            species_id: i64,
        ) -> Result<Option<PetNameRecord>, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.pet {
                FakeAnswer::Named => {
                    Ok(Some(PetNameRecord { species_id, name: "Fresh Pet".into() }))
                }
                FakeAnswer::Missing => Ok(None),
                FakeAnswer::Down => Err(SourceError::Http("down in test".into())),
            }
        }
    }

    /// Adjustable clock for the sparse-refresh window math.
    struct FakeClock(AtomicI64);

    impl ClockPort for FakeClock {
        fn now(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn service(
        catalog: Arc<FakeCatalog>,
        info: Arc<FakeInfo>,
        clock: Arc<FakeClock>,
    ) -> CatalogService {
        CatalogService::new(catalog, info, clock)
    }

    fn sparse_record(item_id: i64, fetched_at: i64) -> NameRecord {
        NameRecord {
            item_id,
            name: "Stale Name".into(),
            item_class: None,
            item_subclass: None,
            inventory_slot: None,
            quality: Some("common".into()),
            icon: None,
            sell_price: None,
            fetched_at,
        }
    }

    fn down_item_info() -> Arc<FakeInfo> {
        Arc::new(FakeInfo {
            item: FakeAnswer::Down,
            pet: FakeAnswer::Missing,
            calls: AtomicUsize::new(0),
        })
    }

    /// A stale sparse row keeps its stored name when the source lost the
    /// item or is down: "cached forever" must not silently demote to a raw
    /// label just because the enrichment refresh came back empty.
    #[test]
    fn stale_sparse_rows_keep_their_name_when_the_source_disagrees() {
        let catalog = Arc::new(FakeCatalog::new());
        catalog.seed_item("retail", sparse_record(5, 1_000_000));
        let clock = Arc::new(FakeClock(AtomicI64::new(1_000_000 + SPARSE_REFRESH_SECONDS + 1)));
        let info = Arc::new(FakeInfo {
            item: FakeAnswer::Missing,
            pet: FakeAnswer::Missing,
            calls: AtomicUsize::new(0),
        });
        let resolved = service(catalog.clone(), info.clone(), clock.clone())
            .ensure(&Flavor::default(), 5)
            .expect("ensure");
        assert_eq!(
            resolved.map(|r| r.name).as_deref(),
            Some("Stale Name"),
            "a stored name survives a source-side disappearance"
        );
        assert_eq!(info.calls(), 1, "the stale row did trigger one refresh attempt");

        let resolved = service(catalog, down_item_info(), clock)
            .ensure(&Flavor::default(), 5)
            .expect("ensure");
        assert_eq!(
            resolved.map(|r| r.name).as_deref(),
            Some("Stale Name"),
            "a stored name also survives a source outage"
        );
    }

    /// A fresh (non-sparse) row answers from the catalog without touching
    /// the source at all.
    #[test]
    fn fresh_rows_never_touch_the_source() {
        let catalog = Arc::new(FakeCatalog::new());
        catalog.seed_item(
            "retail",
            NameRecord {
                item_id: 7,
                name: "Fresh Name".into(),
                item_class: None,
                item_subclass: Some("Cloth".into()),
                inventory_slot: None,
                quality: None,
                icon: None,
                sell_price: None,
                fetched_at: 0,
            },
        );
        let info = down_item_info();
        let resolved = service(catalog, info.clone(), Arc::new(FakeClock(AtomicI64::new(0))))
            .ensure(&Flavor::default(), 7)
            .expect("ensure");
        assert_eq!(resolved.map(|r| r.name).as_deref(), Some("Fresh Name"));
        assert_eq!(info.calls(), 0, "a fresh row is a pure local read");
    }

    /// A failed lookup is remembered briefly: repeated calls inside the
    /// window neither hit the source again nor fail renders, and the
    /// warmer's deferred check sees it.
    #[test]
    fn source_failures_are_remembered_briefly() {
        let info = down_item_info();
        let service = service(
            Arc::new(FakeCatalog::new()),
            info.clone(),
            Arc::new(FakeClock(AtomicI64::new(0))),
        );

        assert!(service.ensure(&Flavor::default(), 9).is_err(), "the outage surfaces once");
        assert_eq!(info.calls(), 1);
        assert_eq!(
            service.ensure(&Flavor::default(), 9).ok(),
            Some(None),
            "inside the error window renders degrade to placeholders"
        );
        assert_eq!(info.calls(), 1, "no second source hit inside the window");
        assert!(service.deferred(&Flavor::default(), 9), "the warmer skips error-cached ids");
        assert!(!service.deferred(&Flavor::default(), 10), "other ids are unaffected");
    }

    /// The warmer's known-checks split items and pets: species ids live in
    /// their own namespace, so a cataloged ITEM with the same numeric id
    /// must not make a pet look known (the regression behind pets silently
    /// never background-warming).
    #[test]
    fn known_and_deferred_split_items_from_pets() {
        let catalog = Arc::new(FakeCatalog::new());
        catalog.seed_item("retail", sparse_record(264, 0));
        let service = service(
            catalog,
            Arc::new(FakeInfo {
                item: FakeAnswer::Missing,
                pet: FakeAnswer::Named,
                calls: AtomicUsize::new(0),
            }),
            Arc::new(FakeClock(AtomicI64::new(0))),
        );

        assert_eq!(service.known(&Flavor::default(), 264), Ok(true), "the item is cataloged");
        assert_eq!(
            service.known_pet(&Flavor::default(), 264),
            Ok(false),
            "the same numeric id is NOT known as a pet"
        );

        let warmed = service.ensure_pet(&Flavor::default(), 264).expect("ensure_pet");
        assert_eq!(warmed.map(|r| r.name).as_deref(), Some("Fresh Pet"));
        assert_eq!(service.known_pet(&Flavor::default(), 264), Ok(true));
    }
}

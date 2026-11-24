//! Request-time caches for the read paths that would otherwise do
//! whole-market work on every call: the watchlist-only Lua feed and
//! `/api/history`'s day rows.
//!
//! Both hold derived data that only changes when ingest writes new folds or
//! the watchlist changes, so a time-to-live is the right shape: the first
//! request after the interval pays the store read, every other request is
//! served from memory. The TTLs follow the cadence the scheduler already
//! keeps for prepared data — `server.feed_publish_secs` for the slim feed
//! (it is then never staler than the published full feed) and
//! [`SCAN_TTL_SECONDS`] for history rows (the prepared scans' cadence).
//!
//! A cold or expired key is rebuilt by the request that hits it. Concurrent
//! misses may rebuild in parallel and race to insert, which is harmless (same
//! inputs, same output) — the store's own connection mutex already
//! serializes the underlying reads.

use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        Arc,
        Mutex,
        PoisonError,
    },
};

use amtracker_core::{
    domain::{
        Category,
        ItemDayRow,
        TargetId,
    },
    error::StoreError,
    ports::ClockPort,
    services::opportunity::SCAN_TTL_SECONDS,
};

/// Longest window `/api/history` serves (`days` clamps to `1..=30`). The
/// cached rows always cover the whole window, so every request shape is a
/// filter over one whole-market read per category.
pub const HISTORY_MAX_DAYS: i64 = 30;

/// The last-rendered slim feed plus the watchlist revision it was built from.
struct SlimFeed {
    revision: u64,
    bytes: Arc<Vec<u8>>,
}

/// Request-time caches over the shared clock; see the module docs.
pub struct ResponseCache {
    slim: TtlCache<TargetId, SlimFeed>,
    history: TtlCache<(TargetId, Category), Vec<ItemDayRow>>,
}

impl ResponseCache {
    /// Wires the caches to the shared clock. `slim_ttl_secs` is the feed
    /// publish interval (`server.feed_publish_secs`).
    pub fn new(clock: Arc<dyn ClockPort>, slim_ttl_secs: i64) -> Self {
        Self {
            slim: TtlCache::new(Arc::clone(&clock), slim_ttl_secs),
            history: TtlCache::new(clock, SCAN_TTL_SECONDS),
        }
    }

    /// The rendered slim Lua feed for `target`: the cached copy while it is
    /// younger than the publish interval AND was rendered from the current
    /// watchlist `revision`; anything else runs `build`. Keying the entry to
    /// the revision (rather than invalidating it from the mutation handlers)
    /// means a watchlist edit shows on the very next request no matter which
    /// code path changed the list.
    ///
    /// # Errors
    ///
    /// Returns the error of `build` when the feed had to be rebuilt and the
    /// export or render failed; nothing is cached then.
    pub fn slim_feed(
        &self,
        target: &TargetId,
        revision: u64,
        build: impl FnOnce() -> Result<Vec<u8>, StoreError>,
    ) -> Result<Arc<Vec<u8>>, StoreError> {
        let entry = self.slim.get_or_insert_with(
            target.clone(),
            |feed| feed.revision == revision,
            || build().map(|bytes| SlimFeed { revision, bytes: Arc::new(bytes) }),
        )?;
        Ok(Arc::clone(&entry.bytes))
    }

    /// The target's day rows of one `category` covering the whole servable
    /// window (`[today - HISTORY_MAX_DAYS + 1, ..]` — `build` reads exactly
    /// that); callers filter by their own `since` day. At most one
    /// whole-market read per category per TTL.
    ///
    /// # Errors
    ///
    /// Returns the error of `build` when the rows had to be re-read and the
    /// store failed; nothing is cached then.
    pub fn history_rows(
        &self,
        target: &TargetId,
        category: Category,
        build: impl FnOnce() -> Result<Vec<ItemDayRow>, StoreError>,
    ) -> Result<Arc<Vec<ItemDayRow>>, StoreError> {
        self.history.get_or_insert_with((target.clone(), category), |_| true, build)
    }
}

/// A small map of derived values stamped with the second they were computed.
/// Unbounded by design: both caches key on configured targets (and
/// categories), so the entry count is a handful.
struct TtlCache<K, V> {
    clock: Arc<dyn ClockPort>,
    ttl_secs: i64,
    entries: Mutex<HashMap<K, (i64, Arc<V>)>>,
}

impl<K: Eq + Hash, V> TtlCache<K, V> {
    fn new(clock: Arc<dyn ClockPort>, ttl_secs: i64) -> Self {
        Self { clock, ttl_secs, entries: Mutex::new(HashMap::new()) }
    }

    /// The entry for `key` while it is younger than the TTL. A poisoned lock
    /// (a panic while holding it) recovers the map: the entries are derived
    /// state, so serving them is always safe.
    fn fresh(&self, key: &K) -> Option<Arc<V>> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let (at, value) = entries.get(key)?;
        (self.clock.now().saturating_sub(*at) < self.ttl_secs).then(|| Arc::clone(value))
    }

    /// Serves the fresh entry for `key` when `still_valid` accepts it; any
    /// other outcome runs `build` and stores its value as the new entry.
    fn get_or_insert_with<E>(
        &self,
        key: K,
        still_valid: impl Fn(&V) -> bool,
        build: impl FnOnce() -> Result<V, E>,
    ) -> Result<Arc<V>, E> {
        if let Some(value) = self.fresh(&key).filter(|value| still_valid(value)) {
            return Ok(value);
        }
        let value = Arc::new(build()?);
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, (self.clock.now(), Arc::clone(&value)));
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect/panic: fixture-driven, deterministic
    // inputs.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::atomic::{
        AtomicUsize,
        Ordering,
    };

    use amtracker_core::domain::ItemRef;

    use super::*;

    /// Clock the tests move by hand: the TTLs here are minutes, far too long
    /// to wait out for real.
    struct SettableClock(Mutex<i64>);

    impl ClockPort for SettableClock {
        fn now(&self) -> i64 {
            *self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl SettableClock {
        fn set(&self, now: i64) {
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) = now;
        }
    }

    fn one_row(item_id: i64) -> Vec<ItemDayRow> {
        vec![ItemDayRow {
            item: ItemRef::Item { item_id },
            category: Category::Item,
            day: 20_000,
            listing_count: 1,
            qty_sum: 1,
            min: 10,
            min_qty: 1,
            p05: 10,
            median: 10,
            qty_median: 10,
            mad: 0,
        }]
    }

    #[test]
    fn fresh_entries_are_served_without_rebuilding() {
        let cache = ResponseCache::new(Arc::new(SettableClock(Mutex::new(1_000))), 300);
        let target = TargetId("t1".into());
        let builds = AtomicUsize::new(0);
        let build = |item_id: i64| -> Result<Vec<ItemDayRow>, StoreError> {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok(one_row(item_id))
        };

        let first = cache.history_rows(&target, Category::Item, || build(1)).expect("rows");
        let second = cache.history_rows(&target, Category::Item, || build(2)).expect("rows");
        assert_eq!(builds.load(Ordering::SeqCst), 1, "the second read is a cache hit");
        assert!(Arc::ptr_eq(&first, &second), "hits share one allocation");
        // Another category is its own entry.
        cache
            .history_rows(&target, Category::Commodity, || {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            })
            .expect("rows");
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn expired_entries_are_rebuilt() {
        let settable = Arc::new(SettableClock(Mutex::new(1_000)));
        let clock: Arc<dyn ClockPort> = settable.clone();
        let cache = ResponseCache::new(Arc::clone(&clock), 300);
        let target = TargetId("t1".into());

        cache.history_rows(&target, Category::Item, || Ok(one_row(7))).expect("rows");
        // One second short of the scan TTL: still served from the cache.
        settable.set(1_000 + SCAN_TTL_SECONDS - 1);
        let aged = cache.history_rows(&target, Category::Item, || Ok(Vec::new())).expect("rows");
        assert_eq!(aged.len(), 1, "still the cached rows");
        // At the TTL the next request pays the read again.
        settable.set(1_000 + SCAN_TTL_SECONDS);
        let rebuilt = cache.history_rows(&target, Category::Item, || Ok(Vec::new())).expect("rows");
        assert!(rebuilt.is_empty(), "the expired entry was rebuilt");
    }

    #[test]
    fn a_new_watchlist_revision_invalidates_a_fresh_slim_feed() {
        let cache = ResponseCache::new(Arc::new(SettableClock(Mutex::new(1_000))), 300);
        let target = TargetId("t1".into());

        cache.slim_feed(&target, 1, || Ok(b"revision one".to_vec())).expect("slim");
        let hit = cache.slim_feed(&target, 1, || Ok(b"rebuilt".to_vec())).expect("slim");
        assert_eq!(hit.as_slice(), b"revision one", "same revision: served from the cache");

        let bumped = cache.slim_feed(&target, 2, || Ok(b"revision two".to_vec())).expect("slim");
        assert_eq!(bumped.as_slice(), b"revision two", "a new revision rebuilds at once");
        // And the rebuilt entry is the one cached now.
        let again = cache.slim_feed(&target, 2, || Ok(b"rebuilt".to_vec())).expect("slim");
        assert_eq!(again.as_slice(), b"revision two");
    }

    #[test]
    fn a_failed_build_leaves_no_entry() {
        let cache = ResponseCache::new(Arc::new(SettableClock(Mutex::new(1_000))), 300);
        let target = TargetId("t1".into());

        let failed =
            cache.slim_feed(&target, 1, || Err(StoreError::Backend("feed export failed".into())));
        assert!(failed.is_err(), "the build error reaches the caller");
        // Nothing was stored: the next request builds again.
        let built = cache.slim_feed(&target, 1, || Ok(b"second try".to_vec())).expect("slim");
        assert_eq!(built.as_slice(), b"second try");
    }

    #[test]
    fn slim_feed_follows_the_publish_interval() {
        let settable = Arc::new(SettableClock(Mutex::new(1_000)));
        let clock: Arc<dyn ClockPort> = settable.clone();
        let cache = ResponseCache::new(Arc::clone(&clock), 60);
        let target = TargetId("t1".into());

        cache.slim_feed(&target, 1, || Ok(b"first".to_vec())).expect("slim");
        settable.set(1_059);
        let aged = cache.slim_feed(&target, 1, || Ok(b"rebuilt".to_vec())).expect("slim");
        assert_eq!(aged.as_slice(), b"first", "one second short of the interval: cached");
        settable.set(1_060);
        let rebuilt = cache.slim_feed(&target, 1, || Ok(b"rebuilt".to_vec())).expect("slim");
        assert_eq!(rebuilt.as_slice(), b"rebuilt", "at the interval: rebuilt");
    }
}

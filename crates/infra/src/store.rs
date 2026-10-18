//! SQLite repositories implementing `SnapshotRepository` and
//! `PriceHistoryRepository` over a single database file (WAL).
//!
//! Durability model (plan §3 / D3): snapshot metadata, item-day folds and the
//! item catalog are permanent; raw listing rows and archived payload files are
//! a bounded working set (`prune_raw_before`).

use std::{
    path::{
        Path,
        PathBuf,
    },
    sync::Mutex,
};

use amtracker_core::{
    domain::{
        utc_day,
        BrowseCursor,
        CatalogCounts,
        Flavor,
        ItemDayRow,
        ItemRef,
        MetricBlock,
        NameRecord,
        NewSnapshot,
        NewWatchlistEntry,
        NormalizedListing,
        PayloadKind,
        PetNameRecord,
        SaleCredit,
        SearchHit,
        SnapshotMeta,
        SnapshotStatus,
        Span,
        TargetId,
        WatchlistEntry,
    },
    error::StoreError,
    fold::DayFold,
    ports::{
        AlertEventRepository,
        AlertRuleRepository,
        ClockPort,
        ItemCatalogRepository,
        MetricsCacheRepository,
        PriceHistoryRepository,
        SalesRepository,
        SnapshotRepository,
        WatchlistRepository,
    },
};
use rusqlite::Connection;

/// Schema version of the store. Version 2 scoped sightings per payload kind
/// (commodity and item auction ids are separate namespaces); version 1 files
/// cannot be migrated faithfully (old rows carry no kind) and are refused.
/// Version 3 added the `pet_catalog` table (a fresh `CREATE TABLE IF NOT
/// EXISTS`, so v2 files upgrade in place without a data migration).
/// Version 5 added the per-reason drop breakdown column (nullable ALTER).
/// Version 6 re-keyed the catalog tables per game flavor and added the
/// enriched name fields; pre-v6 rows carry over as `retail` with NULL
/// enrichment and are upgraded lazily (see `CatalogService`).
const SCHEMA_VERSION: i64 = 6;

/// One-shot v5 → v6 catalog rebuild: both catalog tables move from bare-id
/// primary keys to `(flavor, id)` keys and gain the enrichment columns.
/// Pre-v6 rows carry over as `retail` with NULL enrichment; enrichment
/// happens lazily on later lookups (see `CatalogService`'s sparse-row
/// refresh). Wrapped in an explicit transaction: a half-rebuilt catalog
/// would lose rows.
const CATALOG_V6_MIGRATION: &str = "
BEGIN IMMEDIATE;
CREATE TABLE item_catalog_v6 (
    flavor TEXT NOT NULL DEFAULT 'retail',
    item_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    item_class TEXT,
    item_subclass TEXT,
    inventory_slot TEXT,
    quality TEXT,
    icon TEXT,
    sell_price INTEGER,
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (flavor, item_id)
);
INSERT INTO item_catalog_v6
    (flavor, item_id, name, item_class, item_subclass, inventory_slot, quality, icon, sell_price, fetched_at)
    SELECT 'retail', item_id, name, item_class, NULL, NULL, quality, NULL, NULL, fetched_at
    FROM item_catalog;
DROP TABLE item_catalog;
ALTER TABLE item_catalog_v6 RENAME TO item_catalog;
CREATE INDEX IF NOT EXISTS idx_item_catalog_name ON item_catalog(name);

CREATE TABLE pet_catalog_v6 (
    flavor TEXT NOT NULL DEFAULT 'retail',
    species_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (flavor, species_id)
);
INSERT INTO pet_catalog_v6 (flavor, species_id, name, fetched_at)
    SELECT 'retail', species_id, name, fetched_at FROM pet_catalog;
DROP TABLE pet_catalog;
ALTER TABLE pet_catalog_v6 RENAME TO pet_catalog;
CREATE INDEX IF NOT EXISTS idx_pet_catalog_name ON pet_catalog(name);
COMMIT;
";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS snapshots (
    id INTEGER PRIMARY KEY,
    target_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    source_last_modified INTEGER,
    sha256 TEXT NOT NULL,
    archive_path TEXT,
    entry_count INTEGER NOT NULL,
    dropped_count INTEGER NOT NULL,
    drop_reasons TEXT,
    status TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_snapshots_target ON snapshots(target_id, kind, id DESC);

CREATE TABLE IF NOT EXISTS item_ref (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    item_id INTEGER NOT NULL,
    pet_species INTEGER NOT NULL DEFAULT 0,
    pet_level INTEGER NOT NULL DEFAULT 0,
    pet_quality INTEGER NOT NULL DEFAULT 0,
    pet_breed INTEGER NOT NULL DEFAULT 0,
    UNIQUE (kind, item_id, pet_species, pet_level, pet_quality, pet_breed)
);

CREATE TABLE IF NOT EXISTS listing (
    snapshot_id INTEGER NOT NULL,
    target_id TEXT NOT NULL,
    day INTEGER NOT NULL,
    auction_id INTEGER NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    quantity INTEGER NOT NULL,
    unit_price INTEGER NOT NULL,
    bid INTEGER,
    time_left TEXT NOT NULL,
    context INTEGER,
    bonus_lists TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS idx_listing_day ON listing(target_id, day);
CREATE INDEX IF NOT EXISTS idx_listing_item ON listing(target_id, item_ref_id, snapshot_id);

CREATE TABLE IF NOT EXISTS item_day (
    target_id TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    day INTEGER NOT NULL,
    fold_json TEXT NOT NULL,
    PRIMARY KEY (target_id, item_ref_id, day)
);

CREATE TABLE IF NOT EXISTS metrics_cache (
    target_id TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    span TEXT NOT NULL,
    computed_at INTEGER NOT NULL,
    block_json TEXT NOT NULL,
    PRIMARY KEY (target_id, item_ref_id, span)
);

CREATE TABLE IF NOT EXISTS watchlist (
    target_id TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    note TEXT,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (target_id, item_ref_id)
);

CREATE TABLE IF NOT EXISTS item_catalog (
    flavor TEXT NOT NULL DEFAULT 'retail',
    item_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    item_class TEXT,
    item_subclass TEXT,
    inventory_slot TEXT,
    quality TEXT,
    icon TEXT,
    sell_price INTEGER,
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (flavor, item_id)
);
CREATE INDEX IF NOT EXISTS idx_item_catalog_name ON item_catalog(name);

CREATE TABLE IF NOT EXISTS pet_catalog (
    flavor TEXT NOT NULL DEFAULT 'retail',
    species_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (flavor, species_id)
);
CREATE INDEX IF NOT EXISTS idx_pet_catalog_name ON pet_catalog(name);

CREATE TABLE IF NOT EXISTS sight (
    target_id TEXT NOT NULL,
    auction_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    first_seen_snapshot INTEGER NOT NULL,
    last_seen_snapshot INTEGER NOT NULL,
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    band TEXT NOT NULL,
    quantity INTEGER NOT NULL,
    closed_at INTEGER,
    credited INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (target_id, auction_id, kind)
);
CREATE INDEX IF NOT EXISTS idx_sight_open ON sight(target_id, kind, closed_at);

CREATE TABLE IF NOT EXISTS sales (
    target_id TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    day INTEGER NOT NULL,
    listings INTEGER NOT NULL DEFAULT 0,
    closed INTEGER NOT NULL DEFAULT 0,
    credited INTEGER NOT NULL DEFAULT 0,
    qty_sold INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (target_id, item_ref_id, day)
);

CREATE TABLE IF NOT EXISTS alert_rule (
    id INTEGER PRIMARY KEY,
    target_id TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    condition_json TEXT NOT NULL,
    note TEXT,
    created_at INTEGER NOT NULL,
    last_fired_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_alert_rule_target ON alert_rule(target_id, id);

CREATE TABLE IF NOT EXISTS alert_event (
    id INTEGER PRIMARY KEY,
    -- No FK to alert_rule: events are the audit log and outlive their rule's
    -- removal (rule_id stays as the historical reference).
    rule_id INTEGER NOT NULL,
    target_id TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    condition_json TEXT NOT NULL,
    observed REAL,
    message TEXT NOT NULL,
    fired_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_alert_event_target ON alert_event(target_id, id DESC);
";

/// Single-connection SQLite store (WAL journal) implementing the snapshot,
/// sales, metrics-cache, watchlist, catalog and price-history repositories.
/// All access is serialized through one mutex; archive payloads land under
/// `archive_dir`.
pub struct SqliteStore {
    conn: Mutex<Connection>,
    archive_dir: PathBuf,
    clock: std::sync::Arc<dyn ClockPort>,
}

fn backend<E: std::fmt::Display>(context: &str, error: E) -> StoreError {
    StoreError::Backend(format!("{context}: {error}"))
}

/// Process-unique suffix for temporary write targets: pid plus an in-process
/// counter. Two writers (serve + a CLI one-off against the same volume) must
/// never interleave bytes into the same `.tmp` path before the rename.
#[allow(clippy::cast_possible_truncation)] // pid fits u32 on every supported OS
pub(crate) fn unique_tmp_suffix() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{seq}", std::process::id())
}

impl SqliteStore {
    /// Opens (creating if needed) the database and applies the schema.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] when the data or archive directories
    /// cannot be created, the database cannot be opened or configured (WAL,
    /// synchronous, foreign-keys pragmas), the schema batch cannot be
    /// applied, or a pre-kind (schema v1) database is opened — those cannot
    /// be migrated faithfully and must be rebuilt.
    pub fn open(
        database: &Path,
        archive_dir: &Path,
        clock: std::sync::Arc<dyn ClockPort>,
    ) -> Result<Self, StoreError> {
        if let Some(parent) = database.parent() {
            std::fs::create_dir_all(parent).map_err(|e| backend("create data dir", e))?;
        }
        std::fs::create_dir_all(archive_dir).map_err(|e| backend("create archive dir", e))?;
        let conn = Connection::open(database).map_err(|e| backend("open sqlite", e))?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| backend("journal_mode", e))?;
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(|e| backend("synchronous", e))?;
        // Operators run CLI one-offs against a live `serve` (documented in
        // the README); a 5 s busy wait turns those races into waits instead
        // of `SQLITE_BUSY` errors.
        conn.pragma_update(None, "busy_timeout", 5_000).map_err(|e| backend("busy_timeout", e))?;
        // The schema's `REFERENCES item_ref(id)` clauses are decoration
        // without this: SQLite leaves foreign keys unenforced by default,
        // so a write bug would silently orphan sale/sight/fold rows.
        conn.pragma_update(None, "foreign_keys", "ON").map_err(|e| backend("foreign_keys", e))?;
        let version = Self::read_schema_version(&conn)?;
        Self::check_schema_version(&conn, version)?;
        conn.execute_batch(SCHEMA).map_err(|e| backend("schema", e))?;
        // v5 added the per-reason drop breakdown column. Older files get an
        // in-place ALTER guarded by the version gate and a column check, so
        // both fresh v5 files (column already present) and v4 files (column
        // missing) converge; the column is nullable — pre-v5 rows read as
        // "no breakdown recorded".
        if version < SCHEMA_VERSION {
            let has_column = conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM pragma_table_info('snapshots') WHERE name = 'drop_reasons')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map(|flag| flag == 1)
                .map_err(|e| backend("inspect snapshots schema", e))?;
            if !has_column {
                conn.execute_batch("ALTER TABLE snapshots ADD COLUMN drop_reasons TEXT")
                    .map_err(|e| backend("migrate drop_reasons", e))?;
            }
            // v6 re-keyed the catalog tables per game flavor (composite
            // `(flavor, id)` primary keys, new enrichment columns). A PK
            // change cannot ALTER in place, so both tables are rebuilt in one
            // transaction and pre-v6 rows carry over as `retail` with NULL
            // enrichment (upgraded lazily by the catalog service).
            let catalog_has_flavor = conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM pragma_table_info('item_catalog') WHERE name = 'flavor')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map(|flag| flag == 1)
                .map_err(|e| backend("inspect item_catalog schema", e))?;
            if !catalog_has_flavor {
                conn.execute_batch(CATALOG_V6_MIGRATION)
                    .map_err(|e| backend("migrate catalog to flavor keys", e))?;
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|e| backend("user_version", e))?;
        }
        Ok(Self { conn: Mutex::new(conn), archive_dir: archive_dir.to_path_buf(), clock })
    }

    /// Reads the stored schema version (`PRAGMA user_version`).
    fn read_schema_version(conn: &Connection) -> Result<i64, StoreError> {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| backend("user_version", e))
    }

    /// Refuses schema-v1 databases (a `sight` table without the `kind`
    /// column): old sighting rows cannot be assigned a payload kind, so a
    /// silent migration would fabricate sell-through history. Fresh files and
    /// current v2 files pass through; files from a FUTURE version also pass
    /// (the downgrade-guard in `open` keeps their marker intact).
    fn check_schema_version(conn: &Connection, version: i64) -> Result<(), StoreError> {
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        let legacy = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sight')
                     AND NOT EXISTS (SELECT 1 FROM pragma_table_info('sight') WHERE name = 'kind')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|flag| flag == 1)
            .map_err(|e| backend("inspect sight schema", e))?;
        if legacy {
            return Err(StoreError::Backend(
                "database schema is outdated (pre-v2 sightings without a payload kind); \
                 the file cannot be migrated faithfully - delete the database file and let \
                 the tracker rebuild it (fold history regenerates within a day)"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Lock the connection. A poisoned mutex (another thread panicked while
    /// holding it) is recovered rather than propagated: the underlying
    /// connection stays consistent because every multi-statement operation
    /// runs in a transaction that is either committed or dropped.
    fn open_transaction(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn archive_payload(
        &self,
        target: &TargetId,
        kind: PayloadKind,
        sha: &str,
        raw: &[u8],
    ) -> Result<String, StoreError> {
        let relative =
            PathBuf::from(target.as_str()).join(kind.as_str()).join(format!("{sha}.json"));
        let full = self.archive_dir.join(&relative);
        if !full.exists() {
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).map_err(|e| backend("archive dir", e))?;
            }
            // Unique tmp name per call: two processes (serve + a CLI one-off)
            // archiving the same sha concurrently must not interleave writes
            // into one tmp file.
            let tmp = full.with_extension(format!("tmp.{}", unique_tmp_suffix()));
            std::fs::write(&tmp, raw).map_err(|e| backend("archive write", e))?;
            std::fs::rename(&tmp, &full).map_err(|e| backend("archive rename", e))?;
        }
        relative
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| StoreError::Backend("archive path is not valid utf-8".into()))
    }

    /// Writes a consistent online backup of the database to `path` using
    /// `VACUUM INTO` (a read transaction over the live store; safe while
    /// `serve` runs). The parent directory is created if needed. Fails when
    /// the file already exists — callers rotate first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] when the parent directory cannot be
    /// created or the `VACUUM INTO` statement fails (including an existing
    /// target file).
    pub fn backup_to(&self, path: &Path) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| backend("create backup dir", e))?;
        }
        // `VACUUM INTO` takes a filename literal, not a bound parameter, so
        // the path is inlined as a quoted SQL string (single quotes doubled).
        let sql = format!("VACUUM INTO '{}'", path.to_string_lossy().replace('\'', "''"));
        self.open_transaction().execute_batch(&sql).map_err(|e| backend("backup", e))?;
        Ok(())
    }
}

impl SnapshotRepository for SqliteStore {
    fn record(
        &self,
        snapshot: &NewSnapshot,
        listings: &[NormalizedListing],
        raw: &[u8],
    ) -> Result<i64, StoreError> {
        let sha = sha256_hex(raw);
        let archive_path = self.archive_payload(&snapshot.target, snapshot.kind, &sha, raw)?;
        let day = utc_day(snapshot.fetched_at);

        let conn = self.open_transaction();
        let tx = conn.unchecked_transaction().map_err(|e| backend("begin", e))?;
        let drop_reasons_json = if snapshot.dropped_reasons.is_empty() {
            None
        } else {
            Some(
                serde_json::to_string(&snapshot.dropped_reasons)
                    .map_err(|e| backend("serialize drop reasons", e))?,
            )
        };
        tx.execute(
            "INSERT INTO snapshots (target_id, kind, fetched_at, source_last_modified, sha256, archive_path, entry_count, dropped_count, drop_reasons, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                snapshot.target.as_str(),
                snapshot.kind.as_str(),
                snapshot.fetched_at,
                snapshot.source_last_modified,
                sha,
                archive_path,
                // Entry counts are far below `i64::MAX`; saturation is
                // unreachable and preferred over wrapping.
                i64::try_from(snapshot.entry_count).unwrap_or(i64::MAX),
                i64::try_from(snapshot.dropped_count).unwrap_or(i64::MAX),
                drop_reasons_json,
                snapshot.status.as_str(),
            ],
        )
        .map_err(|e| backend("insert snapshot", e))?;
        let snapshot_id = tx.last_insert_rowid();

        let mut item_ref_ids: HashMap<&ItemRef, i64> = HashMap::new();
        let mut insert_listing = tx
            .prepare(
                "INSERT INTO listing (snapshot_id, target_id, day, auction_id, item_ref_id, quantity, unit_price, bid, time_left, context, bonus_lists)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )
            .map_err(|e| backend("prepare listing", e))?;
        for listing in listings {
            let item_ref_id = if let Some(id) = item_ref_ids.get(&listing.item_ref) {
                *id
            } else {
                let id = get_or_create_item_ref(&tx, &listing.item_ref)?;
                item_ref_ids.insert(&listing.item_ref, id);
                id
            };
            insert_listing
                .execute(rusqlite::params![
                    snapshot_id,
                    snapshot.target.as_str(),
                    day,
                    listing.auction_id,
                    item_ref_id,
                    listing.quantity,
                    listing.unit_price,
                    listing.bid,
                    listing.time_left.as_str(),
                    listing.context,
                    serde_json::to_string(&listing.bonus_lists)
                        .map_err(|e| backend("bonus_lists", e))?,
                ])
                .map_err(|e| backend("insert listing", e))?;
        }
        drop(insert_listing);
        tx.commit().map_err(|e| backend("commit", e))?;
        Ok(snapshot_id)
    }

    fn latest(
        &self,
        target: &TargetId,
        kind: PayloadKind,
    ) -> Result<Option<SnapshotMeta>, StoreError> {
        let conn = self.open_transaction();
        let mut statement = conn
            .prepare(
                "SELECT id, target_id, kind, fetched_at, source_last_modified, entry_count, dropped_count, status, drop_reasons
                 FROM snapshots WHERE target_id = ?1 AND kind = ?2
                 ORDER BY id DESC LIMIT 1",
            )
            .map_err(|e| backend("prepare latest", e))?;
        let mut rows = statement
            .query(rusqlite::params![target.as_str(), kind.as_str()])
            .map_err(|e| backend("query latest", e))?;
        if let Some(row) = rows.next().map_err(|e| backend("latest row", e))? {
            // Counts are stored as non-negative i64; a corrupt negative value
            // degrades to 0 instead of wrapping through a sign cast.
            let entry_count =
                u64::try_from(row.get::<_, i64>(5).map_err(|e| backend("entry_count", e))?)
                    .unwrap_or(0);
            let dropped_count =
                u64::try_from(row.get::<_, i64>(6).map_err(|e| backend("dropped_count", e))?)
                    .unwrap_or(0);
            return Ok(Some(SnapshotMeta {
                id: row.get(0).map_err(|e| backend("latest id", e))?,
                target: TargetId(row.get::<_, String>(1).map_err(|e| backend("latest target", e))?),
                kind: parse_kind(&row.get::<_, String>(2).map_err(|e| backend("latest kind", e))?),
                fetched_at: row.get(3).map_err(|e| backend("latest fetched_at", e))?,
                source_last_modified: row.get(4).map_err(|e| backend("last_modified", e))?,
                entry_count,
                dropped_count,
                dropped_reasons: parse_drop_reasons(
                    row.get(8).map_err(|e| backend("latest drop_reasons", e))?,
                )?,
                status: parse_status(&row.get::<_, String>(7).map_err(|e| backend("status", e))?),
            }));
        }
        Ok(None)
    }

    fn list_recent(
        &self,
        target: &TargetId,
        kind: PayloadKind,
        limit: usize,
    ) -> Result<Vec<SnapshotMeta>, StoreError> {
        let conn = self.open_transaction();
        let mut statement = conn
            .prepare(
                "SELECT id, target_id, kind, fetched_at, source_last_modified, entry_count, dropped_count, status, drop_reasons
                 FROM snapshots WHERE target_id = ?1 AND kind = ?2
                 ORDER BY id DESC LIMIT ?3",
            )
            .map_err(|e| backend("prepare list_recent", e))?;
        let mut rows = statement
            .query(rusqlite::params![
                target.as_str(),
                kind.as_str(),
                i64::try_from(limit).unwrap_or(i64::MAX)
            ])
            .map_err(|e| backend("query list_recent", e))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|e| backend("list_recent row", e))? {
            // Same non-negative normalization as `latest`: counts are stored
            // as non-negative i64; corrupt values degrade to 0.
            let entry_count =
                u64::try_from(row.get::<_, i64>(5).map_err(|e| backend("entry_count", e))?)
                    .unwrap_or(0);
            let dropped_count =
                u64::try_from(row.get::<_, i64>(6).map_err(|e| backend("dropped_count", e))?)
                    .unwrap_or(0);
            out.push(SnapshotMeta {
                id: row.get(0).map_err(|e| backend("list_recent id", e))?,
                target: TargetId(
                    row.get::<_, String>(1).map_err(|e| backend("list_recent target", e))?,
                ),
                kind: parse_kind(
                    &row.get::<_, String>(2).map_err(|e| backend("list_recent kind", e))?,
                ),
                fetched_at: row.get(3).map_err(|e| backend("list_recent fetched_at", e))?,
                source_last_modified: row
                    .get(4)
                    .map_err(|e| backend("list_recent last_modified", e))?,
                entry_count,
                dropped_count,
                dropped_reasons: parse_drop_reasons(
                    row.get(8).map_err(|e| backend("list_recent drop_reasons", e))?,
                )?,
                status: parse_status(
                    &row.get::<_, String>(7).map_err(|e| backend("list_recent status", e))?,
                ),
            });
        }
        Ok(out)
    }
}

impl SalesRepository for SqliteStore {
    fn observe(
        &self,
        target: &TargetId,
        kind: PayloadKind,
        listings: &[NormalizedListing],
        snapshot_id: i64,
        at: i64,
    ) -> Result<(), StoreError> {
        let conn = self.open_transaction();
        let tx = conn.unchecked_transaction().map_err(|e| backend("begin observe", e))?;
        // Prepared once per call, reused per listing: observe runs over whole
        // dumps (thousands of rows), so per-row `prepare` calls dominated.
        let mut update_open = tx
            .prepare(
                "UPDATE sight SET last_seen_snapshot = ?4, last_seen_at = ?5, band = ?6, quantity = ?7
                 WHERE target_id = ?1 AND auction_id = ?2 AND kind = ?3 AND closed_at IS NULL",
            )
            .map_err(|e| backend("prepare observe update", e))?;
        let mut select_prior = tx
            .prepare(
                "SELECT closed_at, credited, quantity FROM sight
                 WHERE target_id = ?1 AND auction_id = ?2 AND kind = ?3",
            )
            .map_err(|e| backend("prepare observe prior", e))?;
        let mut insert_sight = tx
            .prepare(
                "INSERT INTO sight (target_id, auction_id, kind, item_ref_id, first_seen_snapshot, last_seen_snapshot, first_seen_at, last_seen_at, band, quantity)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?6, ?7, ?8)",
            )
            .map_err(|e| backend("prepare observe insert", e))?;
        let mut reopen_sight = tx
            .prepare(
                "UPDATE sight SET closed_at = NULL, credited = 0,
                         first_seen_snapshot = ?4, last_seen_snapshot = ?4,
                         first_seen_at = ?5, last_seen_at = ?5, band = ?6, quantity = ?7
                 WHERE target_id = ?1 AND auction_id = ?2 AND kind = ?3",
            )
            .map_err(|e| backend("prepare observe reopen", e))?;
        let mut item_ref_ids: HashMap<ItemRef, i64> = HashMap::new();
        let mut new_sights: HashMap<i64, i64> = HashMap::new(); // item_ref_id -> count
        for listing in listings {
            let item_ref_id = if let Some(id) = item_ref_ids.get(&listing.item_ref) {
                *id
            } else {
                let id = get_or_create_item_ref(&tx, &listing.item_ref)?;
                item_ref_ids.insert(listing.item_ref.clone(), id);
                id
            };
            // A listing sighted in a previous kind cycle must never match
            // here: commodity and item auction ids are separate namespaces.
            let updated = update_open
                .execute(rusqlite::params![
                    target.as_str(),
                    listing.auction_id,
                    kind.as_str(),
                    snapshot_id,
                    at,
                    listing.time_left.as_str(),
                    listing.quantity,
                ])
                .map_err(|e| backend("observe update", e))?;
            if updated == 0 {
                // No open sight: either brand new, or a closed one we must
                // reopen (the listing vanished for a cycle - maintenance,
                // truncated pull - and came back). Blind INSERTs would trip
                // the primary key forever after.
                if observe_new_or_reopen(
                    &mut select_prior,
                    &mut insert_sight,
                    &mut reopen_sight,
                    &tx,
                    target,
                    kind,
                    listing,
                    item_ref_id,
                    snapshot_id,
                    at,
                )? {
                    *new_sights.entry(item_ref_id).or_insert(0) += 1;
                }
            }
        }
        for (item_ref_id, count) in new_sights {
            bump_sales_row(
                &tx,
                target,
                item_ref_id,
                utc_day(at),
                &SalesDelta { listings: count, closed: 0, credited: 0, qty_sold: 0 },
            )?;
        }
        // Statements borrow `tx`; release them before the commit move.
        drop(update_open);
        drop(select_prior);
        drop(insert_sight);
        drop(reopen_sight);
        tx.commit().map_err(|e| backend("commit observe", e))?;
        Ok(())
    }

    fn close_missing(
        &self,
        target: &TargetId,
        kind: PayloadKind,
        snapshot_id: i64,
        at: i64,
    ) -> Result<Vec<SaleCredit>, StoreError> {
        let conn = self.open_transaction();
        let tx = conn.unchecked_transaction().map_err(|e| backend("begin close", e))?;
        let mut statement = tx
            .prepare(
                "SELECT s.auction_id, s.item_ref_id, s.first_seen_at, s.last_seen_at, s.band, s.quantity,
                        r.kind, r.item_id, r.pet_species, r.pet_level, r.pet_quality, r.pet_breed
                 FROM sight s JOIN item_ref r ON r.id = s.item_ref_id
                 WHERE s.target_id = ?1 AND s.kind = ?2 AND s.closed_at IS NULL AND s.last_seen_snapshot != ?3",
            )
            .map_err(|e| backend("prepare close", e))?;
        let rows = statement
            .query(rusqlite::params![target.as_str(), kind.as_str(), snapshot_id])
            .map_err(|e| backend("query close", e))?
            .mapped(|row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, i64>(11)?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| backend("close row", e))?;
        drop(statement);

        let mut credits = Vec::new();
        for (
            auction_id,
            item_ref_id,
            _first_seen_at,
            last_seen_at,
            band,
            quantity,
            ref_kind,
            ref_item_id,
            pet_species,
            pet_level,
            pet_quality,
            pet_breed,
        ) in rows
        {
            let band = amtracker_core::domain::TimeLeftBand::parse(&band);
            let credited = band.credits_sale(last_seen_at, at);
            // The kind predicate is load-bearing: commodity and item auction
            // ids are separate namespaces, so a close for one kind must never
            // touch the other kind's sight with the same numeric id.
            tx.execute(
                "UPDATE sight SET closed_at = ?4, credited = ?5
                 WHERE target_id = ?1 AND auction_id = ?2 AND kind = ?3 AND closed_at IS NULL",
                rusqlite::params![
                    target.as_str(),
                    auction_id,
                    kind.as_str(),
                    at,
                    i64::from(credited)
                ],
            )
            .map_err(|e| backend("close update", e))?;
            bump_sales_row(
                &tx,
                target,
                item_ref_id,
                utc_day(at),
                &SalesDelta {
                    listings: 0,
                    closed: 1,
                    credited: i64::from(credited),
                    qty_sold: if credited { quantity } else { 0 },
                },
            )?;
            if credited {
                credits.push(SaleCredit {
                    auction_id,
                    item: join_item_ref(
                        &ref_kind,
                        ref_item_id,
                        pet_species,
                        pet_level,
                        pet_quality,
                        pet_breed,
                    ),
                    quantity,
                    at,
                });
            }
        }
        tx.commit().map_err(|e| backend("commit close", e))?;
        Ok(credits)
    }

    fn sale_rate(
        &self,
        target: &TargetId,
        item: &ItemRef,
        days: i64,
    ) -> Result<Option<f64>, StoreError> {
        let conn = self.open_transaction();
        let Some(item_ref_id) = lookup_item_ref(&conn, item)? else {
            return Ok(None);
        };
        let now_day = utc_day(self.clock.now());
        let cutoff_day = now_day - days + 1;
        let (credited, listings): (i64, i64) = conn
            .query_row(
                "SELECT COALESCE(SUM(credited), 0), COALESCE(SUM(listings), 0) FROM sales
                 WHERE target_id = ?1 AND item_ref_id = ?2 AND day >= ?3",
                rusqlite::params![target.as_str(), item_ref_id, cutoff_day],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| backend("sale_rate", e))?;
        if listings == 0 {
            return Ok(None);
        }
        // Daily counts are far below f64's exact-integer range (2^53), and the
        // ratio is a display metric; precision loss on giant inputs is
        // acceptable by design.
        #[allow(clippy::cast_precision_loss)]
        Ok(Some(credited as f64 / listings as f64))
    }

    fn sale_rate_multi(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        days: i64,
    ) -> Result<Option<f64>, StoreError> {
        let conn = self.open_transaction();
        let ids = resolve_item_ids(&conn, items)?;
        if ids.is_empty() {
            return Ok(None);
        }
        let now_day = utc_day(self.clock.now());
        let rates = sale_rates_by_id(&conn, target, &ids, now_day - days + 1)?;
        let mut credited = 0i64;
        let mut listings = 0i64;
        for (c, l) in rates.values() {
            credited += *c;
            listings += *l;
        }
        if listings == 0 {
            return Ok(None);
        }
        // Same display-metric precision trade as `sale_rate`.
        #[allow(clippy::cast_precision_loss)]
        Ok(Some(credited as f64 / listings as f64))
    }

    fn sale_rate_each(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        days: i64,
    ) -> Result<Vec<Option<f64>>, StoreError> {
        let conn = self.open_transaction();
        // Positional resolution (None = unknown item), so the result stays
        // aligned with `items`.
        let ids = item_ids_in_order(&conn, items)?;
        let known: Vec<i64> = ids.iter().filter_map(|id| *id).collect();
        if known.is_empty() {
            return Ok(vec![None; items.len()]);
        }
        let now_day = utc_day(self.clock.now());
        let rates = sale_rates_by_id(&conn, target, &known, now_day - days + 1)?;
        Ok(ids
            .iter()
            .map(|id| {
                id.and_then(|id| rates.get(&id)).and_then(|(credited, listings)| {
                    if *listings == 0 {
                        None
                    } else {
                        // Same display-metric precision trade as `sale_rate`.
                        #[allow(clippy::cast_precision_loss)]
                        Some(*credited as f64 / *listings as f64)
                    }
                })
            })
            .collect())
    }
}

/// Stored day-row shape shared by `item_day_rows` and its item-scoped
/// variant.
type StoredDayRow = (String, i64, String, i64, i64, i64, i64, i64);

/// Runs a day-row SELECT whose columns match [`StoredDayRow`].
fn query_day_rows(
    statement: &mut rusqlite::Statement<'_>,
    params: impl rusqlite::Params,
) -> Result<Vec<StoredDayRow>, StoreError> {
    statement
        .query(params)
        .map_err(|e| backend("query day rows", e))?
        .mapped(|row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| backend("day rows row", e))
}

/// Expands stored day rows (fold JSON + identity) into [`ItemDayRow`]s.
fn day_rows_from_stored(rows: Vec<StoredDayRow>) -> Result<Vec<ItemDayRow>, StoreError> {
    let mut out = Vec::with_capacity(rows.len());
    for (fold_json, day, kind, item_id, species, level, quality, breed) in rows {
        let ser: SerDayFold =
            serde_json::from_str(&fold_json).map_err(|e| backend("deserialize fold", e))?;
        let fold = DayFold::from(ser);
        let histogram = fold.histogram();
        if histogram.listing_count == 0 {
            continue;
        }
        let prices: Vec<i64> = histogram
            .buckets
            .iter()
            .flat_map(|(price, bucket)| {
                // Listing counts are small non-negative values; a corrupt
                // negative count contributes no repeated prices.
                std::iter::repeat_n(*price, usize::try_from(bucket.listings).unwrap_or(0))
            })
            .collect();
        let (min, median, mad) = price_stats_sorted(&prices);
        out.push(ItemDayRow {
            item: join_item_ref(&kind, item_id, species, level, quality, breed),
            day,
            listing_count: histogram.listing_count,
            qty_sum: histogram.qty_sum,
            min,
            median,
            mad,
        });
    }
    Ok(out)
}

/// Resolves item identities to `item_ref` ids, keeping only known ones.
fn resolve_item_ids(conn: &Connection, items: &[ItemRef]) -> Result<Vec<i64>, StoreError> {
    let mut ids = Vec::with_capacity(items.len());
    for item in items {
        if let Some(id) = lookup_item_ref(conn, item)? {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Positional identity resolution: `ids[i]` is the `item_ref` row id of
/// `items[i]`, or `None` when the identity is unknown. One prepared
/// statement serves the whole batch — full-market scans cover thousands of
/// items, and a prepare per item would dominate the mutex hold. Same SQL as
/// [`lookup_item_ref`].
fn item_ids_in_order(conn: &Connection, items: &[ItemRef]) -> Result<Vec<Option<i64>>, StoreError> {
    let mut ids = Vec::with_capacity(items.len());
    let mut statement = conn
        .prepare(
            "SELECT id FROM item_ref WHERE kind = ?1 AND item_id = ?2 AND pet_species = ?3
             AND pet_level = ?4 AND pet_quality = ?5 AND pet_breed = ?6",
        )
        .map_err(|e| backend("prepare item_ref", e))?;
    for item in items {
        let (kind, item_id, species, level, quality, breed) = split_item_ref(item);
        let id = statement
            .query_row(rusqlite::params![kind, item_id, species, level, quality, breed], |row| {
                row.get::<_, i64>(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(backend("query item_ref", other)),
            })?;
        ids.push(id);
    }
    Ok(ids)
}

/// Credited/listings sums per `item_ref` id over the trailing day window,
/// in one grouped query per chunk (the batched fuel for `sale_rate_multi`/
/// `sale_rate_each`). Placeholders are bound values, never interpolated
/// input.
///
/// The id list is chunked far below SQLite's 32 766 bound-parameter
/// ceiling: a very large market must not fail the whole ranking scan with
/// "too many SQL variables".
fn sale_rates_by_id(
    conn: &Connection,
    target: &TargetId,
    ids: &[i64],
    cutoff_day: i64,
) -> Result<HashMap<i64, (i64, i64)>, StoreError> {
    /// Ids per query — 500 keeps one market-scale scan at a handful of
    /// grouped queries while sitting 60× under the driver's bound ceiling.
    const SALE_RATE_CHUNK: usize = 500;
    let mut out: HashMap<i64, (i64, i64)> = HashMap::new();
    for chunk in ids.chunks(SALE_RATE_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT item_ref_id, COALESCE(SUM(credited), 0), COALESCE(SUM(listings), 0) FROM sales
             WHERE target_id = ? AND item_ref_id IN ({placeholders}) AND day >= ?
             GROUP BY item_ref_id"
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 2);
        params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
        for id in chunk {
            params.push(rusqlite::types::Value::Integer(*id));
        }
        params.push(rusqlite::types::Value::Integer(cutoff_day));
        let mut statement = conn.prepare(&sql).map_err(|e| backend("prepare sale_rates", e))?;
        let rows = statement
            .query(rusqlite::params_from_iter(params.iter()))
            .map_err(|e| backend("query sale_rates", e))?
            .mapped(|row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| backend("sale_rates row", e))?;
        out.extend(rows.into_iter().map(|(id, credited, listings)| (id, (credited, listings))));
    }
    Ok(out)
}

/// Sales-aggregate deltas for one (target, item, day) row.
struct SalesDelta {
    listings: i64,
    closed: i64,
    credited: i64,
    qty_sold: i64,
}

/// Sales-aggregate upsert: `listings` is the denominator (unique sights
/// opened that day), `credited`/`qty_sold` the sell-through numerators.
fn bump_sales_row(
    tx: &rusqlite::Transaction<'_>,
    target: &TargetId,
    item_ref_id: i64,
    day: i64,
    delta: &SalesDelta,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO sales (target_id, item_ref_id, day, listings, closed, credited, qty_sold)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (target_id, item_ref_id, day) DO UPDATE SET
           listings = listings + ?4,
           closed = closed + ?5,
           credited = credited + ?6,
           qty_sold = qty_sold + ?7",
        rusqlite::params![
            target.as_str(),
            item_ref_id,
            day,
            delta.listings,
            delta.closed,
            delta.credited,
            delta.qty_sold
        ],
    )
    .map_err(|e| backend("bump sales", e))?;
    Ok(())
}

/// Inserts a brand-new sight or reopens a closed one for the same
/// (target, auction, kind). Reversing the previous close on ITS day keeps
/// the sales ledger honest: a not-credited close must not keep inflating
/// `closed`, and a credited close was a phantom sale the moment the
/// listing returned. Returns `true` when a brand-new sight was opened.
/// Statements arrive pre-prepared (see `observe`).
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn observe_new_or_reopen(
    select_prior: &mut rusqlite::Statement<'_>,
    insert_sight: &mut rusqlite::Statement<'_>,
    reopen_sight: &mut rusqlite::Statement<'_>,
    tx: &rusqlite::Transaction<'_>,
    target: &TargetId,
    kind: PayloadKind,
    listing: &NormalizedListing,
    item_ref_id: i64,
    snapshot_id: i64,
    at: i64,
) -> Result<bool, StoreError> {
    let prior: Option<(Option<i64>, i64, i64)> = select_prior
        .query_row(rusqlite::params![target.as_str(), listing.auction_id, kind.as_str()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(backend("observe prior", other)),
        })?;
    let Some((closed_at, credited, prior_qty)) = prior else {
        insert_sight
            .execute(rusqlite::params![
                target.as_str(),
                listing.auction_id,
                kind.as_str(),
                item_ref_id,
                snapshot_id,
                at,
                listing.time_left.as_str(),
                listing.quantity,
            ])
            .map_err(|e| backend("observe insert", e))?;
        return Ok(true);
    };
    if let Some(closed_at) = closed_at {
        let (d_credited, d_qty) = if credited == 1 { (-credited, -prior_qty) } else { (0, 0) };
        bump_sales_row(
            tx,
            target,
            item_ref_id,
            utc_day(closed_at),
            &SalesDelta { listings: 0, closed: -1, credited: d_credited, qty_sold: d_qty },
        )?;
    }
    reopen_sight
        .execute(rusqlite::params![
            target.as_str(),
            listing.auction_id,
            kind.as_str(),
            snapshot_id,
            at,
            listing.time_left.as_str(),
            listing.quantity,
        ])
        .map_err(|e| backend("observe reopen", e))?;
    Ok(false)
}

/// Rebuild an [`ItemRef`] from its stored row. The pet fields were `i32`
/// domain values before being widened for storage, so the narrowing casts
/// below are exact.
#[allow(clippy::cast_possible_truncation)]
fn join_item_ref(
    kind: &str,
    item_id: i64,
    species: i64,
    level: i64,
    quality: i64,
    breed: i64,
) -> ItemRef {
    match kind {
        "pet" => ItemRef::Pet {
            species,
            level: level as i32,
            quality: quality as i32,
            breed: breed as i32,
        },
        _ => ItemRef::Item { item_id },
    }
}

impl MetricsCacheRepository for SqliteStore {
    fn get(
        &self,
        target: &TargetId,
        item: &ItemRef,
        span: Span,
    ) -> Result<Option<amtracker_core::domain::CachedBlock>, StoreError> {
        let conn = self.open_transaction();
        let Some(item_ref_id) = lookup_item_ref(&conn, item)? else {
            return Ok(None);
        };
        let row = conn
            .query_row(
                "SELECT computed_at, block_json FROM metrics_cache
                 WHERE target_id = ?1 AND item_ref_id = ?2 AND span = ?3",
                rusqlite::params![target.as_str(), item_ref_id, span.name()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(backend("metrics cache get", other)),
            })?;
        let Some((computed_at, json)) = row else {
            return Ok(None);
        };
        let ser: SerMetricBlock =
            serde_json::from_str(&json).map_err(|e| backend("deserialize block", e))?;
        Ok(Some(amtracker_core::domain::CachedBlock { block: ser.into(), computed_at }))
    }

    fn put(
        &self,
        target: &TargetId,
        item: &ItemRef,
        span: Span,
        block: &MetricBlock,
        computed_at: i64,
    ) -> Result<(), StoreError> {
        let conn = self.open_transaction();
        let item_ref_id = get_or_create_item_ref(&conn, item)?;
        let json = serde_json::to_string(&SerMetricBlock::from(block))
            .map_err(|e| backend("serialize block", e))?;
        conn.execute(
            "INSERT OR REPLACE INTO metrics_cache (target_id, item_ref_id, span, computed_at, block_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![target.as_str(), item_ref_id, span.name(), computed_at, json],
        )
        .map_err(|e| backend("metrics cache put", e))?;
        Ok(())
    }

    fn invalidate(&self, target: &TargetId) -> Result<(), StoreError> {
        let conn = self.open_transaction();
        conn.execute(
            "DELETE FROM metrics_cache WHERE target_id = ?1",
            rusqlite::params![target.as_str()],
        )
        .map_err(|e| backend("metrics cache invalidate", e))?;
        Ok(())
    }

    fn invalidate_items(&self, target: &TargetId, items: &[ItemRef]) -> Result<(), StoreError> {
        /// Ids per statement — mirrors `sale_rates_by_id`'s chunking: one
        /// market-scale cycle stays a handful of deletes, far under the
        /// driver's bound-parameter ceiling.
        const CACHE_INVALIDATE_CHUNK: usize = 500;
        if items.is_empty() {
            return Ok(());
        }
        let conn = self.open_transaction();
        // Unknown identities have no cache rows: `put` resolves the same way.
        let ids = resolve_item_ids(&conn, items)?;
        for chunk in ids.chunks(CACHE_INVALIDATE_CHUNK) {
            // Placeholders are bound values, never interpolated input.
            let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
            let sql = format!(
                "DELETE FROM metrics_cache WHERE target_id = ? AND item_ref_id IN ({placeholders})"
            );
            let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 1);
            params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
            for id in chunk {
                params.push(rusqlite::types::Value::Integer(*id));
            }
            conn.execute(&sql, rusqlite::params_from_iter(params))
                .map_err(|e| backend("metrics cache invalidate items", e))?;
        }
        Ok(())
    }

    fn get_many(
        &self,
        target: &TargetId,
        span: Span,
        items: &[ItemRef],
    ) -> Result<Vec<Option<amtracker_core::domain::CachedBlock>>, StoreError> {
        /// Ids per query — mirrors `sale_rates_by_id`'s chunking: one
        /// watchlist-scale read stays a handful of selects, far under the
        /// driver's bound-parameter ceiling.
        const CACHE_GET_CHUNK: usize = 500;
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.open_transaction();
        // Positional resolution: unknown identities have no cache rows (the
        // same rule `put` applies), so they read back as `None`.
        let mut ids: Vec<Option<i64>> = Vec::with_capacity(items.len());
        for item in items {
            ids.push(lookup_item_ref(&conn, item)?);
        }
        let known: Vec<i64> = ids.iter().filter_map(|id| *id).collect();
        let mut by_id: HashMap<i64, amtracker_core::domain::CachedBlock> = HashMap::new();
        for chunk in known.chunks(CACHE_GET_CHUNK) {
            let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
            let sql = format!(
                "SELECT item_ref_id, computed_at, block_json FROM metrics_cache
                 WHERE target_id = ? AND span = ? AND item_ref_id IN ({placeholders})"
            );
            let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 2);
            params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
            params.push(rusqlite::types::Value::Text(span.name().to_string()));
            for id in chunk {
                params.push(rusqlite::types::Value::Integer(*id));
            }
            let mut statement =
                conn.prepare(&sql).map_err(|e| backend("prepare metrics cache get_many", e))?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?))
                })
                .map_err(|e| backend("query metrics cache get_many", e))?;
            for row in rows {
                let (id, computed_at, json) =
                    row.map_err(|e| backend("metrics cache get_many row", e))?;
                let ser: SerMetricBlock =
                    serde_json::from_str(&json).map_err(|e| backend("deserialize block", e))?;
                by_id.insert(
                    id,
                    amtracker_core::domain::CachedBlock { block: ser.into(), computed_at },
                );
            }
        }
        Ok(ids.into_iter().map(|id| id.and_then(|id| by_id.remove(&id))).collect())
    }

    fn put_many(
        &self,
        target: &TargetId,
        span: Span,
        items: &[ItemRef],
        blocks: &[Option<amtracker_core::domain::MetricBlock>],
        computed_at: i64,
    ) -> Result<(), StoreError> {
        let conn = self.open_transaction();
        // One transaction for the whole batch: a market-scale recomputation
        // writes tens of thousands of rows, and per-row commits would dominate
        // the publish.
        let tx =
            conn.unchecked_transaction().map_err(|e| backend("begin metrics cache put_many", e))?;
        for (item, block) in items.iter().zip(blocks.iter()) {
            let Some(block) = block else { continue };
            let item_ref_id = get_or_create_item_ref(&tx, item)?;
            let json = serde_json::to_string(&SerMetricBlock::from(block))
                .map_err(|e| backend("serialize block", e))?;
            tx.execute(
                "INSERT OR REPLACE INTO metrics_cache (target_id, item_ref_id, span, computed_at, block_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![target.as_str(), item_ref_id, span.name(), computed_at, json],
            )
            .map_err(|e| backend("metrics cache put_many", e))?;
        }
        tx.commit().map_err(|e| backend("commit metrics cache put_many", e))?;
        Ok(())
    }
}

impl WatchlistRepository for SqliteStore {
    fn add(&self, entry: &NewWatchlistEntry) -> Result<WatchlistEntry, StoreError> {
        let conn = self.open_transaction();
        let item_ref_id = get_or_create_item_ref(&conn, &entry.item)?;
        let added_at = self.clock.now();
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO watchlist (target_id, item_ref_id, note, added_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![entry.target.as_str(), item_ref_id, entry.note, added_at],
            )
            .map_err(|e| backend("watchlist add", e))?;
        if inserted == 0 {
            return Err(StoreError::Conflict("item already watched".into()));
        }
        Ok(WatchlistEntry {
            target: entry.target.clone(),
            item: entry.item.clone(),
            note: entry.note.clone(),
            added_at,
        })
    }

    fn remove(&self, target: &TargetId, item: &ItemRef) -> Result<bool, StoreError> {
        let conn = self.open_transaction();
        let Some(item_ref_id) = lookup_item_ref(&conn, item)? else {
            return Ok(false);
        };
        let removed = conn
            .execute(
                "DELETE FROM watchlist WHERE target_id = ?1 AND item_ref_id = ?2",
                rusqlite::params![target.as_str(), item_ref_id],
            )
            .map_err(|e| backend("watchlist remove", e))?;
        Ok(removed > 0)
    }

    fn list(&self, target: &TargetId) -> Result<Vec<WatchlistEntry>, StoreError> {
        let conn = self.open_transaction();
        let mut statement = conn
            .prepare(
                "SELECT r.kind, r.item_id, r.pet_species, r.pet_level, r.pet_quality, r.pet_breed, w.note, w.added_at
                 FROM watchlist w JOIN item_ref r ON r.id = w.item_ref_id
                 WHERE w.target_id = ?1 ORDER BY w.added_at",
            )
            .map_err(|e| backend("prepare watchlist", e))?;
        let rows = statement
            .query(rusqlite::params![target.as_str()])
            .map_err(|e| backend("query watchlist", e))?
            .mapped(|row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| backend("watchlist row", e))?;
        Ok(rows
            .into_iter()
            .map(|(kind, item_id, species, level, quality, breed, note, added_at)| WatchlistEntry {
                target: TargetId(target.as_str().to_string()),
                item: join_item_ref(&kind, item_id, species, level, quality, breed),
                note,
                added_at,
            })
            .collect())
    }
}

impl AlertRuleRepository for SqliteStore {
    fn add(
        &self,
        rule: &amtracker_core::domain::NewAlertRule,
    ) -> Result<amtracker_core::domain::AlertRule, StoreError> {
        let conn = self.open_transaction();
        let item_ref_id = get_or_create_item_ref(&conn, &rule.item)?;
        let created_at = self.clock.now();
        let condition_json = serde_json::to_string(&SerAlertCondition::from(&rule.condition))
            .map_err(|e| backend("serialize alert condition", e))?;
        conn.execute(
            "INSERT INTO alert_rule (target_id, item_ref_id, condition_json, note, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                rule.target.as_str(),
                item_ref_id,
                condition_json,
                rule.note,
                created_at
            ],
        )
        .map_err(|e| backend("alert rule add", e))?;
        Ok(amtracker_core::domain::AlertRule {
            id: conn.last_insert_rowid(),
            target: rule.target.clone(),
            item: rule.item.clone(),
            condition: rule.condition,
            note: rule.note.clone(),
            created_at,
            last_fired_at: None,
        })
    }

    fn remove(&self, rule_id: i64) -> Result<bool, StoreError> {
        let conn = self.open_transaction();
        // Events are history: they outlive their rule (the join column keeps
        // its identity), so only the rule row goes.
        let removed = conn
            .execute("DELETE FROM alert_rule WHERE id = ?1", rusqlite::params![rule_id])
            .map_err(|e| backend("alert rule remove", e))?;
        Ok(removed > 0)
    }

    fn list(
        &self,
        target: &TargetId,
    ) -> Result<Vec<amtracker_core::domain::AlertRule>, StoreError> {
        let conn = self.open_transaction();
        let mut statement = conn
            .prepare(
                "SELECT r.id, r.kind, r.item_id, r.pet_species, r.pet_level, r.pet_quality,
                        r.pet_breed, a.condition_json, a.note, a.created_at, a.last_fired_at
                 FROM alert_rule a JOIN item_ref r ON r.id = a.item_ref_id
                 WHERE a.target_id = ?1 ORDER BY a.id",
            )
            .map_err(|e| backend("prepare alert rules", e))?;
        let rows = statement
            .query(rusqlite::params![target.as_str()])
            .map_err(|e| backend("query alert rules", e))?
            .mapped(|row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| backend("alert rule row", e))?;
        rows.into_iter()
            .map(
                |(
                    id,
                    kind,
                    item_id,
                    species,
                    level,
                    quality,
                    breed,
                    json,
                    note,
                    created_at,
                    last_fired_at,
                )| {
                    let ser: SerAlertCondition = serde_json::from_str(&json)
                        .map_err(|e| backend("deserialize alert condition", e))?;
                    Ok(amtracker_core::domain::AlertRule {
                        id,
                        target: TargetId(target.as_str().to_string()),
                        item: join_item_ref(&kind, item_id, species, level, quality, breed),
                        condition: ser.try_into().map_err(StoreError::Backend)?,
                        note,
                        created_at,
                        last_fired_at,
                    })
                },
            )
            .collect()
    }

    fn mark_fired(&self, rule_id: i64, at: i64) -> Result<(), StoreError> {
        let conn = self.open_transaction();
        conn.execute(
            "UPDATE alert_rule SET last_fired_at = ?1 WHERE id = ?2",
            rusqlite::params![at, rule_id],
        )
        .map_err(|e| backend("alert rule mark fired", e))?;
        Ok(())
    }
}

impl AlertEventRepository for SqliteStore {
    fn record(&self, event: &amtracker_core::domain::NewAlertEvent) -> Result<i64, StoreError> {
        let conn = self.open_transaction();
        let item_ref_id = get_or_create_item_ref(&conn, &event.item)?;
        let condition_json = serde_json::to_string(&SerAlertCondition::from(&event.condition))
            .map_err(|e| backend("serialize alert condition", e))?;
        conn.execute(
            "INSERT INTO alert_event
                 (rule_id, target_id, item_ref_id, condition_json, observed, message, fired_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                event.rule_id,
                event.target.as_str(),
                item_ref_id,
                condition_json,
                event.observed,
                event.message,
                event.fired_at,
            ],
        )
        .map_err(|e| backend("alert event record", e))?;
        Ok(conn.last_insert_rowid())
    }

    fn recent(
        &self,
        target: &TargetId,
        limit: usize,
    ) -> Result<Vec<amtracker_core::domain::AlertEvent>, StoreError> {
        let conn = self.open_transaction();
        let mut statement = conn
            .prepare(
                "SELECT e.id, e.rule_id, r.kind, r.item_id, r.pet_species, r.pet_level,
                        r.pet_quality, r.pet_breed, e.condition_json, e.observed, e.message,
                        e.fired_at
                 FROM alert_event e JOIN item_ref r ON r.id = e.item_ref_id
                 WHERE e.target_id = ?1 ORDER BY e.id DESC LIMIT ?2",
            )
            .map_err(|e| backend("prepare alert events", e))?;
        let rows = statement
            .query(rusqlite::params![target.as_str(), i64::try_from(limit).unwrap_or(i64::MAX),])
            .map_err(|e| backend("query alert events", e))?
            .mapped(|row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<f64>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, i64>(11)?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| backend("alert event row", e))?;
        rows.into_iter()
            .map(
                |(
                    id,
                    rule_id,
                    kind,
                    item_id,
                    species,
                    level,
                    quality,
                    breed,
                    json,
                    observed,
                    message,
                    fired_at,
                )| {
                    let ser: SerAlertCondition = serde_json::from_str(&json)
                        .map_err(|e| backend("deserialize alert condition", e))?;
                    Ok(amtracker_core::domain::AlertEvent {
                        id,
                        rule_id,
                        target: TargetId(target.as_str().to_string()),
                        item: join_item_ref(&kind, item_id, species, level, quality, breed),
                        condition: ser.try_into().map_err(StoreError::Backend)?,
                        observed,
                        message,
                        fired_at,
                    })
                },
            )
            .collect()
    }
}

impl ItemCatalogRepository for SqliteStore {
    fn upsert(&self, flavor: &Flavor, records: &[NameRecord]) -> Result<(), StoreError> {
        let conn = self.open_transaction();
        for record in records {
            conn.execute(
                "INSERT INTO item_catalog
                     (flavor, item_id, name, item_class, item_subclass, inventory_slot,
                      quality, icon, sell_price, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT (flavor, item_id) DO UPDATE SET
                     name = excluded.name,
                     item_class = COALESCE(excluded.item_class, item_class),
                     item_subclass = COALESCE(excluded.item_subclass, item_subclass),
                     inventory_slot = COALESCE(excluded.inventory_slot, inventory_slot),
                     quality = COALESCE(excluded.quality, quality),
                     icon = COALESCE(excluded.icon, icon),
                     sell_price = COALESCE(excluded.sell_price, sell_price),
                     fetched_at = excluded.fetched_at",
                rusqlite::params![
                    flavor.as_str(),
                    record.item_id,
                    record.name,
                    record.item_class,
                    record.item_subclass,
                    record.inventory_slot,
                    record.quality,
                    record.icon,
                    record.sell_price,
                    self.clock.now(),
                ],
            )
            .map_err(|e| backend("catalog upsert", e))?;
        }
        Ok(())
    }

    fn upsert_pets(&self, flavor: &Flavor, records: &[PetNameRecord]) -> Result<(), StoreError> {
        let conn = self.open_transaction();
        for record in records {
            conn.execute(
                "INSERT INTO pet_catalog (flavor, species_id, name, fetched_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (flavor, species_id) DO UPDATE SET
                     name = excluded.name, fetched_at = excluded.fetched_at",
                rusqlite::params![
                    flavor.as_str(),
                    record.species_id,
                    record.name,
                    self.clock.now(),
                ],
            )
            .map_err(|e| backend("pet catalog upsert", e))?;
        }
        Ok(())
    }

    fn search(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let conn = self.open_transaction();
        let sanitized = query.replace(['%', '_'], "");
        let pattern = format!("%{sanitized}%");
        let prefix = format!("{sanitized}%");
        let mut statement = conn
            .prepare(
                // One row per identity: the same (kind, id) may exist under
                // several flavors, and search/browse surfaces dedupe to the
                // lexicographically-first flavor's name.
                "SELECT id, name, kind, item_class, quality FROM (
                     SELECT id, name, kind, item_class, quality,
                            ROW_NUMBER() OVER (PARTITION BY kind, id ORDER BY flavor) AS rn
                     FROM (
                         SELECT item_id AS id, name, 'item' AS kind, item_class, quality, flavor
                         FROM item_catalog WHERE name LIKE ?1 COLLATE NOCASE
                         UNION ALL
                         SELECT species_id AS id, name, 'pet' AS kind, NULL, NULL, flavor
                         FROM pet_catalog WHERE name LIKE ?1 COLLATE NOCASE
                     )
                 )
                 WHERE rn = 1
                 ORDER BY (name LIKE ?2 COLLATE NOCASE) DESC, name
                 LIMIT ?3 OFFSET ?4",
            )
            .map_err(|e| backend("prepare search", e))?;
        let mut rows = statement
            .query(rusqlite::params![
                pattern,
                prefix,
                i64::try_from(limit).unwrap_or(i64::MAX),
                i64::try_from(offset).unwrap_or(i64::MAX),
            ])
            .map_err(|e| backend("query search", e))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|e| backend("search next", e))? {
            out.push(catalog_hit_from_row(row)?);
        }
        Ok(out)
    }

    fn by_id(&self, flavor: &Flavor, item_id: i64) -> Result<Option<NameRecord>, StoreError> {
        let conn = self.open_transaction();
        let row = conn
            .query_row(
                "SELECT item_id, name, item_class, item_subclass, inventory_slot, quality,
                        icon, sell_price, fetched_at
                 FROM item_catalog WHERE flavor = ?1 AND item_id = ?2",
                rusqlite::params![flavor.as_str(), item_id],
                |row| {
                    Ok(NameRecord {
                        item_id: row.get(0)?,
                        name: row.get(1)?,
                        item_class: row.get(2)?,
                        item_subclass: row.get(3)?,
                        inventory_slot: row.get(4)?,
                        quality: row.get(5)?,
                        icon: row.get(6)?,
                        sell_price: row.get(7)?,
                        fetched_at: row.get(8)?,
                    })
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(backend("catalog by_id", other)),
            })?;
        Ok(row)
    }

    fn pet_by_species(
        &self,
        flavor: &Flavor,
        species_id: i64,
    ) -> Result<Option<PetNameRecord>, StoreError> {
        let conn = self.open_transaction();
        let row = conn
            .query_row(
                "SELECT species_id, name FROM pet_catalog WHERE flavor = ?1 AND species_id = ?2",
                rusqlite::params![flavor.as_str(), species_id],
                |row| Ok(PetNameRecord { species_id: row.get(0)?, name: row.get(1)? }),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(backend("pet catalog by_species", other)),
            })?;
        Ok(row)
    }

    fn browse(
        &self,
        after: Option<&BrowseCursor>,
        limit: usize,
    ) -> Result<(Vec<SearchHit>, bool), StoreError> {
        let conn = self.open_transaction();
        // One row more than asked: its presence is the `has_more` answer, and
        // it is dropped before returning.
        let fetch = i64::try_from(limit).unwrap_or(i64::MAX).saturating_add(1);
        let mut statement = match after {
            Some(_cursor) => conn
                .prepare(
                    "SELECT id, name, kind, item_class, quality FROM (
                         SELECT id, name, kind, item_class, quality,
                                ROW_NUMBER() OVER (PARTITION BY kind, id ORDER BY flavor) AS rn
                         FROM (
                             SELECT name, 'item' AS kind, item_id AS id, item_class, quality, flavor
                             FROM item_catalog
                             UNION ALL
                             SELECT name, 'pet' AS kind, species_id AS id, NULL, NULL, flavor
                             FROM pet_catalog
                         )
                     )
                     WHERE rn = 1 AND (name, kind, id) > (?1, ?2, ?3)
                     ORDER BY name, kind, id
                     LIMIT ?4",
                )
                .map_err(|e| backend("prepare browse", e))?,
            None => conn
                .prepare(
                    "SELECT id, name, kind, item_class, quality FROM (
                         SELECT id, name, kind, item_class, quality,
                                ROW_NUMBER() OVER (PARTITION BY kind, id ORDER BY flavor) AS rn
                         FROM (
                             SELECT name, 'item' AS kind, item_id AS id, item_class, quality, flavor
                             FROM item_catalog
                             UNION ALL
                             SELECT name, 'pet' AS kind, species_id AS id, NULL, NULL, flavor
                             FROM pet_catalog
                         )
                     )
                     WHERE rn = 1
                     ORDER BY name, kind, id
                     LIMIT ?1",
                )
                .map_err(|e| backend("prepare browse", e))?,
        };
        let mut rows = match after {
            Some(cursor) => statement
                .query(rusqlite::params![cursor.name, cursor.kind, cursor.id, fetch])
                .map_err(|e| backend("query browse", e))?,
            None => {
                statement.query(rusqlite::params![fetch]).map_err(|e| backend("query browse", e))?
            }
        };
        let mut out = Vec::new();
        let mut has_more = false;
        while let Some(row) = rows.next().map_err(|e| backend("browse next", e))? {
            if out.len() >= limit {
                has_more = true;
                break;
            }
            out.push(catalog_hit_from_row(row)?);
        }
        Ok((out, has_more))
    }

    fn counts(&self) -> Result<CatalogCounts, StoreError> {
        let conn = self.open_transaction();
        let items: i64 = conn
            .query_row("SELECT COUNT(DISTINCT item_id) FROM item_catalog", [], |row| row.get(0))
            .map_err(|e| backend("catalog count items", e))?;
        let pets: i64 = conn
            .query_row("SELECT COUNT(DISTINCT species_id) FROM pet_catalog", [], |row| row.get(0))
            .map_err(|e| backend("catalog count pets", e))?;
        Ok(CatalogCounts {
            items: u64::try_from(items).unwrap_or(0),
            pets: u64::try_from(pets).unwrap_or(0),
        })
    }
}

/// Maps one row of the unified catalog queries (`id, name, kind,
/// item_class, quality` — column order as in `search`/`browse`) into the
/// kind-aware hit type. Unknown kind labels fail loudly: they mean the
/// schema and this mapper have drifted apart.
fn catalog_hit_from_row(row: &rusqlite::Row<'_>) -> Result<SearchHit, StoreError> {
    let kind: String = row.get(2).map_err(|e| backend("catalog hit kind", e))?;
    match kind.as_str() {
        "item" => Ok(SearchHit::Item(NameRecord {
            item_id: row.get(0).map_err(|e| backend("catalog hit id", e))?,
            name: row.get(1).map_err(|e| backend("catalog hit name", e))?,
            item_class: row.get(3).map_err(|e| backend("catalog hit class", e))?,
            item_subclass: None,
            inventory_slot: None,
            quality: row.get(4).map_err(|e| backend("catalog hit quality", e))?,
            icon: None,
            sell_price: None,
            fetched_at: 0,
        })),
        "pet" => Ok(SearchHit::Pet(PetNameRecord {
            species_id: row.get(0).map_err(|e| backend("catalog hit id", e))?,
            name: row.get(1).map_err(|e| backend("catalog hit name", e))?,
        })),
        other => Err(StoreError::Backend(format!("unknown catalog row kind '{other}'"))),
    }
}

impl PriceHistoryRepository for SqliteStore {
    fn load_fold(
        &self,
        target: &TargetId,
        item: &ItemRef,
        day: i64,
    ) -> Result<Option<DayFold>, StoreError> {
        let conn = self.open_transaction();
        let item_ref_id = lookup_item_ref(&conn, item)?;
        let Some(item_ref_id) = item_ref_id else {
            return Ok(None);
        };
        let json: Option<String> = conn
            .query_row(
                "SELECT fold_json FROM item_day WHERE target_id = ?1 AND item_ref_id = ?2 AND day = ?3",
                rusqlite::params![target.as_str(), item_ref_id, day],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(backend("load fold", other)),
            })?;
        json.map(|text| serde_json::from_str::<SerDayFold>(&text))
            .transpose()
            .map_err(|e| backend("deserialize fold", e))
            .map(|ser| ser.map(DayFold::from))
    }

    fn load_folds(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        first_day: i64,
        last_day: i64,
    ) -> Result<Vec<Vec<(i64, DayFold)>>, StoreError> {
        /// Ids per query — mirrors `sale_rates_by_id`'s chunking: one
        /// market-scale read stays a handful of selects, far under the
        /// driver's bound-parameter ceiling.
        const FOLD_CHUNK: usize = 500;
        let conn = self.open_transaction();
        // Positional resolution (None = unknown identity has no fold rows),
        // so the result stays aligned with `items`.
        let ids = item_ids_in_order(&conn, items)?;
        let known: Vec<i64> = ids.iter().filter_map(|id| *id).collect();
        let mut by_id: HashMap<i64, Vec<(i64, DayFold)>> = HashMap::new();
        for chunk in known.chunks(FOLD_CHUNK) {
            let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
            let sql = format!(
                "SELECT item_ref_id, day, fold_json FROM item_day
                 WHERE target_id = ? AND day >= ? AND day <= ? AND item_ref_id IN ({placeholders})"
            );
            let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 3);
            params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
            params.push(rusqlite::types::Value::Integer(first_day));
            params.push(rusqlite::types::Value::Integer(last_day));
            for id in chunk {
                params.push(rusqlite::types::Value::Integer(*id));
            }
            let mut statement = conn.prepare(&sql).map_err(|e| backend("prepare load_folds", e))?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?))
                })
                .map_err(|e| backend("query load_folds", e))?;
            for row in rows {
                let (id, day, json) = row.map_err(|e| backend("load_folds row", e))?;
                let ser: SerDayFold =
                    serde_json::from_str(&json).map_err(|e| backend("deserialize fold", e))?;
                by_id.entry(id).or_default().push((day, DayFold::from(ser)));
            }
        }
        Ok(ids
            .into_iter()
            .map(|id| id.and_then(|id| by_id.remove(&id)).unwrap_or_default())
            .collect())
    }

    fn save_fold(
        &self,
        target: &TargetId,
        item: &ItemRef,
        day: i64,
        fold: &DayFold,
    ) -> Result<(), StoreError> {
        let conn = self.open_transaction();
        let item_ref_id = get_or_create_item_ref(&conn, item)?;
        let ser = SerDayFold::from(fold);
        let json = serde_json::to_string(&ser).map_err(|e| backend("serialize fold", e))?;
        conn.execute(
            "INSERT OR REPLACE INTO item_day (target_id, item_ref_id, day, fold_json) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![target.as_str(), item_ref_id, day, json],
        )
        .map_err(|e| backend("save fold", e))?;
        Ok(())
    }

    fn prune_raw_before(&self, target: &TargetId, day: i64) -> Result<u64, StoreError> {
        /// Rows deleted per statement: batching keeps every delete short so
        /// the store mutex is released between statements and concurrent
        /// readers interleave, instead of one multi-million-row transaction
        /// stalling every query for the batch's whole runtime.
        const PRUNE_BATCH: usize = 50_000;
        /// `i64` mirror of `PRUNE_BATCH` for the LIMIT bind parameter.
        const PRUNE_BATCH_I64: i64 = 50_000;
        let mut removed = 0usize;
        loop {
            let conn = self.open_transaction();
            let batch = conn
                .execute(
                    "DELETE FROM listing WHERE rowid IN (
                         SELECT rowid FROM listing WHERE target_id = ?1 AND day < ?2 LIMIT ?3)",
                    rusqlite::params![target.as_str(), day, PRUNE_BATCH_I64],
                )
                .map_err(|e| backend("prune listings", e))?;
            drop(conn);
            removed += batch;
            if batch < PRUNE_BATCH {
                break;
            }
        }
        let conn = self.open_transaction();
        // Best-effort archive cleanup: payload files of old snapshots. The
        // row's path is cleared alongside the file so a re-run neither
        // re-reads a dangling reference nor re-deletes (files may also be
        // absent because a previous run removed them before failing here).
        let mut statement = conn
            .prepare(
                "SELECT DISTINCT archive_path FROM snapshots
                 WHERE target_id = ?1 AND fetched_at < ?2 AND archive_path IS NOT NULL",
            )
            .map_err(|e| backend("prune query", e))?;
        let paths: Vec<String> = statement
            .query_map(rusqlite::params![target.as_str(), day * 86_400], |row| {
                row.get::<_, Option<String>>(0).map(Option::unwrap_or_default)
            })
            .map_err(|e| backend("prune rows", e))?
            .filter_map(Result::ok)
            .filter(|path| !path.is_empty())
            .collect();
        for path in paths {
            // Shared-payload guard: identical raw bytes collapse to one
            // `<sha>.json` archive path, so a path surfaced via an old
            // snapshot may still be the live archive of a newer one. Only
            // reclaim paths with no recent referent — the old row keeps a
            // path pointing at an existing file, which is harmless (and the
            // alternative would destroy the recent snapshot's raw payload).
            let recent: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM snapshots
                     WHERE target_id = ?1 AND archive_path = ?2 AND fetched_at >= ?3",
                    rusqlite::params![target.as_str(), path, day * 86_400],
                    |row| row.get(0),
                )
                .map_err(|e| backend("prune shared-path check", e))?;
            if recent > 0 {
                continue;
            }
            let _ = std::fs::remove_file(self.archive_dir.join(&path));
            let _ = conn.execute(
                "UPDATE snapshots SET archive_path = NULL
                 WHERE target_id = ?1 AND archive_path = ?2",
                rusqlite::params![target.as_str(), path],
            );
        }
        Ok(removed as u64)
    }

    fn items_with_data(
        &self,
        target: &TargetId,
        since_day: i64,
    ) -> Result<Vec<ItemRef>, StoreError> {
        let conn = self.open_transaction();
        let mut statement = conn
            .prepare(
                "SELECT DISTINCT r.kind, r.item_id, r.pet_species, r.pet_level, r.pet_quality, r.pet_breed
                 FROM item_day d JOIN item_ref r ON r.id = d.item_ref_id
                 WHERE d.target_id = ?1 AND d.day >= ?2
                 ORDER BY r.kind, r.item_id, r.pet_species",
            )
            .map_err(|e| backend("prepare items_with_data", e))?;
        let rows = statement
            .query(rusqlite::params![target.as_str(), since_day])
            .map_err(|e| backend("query items_with_data", e))?
            .mapped(|row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| backend("items_with_data row", e))?;
        Ok(rows
            .into_iter()
            .map(|(kind, item_id, species, level, quality, breed)| {
                join_item_ref(&kind, item_id, species, level, quality, breed)
            })
            .collect())
    }

    fn item_day_rows(
        &self,
        target: &TargetId,
        since_day: i64,
    ) -> Result<Vec<ItemDayRow>, StoreError> {
        let conn = self.open_transaction();
        let mut statement = conn
            .prepare(
                "SELECT d.fold_json, d.day, r.kind, r.item_id, r.pet_species, r.pet_level, r.pet_quality, r.pet_breed
                 FROM item_day d JOIN item_ref r ON r.id = d.item_ref_id
                 WHERE d.target_id = ?1 AND d.day >= ?2",
            )
            .map_err(|e| backend("prepare item_day_rows", e))?;
        let rows = query_day_rows(&mut statement, rusqlite::params![target.as_str(), since_day])?;
        day_rows_from_stored(rows)
    }

    fn item_day_rows_for_item(
        &self,
        target: &TargetId,
        item: &ItemRef,
        since_day: i64,
    ) -> Result<Vec<ItemDayRow>, StoreError> {
        let conn = self.open_transaction();
        let mut statement = conn
            .prepare(
                "SELECT d.fold_json, d.day, r.kind, r.item_id, r.pet_species, r.pet_level, r.pet_quality, r.pet_breed
                 FROM item_day d JOIN item_ref r ON r.id = d.item_ref_id
                 WHERE d.target_id = ?1 AND d.day >= ?2
                   AND r.kind = ?3 AND r.item_id = ?4 AND r.pet_species = ?5
                   AND r.pet_level = ?6 AND r.pet_quality = ?7 AND r.pet_breed = ?8",
            )
            .map_err(|e| backend("prepare item_day_rows_for_item", e))?;
        // The identity tuple is the canonical `item_ref` shape (see
        // `split_item_ref`): the item page passes a plain item id, exactly
        // how item rows were written.
        let (kind, item_id, species, level, quality, breed) = split_item_ref(item);
        let rows = query_day_rows(
            &mut statement,
            rusqlite::params![
                target.as_str(),
                since_day,
                kind,
                item_id,
                species,
                level,
                quality,
                breed
            ],
        )?;
        day_rows_from_stored(rows)
    }
}

/// Nearest-rank p05/p25/p75/p95 helper for display tables.
/// Prices must be sorted ascending.
///
/// Direct indexing is safe here: `n > 0` is ensured by the early return and
/// `mid` lies in `1..=n`, so `mid - 1` and `0` are always valid indices — a
/// `.get()` fallback would silently corrupt the statistics.
#[allow(clippy::indexing_slicing)]
fn price_stats_sorted(prices: &[i64]) -> (i64, i64, i64) {
    let n = prices.len();
    if n == 0 {
        return (0, 0, 0);
    }
    let mid = n.div_ceil(2);
    let median = prices[mid - 1];
    let mad = {
        let mut deviations: Vec<i64> = prices.iter().map(|p| (p - median).abs()).collect();
        deviations.sort_unstable();
        deviations[mid - 1]
    };
    (prices[0], median, mad)
}

// ------------------------------------------------------------------ helpers

use std::collections::HashMap;

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    use sha2::{
        Digest,
        Sha256,
    };
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // `String`'s `fmt::Write` impl is infallible.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn parse_kind(value: &str) -> PayloadKind {
    match value {
        "auctions" => PayloadKind::Auctions,
        _ => PayloadKind::Commodities,
    }
}

/// Deserializes the stored drop-reason breakdown. `NULL` (pre-v5 rows, or
/// snapshots with nothing dropped) reads as an empty map; corrupt JSON
/// degrades to empty rather than failing every snapshot read.
fn parse_drop_reasons(
    json: Option<String>,
) -> Result<std::collections::BTreeMap<String, u64>, StoreError> {
    let Some(json) = json else {
        return Ok(std::collections::BTreeMap::new());
    };
    serde_json::from_str(&json).map_err(|e| backend("deserialize drop reasons", e))
}

fn parse_status(value: &str) -> SnapshotStatus {
    // Failed fetches/parse attempts never persist a snapshot row, so stored
    // values are "ok"; anything else (hand-edited rows, future states) reads
    // as Ok rather than inventing states the domain no longer declares.
    let _ = value;
    SnapshotStatus::Ok
}

fn lookup_item_ref(conn: &Connection, item: &ItemRef) -> Result<Option<i64>, StoreError> {
    let (kind, item_id, species, level, quality, breed) = split_item_ref(item);
    let mut statement = conn
        .prepare(
            "SELECT id FROM item_ref WHERE kind = ?1 AND item_id = ?2 AND pet_species = ?3
             AND pet_level = ?4 AND pet_quality = ?5 AND pet_breed = ?6",
        )
        .map_err(|e| backend("prepare item_ref", e))?;
    let mut rows = statement
        .query(rusqlite::params![kind, item_id, species, level, quality, breed])
        .map_err(|e| backend("query item_ref", e))?;
    match rows.next().map_err(|e| backend("item_ref row", e))? {
        Some(row) => Ok(Some(row.get(0).map_err(|e| backend("item_ref id", e))?)),
        None => Ok(None),
    }
}

fn get_or_create_item_ref(conn: &Connection, item: &ItemRef) -> Result<i64, StoreError> {
    if let Some(id) = lookup_item_ref(conn, item)? {
        return Ok(id);
    }
    let (kind, item_id, species, level, quality, breed) = split_item_ref(item);
    conn.execute(
        "INSERT INTO item_ref (kind, item_id, pet_species, pet_level, pet_quality, pet_breed) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![kind, item_id, species, level, quality, breed],
    )
    .map_err(|e| backend("insert item_ref", e))?;
    Ok(conn.last_insert_rowid())
}

fn split_item_ref(item: &ItemRef) -> (&'static str, i64, i64, i64, i64, i64) {
    match item {
        ItemRef::Item { item_id } => ("item", *item_id, 0, 0, 0, 0),
        ItemRef::Pet { species, level, quality, breed } => {
            ("pet", 0, *species, i64::from(*level), i64::from(*quality), i64::from(*breed))
        }
    }
}

// serde mirrors of the fold state (infra-owned serialization format)

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum SerItemRef {
    Item { item_id: i64 },
    Pet { species: i64, level: i32, quality: i32, breed: i32 },
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SerDayFold {
    item: SerItemRef,
    entries: Vec<[i64; 3]>, // [auction_id, price, qty]
}

impl From<&DayFold> for SerDayFold {
    fn from(fold: &DayFold) -> Self {
        SerDayFold {
            item: SerItemRef::from(fold.item()),
            entries: fold.entries().map(|(id, e)| [id, e.price, e.qty]).collect(),
        }
    }
}

impl From<SerDayFold> for DayFold {
    fn from(ser: SerDayFold) -> Self {
        let item = match ser.item {
            SerItemRef::Item { item_id } => ItemRef::Item { item_id },
            SerItemRef::Pet { species, level, quality, breed } => {
                ItemRef::Pet { species, level, quality, breed }
            }
        };
        DayFold::from_parts(
            item,
            ser.entries
                .into_iter()
                .map(|[auction_id, price, qty]| {
                    (auction_id, amtracker_core::fold::DayEntry { price, qty })
                })
                .collect(),
        )
    }
}

impl From<&ItemRef> for SerItemRef {
    fn from(item: &ItemRef) -> Self {
        match item {
            ItemRef::Item { item_id } => SerItemRef::Item { item_id: *item_id },
            ItemRef::Pet { species, level, quality, breed } => SerItemRef::Pet {
                species: *species,
                level: *level,
                quality: *quality,
                breed: *breed,
            },
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SerMetricBlock {
    n: i64,
    min: i64,
    p05: i64,
    p25: i64,
    median: i64,
    p75: i64,
    p95: i64,
    max: i64,
    mad: i64,
    vwap: i64,
    qty_sum: i64,
    listing_count: i64,
    sale_rate: Option<f64>,
}

impl From<&MetricBlock> for SerMetricBlock {
    fn from(block: &MetricBlock) -> Self {
        SerMetricBlock {
            n: block.n,
            min: block.min,
            p05: block.p05,
            p25: block.p25,
            median: block.median,
            p75: block.p75,
            p95: block.p95,
            max: block.max,
            mad: block.mad,
            vwap: block.vwap,
            qty_sum: block.qty_sum,
            listing_count: block.listing_count,
            sale_rate: block.sale_rate,
        }
    }
}

impl From<SerMetricBlock> for MetricBlock {
    fn from(ser: SerMetricBlock) -> Self {
        MetricBlock {
            n: ser.n,
            min: ser.min,
            p05: ser.p05,
            p25: ser.p25,
            median: ser.median,
            p75: ser.p75,
            p95: ser.p95,
            max: ser.max,
            mad: ser.mad,
            vwap: ser.vwap,
            qty_sum: ser.qty_sum,
            listing_count: ser.listing_count,
            sale_rate: ser.sale_rate,
        }
    }
}

/// Serialization mirror for [`amtracker_core::domain::AlertCondition`] — the
/// core stays serde-free; the wire spellings live here.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum SerAlertCondition {
    /// Mirror of `AlertCondition::PriceBelow`.
    PriceBelow {
        /// Threshold in copper per unit.
        copper: i64,
    },
    /// Mirror of `AlertCondition::BelowFloor`.
    BelowFloor {
        /// Window whose median/floor are compared.
        span: String,
        /// Lowercase [`amtracker_core::domain::FloorPercentile`] spelling.
        percentile: String,
    },
    /// Mirror of `AlertCondition::SaleRateAbove`.
    SaleRateAbove {
        /// Threshold fraction (0.0..=1.0).
        rate: f64,
    },
}

impl From<&amtracker_core::domain::AlertCondition> for SerAlertCondition {
    fn from(condition: &amtracker_core::domain::AlertCondition) -> Self {
        use amtracker_core::domain::AlertCondition;
        match *condition {
            AlertCondition::PriceBelow { copper } => Self::PriceBelow { copper },
            AlertCondition::BelowFloor { span, percentile } => Self::BelowFloor {
                span: span.name().to_string(),
                percentile: percentile.as_str().to_string(),
            },
            AlertCondition::SaleRateAbove { rate } => Self::SaleRateAbove { rate },
        }
    }
}

impl TryFrom<SerAlertCondition> for amtracker_core::domain::AlertCondition {
    type Error = String;

    fn try_from(ser: SerAlertCondition) -> Result<Self, Self::Error> {
        use amtracker_core::domain::{
            AlertCondition,
            FloorPercentile,
        };
        match ser {
            SerAlertCondition::PriceBelow { copper } => Ok(AlertCondition::PriceBelow { copper }),
            SerAlertCondition::BelowFloor { span, percentile } => {
                let span = Span::parse(&span)
                    .ok_or_else(|| format!("unknown span '{span}' in alert condition"))?;
                let percentile = FloorPercentile::parse(&percentile).ok_or_else(|| {
                    format!("unknown percentile '{percentile}' in alert condition")
                })?;
                Ok(AlertCondition::BelowFloor { span, percentile })
            }
            SerAlertCondition::SaleRateAbove { rate } => Ok(AlertCondition::SaleRateAbove { rate }),
        }
    }
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect/panic and fixture setup may index:
    // fixed synthetic inputs, not hostile data (the SecOutfall precedent).
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn foreign_keys_are_enforced_on_opened_connections() {
        let dir =
            std::env::temp_dir().join(format!("amtracker-fk-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let store = SqliteStore::open(
            &dir.join("fk.db"),
            &dir.join("raw"),
            std::sync::Arc::new(crate::clock::SystemClock) as std::sync::Arc<dyn ClockPort>,
        )
        .expect("open");

        let conn = store.open_transaction();
        let enforced: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0)).unwrap();
        assert_eq!(enforced, 1, "child-table REFERENCES clauses must be live");

        drop(conn);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn test_store(name: &str) -> (std::path::PathBuf, SqliteStore) {
        let dir = std::env::temp_dir().join(format!(
            "amtracker-{name}-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let store = SqliteStore::open(
            &dir.join("test.db"),
            &dir.join("raw"),
            std::sync::Arc::new(crate::clock::SystemClock) as std::sync::Arc<dyn ClockPort>,
        )
        .expect("open");
        (dir, store)
    }

    /// A minimal listing row for fold construction.
    fn fold_listing(
        item: &ItemRef,
        price: i64,
        qty: i64,
    ) -> amtracker_core::domain::NormalizedListing {
        amtracker_core::domain::NormalizedListing {
            auction_id: 1,
            item_ref: item.clone(),
            kind: amtracker_core::domain::ListingKind::Commodity,
            quantity: qty,
            unit_price: price,
            bid: None,
            time_left: amtracker_core::domain::TimeLeftBand::VeryLong,
            context: None,
            bonus_lists: Vec::new(),
        }
    }

    #[test]
    fn load_folds_stays_aligned_across_chunks_windows_and_unknowns() {
        let (dir, store) = test_store("folds");
        let target = TargetId("eu-1602".into());
        // 600 known identities (crossing the 500-id query chunk) plus an
        // unknown one mid-list.
        let items: Vec<ItemRef> = (0..601).map(|id| ItemRef::Item { item_id: id }).collect();
        for (index, item) in items.iter().enumerate() {
            let mut fold = DayFold::new(item.clone());
            fold.apply(&[fold_listing(item, 100 + i64::try_from(index).unwrap_or(0), 1)]);
            // Even indexes store a fold at `day 50`, odd ones at `day 40` so
            // the window filter can be asserted per item.
            let day = if index % 2 == 0 { 50 } else { 40 };
            store.save_fold(&target, item, day, &fold).expect("save fold");
        }
        let query: Vec<ItemRef> = vec![
            items[0].clone(),                  // known, in window
            ItemRef::Item { item_id: 99_999 }, // unknown
            items[1].clone(),                  // known, fold below the window
            items[600].clone(),                // known, last chunk entry
        ];

        let folds = store.load_folds(&target, &query, 45, 50).expect("load folds");
        assert_eq!(folds.len(), 4, "positional alignment");
        assert_eq!(folds[0].len(), 1, "in-window fold loaded");
        assert_eq!(folds[0][0].0, 50);
        assert!(folds[1].is_empty(), "unknown identity loads empty");
        assert!(folds[2].is_empty(), "out-of-window fold filtered");
        assert_eq!(folds[3].len(), 1, "last chunk entry loaded");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn put_many_round_trips_through_get_many_and_skips_none() {
        let (dir, store) = test_store("cache");
        let target = TargetId("eu-1602".into());
        let items: Vec<ItemRef> = (0..3).map(|id| ItemRef::Item { item_id: id }).collect();
        let histogram = amtracker_core::domain::ItemDayHistogram {
            buckets: std::collections::BTreeMap::from([(
                100,
                amtracker_core::domain::PriceBucket { listings: 2, qty: 5 },
            )]),
            listing_count: 2,
            qty_sum: 5,
        };
        let block = amtracker_core::metrics::block_from_histograms(
            std::slice::from_ref(&histogram),
            Some(0.5),
        )
        .expect("block from non-empty histogram");
        let blocks = [Some(block.clone()), None, Some(block)];
        store.put_many(&target, Span::D7, &items, &blocks, 1_000).expect("put many");

        let cached = store.get_many(&target, Span::D7, &items).expect("get many");
        assert!(cached[0].is_some(), "Some block cached");
        assert!(cached[1].is_none(), "None never cached");
        assert!(cached[2].is_some(), "third block cached");
        assert_eq!(cached[0].as_ref().expect("cached block").computed_at, 1_000);

        // A different span must not see the rows.
        let other = store.get_many(&target, Span::D1, &items).expect("get other span");
        assert!(other.iter().all(Option::is_none));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prune_removes_only_the_target_rows_before_the_day() {
        let (dir, store) = test_store("prune");
        let stale = TargetId("eu-1602".into());
        let fresh = TargetId("eu-1615".into());
        let item = ItemRef::Item { item_id: 7 };
        {
            let conn = store.open_transaction();
            let item_ref_id = get_or_create_item_ref(&conn, &item).expect("item ref");
            for (target_id, day, count) in
                [(stale.as_str(), 100, 30), (stale.as_str(), 105, 20), (fresh.as_str(), 100, 10)]
            {
                for auction in 0..count {
                    conn.execute(
                        "INSERT INTO listing (snapshot_id, target_id, day, auction_id, item_ref_id,
                                              quantity, unit_price, bid, time_left, context, bonus_lists)
                         VALUES (1, ?1, ?2, ?3, ?4, 1, 100, NULL, 'VERY_LONG', NULL, '[]')",
                        rusqlite::params![target_id, day, auction, item_ref_id],
                    )
                    .expect("insert listing");
                }
            }
        }
        let removed = store.prune_raw_before(&stale, 105).expect("prune");
        assert_eq!(removed, 30, "only the stale target's old rows");

        let conn = store.open_transaction();
        let remaining: i64 =
            conn.query_row("SELECT COUNT(*) FROM listing", [], |row| row.get(0)).expect("count");
        assert_eq!(remaining, 30, "stale target's fresh rows + the other target survive");
        drop(conn);
        std::fs::remove_dir_all(&dir).ok();
    }
}

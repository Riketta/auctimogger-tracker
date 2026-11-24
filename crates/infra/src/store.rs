//! SQLite repositories implementing `SnapshotRepository` and
//! `PriceHistoryRepository` over a single database file (WAL).
//!
//! Durability model: snapshot metadata, item-day folds and the
//! item catalog are permanent; raw listing rows and archived payload files are
//! a bounded working set (`prune_raw_before`).

use std::{
    io::Write as _,
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
        Category,
        ClassSummary,
        Flavor,
        ItemDayRow,
        ItemHourRow,
        ItemRef,
        ListingKind,
        ListingPage,
        ListingQuery,
        ListingRow,
        ListingSortKey,
        MetricBlock,
        NameRecord,
        NewSnapshot,
        NewWatchlistEntry,
        NormalizedListing,
        PayloadKind,
        PetNameRecord,
        SaleCredit,
        SearchHit,
        SnapshotFloor,
        SnapshotListing,
        SnapshotMeta,
        SnapshotStatus,
        Span,
        TargetId,
        TimeLeftBand,
        WatchlistEntry,
        LISTING_PAGE_SIZE,
    },
    error::StoreError,
    fold::{
        DayEntry,
        DayFold,
    },
    ports::{
        AlertEventRepository,
        AlertRuleRepository,
        ClockPort,
        ItemCatalogRepository,
        MetricsCacheRepository,
        PriceHistoryRepository,
        SalesRepository,
        SnapshotRepository,
        SpaceMaintenance,
        SpaceStats,
        WatchlistRepository,
    },
};
use flate2::{
    write::GzEncoder,
    Compression,
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
/// Version 7 keyed day folds, sale aggregates and the metrics cache per
/// market category (item / commodity / pet). Pre-v7 merged aggregates cannot
/// be split faithfully (a merged fold row carries no category), so they are
/// dropped and the day folds are rebuilt exactly from the retained raw
/// listings (see `rebuild_category_folds`); sale and cache rows re-accumulate.
/// Version 8 added the snapshot-viewer read surface: catalog item levels,
/// the per-snapshot class-summary cache, and snapshot-leading listing
/// indexes for the browse page queries (all idempotent ALTER/CREATE steps).
/// The pre-v7 aggregate pass inside the same gate is shape-guarded, so a
/// v7 file upgrading to v8 skips it instead of re-running the destructive
/// rebuild.
/// Version 9 added the expression index behind the time-left sort (the
/// semantic band rank): the CASE sort temp-b-treed the snapshot's whole
/// listing set on every page view (~360 ms at 400k listings); the index
/// serves the order instead.
///
/// Current schema shape. v10 added `sales.eligible` (the credit-eligible
/// sale-rate denominator) and v11 sold-price evidence (`sight.last_unit_price`,
/// `sales.sold_prices`); each is a shape-guarded ALTER at the call site, so
/// upgraded files converge with fresh ones, and pre-upgrade rows read as
/// "no such evidence". Post-v10 additive shapes — the
/// `item_hour` intraday fold with its `idx_item_hour_day` retention index and
/// the `snapshot_item_floor` live-summary table — ride the unconditional
/// idempotent `SCHEMA` batch instead of a gated migration: each converges by
/// itself (fold cells inside the raw window regenerate exactly from raw
/// listings, see `backfill_item_hours`; floor rows of a snapshot recorded
/// before the table existed are rebuilt on first read, see
/// `SnapshotRepository::floors`). The `idx_item_day_window` scan index is
/// created after the migration ladder instead — it needs the v7 `category`
/// column and must reach current-version files too.
const SCHEMA_VERSION: i64 = 11;

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

/// One-shot pre-v7 category rebuild: the aggregate tables move from merged
/// per-item rows to per-category rows. A PK change cannot ALTER in place, and
/// a merged fold row cannot be split faithfully (it carries no category), so
/// the aggregate tables are recreated empty and the day folds are re-derived
/// from the retained raw listings (still inside raw retention) by replaying
/// them in snapshot order — the same last-observation-wins semantics live
/// ingest applies. Sale aggregates and metric-cache rows are dropped, not
/// rebuilt: sale rates re-accumulate over their rolling window and metric
/// blocks recompute on demand. The pass reads only `listing`/`snapshots`/
/// `item_ref`, which it never modifies, so a crash before the version stamp
/// can safely re-run it. It is shape-guarded at the call site (the v7
/// `category` column on `item_day`) so only genuinely pre-v7 files pay the
/// destructive pass — a v7 file opened by a newer build must skip it, or its
/// sale aggregates would be wiped and its folds replayed for nothing.
const CATEGORY_V7_MIGRATION: &str = "
BEGIN IMMEDIATE;
DROP TABLE IF EXISTS metrics_cache;
DROP TABLE IF EXISTS sales;
DROP TABLE IF EXISTS item_day;
CREATE TABLE item_day (
    target_id TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    day INTEGER NOT NULL,
    category TEXT NOT NULL,
    fold_json TEXT NOT NULL,
    PRIMARY KEY (target_id, item_ref_id, day, category)
);
CREATE TABLE metrics_cache (
    target_id TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    category TEXT NOT NULL,
    span TEXT NOT NULL,
    computed_at INTEGER NOT NULL,
    block_json TEXT NOT NULL,
    PRIMARY KEY (target_id, item_ref_id, category, span)
);
CREATE TABLE sales (
    target_id TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    day INTEGER NOT NULL,
    category TEXT NOT NULL,
    listings INTEGER NOT NULL DEFAULT 0,
    closed INTEGER NOT NULL DEFAULT 0,
    credited INTEGER NOT NULL DEFAULT 0,
    qty_sold INTEGER NOT NULL DEFAULT 0,
    eligible INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (target_id, item_ref_id, day, category)
);
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

CREATE TABLE IF NOT EXISTS snapshot_class_summary (
    snapshot_id INTEGER NOT NULL,
    flavor TEXT NOT NULL,
    item_class TEXT NOT NULL,
    listings INTEGER NOT NULL,
    PRIMARY KEY (snapshot_id, flavor, item_class)
);
CREATE INDEX IF NOT EXISTS idx_listing_snapshot_price
    ON listing(snapshot_id, unit_price, auction_id);
CREATE INDEX IF NOT EXISTS idx_listing_snapshot_qty
    ON listing(snapshot_id, quantity, auction_id);
CREATE INDEX IF NOT EXISTS idx_listing_snapshot_time
    ON listing(snapshot_id, CASE time_left WHEN 'SHORT' THEN 1 WHEN 'MEDIUM' THEN 2 WHEN 'LONG' THEN 3 WHEN 'VERY_LONG' THEN 4 ELSE 5 END, auction_id);

CREATE TABLE IF NOT EXISTS item_day (
    target_id TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    day INTEGER NOT NULL,
    category TEXT NOT NULL,
    fold_json TEXT NOT NULL,
    PRIMARY KEY (target_id, item_ref_id, day, category)
);

CREATE TABLE IF NOT EXISTS item_hour (
    target_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    day INTEGER NOT NULL,
    hour INTEGER NOT NULL,
    mins TEXT NOT NULL,
    floor INTEGER NOT NULL,
    PRIMARY KEY (target_id, kind, item_ref_id, day, hour)
);
CREATE INDEX IF NOT EXISTS idx_item_hour_day ON item_hour(target_id, day);

CREATE TABLE IF NOT EXISTS snapshot_item_floor (
    snapshot_id INTEGER NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    floor_price INTEGER NOT NULL,
    floor_qty INTEGER NOT NULL,
    listing_count INTEGER NOT NULL,
    qty_sum INTEGER NOT NULL,
    PRIMARY KEY (snapshot_id, item_ref_id)
);

CREATE TABLE IF NOT EXISTS metrics_cache (
    target_id TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    category TEXT NOT NULL,
    span TEXT NOT NULL,
    computed_at INTEGER NOT NULL,
    block_json TEXT NOT NULL,
    PRIMARY KEY (target_id, item_ref_id, category, span)
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
    item_level INTEGER,
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
    -- v11: unit price (copper) at the last sighting — what a credited sale
    -- is booked at. NULL on rows closed before the column existed.
    last_unit_price INTEGER,
    closed_at INTEGER,
    credited INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (target_id, auction_id, kind)
);
CREATE INDEX IF NOT EXISTS idx_sight_open ON sight(target_id, kind, closed_at);

CREATE TABLE IF NOT EXISTS sales (
    target_id TEXT NOT NULL,
    item_ref_id INTEGER NOT NULL REFERENCES item_ref(id),
    day INTEGER NOT NULL,
    category TEXT NOT NULL,
    listings INTEGER NOT NULL DEFAULT 0,
    closed INTEGER NOT NULL DEFAULT 0,
    credited INTEGER NOT NULL DEFAULT 0,
    qty_sold INTEGER NOT NULL DEFAULT 0,
    eligible INTEGER NOT NULL DEFAULT 0,
    -- v11: JSON [[unit price, quantity], ..] of the day's priced sold units.
    sold_prices TEXT,
    PRIMARY KEY (target_id, item_ref_id, day, category)
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

/// Snapshot-viewer tables for v8 (idempotent on every open below the version
/// stamp): the class-summary cache fills lazily on first browse read.
const SNAPSHOT_VIEWER_V8_MIGRATION: &str = "
CREATE TABLE IF NOT EXISTS snapshot_class_summary (
    snapshot_id INTEGER NOT NULL,
    flavor TEXT NOT NULL,
    item_class TEXT NOT NULL,
    listings INTEGER NOT NULL,
    PRIMARY KEY (snapshot_id, flavor, item_class)
);
CREATE INDEX IF NOT EXISTS idx_listing_snapshot_price
    ON listing(snapshot_id, unit_price, auction_id);
CREATE INDEX IF NOT EXISTS idx_listing_snapshot_qty
    ON listing(snapshot_id, quantity, auction_id);
";

/// The semantic time-left rank: Short < Medium < Long < Very long. The
/// `idx_listing_snapshot_time` index (fresh SCHEMA and the v9 migration
/// step) and the query's ORDER BY are both built from this constant, so the
/// planner satisfies the time sort from the index instead of temp-b-treeing
/// the snapshot's whole listing set. Keep it the single source of truth.
const TIME_RANK_EXPR: &str = "CASE time_left WHEN 'SHORT' THEN 1 WHEN 'MEDIUM' THEN 2 \
     WHEN 'LONG' THEN 3 WHEN 'VERY_LONG' THEN 4 ELSE 5 END";

/// Single-connection SQLite store (WAL journal) implementing the snapshot,
/// sales, metrics-cache, watchlist, catalog and price-history repositories.
/// All access is serialized through one mutex; archive payloads land under
/// `archive_dir`.
pub struct SqliteStore {
    conn: Mutex<Connection>,
    archive_dir: PathBuf,
    clock: std::sync::Arc<dyn ClockPort>,
}

/// Whether fresh databases get `PRAGMA auto_vacuum = INCREMENTAL` baked
/// into their header. `Off` preserves the historical never-shrink header;
/// `Incremental` makes freed pages reclaimable by `PRAGMA
/// incremental_vacuum` without a full rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoVacuum {
    /// Historical header: freed pages are only reused, never returned.
    Off,
    /// Header mode 2: freed pages returnable via incremental vacuum.
    Incremental,
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
    ///
    /// `auto_vacuum` decides the fresh-file header: [`AutoVacuum::Incremental`]
    /// bakes `PRAGMA auto_vacuum = INCREMENTAL` in so freed pages are
    /// reclaimable without a full rebuild; [`AutoVacuum::Off`] preserves the
    /// historical never-shrink header. Existing files ignore the pragma
    /// until a full `VACUUM` (the maintenance conversion path).
    // One declarative open-and-migrate sequence: the version ladder reads
    // best as a single ordered block.
    #[allow(clippy::too_many_lines)]
    pub fn open(
        database: &Path,
        archive_dir: &Path,
        clock: std::sync::Arc<dyn ClockPort>,
        auto_vacuum: AutoVacuum,
    ) -> Result<Self, StoreError> {
        if let Some(parent) = database.parent() {
            std::fs::create_dir_all(parent).map_err(|e| backend("create data dir", e))?;
        }
        std::fs::create_dir_all(archive_dir).map_err(|e| backend("create archive dir", e))?;
        let conn = Connection::open(database).map_err(|e| backend("open sqlite", e))?;
        // Must precede journal_mode: WAL writes the header immediately, and
        // auto_vacuum only applies to a database that is still empty (on an
        // existing file this pragma is inert until a full VACUUM).
        let mode = match auto_vacuum {
            AutoVacuum::Off => "NONE",
            AutoVacuum::Incremental => "INCREMENTAL",
        };
        conn.pragma_update(None, "auto_vacuum", mode).map_err(|e| backend("auto_vacuum", e))?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| backend("journal_mode", e))?;
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(|e| backend("synchronous", e))?;
        conn.pragma_update(None, "auto_vacuum", mode).map_err(|e| backend("auto_vacuum", e))?;
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
        // Open/migration telemetry: `serve` and the subscriber-backed CLI
        // commands emit these to stdout/file/Sentry; one-shot commands
        // without observability (backup, config-check) simply skip them.
        let fresh = version == 0;
        if fresh {
            tracing::info!(
                database = %database.display(),
                schema_version = SCHEMA_VERSION,
                "new store created"
            );
        } else {
            tracing::info!(
                database = %database.display(),
                schema_version = version,
                "store opened"
            );
        }
        conn.execute_batch(SCHEMA).map_err(|e| backend("schema", e))?;
        // v5 added the per-reason drop breakdown column. Older files get an
        // in-place ALTER guarded by the version gate and a column check, so
        // both fresh v5 files (column already present) and v4 files (column
        // missing) converge; the column is nullable — pre-v5 rows read as
        // "no breakdown recorded".
        if version < SCHEMA_VERSION {
            if !fresh {
                tracing::info!(
                    from_version = version,
                    to_version = SCHEMA_VERSION,
                    "migrating store schema"
                );
            }
            let has_column = conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM pragma_table_info('snapshots') WHERE name = 'drop_reasons')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map(|flag| flag == 1)
                .map_err(|e| backend("inspect snapshots schema", e))?;
            if !has_column {
                tracing::debug!(
                    from_version = version,
                    "migrating: adding snapshots.drop_reasons (v5)"
                );
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
                tracing::debug!(
                    from_version = version,
                    "migrating: rebuilding catalog tables with (flavor, id) keys (v6); \
                     pre-v6 rows carry over as retail with NULL enrichment"
                );
                conn.execute_batch(CATALOG_V6_MIGRATION)
                    .map_err(|e| backend("migrate catalog to flavor keys", e))?;
            }
            // v7 keyed the aggregate tables per market category. Pre-v7 rows
            // are merged across categories and cannot be split, so the tables
            // are recreated and day folds rebuild from the retained raw
            // listings. Shape-guarded on the v7 `category` column so exactly
            // the genuinely pre-v7 files run the destructive pass (see the
            // migration's doc above) — already-v7 files skip it untouched.
            let day_has_category = conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM pragma_table_info('item_day') WHERE name = 'category')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map(|flag| flag == 1)
                .map_err(|e| backend("inspect item_day schema", e))?;
            if !day_has_category {
                tracing::warn!(
                    from_version = version,
                    "migrating: destructive v7 pass - aggregate tables are rebuilt per \
                     market category from the retained raw listings; pre-v7 day folds \
                     cannot be split by category and are re-derived"
                );
                conn.execute_batch(CATEGORY_V7_MIGRATION)
                    .map_err(|e| backend("migrate aggregates to category keys", e))?;
                Self::rebuild_category_folds(&conn)?;
            }
            // v8 added the snapshot-viewer read surface. All steps are
            // idempotent so fresh v8 files (created at the new shape by the
            // SCHEMA batch above) pass through unchanged.
            let catalog_has_level = conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM pragma_table_info('item_catalog') WHERE name = 'item_level')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map(|flag| flag == 1)
                .map_err(|e| backend("inspect item_catalog schema", e))?;
            if !catalog_has_level {
                tracing::debug!(
                    from_version = version,
                    "migrating: adding item_catalog.item_level (v8)"
                );
                conn.execute_batch("ALTER TABLE item_catalog ADD COLUMN item_level INTEGER")
                    .map_err(|e| backend("migrate item_level", e))?;
            }
            if !fresh {
                tracing::debug!(
                    from_version = version,
                    "migrating: creating snapshot-viewer read tables (v8)"
                );
            }
            conn.execute_batch(SNAPSHOT_VIEWER_V8_MIGRATION)
                .map_err(|e| backend("migrate snapshot viewer tables", e))?;
            // v9 added the expression index behind the time-left sort. One
            // idempotent CREATE, built from the same expression constant the
            // query's ORDER BY uses, so upgraded files converge with fresh
            // ones.
            if !fresh {
                tracing::debug!(
                    from_version = version,
                    "migrating: creating the time-left sort index (v9)"
                );
            }
            conn.execute_batch(&format!(
                "CREATE INDEX IF NOT EXISTS idx_listing_snapshot_time \
                 ON listing(snapshot_id, {TIME_RANK_EXPR}, auction_id)"
            ))
            .map_err(|e| backend("migrate time rank index", e))?;
            // v10 added the credit-eligible sale-rate denominator. Historical
            // rows keep `eligible = 0`; they carry no eligible evidence and
            // read as `None` (the ledger holds no such rows).
            let sales_has_eligible = conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM pragma_table_info('sales') WHERE name = 'eligible')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map(|flag| flag == 1)
                .map_err(|e| backend("inspect sales schema", e))?;
            if !sales_has_eligible {
                if !fresh {
                    tracing::debug!(
                        from_version = version,
                        "migrating: adding sales.eligible (v10)"
                    );
                }
                conn.execute_batch(
                    "ALTER TABLE sales ADD COLUMN eligible INTEGER NOT NULL DEFAULT 0",
                )
                .map_err(|e| backend("migrate sales.eligible", e))?;
            }
            // v11 added sold-price evidence: the unit price a sight was last
            // seen at, and each sales row's histogram of priced sold units.
            // Rows written before it keep NULL in both — nothing older can be
            // back-priced — and read as "no sold-price evidence".
            if !column_exists(&conn, "sight", "last_unit_price")? {
                if !fresh {
                    tracing::debug!(
                        from_version = version,
                        "migrating: adding sight.last_unit_price (v11)"
                    );
                }
                conn.execute_batch("ALTER TABLE sight ADD COLUMN last_unit_price INTEGER")
                    .map_err(|e| backend("migrate sight.last_unit_price", e))?;
            }
            if !column_exists(&conn, "sales", "sold_prices")? {
                if !fresh {
                    tracing::debug!(
                        from_version = version,
                        "migrating: adding sales.sold_prices (v11)"
                    );
                }
                conn.execute_batch("ALTER TABLE sales ADD COLUMN sold_prices TEXT")
                    .map_err(|e| backend("migrate sales.sold_prices", e))?;
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|e| backend("user_version", e))?;
            if !fresh {
                tracing::info!(
                    from_version = version,
                    to_version = SCHEMA_VERSION,
                    "store schema migration complete"
                );
            }
        }
        // The day-fold window index behind the whole-market scans. Created
        // AFTER the ladder, not in `SCHEMA`: it names the v7 `category`
        // column, which a pre-v7 file only gains from the migration above,
        // and it must also reach files already at the current version (the
        // ladder is gated on `version < SCHEMA_VERSION`). Idempotent.
        let has_window_index = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1)",
                rusqlite::params!["idx_item_day_window"],
                |row| row.get::<_, i64>(0),
            )
            .map(|flag| flag == 1)
            .map_err(|e| backend("inspect day-fold indexes", e))?;
        if !has_window_index {
            tracing::info!("creating the day-fold window index (idx_item_day_window)");
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_item_day_window ON item_day(target_id, category, day)",
            )
            .map_err(|e| backend("create day-fold window index", e))?;
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
    /// current-version files pass through; files from a FUTURE version are
    /// refused — running older code against newer tables would misread them
    /// in every query, and the version stamp is never lowered, so refusing
    /// costs nothing the file can't get back.
    fn check_schema_version(conn: &Connection, version: i64) -> Result<(), StoreError> {
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        if version > SCHEMA_VERSION {
            return Err(StoreError::Backend(format!(
                "database schema is v{version}, but this build expects v{SCHEMA_VERSION}; \
                 running an older build against newer tables would misread them - \
                 use a matching build, or delete the database file and let the \
                 tracker rebuild it (fold history regenerates within a day)"
            )));
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
            PathBuf::from(target.as_str()).join(kind.as_str()).join(format!("{sha}.json.gz"));
        let full = self.archive_dir.join(&relative);
        if !full.exists() {
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).map_err(|e| backend("archive dir", e))?;
            }
            // Gzip (default level 6): auction JSON collapses ~10x, and the
            // write stays on the ingest path, so the level trades little
            // ratio for latency. Content-addressed name — identical raw
            // bytes still collapse into one tmp-file-renamed file.
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(raw).map_err(|e| backend("archive compress", e))?;
            let compressed = encoder.finish().map_err(|e| backend("archive compress", e))?;
            // Unique tmp name per call: two processes (serve + a CLI one-off)
            // archiving the same sha concurrently must not interleave writes
            // into one tmp file.
            let tmp = full.with_extension(format!("tmp.{}", unique_tmp_suffix()));
            std::fs::write(&tmp, &compressed).map_err(|e| backend("archive write", e))?;
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

    /// One-shot pre-v7 day-fold rebuild (see `CATEGORY_V7_MIGRATION`): the
    /// aggregate tables were just recreated empty; this replays the retained
    /// raw listings in snapshot order into per-category folds — the exact
    /// last-observation-wins semantics live ingest applies — and writes one
    /// `item_day` row per (target, item, day, category). Streams one group at
    /// a time over the ordered scan, so memory stays bounded regardless of
    /// market size. Sale and cache tables stay empty: they re-accumulate.
    fn rebuild_category_folds(conn: &Connection) -> Result<(), StoreError> {
        /// One group's accumulation: per category, the item identity and the
        /// last-observation-wins entry map that becomes the day's fold.
        type Group = HashMap<Category, (ItemRef, std::collections::HashMap<i64, DayEntry>)>;
        let tx = conn.unchecked_transaction().map_err(|e| backend("begin category rebuild", e))?;
        {
            let mut select = tx
                .prepare(
                    "SELECT l.target_id, l.item_ref_id, l.day, l.snapshot_id, l.auction_id,
                        l.quantity, l.unit_price, s.kind, r.kind, r.item_id, r.pet_species,
                        r.pet_level, r.pet_quality, r.pet_breed
                    FROM listing l
                    JOIN snapshots s ON s.id = l.snapshot_id
                    JOIN item_ref r ON r.id = l.item_ref_id
                    ORDER BY l.target_id, l.item_ref_id, l.day, l.snapshot_id",
                )
                .map_err(|e| backend("prepare category rebuild", e))?;
            let rows =
                select.query([]).map_err(|e| backend("query category rebuild", e))?.mapped(|row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, i64>(10)?,
                        row.get::<_, i64>(11)?,
                        row.get::<_, i64>(12)?,
                        row.get::<_, i64>(13)?,
                    ))
                });
            let mut save = tx
                .prepare(
                    "INSERT OR REPLACE INTO item_day (target_id, item_ref_id, day, category, fold_json)
                    VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(|e| backend("prepare category rebuild save", e))?;

            let mut current: Option<((String, i64, i64), Group)> = None;
            for row in rows {
                let (
                    target,
                    item_ref_id,
                    day,
                    _snapshot_id,
                    auction_id,
                    quantity,
                    unit_price,
                    payload_kind,
                    ref_kind,
                    item_id,
                    species,
                    level,
                    quality,
                    breed,
                ) = row.map_err(|e| backend("category rebuild row", e))?;
                // Contiguous groups in the ordered scan: a key change flushes
                // the finished group exactly once.
                let same_group = matches!(
                    &current,
                    Some(((key_target, key_ref, key_day), _))
                        if key_target.as_str() == target.as_str()
                            && *key_ref == item_ref_id
                            && *key_day == day
                );
                if !same_group {
                    if let Some(((key_target, key_ref, key_day), group)) = current.take() {
                        save_rebuilt_group(&mut save, &key_target, key_ref, key_day, group)?;
                    }
                    current = Some(((target, item_ref_id, day), Group::new()));
                }
                let item = join_item_ref(&ref_kind, item_id, species, level, quality, breed);
                let listing_kind = if payload_kind == "commodities" {
                    ListingKind::Commodity
                } else {
                    ListingKind::Item
                };
                let category = Category::derive(listing_kind, &item);
                if let Some((_, group)) = &mut current {
                    let (_, entries) = group
                        .entry(category)
                        .or_insert_with(|| (item, std::collections::HashMap::new()));
                    entries.insert(auction_id, DayEntry { price: unit_price, qty: quantity });
                }
            }
            if let Some(((key_target, key_ref, key_day), group)) = current.take() {
                save_rebuilt_group(&mut save, &key_target, key_ref, key_day, group)?;
            }
        }
        tx.commit().map_err(|e| backend("commit category rebuild", e))?;
        Ok(())
    }
}

impl SpaceMaintenance for SqliteStore {
    fn space_stats(&self) -> Result<SpaceStats, StoreError> {
        let conn = self.open_transaction();
        let read = |pragma: &str| -> Result<i64, StoreError> {
            conn.query_row(&format!("PRAGMA {pragma}"), [], |row| row.get(0))
                .map_err(|e| backend(pragma, e))
        };
        Ok(SpaceStats {
            page_size: read("page_size")?,
            page_count: read("page_count")?,
            freelist_count: read("freelist_count")?,
        })
    }

    fn auto_vacuum_mode(&self) -> Result<i64, StoreError> {
        self.open_transaction()
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
            .map_err(|e| backend("auto_vacuum", e))
    }

    fn enable_incremental_auto_vacuum(&self) -> Result<(), StoreError> {
        let conn = self.open_transaction();
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .map_err(|e| backend("auto_vacuum", e))?;
        // The header change only takes effect through a full rebuild, which
        // is exactly what makes this call the one-time legacy conversion.
        conn.execute_batch("VACUUM").map_err(|e| backend("vacuum conversion", e))
    }

    fn incremental_vacuum(&self) -> Result<(), StoreError> {
        self.open_transaction()
            .execute_batch("PRAGMA incremental_vacuum;")
            .map_err(|e| backend("incremental vacuum", e))
    }

    fn full_vacuum(&self) -> Result<(), StoreError> {
        self.open_transaction().execute_batch("VACUUM").map_err(|e| backend("vacuum", e))
    }

    fn checkpoint_wal(&self) -> Result<(), StoreError> {
        self.open_transaction()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|e| backend("wal checkpoint", e))
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

        // The archive file is already on disk; if the database write below
        // fails, it would be orphaned forever (prune only reclaims paths
        // referenced by snapshot rows, never directory scans) — so a failed
        // record reclaims its own archive, best effort.
        let record = (|| -> Result<i64, StoreError> {
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
            // This snapshot's per-item listing summary, built in the same
            // pass as the listing rows: the intraday fold's one value per
            // (item, hour cell) and the snapshot's live-floor rows below.
            let mut floors: HashMap<i64, FloorAccumulator> = HashMap::new();
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
                floors
                    .entry(item_ref_id)
                    .or_insert_with(|| FloorAccumulator::new(listing.unit_price))
                    .observe(listing.unit_price, listing.quantity);
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
            // Permanent intraday fold: one upsert per item seen in this
            // snapshot — the when-to-buy hour view reads this fold and
            // outlives raw-listing retention by its own knob.
            let hour = snapshot.fetched_at.rem_euclid(86_400) / 3_600;
            let hour_mins: HashMap<i64, i64> =
                floors.iter().map(|(id, summary)| (*id, summary.floor_price)).collect();
            upsert_item_hours(&tx, &snapshot.target, snapshot.kind, day, hour, &hour_mins)?;
            // Live side of the same snapshot: what each item is listed at
            // right now, a point read for the deal ranking and alerts.
            insert_snapshot_floors(&tx, snapshot_id, &floors)?;
            tx.commit().map_err(|e| backend("commit", e))?;
            Ok(snapshot_id)
        })();
        if record.is_err() {
            let _ = std::fs::remove_file(self.archive_dir.join(&archive_path));
        }
        record
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
                kind: parse_kind(&row.get::<_, String>(2).map_err(|e| backend("latest kind", e))?)?,
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

    fn floors(
        &self,
        target: &TargetId,
        kind: PayloadKind,
        snapshot_id: i64,
    ) -> Result<HashMap<ItemRef, SnapshotFloor>, StoreError> {
        let conn = self.open_transaction();
        // A snapshot recorded before the floor rows existed (an older binary,
        // or this very upgrade) heals on first read: its summary is rebuilt
        // from the retained listings under the same hold, once.
        if !snapshot_has_floors(&conn, snapshot_id)? {
            rebuild_snapshot_floors(&conn, target, Some(kind), snapshot_id)?;
        }
        // The (snapshot, item) primary key serves the whole filter.
        let mut statement = conn
            .prepare(
                "SELECT r.kind, r.item_id, r.pet_species, r.pet_level, r.pet_quality, r.pet_breed,
                        f.floor_price, f.floor_qty, f.listing_count, f.qty_sum
                 FROM snapshot_item_floor f JOIN item_ref r ON r.id = f.item_ref_id
                 WHERE f.snapshot_id = ?1",
            )
            .map_err(|e| backend("prepare snapshot floors", e))?;
        let rows = statement
            .query_map(rusqlite::params![snapshot_id], stored_floor_from_row)
            .map_err(|e| backend("query snapshot floors", e))?;
        let mut out: HashMap<ItemRef, SnapshotFloor> = HashMap::new();
        for row in rows {
            let stored = row.map_err(|e| backend("snapshot floor row", e))?;
            out.insert(
                join_item_ref(
                    &stored.kind,
                    stored.item_id,
                    stored.species,
                    stored.level,
                    stored.quality,
                    stored.breed,
                ),
                stored.floor,
            );
        }
        Ok(out)
    }

    fn floor(
        &self,
        target: &TargetId,
        item: &ItemRef,
        snapshot_id: i64,
    ) -> Result<Option<SnapshotFloor>, StoreError> {
        let conn = self.open_transaction();
        let Some(item_ref_id) = lookup_item_ref(&conn, item)? else {
            return Ok(None);
        };
        if !snapshot_has_floors(&conn, snapshot_id)? {
            rebuild_snapshot_floors(&conn, target, None, snapshot_id)?;
        }
        conn.query_row(
            "SELECT floor_price, floor_qty, listing_count, qty_sum FROM snapshot_item_floor
             WHERE snapshot_id = ?1 AND item_ref_id = ?2",
            rusqlite::params![snapshot_id, item_ref_id],
            |row| {
                Ok(SnapshotFloor {
                    floor_price: row.get(0)?,
                    floor_qty: row.get::<_, i64>(1)?.max(0),
                    listing_count: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                    qty_sum: row.get(3)?,
                })
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(backend("snapshot floor", other)),
        })
    }

    fn listings_for_item(
        &self,
        target: &TargetId,
        item: &ItemRef,
        kind: PayloadKind,
        snapshot_id: i64,
    ) -> Result<Vec<SnapshotListing>, StoreError> {
        let conn = self.open_transaction();
        let Some(item_ref_id) = lookup_item_ref(&conn, item)? else {
            return Ok(Vec::new());
        };
        // One seek over the item's listings in that snapshot through the
        // (target, item, snapshot) index; the sighting join is a primary-key
        // lookup per row.
        let mut statement = conn
            .prepare(
                "SELECT l.auction_id, l.unit_price, l.quantity, s.first_seen_snapshot, s.first_seen_at
                 FROM listing l
                 LEFT JOIN sight s
                   ON s.target_id = l.target_id AND s.auction_id = l.auction_id AND s.kind = ?2
                 WHERE l.target_id = ?1 AND l.item_ref_id = ?3 AND l.snapshot_id = ?4
                 ORDER BY l.unit_price, l.auction_id",
            )
            .map_err(|e| backend("prepare snapshot listings", e))?;
        let rows = statement
            .query_map(
                rusqlite::params![target.as_str(), kind.as_str(), item_ref_id, snapshot_id],
                |row| {
                    Ok(SnapshotListing {
                        auction_id: row.get(0)?,
                        unit_price: row.get(1)?,
                        quantity: row.get(2)?,
                        first_seen_snapshot: row.get(3)?,
                        first_seen_at: row.get(4)?,
                    })
                },
            )
            .map_err(|e| backend("query snapshot listings", e))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| backend("snapshot listing row", e))
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
                )?,
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

    fn snapshot_by_id(&self, id: i64) -> Result<Option<SnapshotMeta>, StoreError> {
        let conn = self.open_transaction();
        let mut statement = conn
            .prepare(
                "SELECT id, target_id, kind, fetched_at, source_last_modified, entry_count, dropped_count, status, drop_reasons
                 FROM snapshots WHERE id = ?1",
            )
            .map_err(|e| backend("prepare snapshot_by_id", e))?;
        let mut rows = statement
            .query(rusqlite::params![id])
            .map_err(|e| backend("query snapshot_by_id", e))?;
        let Some(row) = rows.next().map_err(|e| backend("snapshot_by_id row", e))? else {
            return Ok(None);
        };
        let entry_count =
            u64::try_from(row.get::<_, i64>(5).map_err(|e| backend("entry_count", e))?)
                .unwrap_or(0);
        let dropped_count =
            u64::try_from(row.get::<_, i64>(6).map_err(|e| backend("dropped_count", e))?)
                .unwrap_or(0);
        Ok(Some(SnapshotMeta {
            id: row.get(0).map_err(|e| backend("snapshot_by_id id", e))?,
            target: TargetId(
                row.get::<_, String>(1).map_err(|e| backend("snapshot_by_id target", e))?,
            ),
            kind: parse_kind(
                &row.get::<_, String>(2).map_err(|e| backend("snapshot_by_id kind", e))?,
            )?,
            fetched_at: row.get(3).map_err(|e| backend("snapshot_by_id fetched_at", e))?,
            source_last_modified: row
                .get(4)
                .map_err(|e| backend("snapshot_by_id last_modified", e))?,
            entry_count,
            dropped_count,
            dropped_reasons: parse_drop_reasons(
                row.get(8).map_err(|e| backend("snapshot_by_id drop_reasons", e))?,
            )?,
            status: parse_status(
                &row.get::<_, String>(7).map_err(|e| backend("snapshot_by_id status", e))?,
            ),
        }))
    }

    fn listings_page(
        &self,
        flavor: &Flavor,
        snapshot_id: i64,
        query: &ListingQuery,
    ) -> Result<ListingPage, StoreError> {
        let conn = self.open_transaction();
        // Defensive clamp: a 0 page must not underflow the offset math (the
        // service normalizes the response afterwards).
        let page = i64::from(query.page.max(1));
        // The name prefix is a user string: LIKE wildcards are escaped so
        // `100%` means the literal prefix, not a wildcard.
        let prefix_pattern = query.name_prefix.as_ref().map(|prefix| {
            format!("{}%", prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))
        });
        // Sort keys are a fixed whitelist (never interpolated user data); the
        // band rank orders the semantic Short < Medium < Long < VeryLong and
        // is built from the same constant as the time index, so that sort
        // reads the index instead of temp-b-treeing the snapshot.
        let sort_expr = match query.sort {
            ListingSortKey::Price => "l.unit_price".to_string(),
            ListingSortKey::Quantity => "l.quantity".to_string(),
            ListingSortKey::TimeLeft => TIME_RANK_EXPR.to_string(),
        };
        let direction = if query.desc { "DESC" } else { "ASC" };

        // Fixed nullable filter slots: ?3 prefix, ?4 item id, ?5 class,
        // ?6 quality. Name/level/quality reads come from the flavor-scoped
        // catalog; pets resolve their name from the species catalog and
        // carry level/quality in the identity tuple.
        let from_where = "FROM listing l
             JOIN item_ref r ON r.id = l.item_ref_id
             LEFT JOIN item_catalog ic ON ic.flavor = ?1 AND ic.item_id = r.item_id AND r.kind = 'item'
             LEFT JOIN pet_catalog pc ON pc.flavor = ?1 AND pc.species_id = r.pet_species AND r.kind = 'pet'
             WHERE l.snapshot_id = ?2
               AND (?3 IS NULL OR COALESCE(ic.name, pc.name) LIKE ?3 ESCAPE '\\')
               AND (?4 IS NULL OR (r.kind = 'item' AND r.item_id = ?4))
               AND (?5 IS NULL OR ic.item_class = ?5 COLLATE NOCASE)
               AND (?6 IS NULL OR ic.quality = ?6 COLLATE NOCASE)";

        // Unfiltered views don't need the catalog joins at all: the plain
        // snapshot-index count is ~9 ms at 400k listings, while the joined
        // walk evaluates every LEFT JOIN per row (~330 ms) because the
        // filter slots may bind. Any filter keeps the joined count.
        let total = if query.name_prefix.is_none()
            && query.item_id.is_none()
            && query.class.is_none()
            && query.quality.is_none()
        {
            u64::try_from(
                conn.query_row(
                    "SELECT COUNT(*) FROM listing WHERE snapshot_id = ?1",
                    rusqlite::params![snapshot_id],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|e| backend("count listings", e))?,
            )
            .unwrap_or(0)
        } else {
            u64::try_from(
                conn.query_row(
                    &format!("SELECT COUNT(*) {from_where}"),
                    rusqlite::params![
                        flavor.as_str(),
                        snapshot_id,
                        prefix_pattern,
                        query.item_id,
                        query.class,
                        query.quality,
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|e| backend("count listings", e))?,
            )
            .unwrap_or(0)
        };
        let pages = u32::try_from(total.div_ceil(u64::try_from(LISTING_PAGE_SIZE).unwrap_or(1)))
            .unwrap_or(u32::MAX);
        let served_page = page.min(i64::from(pages.max(1)));

        let sql = format!(
            "SELECT l.auction_id, l.quantity, l.unit_price, l.bid, l.time_left, \
             r.kind, r.item_id, r.pet_species, r.pet_level, r.pet_quality, r.pet_breed, \
             COALESCE(ic.name, pc.name), ic.quality, ic.item_level \
             {from_where} \
             ORDER BY {sort_expr} {direction}, l.auction_id ASC \
             LIMIT ?7 OFFSET ?8"
        );
        let mut statement = conn.prepare(&sql).map_err(|e| backend("prepare listings", e))?;
        let rows = statement
            .query_map(
                rusqlite::params![
                    flavor.as_str(),
                    snapshot_id,
                    prefix_pattern,
                    query.item_id,
                    query.class,
                    query.quality,
                    i64::try_from(LISTING_PAGE_SIZE).unwrap_or(i64::MAX),
                    (served_page - 1) * i64::try_from(LISTING_PAGE_SIZE).unwrap_or(i64::MAX),
                ],
                listing_row_from_row,
            )
            .map_err(|e| backend("query listings", e))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| backend("listings row", e))?);
        }
        Ok(ListingPage {
            rows: out,
            total,
            page: u32::try_from(served_page).unwrap_or(u32::MAX),
            pages,
        })
    }

    fn snapshot_class_summary(
        &self,
        flavor: &Flavor,
        snapshot_id: i64,
    ) -> Result<Vec<ClassSummary>, StoreError> {
        let conn = self.open_transaction();
        // Catalog watermark for the flavor: (row count, newest fetch stamp).
        // Snapshots are immutable, but the catalog is not — later enrichment
        // adds classes — so a cached summary carries the watermark it was
        // computed against and recomputes once that watermark moves. A
        // sentinel row (`watermark:` class label) stores it alongside the
        // cached counts and is filtered out of every read.
        let watermark: String = {
            let (count, newest): (i64, i64) = conn
                .query_row(
                    "SELECT COUNT(*), COALESCE(MAX(fetched_at), 0) FROM item_catalog \
                     WHERE flavor = ?1",
                    rusqlite::params![flavor.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|e| backend("catalog watermark", e))?;
            format!("watermark:{count}:{newest}")
        };
        let mut statement = conn
            .prepare(
                "SELECT item_class, listings FROM snapshot_class_summary \
                 WHERE snapshot_id = ?1 AND flavor = ?2",
            )
            .map_err(|e| backend("prepare class summary", e))?;
        let rows = statement
            .query_map(rusqlite::params![snapshot_id, flavor.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| backend("query class summary", e))?;
        let mut cached_watermark = None;
        let mut cached = Vec::new();
        for row in rows {
            let (class, listings) = row.map_err(|e| backend("class summary row", e))?;
            if let Some(stamp) = class.strip_prefix("watermark:") {
                cached_watermark = Some(stamp.to_owned());
            } else {
                cached.push(ClassSummary { class, listings: u64::try_from(listings).unwrap_or(0) });
            }
        }
        drop(statement);
        if cached_watermark.as_deref() == Some(watermark.as_str()) {
            cached.sort_by(|a, b| b.listings.cmp(&a.listings).then(a.class.cmp(&b.class)));
            return Ok(cached);
        }
        // First read for this snapshot/flavor, or the catalog moved since the
        // cache was written (a cached-empty snapshot gains classes the same
        // way): aggregate from the listings and rewrite the cache.
        let aggregate_sql = "SELECT ic.item_class, COUNT(*) \
             FROM listing l \
             JOIN item_ref r ON r.id = l.item_ref_id \
             JOIN item_catalog ic ON ic.flavor = ?1 AND ic.item_id = r.item_id \
             WHERE l.snapshot_id = ?2 AND r.kind = 'item' AND ic.item_class IS NOT NULL \
             GROUP BY ic.item_class";
        let mut statement =
            conn.prepare(aggregate_sql).map_err(|e| backend("prepare class aggregate", e))?;
        let rows = statement
            .query_map(rusqlite::params![flavor.as_str(), snapshot_id], |row| {
                Ok(ClassSummary {
                    class: row.get(0)?,
                    listings: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                })
            })
            .map_err(|e| backend("query class aggregate", e))?;
        let mut computed = Vec::new();
        for row in rows {
            computed.push(row.map_err(|e| backend("class aggregate row", e))?);
        }
        drop(statement);
        let tx = conn.unchecked_transaction().map_err(|e| backend("begin", e))?;
        tx.execute(
            "DELETE FROM snapshot_class_summary WHERE snapshot_id = ?1 AND flavor = ?2",
            rusqlite::params![snapshot_id, flavor.as_str()],
        )
        .map_err(|e| backend("clear class summary", e))?;
        tx.execute(
            "INSERT INTO snapshot_class_summary (snapshot_id, flavor, item_class, listings) \
             VALUES (?1, ?2, ?3, 0)",
            rusqlite::params![snapshot_id, flavor.as_str(), watermark],
        )
        .map_err(|e| backend("cache class summary watermark", e))?;
        for summary in &computed {
            tx.execute(
                "INSERT INTO snapshot_class_summary (snapshot_id, flavor, item_class, listings) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    snapshot_id,
                    flavor.as_str(),
                    summary.class,
                    i64::try_from(summary.listings).unwrap_or(i64::MAX),
                ],
            )
            .map_err(|e| backend("cache class summary", e))?;
        }
        tx.commit().map_err(|e| backend("commit class summary", e))?;
        computed.sort_by(|a, b| b.listings.cmp(&a.listings).then(a.class.cmp(&b.class)));
        Ok(computed)
    }
}

/// Maps one `listings_page` row (listing + `item_ref` + both catalogs) into
/// the viewer's read model.
fn listing_row_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ListingRow> {
    let kind_text: String = row.get(5)?;
    let item = if kind_text == "pet" {
        ItemRef::Pet {
            species: row.get(7)?,
            level: i32::try_from(row.get::<_, i64>(8)?).unwrap_or(0),
            quality: i32::try_from(row.get::<_, i64>(9)?).unwrap_or(0),
            breed: i32::try_from(row.get::<_, i64>(10)?).unwrap_or(0),
        }
    } else {
        ItemRef::Item { item_id: row.get(6)? }
    };
    Ok(ListingRow {
        auction_id: row.get(0)?,
        item,
        quantity: row.get(1)?,
        unit_price: row.get(2)?,
        bid: row.get(3)?,
        time_left: TimeLeftBand::parse(&row.get::<_, String>(4)?),
        name: row.get(11)?,
        quality: row.get(12)?,
        item_level: row.get(13)?,
    })
}

impl SalesRepository for SqliteStore {
    #[allow(clippy::too_many_lines)]
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
                "UPDATE sight SET last_seen_snapshot = ?4, last_seen_at = ?5, band = ?6, quantity = ?7,
                         last_unit_price = ?8
                 WHERE target_id = ?1 AND auction_id = ?2 AND kind = ?3 AND closed_at IS NULL",
            )
            .map_err(|e| backend("prepare observe update", e))?;
        let mut select_prior = tx
            .prepare(
                "SELECT s.closed_at, s.credited, s.quantity, s.first_seen_at, s.item_ref_id,
                        r.kind, r.item_id, r.pet_species, r.pet_level, r.pet_quality, r.pet_breed, s.band,
                        s.last_unit_price
                 FROM sight s JOIN item_ref r ON r.id = s.item_ref_id
                 WHERE s.target_id = ?1 AND s.auction_id = ?2 AND s.kind = ?3",
            )
            .map_err(|e| backend("prepare observe prior", e))?;
        let mut insert_sight = tx
            .prepare(
                "INSERT INTO sight (target_id, auction_id, kind, item_ref_id, first_seen_snapshot, last_seen_snapshot, first_seen_at, last_seen_at, band, quantity, last_unit_price)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?6, ?7, ?8, ?9)",
            )
            .map_err(|e| backend("prepare observe insert", e))?;
        let mut reopen_sight = tx
            .prepare(
                "UPDATE sight SET closed_at = NULL, credited = 0, item_ref_id = ?8,
                         first_seen_snapshot = ?4, last_seen_snapshot = ?4,
                         first_seen_at = ?5, last_seen_at = ?5, band = ?6, quantity = ?7,
                         last_unit_price = ?9
                 WHERE target_id = ?1 AND auction_id = ?2 AND kind = ?3",
            )
            .map_err(|e| backend("prepare observe reopen", e))?;
        let mut select_open_qty = tx
            .prepare(
                "SELECT quantity FROM sight
                 WHERE target_id = ?1 AND auction_id = ?2 AND kind = ?3 AND closed_at IS NULL",
            )
            .map_err(|e| backend("prepare observe open qty", e))?;
        let mut item_ref_ids: HashMap<ItemRef, i64> = HashMap::new();
        // New sights per (ref, category), booked as listing counts after the
        // loop. Commodity partial fills: (ref, category) -> posted unit price
        // -> units bought off live listings this cycle.
        let mut new_sights: HashMap<(i64, Category), i64> = HashMap::new();
        let mut partial_fills: HashMap<(i64, Category), BTreeMap<i64, i64>> = HashMap::new();
        for listing in listings {
            let item_ref_id = if let Some(id) = item_ref_ids.get(&listing.item_ref) {
                *id
            } else {
                let id = get_or_create_item_ref(&tx, &listing.item_ref)?;
                item_ref_ids.insert(listing.item_ref.clone(), id);
                id
            };
            // The payload kind fixes the category for every listing in this
            // batch; the identity shape splits item-AH rows into item/pet.
            let category = Category::derive(listing.kind, &listing.item_ref);
            // A live commodity auction id persists across partial buys (the
            // unit price is fixed at post), and a seller cannot shrink a
            // live listing - cancels and reposts vanish and return under a
            // new id - so a quantity drop on the SAME open sight is fill
            // evidence, more direct than the vanish rule. Item-AH stacks
            // sell atomically, so only the commodities payload pays this
            // extra indexed read.
            if matches!(kind, PayloadKind::Commodities) {
                let prior_qty: Option<i64> = select_open_qty
                    .query_row(
                        rusqlite::params![target.as_str(), listing.auction_id, kind.as_str()],
                        |row| row.get(0),
                    )
                    .map(Some)
                    .or_else(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        other => Err(backend("observe open qty", other)),
                    })?;
                if let Some(prior_qty) = prior_qty.filter(|prior| *prior > listing.quantity) {
                    *partial_fills
                        .entry((item_ref_id, category))
                        .or_default()
                        .entry(listing.unit_price)
                        .or_insert(0) += prior_qty - listing.quantity;
                }
            }
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
                    listing.unit_price,
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
                    *new_sights.entry((item_ref_id, category)).or_insert(0) += 1;
                }
            }
        }
        for ((item_ref_id, category), count) in new_sights {
            bump_sales_row(
                &tx,
                target,
                item_ref_id,
                utc_day(at),
                category,
                &SalesDelta { listings: count, closed: 0, credited: 0, qty_sold: 0, eligible: 0 },
            )?;
        }
        // Partial-fill volume books on the observation day - the day the
        // units left the market - exactly like credited vanish quantities
        // book on their close day, and at the listing's posted unit price.
        for ((item_ref_id, category), fills) in partial_fills {
            let units = fills.values().copied().fold(0i64, i64::saturating_add);
            if units <= 0 {
                continue;
            }
            bump_sales_row(
                &tx,
                target,
                item_ref_id,
                utc_day(at),
                category,
                &SalesDelta { listings: 0, closed: 0, credited: 0, qty_sold: units, eligible: 0 },
            )?;
            add_sold_prices(&tx, target, item_ref_id, utc_day(at), category, &fills)?;
        }
        // Statements borrow `tx`; release them before the commit move.
        drop(update_open);
        drop(select_prior);
        drop(insert_sight);
        drop(reopen_sight);
        drop(select_open_qty);
        tx.commit().map_err(|e| backend("commit observe", e))?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
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
                        r.kind, r.item_id, r.pet_species, r.pet_level, r.pet_quality, r.pet_breed, s.last_unit_price
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
                    row.get::<_, Option<i64>>(12)?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| backend("close row", e))?;
        drop(statement);

        let mut credits = Vec::new();
        for (
            auction_id,
            item_ref_id,
            first_seen_at,
            last_seen_at,
            band,
            quantity,
            ref_kind,
            ref_item_id,
            pet_species,
            pet_level,
            pet_quality,
            pet_breed,
            last_unit_price,
        ) in rows
        {
            let band = amtracker_core::domain::TimeLeftBand::parse(&band);
            let credited = band.credits_sale(last_seen_at, at);
            // Credit eligibility: LONG/VERY_LONG last-seen bands can clear
            // the one-hour poll cadence, so their closes belong in the
            // denominator whether or not they credited (a slow vanish is
            // expiry evidence). A close that credited is always eligible -
            // the credit itself proves it could have.
            let eligible = credited || band.is_credit_eligible();
            // The payload kind fixes this close's category; the identity
            // shape splits item-AH sightings into item/pet sale rows.
            let item = join_item_ref(
                &ref_kind,
                ref_item_id,
                pet_species,
                pet_level,
                pet_quality,
                pet_breed,
            );
            let category = Category::derive(
                match kind {
                    PayloadKind::Commodities => ListingKind::Commodity,
                    PayloadKind::Auctions => ListingKind::Item,
                },
                &item,
            );
            // The kind predicate is load-bearing: commodity and item auction
            // ids are separate namespaces, so a close for one kind must never
            // touch the other kind's sight with the same numeric id. The
            // snapshot guard re-checks `last_seen_snapshot` under the write:
            // the candidate scan and this UPDATE are not atomic across
            // processes (a CLI fetch against a live serve shares the database,
            // see `open_transaction`), so a concurrent cycle may have
            // re-sighted the auction under a NEWER snapshot between the two -
            // such a sight never vanished and must not be closed.
            let changed = tx
                .execute(
                    "UPDATE sight SET closed_at = ?4, credited = ?5
                     WHERE target_id = ?1 AND auction_id = ?2 AND kind = ?3
                       AND closed_at IS NULL AND last_seen_snapshot <= ?6",
                    rusqlite::params![
                        target.as_str(),
                        auction_id,
                        kind.as_str(),
                        at,
                        i64::from(credited),
                        snapshot_id
                    ],
                )
                .map_err(|e| backend("close update", e))?;
            if changed == 0 {
                continue;
            }
            // The close event and the sold units book on the close day (the
            // day the volume left the market). The credit and its eligible
            // denominator book on the sight's OPEN day - the same day its
            // `listings` denominator was booked - so a sale rate over a
            // trailing window always compares a day's credits against that
            // same day's opens. Booking credits on the close day instead
            // would let sights opened before the window inflate the ratio
            // past 1.0.
            bump_sales_row(
                &tx,
                target,
                item_ref_id,
                utc_day(at),
                category,
                &SalesDelta {
                    listings: 0,
                    closed: 1,
                    credited: 0,
                    qty_sold: if credited { quantity } else { 0 },
                    eligible: 0,
                },
            )?;
            bump_sales_row(
                &tx,
                target,
                item_ref_id,
                utc_day(first_seen_at),
                category,
                &SalesDelta {
                    listings: 0,
                    closed: 0,
                    credited: i64::from(credited),
                    qty_sold: 0,
                    eligible: i64::from(eligible),
                },
            )?;
            if credited {
                // Priced when the sight's last sighting recorded one; a sight
                // last seen before v11 sells unpriced (quantity only).
                if let Some(price) = last_unit_price {
                    add_sold_prices(
                        &tx,
                        target,
                        item_ref_id,
                        utc_day(at),
                        category,
                        &BTreeMap::from([(price, quantity)]),
                    )?;
                }
                credits.push(SaleCredit { auction_id, item, quantity, at });
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
        category: Category,
    ) -> Result<Option<f64>, StoreError> {
        let conn = self.open_transaction();
        let Some(item_ref_id) = lookup_item_ref(&conn, item)? else {
            return Ok(None);
        };
        let now_day = utc_day(self.clock.now());
        let cutoff_day = now_day - days + 1;
        // Denominator: credit-eligible closes only. A window without any is
        // NO evidence — `None`, never a zero: a listing that simply has not
        // closed yet must not read as "never sells". (Rows booked before
        // `eligible` existed would have needed a listings fallback; the
        // ledger holds none.)
        let (credited, denominator): (i64, i64) = conn
            .query_row(
                "SELECT COALESCE(SUM(credited), 0), COALESCE(SUM(eligible), 0)
                 FROM sales
                 WHERE target_id = ?1 AND item_ref_id = ?2 AND day >= ?3 AND category = ?4",
                rusqlite::params![target.as_str(), item_ref_id, cutoff_day, category.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| backend("sale_rate", e))?;
        if denominator == 0 {
            return Ok(None);
        }
        // Daily counts are far below f64's exact-integer range (2^53), and the
        // ratio is a display metric; precision loss on giant inputs is
        // acceptable by design.
        #[allow(clippy::cast_precision_loss)]
        Ok(Some(credited as f64 / denominator as f64))
    }

    fn sale_rate_multi(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        days: i64,
        category: Category,
    ) -> Result<Option<f64>, StoreError> {
        let conn = self.open_transaction();
        let ids = resolve_item_ids(&conn, items)?;
        if ids.is_empty() {
            return Ok(None);
        }
        let now_day = utc_day(self.clock.now());
        let rates = sale_rates_by_id(&conn, target, &ids, now_day - days + 1, category)?;
        let mut credited = 0i64;
        let mut denominator = 0i64;
        for (c, d) in rates.values() {
            credited += *c;
            denominator += *d;
        }
        if denominator == 0 {
            return Ok(None);
        }
        // Same display-metric precision trade as `sale_rate`.
        #[allow(clippy::cast_precision_loss)]
        Ok(Some(credited as f64 / denominator as f64))
    }

    fn sale_rate_each(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        days: i64,
        category: Category,
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
        let rates = sale_rates_by_id(&conn, target, &known, now_day - days + 1, category)?;
        Ok(ids
            .iter()
            .map(|id| {
                id.and_then(|id| rates.get(&id)).and_then(|(credited, denominator)| {
                    if *denominator == 0 {
                        None
                    } else {
                        // Same display-metric precision trade as `sale_rate`.
                        #[allow(clippy::cast_precision_loss)]
                        Some(*credited as f64 / *denominator as f64)
                    }
                })
            })
            .collect())
    }

    fn sold_price_each(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        days: i64,
        category: Category,
    ) -> Result<Vec<Vec<(i64, i64)>>, StoreError> {
        let conn = self.open_transaction();
        // Positional resolution (None = unknown item), so the result stays
        // aligned with `items`; an unknown identity or a day without priced
        // sales reads as an empty histogram.
        let ids = item_ids_in_order(&conn, items)?;
        let known: Vec<i64> = ids.iter().filter_map(|id| *id).collect();
        if known.is_empty() {
            return Ok(vec![Vec::new(); items.len()]);
        }
        let now_day = utc_day(self.clock.now());
        let sold = sold_prices_by_id(&conn, target, &known, now_day - days + 1, category)?;
        Ok(ids
            .iter()
            .map(|id| id.and_then(|id| sold.get(&id)).cloned().unwrap_or_default())
            .collect())
    }

    fn qty_sold(
        &self,
        target: &TargetId,
        item: &ItemRef,
        days: i64,
        category: Category,
    ) -> Result<Option<i64>, StoreError> {
        let conn = self.open_transaction();
        let Some(item_ref_id) = lookup_item_ref(&conn, item)? else {
            return Ok(None);
        };
        let now_day = utc_day(self.clock.now());
        let cutoff_day = now_day - days + 1;
        // SUM is NULL over an empty window — "no sales rows" reads as None,
        // distinct from a genuine zero.
        conn.query_row(
            "SELECT SUM(qty_sold) FROM sales
         WHERE target_id = ?1 AND item_ref_id = ?2 AND day >= ?3 AND category = ?4",
            rusqlite::params![target.as_str(), item_ref_id, cutoff_day, category.as_str()],
            |row| row.get(0),
        )
        .map_err(|e| backend("qty_sold", e))
    }

    fn qty_sold_each(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        days: i64,
        category: Category,
    ) -> Result<Vec<Option<i64>>, StoreError> {
        let conn = self.open_transaction();
        // Positional resolution (None = unknown item), so the result stays
        // aligned with `items`.
        let ids = item_ids_in_order(&conn, items)?;
        let known: Vec<i64> = ids.iter().filter_map(|id| *id).collect();
        if known.is_empty() {
            return Ok(vec![None; items.len()]);
        }
        let now_day = utc_day(self.clock.now());
        let units = qty_solds_by_id(&conn, target, &known, now_day - days + 1, category)?;
        Ok(ids.iter().map(|id| id.and_then(|id| units.get(&id)).copied()).collect())
    }

    fn qty_sold_multi(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        days: i64,
        category: Category,
    ) -> Result<Option<i64>, StoreError> {
        let conn = self.open_transaction();
        let ids = resolve_item_ids(&conn, items)?;
        if ids.is_empty() {
            return Ok(None);
        }
        let now_day = utc_day(self.clock.now());
        let units = qty_solds_by_id(&conn, target, &ids, now_day - days + 1, category)?;
        if units.is_empty() {
            return Ok(None);
        }
        // Saturating: one absurd day must not overflow the species sum.
        Ok(Some(units.values().fold(0i64, |sum, units| sum.saturating_add(*units))))
    }

    fn sale_recency(
        &self,
        target: &TargetId,
        item: &ItemRef,
        category: Category,
    ) -> Result<Option<amtracker_core::domain::SaleRecency>, StoreError> {
        let conn = self.open_transaction();
        let Some(item_ref_id) = lookup_item_ref(&conn, item)? else {
            return Ok(None);
        };
        Ok(sale_recencies_by_id(&conn, target, &[item_ref_id], category)?.remove(&item_ref_id))
    }

    fn sale_recency_each(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        category: Category,
    ) -> Result<Vec<Option<amtracker_core::domain::SaleRecency>>, StoreError> {
        let conn = self.open_transaction();
        // Positional resolution (None = unknown item), so the result stays
        // aligned with `items`.
        let ids = item_ids_in_order(&conn, items)?;
        let known: Vec<i64> = ids.iter().filter_map(|id| *id).collect();
        if known.is_empty() {
            return Ok(vec![None; items.len()]);
        }
        let recencies = sale_recencies_by_id(&conn, target, &known, category)?;
        Ok(ids.iter().map(|id| id.and_then(|id| recencies.get(&id)).copied()).collect())
    }

    fn sale_recency_multi(
        &self,
        target: &TargetId,
        items: &[ItemRef],
        category: Category,
    ) -> Result<Option<amtracker_core::domain::SaleRecency>, StoreError> {
        let conn = self.open_transaction();
        let ids = resolve_item_ids(&conn, items)?;
        if ids.is_empty() {
            return Ok(None);
        }
        pooled_sale_recency_by_id(&conn, target, &ids, category)
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
/// `category` is the query's scope: every stored row it covered belongs to
/// that one category.
fn day_rows_from_stored(
    rows: Vec<StoredDayRow>,
    category: Category,
) -> Result<Vec<ItemDayRow>, StoreError> {
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
        let (min, p05, median, mad) = price_stats_sorted(&prices);
        // The quantity-weighted per-unit price falls back to the listing
        // median when nothing carries quantity (single-unit goods make the
        // two identical anyway).
        let qty_median =
            amtracker_core::metrics::quantity_weighted_median(std::slice::from_ref(&histogram))
                .unwrap_or(median);
        let min_qty = histogram.buckets.get(&min).map_or(0, |bucket| bucket.qty.max(0));
        out.push(ItemDayRow {
            item: join_item_ref(&kind, item_id, species, level, quality, breed),
            category,
            day,
            listing_count: histogram.listing_count,
            qty_sum: histogram.qty_sum,
            min,
            min_qty,
            p05,
            median,
            qty_median,
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

/// Credited/eligible sums per `item_ref` id over the trailing day window,
/// in one grouped query per chunk (the batched fuel for `sale_rate_multi`/
/// `sale_rate_each`). The denominator counts credit-eligible closes only — an
/// id whose window holds none gets 0 and reads as `None`, the same "no
/// evidence" contract as `sale_rate`. Placeholders are bound values, never
/// interpolated input.
///
/// The id list is chunked far below SQLite's 32 766 bound-parameter
/// ceiling: a very large market must not fail the whole ranking scan with
/// "too many SQL variables".
fn sale_rates_by_id(
    conn: &Connection,
    target: &TargetId,
    ids: &[i64],
    cutoff_day: i64,
    category: Category,
) -> Result<HashMap<i64, (i64, i64)>, StoreError> {
    /// Ids per query — 500 keeps one market-scale scan at a handful of
    /// grouped queries while sitting 60× under the driver's bound ceiling.
    const SALE_RATE_CHUNK: usize = 500;
    let mut out: HashMap<i64, (i64, i64)> = HashMap::new();
    for chunk in ids.chunks(SALE_RATE_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT item_ref_id, COALESCE(SUM(credited), 0), COALESCE(SUM(eligible), 0)
             FROM sales
             WHERE target_id = ? AND item_ref_id IN ({placeholders}) AND day >= ? AND category = ?
             GROUP BY item_ref_id"
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 3);
        params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
        for id in chunk {
            params.push(rusqlite::types::Value::Integer(*id));
        }
        params.push(rusqlite::types::Value::Integer(cutoff_day));
        params.push(rusqlite::types::Value::Text(category.as_str().to_string()));
        let mut statement = conn.prepare(&sql).map_err(|e| backend("prepare sale_rates", e))?;
        let rows = statement
            .query(rusqlite::params_from_iter(params.iter()))
            .map_err(|e| backend("query sale_rates", e))?
            .mapped(|row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| backend("sale_rates row", e))?;
        out.extend(
            rows.into_iter().map(|(id, credited, denominator)| (id, (credited, denominator))),
        );
    }
    Ok(out)
}

/// Priced sold units per `item_ref` id over the window, as (unit price,
/// quantity) buckets sorted by price — the batched fuel for
/// `sold_price_each`. Every day's histogram of the window merges into one;
/// ids without a priced sale are absent from the result.
fn sold_prices_by_id(
    conn: &Connection,
    target: &TargetId,
    ids: &[i64],
    cutoff_day: i64,
    category: Category,
) -> Result<HashMap<i64, Vec<(i64, i64)>>, StoreError> {
    /// Same chunking rationale as [`sale_rates_by_id`].
    const SOLD_PRICE_CHUNK: usize = 500;
    let mut out: HashMap<i64, BTreeMap<i64, i64>> = HashMap::new();
    for chunk in ids.chunks(SOLD_PRICE_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT item_ref_id, sold_prices FROM sales
             WHERE target_id = ? AND item_ref_id IN ({placeholders}) AND day >= ? AND category = ?
               AND sold_prices IS NOT NULL"
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 3);
        params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
        for id in chunk {
            params.push(rusqlite::types::Value::Integer(*id));
        }
        params.push(rusqlite::types::Value::Integer(cutoff_day));
        params.push(rusqlite::types::Value::Text(category.as_str().to_string()));
        let mut statement = conn.prepare(&sql).map_err(|e| backend("prepare sold_prices", e))?;
        let rows = statement
            .query(rusqlite::params_from_iter(params.iter()))
            .map_err(|e| backend("query sold_prices", e))?
            .mapped(|row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| backend("sold_prices row", e))?;
        for (id, json) in rows {
            let pairs: Vec<(i64, i64)> =
                serde_json::from_str(&json).map_err(|e| backend("parse sold prices", e))?;
            let histogram = out.entry(id).or_default();
            for (price, qty) in pairs {
                let next = histogram.get(&price).copied().unwrap_or(0).saturating_add(qty);
                histogram.insert(price, next);
            }
        }
    }
    Ok(out.into_iter().map(|(id, histogram)| (id, histogram.into_iter().collect())).collect())
}

/// Units credited as sold per `item_ref` id over the window, in one chunked
/// grouped query per batch (the batched fuel for `qty_sold_each` and
/// `qty_sold_multi`). Ids with no sales rows at all are absent from the
/// result — "no evidence" stays distinct from a genuine zero.
fn qty_solds_by_id(
    conn: &Connection,
    target: &TargetId,
    ids: &[i64],
    cutoff_day: i64,
    category: Category,
) -> Result<HashMap<i64, i64>, StoreError> {
    /// Same chunking rationale as [`sale_rates_by_id`].
    const QTY_SOLD_CHUNK: usize = 500;
    let mut out: HashMap<i64, i64> = HashMap::new();
    for chunk in ids.chunks(QTY_SOLD_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT item_ref_id, SUM(qty_sold) FROM sales
             WHERE target_id = ? AND item_ref_id IN ({placeholders}) AND day >= ? AND category = ?
             GROUP BY item_ref_id"
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 3);
        params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
        for id in chunk {
            params.push(rusqlite::types::Value::Integer(*id));
        }
        params.push(rusqlite::types::Value::Integer(cutoff_day));
        params.push(rusqlite::types::Value::Text(category.as_str().to_string()));
        let mut statement = conn.prepare(&sql).map_err(|e| backend("prepare qty_solds", e))?;
        let rows = statement
            .query(rusqlite::params_from_iter(params.iter()))
            .map_err(|e| backend("query qty_solds", e))?
            .mapped(|row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| backend("qty_solds row", e))?;
        out.extend(rows);
    }
    Ok(out)
}

/// Last credited-sale day plus net closures after it, per `item_ref` id, in
/// one chunked CTE query per batch (the batched fuel for `sale_recency` and
/// `sale_recency_each`). Ids without any credited sale are absent from the
/// result — "days since last sale" is meaningless without a sale. Closures
/// count strictly after the sale day (`day > last_sale_day`): same-day
/// closures belong to the sale day's own ledger row, mirroring how the
/// close-day booking writes them.
fn sale_recencies_by_id(
    conn: &Connection,
    target: &TargetId,
    ids: &[i64],
    category: Category,
) -> Result<HashMap<i64, amtracker_core::domain::SaleRecency>, StoreError> {
    /// Ids per query — same chunking rationale as [`sale_rates_by_id`].
    const SALE_RECENCY_CHUNK: usize = 500;
    let mut out = HashMap::new();
    for chunk in ids.chunks(SALE_RECENCY_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "WITH last_sale AS (\n             SELECT item_ref_id, MAX(day) AS last_sale_day FROM sales\n             WHERE target_id = ? AND item_ref_id IN ({placeholders}) AND category = ?\n               AND credited > 0\n             GROUP BY item_ref_id)\n             SELECT s.item_ref_id, ls.last_sale_day,\n                    COALESCE(SUM(CASE WHEN s.day > ls.last_sale_day THEN s.closed ELSE 0 END), 0)\n             FROM sales s JOIN last_sale ls ON ls.item_ref_id = s.item_ref_id\n             WHERE s.target_id = ? AND s.item_ref_id IN ({placeholders}) AND s.category = ?\n             GROUP BY s.item_ref_id"
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() * 2 + 4);
        params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
        for id in chunk {
            params.push(rusqlite::types::Value::Integer(*id));
        }
        params.push(rusqlite::types::Value::Text(category.as_str().to_string()));
        params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
        for id in chunk {
            params.push(rusqlite::types::Value::Integer(*id));
        }
        params.push(rusqlite::types::Value::Text(category.as_str().to_string()));
        let mut statement = conn.prepare(&sql).map_err(|e| backend("prepare sale_recencies", e))?;
        let rows = statement
            .query(rusqlite::params_from_iter(params.iter()))
            .map_err(|e| backend("query sale_recencies", e))?
            .mapped(|row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| backend("sale_recencies row", e))?;
        out.extend(rows.into_iter().map(|(id, last_sale_day, closures)| {
            (id, amtracker_core::domain::SaleRecency { last_sale_day, closures_since: closures })
        }));
    }
    Ok(out)
}

/// Pooled ledger recency over several `item_ref` ids (pet variants of one
/// species share their market): the pooled last-sale day is the MOST RECENT
/// member's (`MAX` — any variant selling resets the species' clock) and the
/// closures are the exact sum over EVERY member's rows strictly after that
/// day; members that never credited a sale contribute their closures too,
/// since churn in the pooled window is species evidence. `None` when no
/// member ever credited a sale. Two chunked passes (pooled day, then
/// closures) stay exact at any group size; placeholders are bound values,
/// never interpolated input.
fn pooled_sale_recency_by_id(
    conn: &Connection,
    target: &TargetId,
    ids: &[i64],
    category: Category,
) -> Result<Option<amtracker_core::domain::SaleRecency>, StoreError> {
    /// Ids per query — same chunking rationale as [`sale_rates_by_id`].
    const POOLED_RECENCY_CHUNK: usize = 500;
    // Pass 1: the most recent credited-sale day across the whole group.
    let mut pooled_day: Option<i64> = None;
    for chunk in ids.chunks(POOLED_RECENCY_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT MAX(day) FROM sales WHERE target_id = ? AND item_ref_id IN ({placeholders})\n             AND category = ? AND credited > 0"
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 2);
        params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
        for id in chunk {
            params.push(rusqlite::types::Value::Integer(*id));
        }
        params.push(rusqlite::types::Value::Text(category.as_str().to_string()));
        let mut statement =
            conn.prepare(&sql).map_err(|e| backend("prepare pooled recency day", e))?;
        let day = statement
            .query_row(rusqlite::params_from_iter(params.iter()), |row| {
                row.get::<_, Option<i64>>(0)
            })
            .map_err(|e| backend("query pooled recency day", e))?;
        pooled_day = match (pooled_day, day) {
            (Some(current), Some(day)) => Some(current.max(day)),
            (None, Some(day)) => Some(day),
            (current, None) => current,
        };
    }
    let Some(last_sale_day) = pooled_day else {
        return Ok(None);
    };
    // Pass 2: every member's closures strictly after the pooled day.
    let mut closures_since = 0i64;
    for chunk in ids.chunks(POOLED_RECENCY_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT COALESCE(SUM(closed), 0) FROM sales WHERE target_id = ?\n             AND item_ref_id IN ({placeholders}) AND category = ? AND day > ?"
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 3);
        params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
        for id in chunk {
            params.push(rusqlite::types::Value::Integer(*id));
        }
        params.push(rusqlite::types::Value::Text(category.as_str().to_string()));
        params.push(rusqlite::types::Value::Integer(last_sale_day));
        let mut statement =
            conn.prepare(&sql).map_err(|e| backend("prepare pooled recency closures", e))?;
        let sum = statement
            .query_row(rusqlite::params_from_iter(params.iter()), |row| row.get::<_, i64>(0))
            .map_err(|e| backend("query pooled recency closures", e))?;
        closures_since += sum;
    }
    Ok(Some(amtracker_core::domain::SaleRecency { last_sale_day, closures_since }))
}

/// Persists one rebuilt (target, item, day) group: one fold row per category
/// that accumulated entries, serialized exactly like live writes.
fn save_rebuilt_group(
    save: &mut rusqlite::Statement<'_>,
    target: &str,
    item_ref_id: i64,
    day: i64,
    group: HashMap<Category, (ItemRef, HashMap<i64, DayEntry>)>,
) -> Result<(), StoreError> {
    for (category, (item, entries)) in group {
        if entries.is_empty() {
            continue;
        }
        let fold = DayFold::from_parts(item, entries);
        let ser = SerDayFold::from(&fold);
        let json = serde_json::to_string(&ser).map_err(|e| backend("serialize rebuilt fold", e))?;
        save.execute(rusqlite::params![target, item_ref_id, day, category.as_str(), json])
            .map_err(|e| backend("save rebuilt fold", e))?;
    }
    Ok(())
}

/// Whether `table` already has a column named `column` (the migration
/// ladder's shape guard: a fresh file gains the column from `SCHEMA`, an
/// upgraded one from the ALTER, and both converge).
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, StoreError> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        rusqlite::params![table, column],
        |row| row.get::<_, i64>(0),
    )
    .map(|flag| flag == 1)
    .map_err(|e| backend("inspect table schema", e))
}

/// Sales-aggregate deltas for one (target, item, day, category) row.
struct SalesDelta {
    listings: i64,
    closed: i64,
    credited: i64,
    qty_sold: i64,
    eligible: i64,
}

/// Sales-aggregate upsert: `listings` is the denominator (unique sights
/// opened that day), `credited`/`qty_sold` the sell-through numerators.
fn bump_sales_row(
    tx: &rusqlite::Transaction<'_>,
    target: &TargetId,
    item_ref_id: i64,
    day: i64,
    category: Category,
    delta: &SalesDelta,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO sales (target_id, item_ref_id, day, category, listings, closed, credited, qty_sold, eligible)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (target_id, item_ref_id, day, category) DO UPDATE SET
           listings = listings + ?5,
           closed = closed + ?6,
           credited = credited + ?7,
           qty_sold = qty_sold + ?8,
           eligible = eligible + ?9",
        rusqlite::params![
            target.as_str(),
            item_ref_id,
            day,
            category.as_str(),
            delta.listings,
            delta.closed,
            delta.credited,
            delta.qty_sold,
            delta.eligible
        ],
    )
    .map_err(|e| backend("bump sales", e))?;
    Ok(())
}

/// Books priced sold units onto a sales row's `sold_prices` histogram (JSON
/// `[[unit price, quantity], ..]`, sorted by price). `fills` may carry
/// negative quantities: a reopened sight reverses its phantom sale exactly
/// where the close booked it. A bucket emptied that way drops out, and a
/// histogram left empty reverts to NULL — "no priced evidence". The row must
/// already exist: every caller books the matching `qty_sold` delta first.
fn add_sold_prices(
    tx: &rusqlite::Transaction<'_>,
    target: &TargetId,
    item_ref_id: i64,
    day: i64,
    category: Category,
    fills: &BTreeMap<i64, i64>,
) -> Result<(), StoreError> {
    if fills.is_empty() {
        return Ok(());
    }
    // NULL until the day's first priced sale: read as an option, never a string.
    let current: Option<String> = tx
        .query_row(
            "SELECT sold_prices FROM sales\n             WHERE target_id = ?1 AND item_ref_id = ?2 AND day = ?3 AND category = ?4",
            rusqlite::params![target.as_str(), item_ref_id, day, category.as_str()],
            |row| row.get(0),
        )
        .map_err(|e| backend("read sold prices", e))?;
    let mut histogram: BTreeMap<i64, i64> = current
        .as_deref()
        .map(serde_json::from_str::<Vec<(i64, i64)>>)
        .transpose()
        .map_err(|e| backend("parse sold prices", e))?
        .unwrap_or_default()
        .into_iter()
        .collect();
    for (price, qty) in fills {
        let next = histogram.get(price).copied().unwrap_or(0).saturating_add(*qty);
        if next <= 0 {
            histogram.remove(price);
        } else {
            histogram.insert(*price, next);
        }
    }
    // An empty histogram is stored as NULL, like a day that never priced a
    // sale, so the read path sees one spelling of "no evidence".
    let stored = if histogram.is_empty() {
        None
    } else {
        let pairs: Vec<(i64, i64)> = histogram.into_iter().collect();
        Some(serde_json::to_string(&pairs).map_err(|e| backend("serialize sold prices", e))?)
    };
    let changed = tx
        .execute(
            "UPDATE sales SET sold_prices = ?5
             WHERE target_id = ?1 AND item_ref_id = ?2 AND day = ?3 AND category = ?4",
            rusqlite::params![target.as_str(), item_ref_id, day, category.as_str(), stored],
        )
        .map_err(|e| backend("book sold prices", e))?;
    if changed != 1 {
        return Err(StoreError::Backend(format!(
            "sold prices: no sales row for item ref {item_ref_id} on day {day}"
        )));
    }
    Ok(())
}

/// Inserts a brand-new sight or reopens a closed one for the same
/// (target, auction, kind). Reversing the previous close under the SIGHT'S
/// OWN identity (not the incoming listing's) keeps the sales ledger honest:
/// a not-credited close must not keep inflating `closed`, and a credited
/// close was a phantom sale the moment the listing returned. Reversals book
/// on the same days the originals did (close event on the close day, credit
/// on the open day) so every window sum stays consistent. Returns `true`
/// when a fresh listing event must be booked: a brand-new sight, or a sight
/// that reopened under a DIFFERENT identity (the new identity never had an
/// open of its own - without booking one, its later credits would have no
/// denominator). Statements arrive pre-prepared (see `observe`).
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
    #[allow(clippy::type_complexity)]
    let prior: Option<(
        Option<i64>,
        i64,
        i64,
        i64,
        i64,
        String,
        i64,
        i64,
        i64,
        i64,
        i64,
        String,
        Option<i64>,
    )> = select_prior
        .query_row(rusqlite::params![target.as_str(), listing.auction_id, kind.as_str()], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
                row.get(12)?,
            ))
        })
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(backend("observe prior", other)),
        })?;
    let Some((
        closed_at,
        credited,
        prior_qty,
        prior_first_seen_at,
        prior_item_ref_id,
        prior_kind,
        prior_item_id,
        prior_species,
        prior_level,
        prior_quality,
        prior_breed,
        prior_band,
        prior_price,
    )) = prior
    else {
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
                listing.unit_price,
            ])
            .map_err(|e| backend("observe insert", e))?;
        return Ok(true);
    };
    if let Some(closed_at) = closed_at {
        // The prior close's ledger entries live under the sight's stored
        // identity and on the days they were booked - reverse exactly there.
        // The stored band is the band the close evaluated (nothing rewrites
        // it between close and reopen), so eligibility recomputes to the
        // same verdict the close booked.
        let prior_item = join_item_ref(
            &prior_kind,
            prior_item_id,
            prior_species,
            prior_level,
            prior_quality,
            prior_breed,
        );
        let prior_category = Category::derive(listing.kind, &prior_item);
        let prior_eligible = credited == 1
            || amtracker_core::domain::TimeLeftBand::parse(&prior_band).is_credit_eligible();
        bump_sales_row(
            tx,
            target,
            prior_item_ref_id,
            utc_day(closed_at),
            prior_category,
            &SalesDelta {
                listings: 0,
                closed: -1,
                credited: 0,
                qty_sold: if credited == 1 { -prior_qty } else { 0 },
                eligible: 0,
            },
        )?;
        // A credited close's priced units leave the close day's sold histogram
        // together with its quantity.
        if let Some(prior_price) = prior_price.filter(|_| credited == 1) {
            add_sold_prices(
                tx,
                target,
                prior_item_ref_id,
                utc_day(closed_at),
                prior_category,
                &BTreeMap::from([(prior_price, -prior_qty)]),
            )?;
        }
        bump_sales_row(
            tx,
            target,
            prior_item_ref_id,
            utc_day(prior_first_seen_at),
            prior_category,
            &SalesDelta {
                listings: 0,
                closed: 0,
                credited: -credited,
                qty_sold: 0,
                eligible: -i64::from(prior_eligible),
            },
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
            item_ref_id,
            listing.unit_price,
        ])
        .map_err(|e| backend("observe reopen", e))?;
    Ok(item_ref_id != prior_item_ref_id)
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
        category: Category,
        span: Span,
    ) -> Result<Option<amtracker_core::domain::CachedBlock>, StoreError> {
        let conn = self.open_transaction();
        let Some(item_ref_id) = lookup_item_ref(&conn, item)? else {
            return Ok(None);
        };
        let row = conn
            .query_row(
                "SELECT computed_at, block_json FROM metrics_cache
                 WHERE target_id = ?1 AND item_ref_id = ?2 AND category = ?3 AND span = ?4",
                rusqlite::params![target.as_str(), item_ref_id, category.as_str(), span.name()],
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
        category: Category,
        span: Span,
        block: &MetricBlock,
        computed_at: i64,
    ) -> Result<(), StoreError> {
        let conn = self.open_transaction();
        let item_ref_id = get_or_create_item_ref(&conn, item)?;
        let json = serde_json::to_string(&SerMetricBlock::from(block))
            .map_err(|e| backend("serialize block", e))?;
        conn.execute(
            "INSERT OR REPLACE INTO metrics_cache (target_id, item_ref_id, category, span, computed_at, block_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                target.as_str(),
                item_ref_id,
                category.as_str(),
                span.name(),
                computed_at,
                json
            ],
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

    fn invalidate_items(
        &self,
        target: &TargetId,
        category: Category,
        items: &[ItemRef],
    ) -> Result<(), StoreError> {
        /// Ids per statement — mirrors `sale_rates_by_id`'s chunking: one
        /// market-scale cycle stays a handful of deletes, far under the
        /// driver's bound-parameter ceiling.
        const CACHE_INVALIDATE_CHUNK: usize = 500;
        if items.is_empty() {
            return Ok(());
        }
        let conn = self.open_transaction();
        // Unknown identities have no cache rows: `put` resolves the same way.
        // One prepared statement serves the whole batch (`item_ids_in_order`,
        // same SQL as `lookup_item_ref`) — a prepare per item would dominate
        // the mutex hold on market-scale cycles.
        let ids: Vec<i64> = item_ids_in_order(&conn, items)?.into_iter().flatten().collect();
        for chunk in ids.chunks(CACHE_INVALIDATE_CHUNK) {
            // Placeholders are bound values, never interpolated input.
            let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
            let sql = format!(
                "DELETE FROM metrics_cache
                 WHERE target_id = ? AND category = ? AND item_ref_id IN ({placeholders})"
            );
            let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 2);
            params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
            params.push(rusqlite::types::Value::Text(category.as_str().to_string()));
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
        category: Category,
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
        // same rule `put` applies), so they read back as `None`. One prepared
        // statement serves the whole batch (`item_ids_in_order`) instead of a
        // prepare per item; the result order matches the input exactly.
        let ids = item_ids_in_order(&conn, items)?;
        let known: Vec<i64> = ids.iter().filter_map(|id| *id).collect();
        let mut by_id: HashMap<i64, amtracker_core::domain::CachedBlock> = HashMap::new();
        for chunk in known.chunks(CACHE_GET_CHUNK) {
            let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
            let sql = format!(
                "SELECT item_ref_id, computed_at, block_json FROM metrics_cache
                 WHERE target_id = ? AND category = ? AND span = ? AND item_ref_id IN ({placeholders})"
            );
            let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 3);
            params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
            params.push(rusqlite::types::Value::Text(category.as_str().to_string()));
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
        category: Category,
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
                "INSERT OR REPLACE INTO metrics_cache (target_id, item_ref_id, category, span, computed_at, block_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    target.as_str(),
                    item_ref_id,
                    category.as_str(),
                    span.name(),
                    computed_at,
                    json
                ],
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

    fn record_firing(
        &self,
        event: &amtracker_core::domain::NewAlertEvent,
    ) -> Result<i64, StoreError> {
        // One transaction: an event row without its cooldown anchor would
        // make the next evaluation pass re-fire and re-dispatch a duplicate.
        let conn = self.open_transaction();
        let tx = conn.unchecked_transaction().map_err(|e| backend("begin alert firing", e))?;
        let item_ref_id = get_or_create_item_ref(&tx, &event.item)?;
        let condition_json = serde_json::to_string(&SerAlertCondition::from(&event.condition))
            .map_err(|e| backend("serialize alert condition", e))?;
        tx.execute(
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
        let event_id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE alert_rule SET last_fired_at = ?1 WHERE id = ?2",
            rusqlite::params![event.fired_at, event.rule_id],
        )
        .map_err(|e| backend("alert rule mark fired", e))?;
        tx.commit().map_err(|e| backend("commit alert firing", e))?;
        Ok(event_id)
    }
}

impl AlertEventRepository for SqliteStore {
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
                      quality, icon, sell_price, item_level, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT (flavor, item_id) DO UPDATE SET
                     name = excluded.name,
                     item_class = COALESCE(excluded.item_class, item_class),
                     item_subclass = COALESCE(excluded.item_subclass, item_subclass),
                     inventory_slot = COALESCE(excluded.inventory_slot, inventory_slot),
                     quality = COALESCE(excluded.quality, quality),
                     icon = COALESCE(excluded.icon, icon),
                     sell_price = COALESCE(excluded.sell_price, sell_price),
                     item_level = COALESCE(excluded.item_level, item_level),
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
                    record.item_level,
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
                        icon, sell_price, item_level, fetched_at
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
                        item_level: row.get(8)?,
                        fetched_at: row.get(9)?,
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
            item_level: None,
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
        category: Category,
    ) -> Result<Option<DayFold>, StoreError> {
        let conn = self.open_transaction();
        let item_ref_id = lookup_item_ref(&conn, item)?;
        let Some(item_ref_id) = item_ref_id else {
            return Ok(None);
        };
        let json: Option<String> = conn
            .query_row(
                "SELECT fold_json FROM item_day INDEXED BY sqlite_autoindex_item_day_1
                 WHERE target_id = ?1 AND item_ref_id = ?2 AND day = ?3 AND category = ?4",
                rusqlite::params![target.as_str(), item_ref_id, day, category.as_str()],
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
        category: Category,
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
            // Pinned to the PK like every per-item `item_day` read: one seek
            // per id, never the category window (see `item_day_rows_for_item`).
            let sql = format!(
                "SELECT item_ref_id, day, fold_json FROM item_day INDEXED BY sqlite_autoindex_item_day_1
                 WHERE target_id = ? AND day >= ? AND day <= ? AND category = ?
                   AND item_ref_id IN ({placeholders})"
            );
            let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 4);
            params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
            params.push(rusqlite::types::Value::Integer(first_day));
            params.push(rusqlite::types::Value::Integer(last_day));
            params.push(rusqlite::types::Value::Text(category.as_str().to_string()));
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
        category: Category,
        fold: &DayFold,
    ) -> Result<(), StoreError> {
        let conn = self.open_transaction();
        let item_ref_id = get_or_create_item_ref(&conn, item)?;
        let ser = SerDayFold::from(fold);
        let json = serde_json::to_string(&ser).map_err(|e| backend("serialize fold", e))?;
        conn.execute(
            "INSERT OR REPLACE INTO item_day (target_id, item_ref_id, day, category, fold_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![target.as_str(), item_ref_id, day, category.as_str(), json],
        )
        .map_err(|e| backend("save fold", e))?;
        Ok(())
    }

    fn save_folds(
        &self,
        target: &TargetId,
        folds: &[(ItemRef, i64, Category, DayFold)],
    ) -> Result<(), StoreError> {
        if folds.is_empty() {
            return Ok(());
        }
        // One transaction + one prepared statement for the whole batch: the
        // per-call `prepare` and per-fold autocommit commit are exactly the
        // costs the cycle's bulk path exists to avoid.
        let conn = self.open_transaction();
        let tx = conn.unchecked_transaction().map_err(|e| backend("begin save_folds", e))?;
        let mut insert = tx
            .prepare(
                "INSERT OR REPLACE INTO item_day (target_id, item_ref_id, day, category, fold_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|e| backend("prepare save_folds", e))?;
        let mut item_ref_ids: HashMap<ItemRef, i64> = HashMap::new();
        for (item, day, category, fold) in folds {
            let item_ref_id = if let Some(id) = item_ref_ids.get(item) {
                *id
            } else {
                let id = get_or_create_item_ref(&tx, item)?;
                item_ref_ids.insert(item.clone(), id);
                id
            };
            let ser = SerDayFold::from(fold);
            let json = serde_json::to_string(&ser).map_err(|e| backend("serialize fold", e))?;
            insert
                .execute(rusqlite::params![
                    target.as_str(),
                    item_ref_id,
                    day,
                    category.as_str(),
                    json
                ])
                .map_err(|e| backend("save fold", e))?;
        }
        drop(insert);
        tx.commit().map_err(|e| backend("commit save_folds", e))?;
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
        // The class-summary cache would otherwise outlive its data: once a
        // snapshot's raw listings are pruned, its pinned view shows an empty
        // table while the cached rows keep populating the class dropdown.
        let conn = self.open_transaction();
        conn.execute(
            "DELETE FROM snapshot_class_summary WHERE snapshot_id NOT IN \
             (SELECT DISTINCT snapshot_id FROM listing)",
            [],
        )
        .map_err(|e| backend("prune class summaries", e))?;
        drop(conn);
        // The live-floor rows follow their listings out: a snapshot fetched
        // before the cutoff day has no raw rows left to summarize.
        prune_snapshot_floors(self, target, day)?;
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
            // `<sha>.json.gz` archive path, so a path surfaced via an old
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

    fn prune_sights_before(&self, target: &TargetId, day: i64) -> Result<u64, StoreError> {
        // Same batching discipline as `prune_raw_before`: every delete stays
        // short so the store mutex is released between statements.
        const PRUNE_BATCH: usize = 50_000;
        const PRUNE_BATCH_I64: i64 = 50_000;
        let mut removed = 0usize;
        loop {
            let conn = self.open_transaction();
            let batch = conn
                .execute(
                    "DELETE FROM sight WHERE rowid IN (
                         SELECT rowid FROM sight
                         WHERE target_id = ?1 AND closed_at IS NOT NULL AND closed_at < ?2
                         LIMIT ?3)",
                    rusqlite::params![target.as_str(), day * 86_400, PRUNE_BATCH_I64],
                )
                .map_err(|e| backend("prune sights", e))?;
            drop(conn);
            removed += batch;
            if batch < PRUNE_BATCH {
                break;
            }
        }
        Ok(removed as u64)
    }

    fn items_with_data(
        &self,
        target: &TargetId,
        since_day: i64,
        category: Category,
    ) -> Result<Vec<ItemRef>, StoreError> {
        let conn = self.open_transaction();
        let mut statement = conn
            .prepare(
                "SELECT DISTINCT r.kind, r.item_id, r.pet_species, r.pet_level, r.pet_quality, r.pet_breed
                 FROM item_day d JOIN item_ref r ON r.id = d.item_ref_id
                 WHERE d.target_id = ?1 AND d.day >= ?2 AND d.category = ?3
                 ORDER BY r.kind, r.item_id, r.pet_species",
            )
            .map_err(|e| backend("prepare items_with_data", e))?;
        let rows = statement
            .query(rusqlite::params![target.as_str(), since_day, category.as_str()])
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
        category: Category,
    ) -> Result<Vec<ItemDayRow>, StoreError> {
        let conn = self.open_transaction();
        let mut statement = conn
            .prepare(
                "SELECT d.fold_json, d.day, r.kind, r.item_id, r.pet_species, r.pet_level, r.pet_quality, r.pet_breed
                 FROM item_day d JOIN item_ref r ON r.id = d.item_ref_id
                 WHERE d.target_id = ?1 AND d.day >= ?2 AND d.category = ?3
                 ORDER BY d.day, r.kind, r.item_id",
            )
            .map_err(|e| backend("prepare item_day_rows", e))?;
        let rows = query_day_rows(
            &mut statement,
            rusqlite::params![target.as_str(), since_day, category.as_str()],
        )?;
        day_rows_from_stored(rows, category)
    }

    fn item_day_rows_for_item(
        &self,
        target: &TargetId,
        item: &ItemRef,
        since_day: i64,
        category: Category,
    ) -> Result<Vec<ItemDayRow>, StoreError> {
        let conn = self.open_transaction();
        let Some(item_ref_id) = lookup_item_ref(&conn, item)? else {
            return Ok(Vec::new());
        };
        // One primary-key range per item. The read is pinned to the PK:
        // without statistics the planner also favours `idx_item_day_window`
        // here, which would scan the whole category window for this single
        // identity instead of seeking it.
        let mut statement = conn
            .prepare(
                "SELECT d.fold_json, d.day, r.kind, r.item_id, r.pet_species, r.pet_level, r.pet_quality, r.pet_breed
                 FROM item_day d INDEXED BY sqlite_autoindex_item_day_1
                 JOIN item_ref r ON r.id = d.item_ref_id
                 WHERE d.target_id = ?1 AND d.item_ref_id = ?2 AND d.day >= ?3 AND d.category = ?4
                 ORDER BY d.day",
            )
            .map_err(|e| backend("prepare item_day_rows_for_item", e))?;
        let rows = query_day_rows(
            &mut statement,
            rusqlite::params![target.as_str(), item_ref_id, since_day, category.as_str()],
        )?;
        day_rows_from_stored(rows, category)
    }

    fn item_hour_cells(
        &self,
        target: &TargetId,
        item: &ItemRef,
        kind: PayloadKind,
        since_day: i64,
    ) -> Result<Vec<ItemHourRow>, StoreError> {
        let conn = self.open_transaction();
        let Some(item_ref_id) = lookup_item_ref(&conn, item)? else {
            return Ok(Vec::new());
        };
        // The PK (target, kind, item, day, hour) serves the whole filter and
        // hands rows sorted; only one row per observed hour cell crosses the
        // port (the fold pre-collapses the raw rows at ingest/backfill).
        let mut statement = conn
            .prepare(
                "SELECT day, hour, floor, mins FROM item_hour
                 WHERE target_id = ?1 AND item_ref_id = ?2 AND kind = ?3 AND day >= ?4
                 ORDER BY day, hour",
            )
            .map_err(|e| backend("prepare item_hour_cells", e))?;
        let rows = statement
            .query_map(
                rusqlite::params![target.as_str(), item_ref_id, kind.as_str(), since_day],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .map_err(|e| backend("query item_hour_cells", e))?;
        let mut out = Vec::new();
        for row in rows {
            let (day, hour, floor, mins_json) =
                row.map_err(|e| backend("read item_hour_cells", e))?;
            let mins: Vec<i64> =
                serde_json::from_str(&mins_json).map_err(|e| backend("parse item_hour mins", e))?;
            out.push(ItemHourRow { day, hour, floor, mins });
        }
        Ok(out)
    }

    fn prune_item_hours_before(&self, target: &TargetId, day: i64) -> Result<u64, StoreError> {
        // The `idx_item_hour_day(target_id, day)` index serves the range —
        // the PK cannot seek `day` (kind/item sit between), so without it
        // each daily delete would scan the target's whole retained fold.
        delete_item_hours_batched(self, "prune item hours", "day < ?2", target, day)
    }

    fn clear_item_hours_from(&self, target: &TargetId, day: i64) -> Result<u64, StoreError> {
        // The repair mirror of `prune_item_hours_before` (same index, same
        // batching): drop the cells from `day` on so the next backfill
        // rebuilds exactly those hours from the raw window.
        delete_item_hours_batched(self, "clear item hours", "day >= ?2", target, day)
    }

    fn backfill_item_hours(&self, since_day: i64) -> Result<u64, StoreError> {
        // One statement per (target, kind, day, HOUR) — see
        // `rebuild_item_hour`: each rebuild is driven by the hour's concrete
        // snapshot-id list through the `snapshot_id` indexes, so no statement
        // ever sorts or scans a whole day's/week's listing set. It is atomic
        // per hour, which keeps the skip invariant exact: an hour that holds
        // ANY cell is complete (interrupted passes resume at hour
        // granularity), and a day never gains raw rows retroactively
        // (`fetched_at` is ingest time), so a skipped hour cannot go stale.
        //
        // The horizon is what keeps the pass bounded — see
        // `unreconciled_item_hours`.
        let candidates = {
            let conn = self.open_transaction();
            unreconciled_item_hours(&conn, since_day)?
        };
        // Progress is logged per rebuilt hour: an operator watching a fresh
        // deployment must see the pass move instead of a silent gap between
        // the start and completion lines.
        tracing::info!(
            since_day,
            candidate_hours = candidates.len(),
            "item hour backfill: hour scan complete"
        );
        let mut written = 0u64;
        let mut failed = 0u32;
        for (target, kind, day, hour) in candidates {
            // One mutex hold per hour — see `rebuild_item_hour`.
            let started = std::time::Instant::now();
            let outcome = {
                let conn = self.open_transaction();
                rebuild_item_hour(&conn, &target, &kind, day, hour)
            };
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let (snapshots, cells) = match outcome {
                Ok(counts) => counts,
                Err(error) => {
                    // The hour's statement is atomic, so a failed hour holds
                    // no cell and stays a candidate for the next run; one bad
                    // hour (a busy store, a corrupt row) must not stall the
                    // rest of the window behind it.
                    failed += 1;
                    tracing::warn!(
                        target = %target,
                        kind = %kind,
                        day,
                        hour,
                        error = %error,
                        "item hour backfill: hour failed, continuing with the next hour"
                    );
                    continue;
                }
            };
            if cells == 0 {
                tracing::debug!(
                    target = %target,
                    kind = %kind,
                    day,
                    hour,
                    snapshots,
                    "item hour backfill: hour has no raw rows, nothing written"
                );
                continue;
            }
            written += cells;
            tracing::info!(
                target = %target,
                kind = %kind,
                day,
                hour,
                snapshots,
                cells,
                elapsed_ms,
                "item hour backfill: hour rebuilt"
            );
            // Reader fairness: a short pause between rebuilt hours lets
            // queued page renders and cycles take the store mutex between
            // the pass's statements.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if failed > 0 {
            tracing::warn!(
                failed,
                "item hour backfill: hours failed and stay unreconciled; the next run retries them"
            );
        }
        Ok(written)
    }
}

/// Minimum, nearest-rank p05, median and MAD of one day's sorted prices (the
/// display-table day-row statistics). Even counts take the midpoint, the
/// same rule as the metrics engine's blocks. Prices must be sorted ascending.
///
/// Direct indexing is safe here: `n > 0` is ensured by the early return and
/// `mid` and the p05 rank lie in `1..=n`, so every index is valid — a
/// `.get()` fallback would silently corrupt the statistics.
#[allow(clippy::indexing_slicing)]
fn price_stats_sorted(prices: &[i64]) -> (i64, i64, i64, i64) {
    let n = prices.len();
    if n == 0 {
        return (0, 0, 0, 0);
    }
    // Nearest-rank p05, same rule as the metrics engine: ceil(0.05·n)
    // clamped to [1, n]; `div_ceil(20)` is that rank in integer math and for
    // n ≥ 1 always lands in [1, n] (≤ 20 listings degenerate p05 → min).
    let p05 = prices[n.div_ceil(20) - 1];
    let mid = n.div_ceil(2);
    // Midpoints on even counts, exactly like the metrics engine: a day row's
    // median is the same statistic as its block's, so the chart and the block
    // beside it never disagree by half a listing (the old lower-middle pick
    // also shrank day-row MADs, inflating the ranking's stability component
    // on thin even-count days).
    let median =
        if n % 2 == 1 { prices[mid - 1] } else { i64::midpoint(prices[mid - 1], prices[mid]) };
    let mad = {
        let mut deviations: Vec<i64> = prices.iter().map(|p| (p - median).abs()).collect();
        deviations.sort_unstable();
        if n % 2 == 1 {
            deviations[mid - 1]
        } else {
            i64::midpoint(deviations[mid - 1], deviations[mid])
        }
    };
    (prices[0], p05, median, mad)
}

// ------------------------------------------------------------------ helpers

use std::collections::{
    BTreeMap,
    HashMap,
};

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

/// Stored snapshot kinds come only from `PayloadKind::as_str`, so anything
/// else is a drifted/corrupt row: failing loudly beats silently mis-scoping
/// snapshots into the wrong namespace (unlike `parse_status`, whose
/// degradation is deliberate and documented).
fn parse_kind(value: &str) -> Result<PayloadKind, StoreError> {
    match value {
        "auctions" => Ok(PayloadKind::Auctions),
        "commodities" => Ok(PayloadKind::Commodities),
        other => Err(StoreError::Backend(format!("unknown snapshot kind '{other}'"))),
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

/// Per-item listing summary built during one `record` pass (or a rebuild):
/// the numbers [`SnapshotFloor`] publishes.
#[derive(Clone, Copy)]
struct FloorAccumulator {
    floor_price: i64,
    floor_qty: i64,
    listing_count: u64,
    qty_sum: i64,
}

impl FloorAccumulator {
    fn new(price: i64) -> Self {
        Self { floor_price: price, floor_qty: 0, listing_count: 0, qty_sum: 0 }
    }

    /// Folds one listing in. Quantities are non-negative by construction; a
    /// corrupt negative value adds nothing instead of corrupting the sums.
    fn observe(&mut self, price: i64, quantity: i64) {
        if price < self.floor_price {
            self.floor_price = price;
            self.floor_qty = 0;
        }
        let quantity = quantity.max(0);
        if price == self.floor_price {
            self.floor_qty = self.floor_qty.saturating_add(quantity);
        }
        self.listing_count = self.listing_count.saturating_add(1);
        self.qty_sum = self.qty_sum.saturating_add(quantity);
    }
}

/// One `snapshot_item_floor` row joined to its identity columns.
struct StoredFloor {
    kind: String,
    item_id: i64,
    species: i64,
    level: i64,
    quality: i64,
    breed: i64,
    floor: SnapshotFloor,
}

fn stored_floor_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredFloor> {
    Ok(StoredFloor {
        kind: row.get(0)?,
        item_id: row.get(1)?,
        species: row.get(2)?,
        level: row.get(3)?,
        quality: row.get(4)?,
        breed: row.get(5)?,
        floor: SnapshotFloor {
            floor_price: row.get(6)?,
            floor_qty: row.get::<_, i64>(7)?.max(0),
            listing_count: u64::try_from(row.get::<_, i64>(8)?).unwrap_or(0),
            qty_sum: row.get(9)?,
        },
    })
}

/// Writes one snapshot's per-item listing summaries (`record`'s write
/// path). Rows go in item order so the inserts touch the (snapshot, item) key
/// pages locally.
fn insert_snapshot_floors(
    conn: &Connection,
    snapshot_id: i64,
    floors: &HashMap<i64, FloorAccumulator>,
) -> Result<(), StoreError> {
    let mut rows: Vec<(&i64, &FloorAccumulator)> = floors.iter().collect();
    rows.sort_unstable_by_key(|(item_ref_id, _)| **item_ref_id);
    let mut insert = conn
        .prepare(
            "INSERT OR REPLACE INTO snapshot_item_floor
                (snapshot_id, item_ref_id, floor_price, floor_qty, listing_count, qty_sum)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .map_err(|e| backend("prepare snapshot floor", e))?;
    for (item_ref_id, summary) in rows {
        insert
            .execute(rusqlite::params![
                snapshot_id,
                item_ref_id,
                summary.floor_price,
                summary.floor_qty,
                i64::try_from(summary.listing_count).unwrap_or(i64::MAX),
                summary.qty_sum,
            ])
            .map_err(|e| backend("insert snapshot floor", e))?;
    }
    Ok(())
}

fn snapshot_has_floors(conn: &Connection, snapshot_id: i64) -> Result<bool, StoreError> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM snapshot_item_floor WHERE snapshot_id = ?1)",
        rusqlite::params![snapshot_id],
        |row| row.get::<_, i64>(0),
    )
    .map(|flag| flag == 1)
    .map_err(|e| backend("probe snapshot floors", e))
}

/// Rebuilds a snapshot's floor rows from its retained listings, so a
/// snapshot recorded before the table existed converges on first read. One
/// bounded pass over that snapshot's own listing rows: `INDEXED BY` pins the
/// `snapshot_id` index — without statistics the planner also considers the
/// (target, item, snapshot) index, which this filter can only scan. `kind`
/// is `None` for the single-item read, which carries no kind to check.
fn rebuild_snapshot_floors(
    conn: &Connection,
    target: &TargetId,
    kind: Option<PayloadKind>,
    snapshot_id: i64,
) -> Result<(), StoreError> {
    let owner: Option<(String, String)> = conn
        .query_row(
            "SELECT target_id, kind FROM snapshots WHERE id = ?1",
            rusqlite::params![snapshot_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(backend("snapshot owner", other)),
        })?;
    let Some((owner_target, owner_kind)) = owner else {
        return Ok(());
    };
    if owner_target != target.as_str() || kind.is_some_and(|kind| owner_kind != kind.as_str()) {
        return Err(StoreError::Backend(format!(
            "snapshot {snapshot_id} belongs to another target/kind"
        )));
    }
    let tx = conn.unchecked_transaction().map_err(|e| backend("begin floor rebuild", e))?;
    let mut floors: HashMap<i64, FloorAccumulator> = HashMap::new();
    {
        let mut statement = tx
            .prepare(
                "SELECT item_ref_id, unit_price, quantity FROM listing
                 INDEXED BY idx_listing_snapshot_price
                 WHERE snapshot_id = ?1",
            )
            .map_err(|e| backend("prepare floor rebuild", e))?;
        let rows = statement
            .query_map(rusqlite::params![snapshot_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
            })
            .map_err(|e| backend("query floor rebuild", e))?;
        for row in rows {
            let (item_ref_id, price, quantity) =
                row.map_err(|e| backend("floor rebuild row", e))?;
            floors
                .entry(item_ref_id)
                .or_insert_with(|| FloorAccumulator::new(price))
                .observe(price, quantity);
        }
    }
    tracing::info!(
        snapshot_id,
        items = floors.len(),
        "snapshot floor rows rebuilt from retained listings"
    );
    insert_snapshot_floors(&tx, snapshot_id, &floors)?;
    tx.commit().map_err(|e| backend("commit floor rebuild", e))
}

/// Drops the floor rows of the target's snapshots fetched before `day` —
/// the cutoff `prune_raw_before` applies to their listings (a listing's day
/// is its snapshot's fetch day), so no summary outlives the rows it
/// summarizes. Walks the small `snapshots` table by id cursor with a
/// primary-key-seek delete per batch, never a scan of the floor table.
fn prune_snapshot_floors(
    store: &SqliteStore,
    target: &TargetId,
    day: i64,
) -> Result<(), StoreError> {
    /// Snapshot ids per delete statement.
    const FLOOR_PRUNE_BATCH: usize = 500;
    /// `i64` mirror of `FLOOR_PRUNE_BATCH` for the LIMIT bind parameter.
    const FLOOR_PRUNE_BATCH_I64: i64 = 500;
    let cutoff = day * 86_400;
    let mut after_id = 0i64;
    loop {
        let conn = store.open_transaction();
        let mut statement = conn
            .prepare(
                "SELECT id FROM snapshots
                 WHERE target_id = ?1 AND fetched_at < ?2 AND id > ?3
                 ORDER BY id LIMIT ?4",
            )
            .map_err(|e| backend("prepare prune snapshot floors", e))?;
        let ids: Vec<i64> = statement
            .query_map(
                rusqlite::params![target.as_str(), cutoff, after_id, FLOOR_PRUNE_BATCH_I64],
                |row| row.get(0),
            )
            .map_err(|e| backend("query prune snapshot floors", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| backend("prune snapshot floors row", e))?;
        drop(statement);
        let Some(newest) = ids.last().copied() else {
            return Ok(());
        };
        let placeholders = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(", ");
        let sql = format!("DELETE FROM snapshot_item_floor WHERE snapshot_id IN ({placeholders})");
        conn.execute(&sql, rusqlite::params_from_iter(ids.iter()))
            .map_err(|e| backend("prune snapshot floors", e))?;
        drop(conn);
        if ids.len() < FLOOR_PRUNE_BATCH {
            return Ok(());
        }
        after_id = newest;
    }
}

/// Upserts the snapshot's per-item minimums into the permanent intraday hour
/// fold (`record`'s write path): fresh cells start as the one-element array
/// `[min]`; an existing cell appends the new minimum to its JSON array and
/// keeps `floor` exact, so the fold holds every per-snapshot minimum for the
/// hour — exactly what the when-to-buy view's typical/floor math needs.
fn upsert_item_hours(
    conn: &Connection,
    target: &TargetId,
    kind: PayloadKind,
    day: i64,
    hour: i64,
    hour_mins: &HashMap<i64, i64>,
) -> Result<(), StoreError> {
    let mut upsert = conn
        .prepare(
            "INSERT INTO item_hour (target_id, kind, item_ref_id, day, hour, mins, floor)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (target_id, kind, item_ref_id, day, hour) DO UPDATE SET
               mins = json_insert(item_hour.mins, '$[#]', ?7),
               floor = MIN(item_hour.floor, ?7)",
        )
        .map_err(|e| backend("prepare item_hour", e))?;
    for (item_ref_id, min) in hour_mins {
        upsert
            .execute(rusqlite::params![
                target.as_str(),
                kind.as_str(),
                item_ref_id,
                day,
                hour,
                format!("[{min}]"),
                min,
            ])
            .map_err(|e| backend("upsert item_hour", e))?;
    }
    Ok(())
}

/// One (target, kind, day, hour) rebuild for `backfill_item_hours`: rewrites
/// every hour cell of that hour from ALL its raw-retained listings (exact,
/// atomic, idempotent — `mins` keeps one entry per snapshot). The concrete
/// snapshot-id list drives the scan through the `snapshot_id` indexes (a
/// subquery instead would make SQLite bloom-filter a full `listing` table
/// scan per statement), and the aggregation covers only that hour's rows, so
/// no statement ever sorts a whole day's listing set. Returns the number of
/// hour cells written.
///
/// The `IN` placeholder list is built from `ids.len()` only — never from
/// data — so the formatted SQL is as parameterized as the rest.
fn backfill_item_hour_cells(
    conn: &Connection,
    target: &str,
    kind: &str,
    day: i64,
    hour: i64,
    ids: &[i64],
) -> Result<u64, StoreError> {
    let placeholders = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "INSERT OR REPLACE INTO item_hour
           (target_id, kind, item_ref_id, day, hour, mins, floor)
         SELECT ?1, ?2, m.item_ref_id, ?3, ?4,
                json_group_array(m.min_price), MIN(m.min_price)
         FROM (SELECT l.snapshot_id, l.item_ref_id, MIN(l.unit_price) AS min_price
               FROM listing l
               WHERE l.snapshot_id IN ({placeholders})
               GROUP BY l.snapshot_id, l.item_ref_id) m
         GROUP BY m.item_ref_id"
    );
    let mut args: Vec<&dyn rusqlite::ToSql> = vec![&target, &kind, &day, &hour];
    args.extend(ids.iter().map(|id| id as &dyn rusqlite::ToSql));
    conn.execute(sql.as_str(), args.as_slice())
        .map(|cells| u64::try_from(cells).unwrap_or(0))
        .map_err(|e| backend("backfill item hours", e))
}

/// The batched delete both hour-fold pruning (`day < cutoff`) and the repair
/// clear (`day >= from`) share: one `DELETE … LIMIT` statement per mutex
/// hold, repeated until a batch comes back short, so the store mutex is
/// released between statements and concurrent readers interleave (the
/// discipline of `prune_raw_before`). `idx_item_hour_day(target_id, day)`
/// serves both ranges. `predicate` is a literal fragment over `?2` — never
/// data — so the formatted SQL is as parameterized as the rest.
fn delete_item_hours_batched(
    store: &SqliteStore,
    operation: &'static str,
    predicate: &'static str,
    target: &TargetId,
    day: i64,
) -> Result<u64, StoreError> {
    const BATCH: usize = 50_000;
    const BATCH_I64: i64 = 50_000;
    let sql = format!(
        "DELETE FROM item_hour WHERE rowid IN (
             SELECT rowid FROM item_hour WHERE target_id = ?1 AND {predicate} LIMIT ?3)"
    );
    let mut removed = 0usize;
    loop {
        let conn = store.open_transaction();
        let batch = conn
            .execute(sql.as_str(), rusqlite::params![target.as_str(), day, BATCH_I64])
            .map_err(|e| backend(operation, e))?;
        drop(conn);
        removed += batch;
        if batch < BATCH {
            break;
        }
    }
    Ok(removed as u64)
}

/// Discovery for `backfill_item_hours`: the `(target, kind, day, hour)`
/// tuples from `since_day` on that hold a snapshot but no fold cell, sorted
/// oldest first. One statement under the caller's mutex hold.
///
/// It reads `snapshots`, NEVER `listing` — one row per snapshot (thousands)
/// instead of one per raw listing (tens of millions on a full window) — and
/// `listing.day` is always `utc_day(fetched_at)` (`record` derives both from
/// the same timestamp), so the hour tuples match exactly.
///
/// The horizon is part of the contract, not an optimization. Snapshot rows
/// outlive their listings and the fold's retention outlives the raw window,
/// so without `since_day` every hour older than both would be a candidate on
/// every call, forever. Worse, the `NOT EXISTS` probe seeks only
/// `(target_id, kind)` — the PK cannot seek `day`/`hour` past `item_ref_id`,
/// and the planner prefers the covering PK over `idx_item_hour_day` — so a
/// non-matching probe scans the target's whole fold: each forgotten hour
/// would cost a full partition scan under the store mutex. Hours before
/// `since_day` have no raw rows to fold, so skipping them is exact.
fn unreconciled_item_hours(
    conn: &Connection,
    since_day: i64,
) -> Result<Vec<(String, String, i64, i64)>, StoreError> {
    let mut statement = conn
        .prepare(
            "SELECT DISTINCT s.target_id, s.kind, s.fetched_at / 86400,
                    (s.fetched_at % 86400) / 3600
             FROM snapshots s
             WHERE s.fetched_at >= ?1 * 86400
               AND NOT EXISTS (
                 SELECT 1 FROM item_hour h
                 WHERE h.target_id = s.target_id AND h.kind = s.kind
                   AND h.day = s.fetched_at / 86400
                   AND h.hour = (s.fetched_at % 86400) / 3600)",
        )
        .map_err(|e| backend("prepare backfill hours", e))?;
    let rows = statement
        .query_map(rusqlite::params![since_day], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|e| backend("query backfill hours", e))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| backend("read backfill hours", e))?);
    }
    out.sort_unstable();
    Ok(out)
}

/// One hour of `backfill_item_hours`: the hour's snapshot ids, then the cells
/// they drive, both on the caller's connection — one mutex hold, because the
/// list and the rebuild must see the same store (a `record()` landing between
/// two holds would be overwritten by the INSERT OR REPLACE). Returns
/// `(snapshots, cells written)`; an hour with no snapshot left (cannot happen
/// per discovery, guarded anyway) answers `(0, 0)` without a rebuild.
fn rebuild_item_hour(
    conn: &Connection,
    target: &str,
    kind: &str,
    day: i64,
    hour: i64,
) -> Result<(usize, u64), StoreError> {
    let ids = snapshot_ids_for_hour(conn, target, kind, day, hour)?;
    if ids.is_empty() {
        return Ok((0, 0));
    }
    let cells = backfill_item_hour_cells(conn, target, kind, day, hour, &ids)?;
    Ok((ids.len(), cells))
}

/// The hour's concrete snapshot ids. `idx_snapshots_target(target_id, kind,
/// …)` serves the target/kind range and `fetched_at` is filtered per row —
/// a few thousand rows per (target, kind), cheap next to the rebuild it
/// drives.
fn snapshot_ids_for_hour(
    conn: &Connection,
    target: &str,
    kind: &str,
    day: i64,
    hour: i64,
) -> Result<Vec<i64>, StoreError> {
    let mut statement = conn
        .prepare(
            "SELECT id FROM snapshots
             WHERE target_id = ?1 AND kind = ?2
               AND fetched_at >= ?3 * 86400 AND fetched_at < (?3 + 1) * 86400
               AND (fetched_at % 86400) / 3600 = ?4",
        )
        .map_err(|e| backend("prepare backfill snapshot ids", e))?;
    let rows = statement
        .query_map(rusqlite::params![target, kind, day, hour], |row| row.get::<_, i64>(0))
        .map_err(|e| backend("query backfill snapshot ids", e))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| backend("read backfill snapshot ids", e))?);
    }
    Ok(out)
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
    /// `default` so cache rows written before the ledger-recency upgrade
    /// deserialize as "absent" instead of failing the whole read.
    #[serde(default)]
    sale_rate_180d: Option<f64>,
    #[serde(default)]
    days_since_last_sale: Option<i64>,
    #[serde(default)]
    closures_since_last_sale: Option<i64>,
    #[serde(default)]
    qty_sold: Option<i64>,
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
            sale_rate_180d: block.sale_rate_180d,
            days_since_last_sale: block.days_since_last_sale,
            closures_since_last_sale: block.closures_since_last_sale,
            qty_sold: block.qty_sold,
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
            sale_rate_180d: ser.sale_rate_180d,
            days_since_last_sale: ser.days_since_last_sale,
            closures_since_last_sale: ser.closures_since_last_sale,
            qty_sold: ser.qty_sold,
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
    /// Mirror of `AlertCondition::FloorBelow`.
    FloorBelow {
        /// Threshold in copper per unit.
        copper: i64,
    },
    /// Mirror of `AlertCondition::FloorBelowValue`.
    FloorBelowValue {
        /// Threshold fraction of the 7-day value (0.0..=1.0).
        fraction: f64,
    },
    /// Mirror of `AlertCondition::NewListingBelow`.
    NewListingBelow {
        /// Threshold fraction of the 7-day value (0.0..=1.0).
        fraction: f64,
    },
    /// Mirror of `AlertCondition::SaleRateAbove`.
    SaleRateAbove {
        /// Threshold fraction (0.0..=1.0).
        rate: f64,
    },
    /// Mirror of `AlertCondition::DaysSinceLastSaleAbove`.
    DaysSinceLastSaleAbove {
        /// Threshold in whole days.
        days: i64,
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
            AlertCondition::FloorBelow { copper } => Self::FloorBelow { copper },
            AlertCondition::FloorBelowValue { fraction } => Self::FloorBelowValue { fraction },
            AlertCondition::NewListingBelow { fraction } => Self::NewListingBelow { fraction },
            AlertCondition::SaleRateAbove { rate } => Self::SaleRateAbove { rate },
            AlertCondition::DaysSinceLastSaleAbove { days } => {
                Self::DaysSinceLastSaleAbove { days }
            }
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
            SerAlertCondition::FloorBelow { copper } => Ok(AlertCondition::FloorBelow { copper }),
            SerAlertCondition::FloorBelowValue { fraction } => {
                Ok(AlertCondition::FloorBelowValue { fraction })
            }
            SerAlertCondition::NewListingBelow { fraction } => {
                Ok(AlertCondition::NewListingBelow { fraction })
            }
            SerAlertCondition::SaleRateAbove { rate } => Ok(AlertCondition::SaleRateAbove { rate }),
            SerAlertCondition::DaysSinceLastSaleAbove { days } => {
                Ok(AlertCondition::DaysSinceLastSaleAbove { days })
            }
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
            AutoVacuum::Off,
        )
        .expect("open");

        let conn = store.open_transaction();
        let enforced: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0)).unwrap();
        assert_eq!(enforced, 1, "child-table REFERENCES clauses must be live");

        drop(conn);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn test_store(name: &str) -> (std::path::PathBuf, SqliteStore) {
        test_store_auto(name, AutoVacuum::Off)
    }

    fn test_store_auto(name: &str, auto_vacuum: AutoVacuum) -> (std::path::PathBuf, SqliteStore) {
        // A per-process counter on top of the pid: an aborted run leaves its
        // directory behind, and Windows recycles pids eagerly enough for a
        // later run to reopen that stale database.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "amtracker-{name}-{}-{}-{}",
            std::process::id(),
            line!(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let store = SqliteStore::open(
            &dir.join("test.db"),
            &dir.join("raw"),
            std::sync::Arc::new(crate::clock::SystemClock) as std::sync::Arc<dyn ClockPort>,
            auto_vacuum,
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
            store
                .save_fold(&target, item, day, amtracker_core::domain::Category::Commodity, &fold)
                .expect("save fold");
        }
        let query: Vec<ItemRef> = vec![
            items[0].clone(),                  // known, in window
            ItemRef::Item { item_id: 99_999 }, // unknown
            items[1].clone(),                  // known, fold below the window
            items[600].clone(),                // known, last chunk entry
        ];

        let folds = store
            .load_folds(&target, &query, 45, 50, amtracker_core::domain::Category::Commodity)
            .expect("load folds");
        assert_eq!(folds.len(), 4, "positional alignment");
        assert_eq!(folds[0].len(), 1, "in-window fold loaded");
        assert_eq!(folds[0][0].0, 50);
        assert!(folds[1].is_empty(), "unknown identity loads empty");
        assert!(folds[2].is_empty(), "out-of-window fold filtered");
        assert_eq!(folds[3].len(), 1, "last chunk entry loaded");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A commodity listing row with an explicit auction id, band and price.
    fn priced_listing(auction_id: i64, item: &ItemRef, price: i64, qty: i64) -> NormalizedListing {
        NormalizedListing {
            auction_id,
            item_ref: item.clone(),
            kind: ListingKind::Commodity,
            quantity: qty,
            unit_price: price,
            bid: None,
            time_left: TimeLeftBand::Medium,
            context: None,
            bonus_lists: Vec::new(),
        }
    }

    /// v11 sold prices: a credited close books its sight's last-seen price and
    /// a partial fill its posted one; a reopened sight reverses the phantom
    /// sale's priced units; a sight last seen without a price (a pre-v11 row)
    /// sells unpriced. The histogram and `qty_sold` stay in step throughout.
    #[test]
    fn sold_prices_follow_credited_closes_fills_and_reversals() {
        let (dir, store) = test_store("sold-prices");
        let target = TargetId("eu-1602".into());
        let item = ItemRef::Item { item_id: 194_784 };
        // Near the real clock: the read path applies a trailing-day window.
        let at = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
        )
        .unwrap_or(0);
        let sold = |store: &SqliteStore, category: Category| -> Vec<(i64, i64)> {
            store
                .sold_price_each(&target, std::slice::from_ref(&item), 14, category)
                .expect("sold prices")
                .pop()
                .unwrap_or_default()
        };
        let commodity = Category::Commodity;

        // Two live stacks: A (10 at 120) and B (5 at 150).
        store
            .observe(
                &target,
                PayloadKind::Commodities,
                &[priced_listing(1, &item, 120, 10), priced_listing(2, &item, 150, 5)],
                1,
                at,
            )
            .expect("observe 1");
        // A drops to 4 units (6 bought at 120); C appears (2 at 300). B is gone.
        store
            .observe(
                &target,
                PayloadKind::Commodities,
                &[priced_listing(1, &item, 120, 4), priced_listing(3, &item, 300, 2)],
                2,
                at + 60,
            )
            .expect("observe 2");
        // B vanished a minute after a MEDIUM sighting: credited at 150.
        let credits =
            store.close_missing(&target, PayloadKind::Commodities, 2, at + 60).expect("close 2");
        assert_eq!(credits.len(), 1, "B's vanish credits");
        assert_eq!(sold(&store, commodity), vec![(120, 6), (150, 5)]);
        assert_eq!(store.qty_sold(&target, &item, 14, commodity), Ok(Some(11)));

        // B is back: the reopen reverses the phantom sale, priced units included.
        store
            .observe(
                &target,
                PayloadKind::Commodities,
                &[priced_listing(2, &item, 150, 5)],
                3,
                at + 120,
            )
            .expect("observe 3");
        assert_eq!(sold(&store, commodity), vec![(120, 6)]);
        assert_eq!(store.qty_sold(&target, &item, 14, commodity), Ok(Some(6)));

        // C was last seen before prices were recorded (a pre-v11 row): when
        // everything vanishes it sells unpriced, the others at their prices.
        store
            .open_transaction()
            .execute(
                "UPDATE sight SET last_unit_price = NULL
                 WHERE target_id = ?1 AND auction_id = 3 AND kind = ?2",
                rusqlite::params![target.as_str(), PayloadKind::Commodities.as_str()],
            )
            .expect("strip the recorded price");
        let credits =
            store.close_missing(&target, PayloadKind::Commodities, 4, at + 180).expect("close 4");
        assert_eq!(credits.len(), 3, "A, B and C all credit");
        assert_eq!(
            sold(&store, commodity),
            vec![(120, 10), (150, 5)],
            "no 300 bucket: C sold unpriced"
        );
        assert_eq!(store.qty_sold(&target, &item, 14, commodity), Ok(Some(17)));

        // Positional alignment: an unknown identity reads as an empty histogram,
        // and another category's ledger is separate.
        let unknown = ItemRef::Item { item_id: 999_999 };
        let rows = store
            .sold_price_each(&target, &[item.clone(), unknown], 14, commodity)
            .expect("sold prices");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec![(120, 10), (150, 5)]);
        assert_eq!(rows[1], Vec::<(i64, i64)>::new());
        assert_eq!(
            sold(&store, Category::Item),
            Vec::<(i64, i64)>::new(),
            "the item-AH ledger holds nothing"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Seeds one snapshot row with its listings (the snapshot-min test's
    /// fixture writer; target and day are the test's constants).
    fn seed_snapshot_min(
        conn: &Connection,
        item_ref_id: i64,
        snapshot_id: i64,
        fetched_at: i64,
        kind: &str,
        rows: &[(i64, i64)],
    ) {
        conn.execute(
            "INSERT INTO snapshots (id, target_id, kind, fetched_at, sha256, entry_count, dropped_count, status)
             VALUES (?1, 'eu-1602', ?2, ?3, ?4, ?5, 0, 'ok')",
            rusqlite::params![
                snapshot_id,
                kind,
                fetched_at,
                format!("sha-{snapshot_id}"),
                i64::try_from(rows.len()).unwrap_or(0)
            ],
        )
        .expect("seed snapshot");
        for (auction_id, price) in rows {
            conn.execute(
                "INSERT INTO listing (snapshot_id, target_id, day, auction_id, item_ref_id, quantity, unit_price, time_left)
                 VALUES (?1, 'eu-1602', 500, ?2, ?3, 1, ?4, 'LONG')",
                rusqlite::params![snapshot_id, auction_id, item_ref_id, price],
            )
            .expect("seed listing");
        }
    }

    #[test]
    fn item_hour_backfill_builds_cells_from_raw_rows() {
        let (dir, store) = test_store("snapshot-min");
        let target = TargetId("eu-1602".into());
        let item = ItemRef::Item { item_id: 77 };
        let day = 500_i64;
        {
            let conn = store.open_transaction();
            let item_ref_id = get_or_create_item_ref(&conn, &item).expect("item ref");
            // Snapshot 3 carries the identity in the WRONG payload kind — the
            // market scoping must keep it in its own kind's cell.
            seed_snapshot_min(
                &conn,
                item_ref_id,
                1,
                day * 86_400 + 2 * 3_600,
                "commodities",
                &[(10, 300), (11, 120), (12, 500)],
            );
            seed_snapshot_min(
                &conn,
                item_ref_id,
                2,
                day * 86_400 + 2 * 3_600 + 60,
                "commodities",
                &[(13, 150)],
            );
            seed_snapshot_min(
                &conn,
                item_ref_id,
                3,
                day * 86_400 + 5 * 3_600,
                "auctions",
                &[(14, 90)],
            );
            // A snapshot whose listings were all dropped: harmless — the
            // rebuild of its hour simply writes zero cells.
            seed_snapshot_min(
                &conn,
                item_ref_id,
                4,
                (day + 1) * 86_400 + 9 * 3_600,
                "commodities",
                &[],
            );
        }
        // Raw rows predate the fold: one backfill pass turns them into the
        // permanent hour cells, exactly once per (kind, item, hour).
        let written = store.backfill_item_hours(day).expect("backfill");
        assert_eq!(written, 2, "one commodities cell + one auctions cell");
        let rerun = store.backfill_item_hours(day).expect("backfill again");
        assert_eq!(rerun, 0, "reconciled hours are skipped; their cells stay untouched");

        let cells =
            store.item_hour_cells(&target, &item, PayloadKind::Commodities, day).expect("cells");
        assert_eq!(cells.len(), 1, "both hour-2 snapshots collapse into one cell");
        assert_eq!((cells[0].day, cells[0].hour), (day, 2));
        assert_eq!(cells[0].floor, 120, "min across the hour's snapshots");
        let mut mins = cells[0].mins.clone();
        mins.sort_unstable();
        assert_eq!(mins, [120, 150]);

        let auctions =
            store.item_hour_cells(&target, &item, PayloadKind::Auctions, day).expect("cells");
        assert_eq!(auctions.len(), 1, "wrong-kind snapshot stays out of the market");
        assert_eq!(auctions[0].mins, [90]);

        let beyond = store
            .item_hour_cells(&target, &item, PayloadKind::Commodities, day + 1)
            .expect("cells");
        assert!(beyond.is_empty(), "the since-day cutoff prunes old cells");
        let unknown = store
            .item_hour_cells(
                &target,
                &ItemRef::Item { item_id: 9_999 },
                PayloadKind::Commodities,
                day,
            )
            .expect("cells");
        assert!(unknown.is_empty(), "unseen identity answers empty, not an error");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn item_hour_backfill_resumes_per_hour_and_honors_the_horizon() {
        let (dir, store) = test_store("snapshot-min-horizon");
        let target = TargetId("eu-1602".into());
        let item = ItemRef::Item { item_id: 77 };
        let day = 500_i64;
        {
            let conn = store.open_transaction();
            let item_ref_id = get_or_create_item_ref(&conn, &item).expect("item ref");
            seed_snapshot_min(
                &conn,
                item_ref_id,
                1,
                day * 86_400 + 2 * 3_600,
                "commodities",
                &[(10, 300)],
            );
            seed_snapshot_min(
                &conn,
                item_ref_id,
                2,
                day * 86_400 + 5 * 3_600,
                "commodities",
                &[(11, 150)],
            );
            // The day before the horizon the passes below start.
            seed_snapshot_min(
                &conn,
                item_ref_id,
                3,
                (day - 1) * 86_400 + 2 * 3_600,
                "commodities",
                &[(12, 90)],
            );
        }
        assert_eq!(
            store.backfill_item_hours(day).expect("backfill"),
            2,
            "both of the day's hours, none of the day before the horizon"
        );
        // An interrupted pass leaves whole hours without cells: dropping one
        // hour's cell (the atomicity unit) makes exactly that hour rebuild,
        // and its sibling on the same day stays untouched.
        store
            .open_transaction()
            .execute("DELETE FROM item_hour WHERE hour = 5", [])
            .expect("drop one hour");
        assert_eq!(
            store.backfill_item_hours(day).expect("resume"),
            1,
            "only the missing hour rebuilds"
        );
        let cells =
            store.item_hour_cells(&target, &item, PayloadKind::Commodities, day).expect("cells");
        assert_eq!(cells.len(), 2, "the sibling hour stayed, the dropped hour is back");
        assert_eq!((cells[0].hour, cells[0].mins.as_slice()), (2, [300].as_slice()));
        assert_eq!((cells[1].hour, cells[1].mins.as_slice()), (5, [150].as_slice()));
        // Widening the horizon reaches the older day (`item_hour_cells` reads
        // `day >= since_day`, so the newer day's two cells come along).
        assert_eq!(store.backfill_item_hours(day - 1).expect("wider backfill"), 1);
        let older = store
            .item_hour_cells(&target, &item, PayloadKind::Commodities, day - 1)
            .expect("cells");
        assert_eq!(older.len(), 3, "the older day joins the two cells the newer day holds");
        assert_eq!((older[0].day, older[0].mins.as_slice()), (day - 1, [90].as_slice()));
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
            amtracker_core::domain::BlockLedger {
                sale_rate: Some(0.5),
                ..amtracker_core::domain::BlockLedger::NONE
            },
        )
        .expect("block from non-empty histogram");
        let blocks = [Some(block.clone()), None, Some(block)];
        store
            .put_many(
                &target,
                amtracker_core::domain::Category::Commodity,
                Span::D7,
                &items,
                &blocks,
                1_000,
            )
            .expect("put many");

        let cached = store
            .get_many(&target, amtracker_core::domain::Category::Commodity, Span::D7, &items)
            .expect("get many");
        assert!(cached[0].is_some(), "Some block cached");
        assert!(cached[1].is_none(), "None never cached");
        assert!(cached[2].is_some(), "third block cached");
        assert_eq!(cached[0].as_ref().expect("cached block").computed_at, 1_000);

        // A different span must not see the rows.
        let other = store
            .get_many(&target, amtracker_core::domain::Category::Commodity, Span::D1, &items)
            .expect("get other span");
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

    #[test]
    fn incremental_auto_vacuum_is_baked_into_fresh_headers() {
        let (dir, store) = test_store_auto("vacuum-header", AutoVacuum::Incremental);
        assert_eq!(SpaceMaintenance::auto_vacuum_mode(&store).expect("mode"), 2);
        drop(store);

        let (dir_off, store_off) = test_store_auto("vacuum-header-off", AutoVacuum::Off);
        assert_eq!(SpaceMaintenance::auto_vacuum_mode(&store_off).expect("mode"), 0);
        drop(store_off);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dir_off).ok();
    }

    #[test]
    fn space_stats_and_full_vacuum_reclaim_freed_pages() {
        let (dir, store) = test_store("vacuum-shrink");
        let stale = TargetId("eu-1602".into());
        {
            let conn = store.open_transaction();
            let item_ref_id =
                get_or_create_item_ref(&conn, &ItemRef::Item { item_id: 9 }).expect("item ref");
            for auction in 0..2_000 {
                conn.execute(
                    "INSERT INTO listing (snapshot_id, target_id, day, auction_id, item_ref_id,
                                          quantity, unit_price, bid, time_left, context, bonus_lists)
                     VALUES (1, ?1, 100, ?2, ?3, 1, 100, NULL, 'VERY_LONG', NULL, '[]')",
                    rusqlite::params![stale.as_str(), auction, item_ref_id],
                )
                .expect("insert listing");
            }
        }
        store.prune_raw_before(&stale, 101).expect("prune");

        let before = SpaceMaintenance::space_stats(&store).expect("stats before");
        assert!(before.freelist_count > 0, "deletes must leave free pages");
        SpaceMaintenance::full_vacuum(&store).expect("vacuum");
        let after = SpaceMaintenance::space_stats(&store).expect("stats after");
        assert_eq!(after.freelist_count, 0, "full vacuum empties the freelist");
        assert!(
            after.total_bytes() < before.total_bytes(),
            "file must shrink: {} -> {}",
            before.total_bytes(),
            after.total_bytes()
        );
        SpaceMaintenance::checkpoint_wal(&store).expect("checkpoint");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn archived_payloads_are_gzip_compressed_and_roundtrip() {
        let (dir, store) = test_store("archive-gzip");
        let raw: &[u8] = br#"{"auctions":[{"auc":1,"item":2589,"qty":20,"buy":100}]}"#;
        let sha = sha256_hex(raw);
        let path = store
            .archive_payload(&TargetId("eu-1602".into()), PayloadKind::Commodities, &sha, raw)
            .expect("archive");
        assert!(path.ends_with(".json.gz"), "compressed extension: {path}");
        let full = dir.join("raw").join(&path);
        assert!(full.exists(), "archive file written");
        let mut decoder = flate2::read::GzDecoder::new(std::fs::File::open(&full).expect("open"));
        let mut payload = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut payload).expect("gunzip");
        assert_eq!(payload, raw, "decompressed bytes match the payload");
        std::fs::remove_dir_all(&dir).ok();
    }
}

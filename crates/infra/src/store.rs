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
        ItemDayRow,
        ItemRef,
        MetricBlock,
        NameRecord,
        NewSnapshot,
        NewWatchlistEntry,
        NormalizedListing,
        PayloadKind,
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
const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS snapshots (
    id INTEGER PRIMARY KEY,
    target_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    source_last_modified INTEGER,
    sha256 TEXT NOT NULL,
    archive_path TEXT NOT NULL,
    entry_count INTEGER NOT NULL,
    dropped_count INTEGER NOT NULL,
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
    item_id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    item_class TEXT,
    quality TEXT,
    fetched_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_item_catalog_name ON item_catalog(name);

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

impl SqliteStore {
    /// Opens (creating if needed) the database and applies the schema.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] when the data or archive directories
    /// cannot be created, the database cannot be opened or configured (WAL,
    /// synchronous pragma), the schema batch cannot be applied, or a
    /// pre-kind (schema v1) database is opened — those cannot be migrated
    /// faithfully and must be rebuilt.
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
        Self::check_schema_version(&conn)?;
        conn.execute_batch(SCHEMA).map_err(|e| backend("schema", e))?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|e| backend("user_version", e))?;
        Ok(Self { conn: Mutex::new(conn), archive_dir: archive_dir.to_path_buf(), clock })
    }

    /// Refuses schema-v1 databases (a `sight` table without the `kind`
    /// column): old sighting rows cannot be assigned a payload kind, so a
    /// silent migration would fabricate sell-through history. Fresh files and
    /// current v2 files pass through.
    fn check_schema_version(conn: &Connection) -> Result<(), StoreError> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| backend("user_version", e))?;
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
            let tmp = full.with_extension("tmp");
            std::fs::write(&tmp, raw).map_err(|e| backend("archive write", e))?;
            std::fs::rename(&tmp, &full).map_err(|e| backend("archive rename", e))?;
        }
        relative
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| StoreError::Backend("archive path is not valid utf-8".into()))
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
        tx.execute(
            "INSERT INTO snapshots (target_id, kind, fetched_at, source_last_modified, sha256, archive_path, entry_count, dropped_count, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
                "SELECT id, target_id, kind, fetched_at, source_last_modified, entry_count, dropped_count, status
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
                status: parse_status(&row.get::<_, String>(7).map_err(|e| backend("status", e))?),
            }));
        }
        Ok(None)
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
            let updated = tx
                .execute(
                    "UPDATE sight SET last_seen_snapshot = ?4, last_seen_at = ?5, band = ?6, quantity = ?7
                     WHERE target_id = ?1 AND auction_id = ?2 AND kind = ?3 AND closed_at IS NULL",
                    rusqlite::params![
                        target.as_str(),
                        listing.auction_id,
                        kind.as_str(),
                        snapshot_id,
                        at,
                        listing.time_left.as_str(),
                        listing.quantity,
                    ],
                )
                .map_err(|e| backend("observe update", e))?;
            if updated == 0 {
                // No open sight: either brand new, or a closed one we must
                // reopen (the listing vanished for a cycle - maintenance,
                // truncated pull - and came back). Blind INSERTs would trip
                // the primary key forever after.
                if insert_or_reopen_sight(&tx, target, kind, listing, item_ref_id, snapshot_id, at)?
                {
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
            tx.execute(
                "UPDATE sight SET closed_at = ?3, credited = ?4
                 WHERE target_id = ?1 AND auction_id = ?2 AND closed_at IS NULL",
                rusqlite::params![target.as_str(), auction_id, at, i64::from(credited)],
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
        if items.is_empty() {
            return Ok(None);
        }
        let conn = self.open_transaction();
        let mut ids = Vec::with_capacity(items.len());
        for item in items {
            if let Some(id) = lookup_item_ref(&conn, item)? {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            return Ok(None);
        }
        // Placeholders are bound values, never interpolated input.
        let placeholders = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT COALESCE(SUM(credited), 0), COALESCE(SUM(listings), 0) FROM sales
             WHERE target_id = ? AND item_ref_id IN ({placeholders}) AND day >= ?"
        );
        let now_day = utc_day(self.clock.now());
        let cutoff_day = now_day - days + 1;
        let mut statement =
            conn.prepare(&sql).map_err(|e| backend("prepare sale_rate_multi", e))?;
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(ids.len() + 2);
        params.push(rusqlite::types::Value::Text(target.as_str().to_string()));
        for id in &ids {
            params.push(rusqlite::types::Value::Integer(*id));
        }
        params.push(rusqlite::types::Value::Integer(cutoff_day));
        let (credited, listings): (i64, i64) = statement
            .query_row(rusqlite::params_from_iter(params.iter()), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(|e| backend("sale_rate_multi", e))?;
        if listings == 0 {
            return Ok(None);
        }
        // Same display-metric precision trade as `sale_rate`.
        #[allow(clippy::cast_precision_loss)]
        Ok(Some(credited as f64 / listings as f64))
    }
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
#[allow(clippy::too_many_lines)]
fn insert_or_reopen_sight(
    tx: &rusqlite::Transaction<'_>,
    target: &TargetId,
    kind: PayloadKind,
    listing: &NormalizedListing,
    item_ref_id: i64,
    snapshot_id: i64,
    at: i64,
) -> Result<bool, StoreError> {
    let prior: Option<(Option<i64>, i64, i64)> = tx
        .query_row(
            "SELECT closed_at, credited, quantity FROM sight
             WHERE target_id = ?1 AND auction_id = ?2 AND kind = ?3",
            rusqlite::params![target.as_str(), listing.auction_id, kind.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(backend("observe prior", other)),
        })?;
    let Some((closed_at, credited, prior_qty)) = prior else {
        tx.execute(
            "INSERT INTO sight (target_id, auction_id, kind, item_ref_id, first_seen_snapshot, last_seen_snapshot, first_seen_at, last_seen_at, band, quantity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?6, ?7, ?8)",
            rusqlite::params![
                target.as_str(),
                listing.auction_id,
                kind.as_str(),
                item_ref_id,
                snapshot_id,
                at,
                listing.time_left.as_str(),
                listing.quantity,
            ],
        )
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
    tx.execute(
        "UPDATE sight SET closed_at = NULL, credited = 0,
                 first_seen_snapshot = ?4, last_seen_snapshot = ?4,
                 first_seen_at = ?5, last_seen_at = ?5, band = ?6, quantity = ?7
         WHERE target_id = ?1 AND auction_id = ?2 AND kind = ?3",
        rusqlite::params![
            target.as_str(),
            listing.auction_id,
            kind.as_str(),
            snapshot_id,
            at,
            listing.time_left.as_str(),
            listing.quantity,
        ],
    )
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

impl ItemCatalogRepository for SqliteStore {
    fn upsert(&self, records: &[NameRecord]) -> Result<(), StoreError> {
        let conn = self.open_transaction();
        for record in records {
            conn.execute(
                "INSERT INTO item_catalog (item_id, name, item_class, quality, fetched_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (item_id) DO UPDATE SET name = excluded.name, item_class = excluded.item_class, quality = excluded.quality, fetched_at = excluded.fetched_at",
                rusqlite::params![
                    record.item_id,
                    record.name,
                    record.item_class,
                    record.quality,
                    self.clock.now(),
                ],
            )
            .map_err(|e| backend("catalog upsert", e))?;
        }
        Ok(())
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, StoreError> {
        let conn = self.open_transaction();
        let sanitized = query.replace(['%', '_'], "");
        let pattern = format!("%{sanitized}%");
        let prefix = format!("{sanitized}%");
        let mut statement = conn
            .prepare(
                "SELECT item_id, name, item_class, quality FROM item_catalog
                 WHERE name LIKE ?1 COLLATE NOCASE
                 ORDER BY (name LIKE ?2 COLLATE NOCASE) DESC, name
                 LIMIT ?3",
            )
            .map_err(|e| backend("prepare search", e))?;
        let mut rows = statement
            .query(rusqlite::params![pattern, prefix, i64::try_from(limit).unwrap_or(i64::MAX)])
            .map_err(|e| backend("query search", e))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|e| backend("search next", e))? {
            out.push(SearchHit {
                item_id: row.get(0).map_err(|e| backend("search id", e))?,
                name: row.get(1).map_err(|e| backend("search name", e))?,
                item_class: row.get(2).map_err(|e| backend("search class", e))?,
                quality: row.get(3).map_err(|e| backend("search quality", e))?,
            });
        }
        Ok(out)
    }

    fn by_id(&self, item_id: i64) -> Result<Option<NameRecord>, StoreError> {
        let conn = self.open_transaction();
        let row = conn
            .query_row(
                "SELECT item_id, name, item_class, quality FROM item_catalog WHERE item_id = ?1",
                rusqlite::params![item_id],
                |row| {
                    Ok(NameRecord {
                        item_id: row.get(0)?,
                        name: row.get(1)?,
                        item_class: row.get(2)?,
                        quality: row.get(3)?,
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
        let conn = self.open_transaction();
        let removed = conn
            .execute(
                "DELETE FROM listing WHERE target_id = ?1 AND day < ?2",
                rusqlite::params![target.as_str(), day],
            )
            .map_err(|e| backend("prune listings", e))? as u64;
        // Best-effort archive cleanup: payload files of old snapshots.
        let mut statement = conn
            .prepare(
                "SELECT DISTINCT archive_path FROM snapshots WHERE target_id = ?1 AND fetched_at < ?2",
            )
            .map_err(|e| backend("prune query", e))?;
        let paths: Vec<String> = statement
            .query_map(rusqlite::params![target.as_str(), day * 86_400], |row| row.get(0))
            .map_err(|e| backend("prune rows", e))?
            .filter_map(Result::ok)
            .collect();
        for path in paths {
            let _ = std::fs::remove_file(self.archive_dir.join(path));
        }
        Ok(removed)
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
        let rows = statement
            .query(rusqlite::params![target.as_str(), since_day])
            .map_err(|e| backend("query item_day_rows", e))?
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
            .map_err(|e| backend("item_day_rows row", e))?;

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

//! SQLite repositories implementing `SnapshotRepository` and
//! `PriceHistoryRepository` over a single database file (WAL).
//!
//! Durability model (plan §3 / D3): snapshot metadata, item-day folds and the
//! item catalog are permanent; raw listing rows and archived payload files are
//! a bounded working set (`prune_raw_before`).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use amtracker_core::domain::{
    utc_day, ItemRef, MetricBlock, NewSnapshot, NormalizedListing, PayloadKind, SnapshotMeta,
    SnapshotStatus, Span, TargetId,
};
use amtracker_core::error::StoreError;
use amtracker_core::fold::DayFold;
use amtracker_core::ports::{MetricsCacheRepository, PriceHistoryRepository, SnapshotRepository};
use rusqlite::Connection;

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
";

pub struct SqliteStore {
    conn: Mutex<Connection>,
    archive_dir: PathBuf,
}

fn backend<E: std::fmt::Display>(context: &str, error: E) -> StoreError {
    StoreError::Backend(format!("{context}: {error}"))
}

impl SqliteStore {
    /// Opens (creating if needed) the database and applies the schema.
    pub fn open(database: &Path, archive_dir: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = database.parent() {
            std::fs::create_dir_all(parent).map_err(|e| backend("create data dir", e))?;
        }
        std::fs::create_dir_all(archive_dir).map_err(|e| backend("create archive dir", e))?;
        let conn = Connection::open(database).map_err(|e| backend("open sqlite", e))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| backend("journal_mode", e))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| backend("synchronous", e))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| backend("schema", e))?;
        Ok(Self {
            conn: Mutex::new(conn),
            archive_dir: archive_dir.to_path_buf(),
        })
    }

    fn open_transaction(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        Ok(self.conn.lock().unwrap())
    }

    fn archive_payload(
        &self,
        target: &TargetId,
        kind: PayloadKind,
        sha: &str,
        raw: &[u8],
    ) -> Result<String, StoreError> {
        let relative = PathBuf::from(target.as_str())
            .join(kind.as_str())
            .join(format!("{sha}.json"));
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

        let conn = self.open_transaction()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| backend("begin", e))?;
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
                snapshot.entry_count as i64,
                snapshot.dropped_count as i64,
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
            let item_ref_id = match item_ref_ids.get(&listing.item_ref) {
                Some(id) => *id,
                None => {
                    let id = get_or_create_item_ref(&tx, &listing.item_ref)?;
                    item_ref_ids.insert(&listing.item_ref, id);
                    id
                }
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
        let conn = self.open_transaction()?;
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
            return Ok(Some(SnapshotMeta {
                id: row.get(0).map_err(|e| backend("latest id", e))?,
                target: TargetId(
                    row.get::<_, String>(1)
                        .map_err(|e| backend("latest target", e))?,
                ),
                kind: parse_kind(
                    &row.get::<_, String>(2)
                        .map_err(|e| backend("latest kind", e))?,
                ),
                fetched_at: row.get(3).map_err(|e| backend("latest fetched_at", e))?,
                source_last_modified: row.get(4).map_err(|e| backend("last_modified", e))?,
                entry_count: row
                    .get::<_, i64>(5)
                    .map_err(|e| backend("entry_count", e))? as u64,
                dropped_count: row
                    .get::<_, i64>(6)
                    .map_err(|e| backend("dropped_count", e))?
                    as u64,
                status: parse_status(&row.get::<_, String>(7).map_err(|e| backend("status", e))?),
            }));
        }
        Ok(None)
    }
}

impl MetricsCacheRepository for SqliteStore {
    fn get(
        &self,
        target: &TargetId,
        item: &ItemRef,
        span: Span,
    ) -> Result<Option<amtracker_core::domain::CachedBlock>, StoreError> {
        let conn = self.open_transaction()?;
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
        Ok(Some(amtracker_core::domain::CachedBlock {
            block: ser.into(),
            computed_at,
        }))
    }

    fn put(
        &self,
        target: &TargetId,
        item: &ItemRef,
        span: Span,
        block: &MetricBlock,
        computed_at: i64,
    ) -> Result<(), StoreError> {
        let conn = self.open_transaction()?;
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
        let conn = self.open_transaction()?;
        conn.execute(
            "DELETE FROM metrics_cache WHERE target_id = ?1",
            rusqlite::params![target.as_str()],
        )
        .map_err(|e| backend("metrics cache invalidate", e))?;
        Ok(())
    }
}

impl PriceHistoryRepository for SqliteStore {
    fn load_fold(
        &self,
        target: &TargetId,
        item: &ItemRef,
        day: i64,
    ) -> Result<Option<DayFold>, StoreError> {
        let conn = self.open_transaction()?;
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
        let conn = self.open_transaction()?;
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
        let conn = self.open_transaction()?;
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
            .query_map(rusqlite::params![target.as_str(), day * 86_400], |row| {
                row.get(0)
            })
            .map_err(|e| backend("prune rows", e))?
            .filter_map(|r| r.ok())
            .collect();
        for path in paths {
            let _ = std::fs::remove_file(self.archive_dir.join(path));
        }
        Ok(removed)
    }
}

// ------------------------------------------------------------------ helpers

use std::collections::HashMap;

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
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
    match value {
        "parse_failed" => SnapshotStatus::ParseFailed,
        "http_error" => SnapshotStatus::HttpFailed,
        _ => SnapshotStatus::Ok,
    }
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
        .query(rusqlite::params![
            kind, item_id, species, level, quality, breed
        ])
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
        ItemRef::Pet {
            species,
            level,
            quality,
            breed,
        } => (
            "pet",
            0,
            *species,
            *level as i64,
            *quality as i64,
            *breed as i64,
        ),
    }
}

// serde mirrors of the fold state (infra-owned serialization format)

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum SerItemRef {
    Item {
        item_id: i64,
    },
    Pet {
        species: i64,
        level: i32,
        quality: i32,
        breed: i32,
    },
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
            SerItemRef::Pet {
                species,
                level,
                quality,
                breed,
            } => ItemRef::Pet {
                species,
                level,
                quality,
                breed,
            },
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
            ItemRef::Pet {
                species,
                level,
                quality,
                breed,
            } => SerItemRef::Pet {
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

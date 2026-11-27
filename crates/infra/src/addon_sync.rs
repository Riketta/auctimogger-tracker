//! `addon-sync` adapters (`addon/SPEC.md`, section 6.7): the tracker's addon
//! surface over HTTP and the addon folder on disk. Both implement the ports
//! of [`amtracker_core::services::addon_sync`], which owns placement and
//! verification; this module owns the wire — what the addon routes,
//! `/api/health` and `/feed.lua` look like — and how a file lands on disk
//! without ever being seen half-written.

use std::{
    io::Read as _,
    path::PathBuf,
    time::Duration,
};

use amtracker_core::{
    config::is_valid_target_id,
    error::SyncError,
    ports::{
        AddonFolderPort,
        TrackerAddonPort,
    },
    services::addon_sync::{
        valid_bundle_path,
        AddonManifest,
        ManifestFile,
    },
};
use serde::Deserialize;

/// Response caps in bytes: a body at or past its cap is refused rather than
/// read to the end.
const MANIFEST_LIMIT: u64 = 1 << 20;
const HEALTH_LIMIT: u64 = 8 << 20;
const FILE_LIMIT: u64 = 16 << 20;
const CARRIER_LIMIT: u64 = 64 << 20;

/// The shortest connect timeout: a tracker that is down fails fast.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

// ------------------------------------------------------------- the client

/// The tracker's addon surface over HTTP: the two `/api/addon` routes, the
/// target list of `/api/health` and the published carriers.
#[derive(Debug)]
pub struct HttpTrackerAddon {
    agent: ureq::Agent,
    base: String,
}

impl HttpTrackerAddon {
    /// The client for the tracker at `base` (a leading `http://` or
    /// `https://`, a trailing slash tolerated), with `timeout` as the overall
    /// timeout of every request.
    ///
    /// # Errors
    ///
    /// Returns `SyncError::Config` when `base` is not an `http://` or
    /// `https://` address.
    pub fn new(base: &str, timeout: Duration) -> Result<Self, SyncError> {
        let base = base.trim().trim_end_matches('/');
        let lower = base.to_ascii_lowercase();
        let scheme = ["http://", "https://"].into_iter().find(|scheme| lower.starts_with(scheme));
        let Some(scheme) = scheme else {
            return Err(SyncError::Config(format!(
                "--url '{base}' is not an http:// or https:// address (for example http://127.0.0.1:8080)"
            )));
        };
        if base.len() == scheme.len() {
            return Err(SyncError::Config(format!(
                "--url '{base}' names no host (for example http://127.0.0.1:8080)"
            )));
        }
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(timeout.min(CONNECT_TIMEOUT))
            .timeout(timeout)
            .build();
        Ok(Self { agent, base: base.to_owned() })
    }

    /// One GET, its body read whole up to `limit` bytes.
    fn get(&self, url: &str, limit: u64, what: &str) -> Result<Vec<u8>, SyncError> {
        let response = match self.agent.get(url).call() {
            Ok(response) => response,
            Err(ureq::Error::Status(status, _)) => {
                // The body is dropped: the tracker's error documents carry
                // nothing beyond the status.
                let hint = if status == 404 { " (not found)" } else { "" };
                return Err(SyncError::Remote(format!("{what}: HTTP {status}{hint}")));
            }
            Err(error) => return Err(SyncError::Remote(format!("{what}: {error}"))),
        };
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| SyncError::Remote(format!("{what}: read failed: {error}")))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
            return Err(SyncError::Remote(format!("{what}: larger than {limit} bytes")));
        }
        Ok(bytes)
    }
}

/// Whether `text` is a lower- or upper-case hex SHA-256 (64 digits).
fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Checks a downloaded addon file against its manifest entry. The port hands
/// out verified files only; the service adds nothing here.
fn verify_addon_file(file: &ManifestFile, bytes: &[u8]) -> Result<(), SyncError> {
    if !is_sha256_hex(&file.sha256) {
        return Err(SyncError::Verify(format!(
            "manifest entry '{}' carries no well-formed sha256",
            file.path
        )));
    }
    if crate::store::sha256_hex(bytes) != file.sha256.to_ascii_lowercase() {
        return Err(SyncError::Verify(format!(
            "addon file '{}' does not match its manifest sha256",
            file.path
        )));
    }
    Ok(())
}

impl TrackerAddonPort for HttpTrackerAddon {
    fn manifest(&self) -> Result<AddonManifest, SyncError> {
        let url = format!("{}/api/addon/manifest", self.base);
        parse_manifest(&self.get(&url, MANIFEST_LIMIT, "addon manifest")?)
    }

    fn addon_file(&self, file: &ManifestFile) -> Result<Vec<u8>, SyncError> {
        let url = format!("{}/api/addon/file/{}", self.base, file.path);
        let bytes = self.get(&url, FILE_LIMIT, &format!("addon file '{}'", file.path))?;
        verify_addon_file(file, &bytes)?;
        Ok(bytes)
    }

    fn market_targets(&self) -> Result<Vec<String>, SyncError> {
        let url = format!("{}/api/health", self.base);
        parse_health_targets(&self.get(&url, HEALTH_LIMIT, "target list (health)")?)
    }

    fn carrier(&self, target: &str) -> Result<Vec<u8>, SyncError> {
        if !is_valid_target_id(target) {
            return Err(SyncError::Remote(format!(
                "carrier '{target}': not a tracker target id ([a-z0-9][a-z0-9._-]*)"
            )));
        }
        let url = format!("{}/feed.lua?target={target}", self.base);
        self.get(&url, CARRIER_LIMIT, &format!("carrier '{target}'"))
    }
}

// --------------------------------------------------------------- decoding

#[derive(Deserialize)]
struct ManifestJson {
    addon: String,
    version: String,
    #[serde(rename = "feedVersion")]
    feed_version: u32,
    files: Vec<ManifestFileJson>,
}

#[derive(Deserialize)]
struct ManifestFileJson {
    path: String,
    sha256: String,
    size: u64,
}

/// Decodes `GET /api/addon/manifest`.
///
/// # Errors
///
/// Returns `SyncError::Remote` when the document is not the manifest shape.
pub fn parse_manifest(bytes: &[u8]) -> Result<AddonManifest, SyncError> {
    let json: ManifestJson = serde_json::from_slice(bytes)
        .map_err(|error| SyncError::Remote(format!("addon manifest rejected: {error}")))?;
    Ok(AddonManifest {
        addon: json.addon,
        version: json.version,
        feed_version: json.feed_version,
        files: json
            .files
            .into_iter()
            .map(|file| ManifestFile { path: file.path, sha256: file.sha256, size: file.size })
            .collect(),
    })
}

#[derive(Deserialize)]
struct HealthJson {
    targets: Vec<HealthTargetJson>,
}

#[derive(Deserialize)]
struct HealthTargetJson {
    target: String,
}

/// Decodes `GET /api/health` into its target ids, in the tracker's order.
///
/// # Errors
///
/// Returns `SyncError::Remote` when the document is not the health shape.
pub fn parse_health_targets(bytes: &[u8]) -> Result<Vec<String>, SyncError> {
    let json: HealthJson = serde_json::from_slice(bytes)
        .map_err(|error| SyncError::Remote(format!("target list rejected: {error}")))?;
    Ok(json.targets.into_iter().map(|target| target.target).collect())
}

// --------------------------------------------------------------- the folder

/// The addon's folder on disk: `<Interface/AddOns>/AuctiMoggerDatabase`.
pub struct AddonFolder {
    root: PathBuf,
}

impl AddonFolder {
    /// The folder rooted at `root`, created on the first write.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The absolute path of a manifest entry. The same plain-path rule as the
    /// service applies here again, so nothing can be joined outside the root
    /// whatever a caller passes.
    fn resolve(&self, path: &str) -> Result<PathBuf, SyncError> {
        if !valid_bundle_path(path) {
            return Err(SyncError::Io(format!("'{path}' is not a plain addon path")));
        }
        Ok(self.root.join(path))
    }
}

impl AddonFolderPort for AddonFolder {
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>, SyncError> {
        let full = self.resolve(path)?;
        std::fs::read(&full).map(Some).or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(SyncError::Io(format!("read {}: {error}", full.display())))
            }
        })
    }

    fn write(&self, path: &str, bytes: &[u8]) -> Result<(), SyncError> {
        let full = self.resolve(path)?;
        let Some(parent) = full.parent() else {
            return Err(SyncError::Io(format!("{} has no parent folder", full.display())));
        };
        std::fs::create_dir_all(parent)
            .map_err(|error| SyncError::Io(format!("create {}: {error}", parent.display())))?;
        // Unique tmp name per call, beside the target: the rename that
        // follows is atomic on every platform the game runs on, so the
        // client never loads a half-written file.
        let tmp = full.with_extension(format!("tmp-{}", crate::store::unique_tmp_suffix()));
        let write = std::fs::write(&tmp, bytes)
            .and_then(|()| std::fs::rename(&tmp, &full))
            .map_err(|error| SyncError::Io(format!("write {}: {error}", full.display())));
        if write.is_err() {
            // Best effort: a leftover tmp file is never loaded (it is no .lua
            // of the TOC), but it should not linger either.
            let _ = std::fs::remove_file(&tmp);
        }
        write
    }
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap, panic and index: fixture-driven inputs.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

    use std::{
        collections::BTreeMap,
        path::PathBuf,
        sync::atomic::{
            AtomicU64,
            Ordering,
        },
        time::Duration,
    };

    use amtracker_core::{
        domain::{
            Category,
            DatabaseEntry,
            DatabaseSnapshot,
            Flavor,
            ItemRecord,
            ItemRef,
            MarketMeta,
            MetricBlock,
            Span,
            TargetId,
        },
        error::SyncError,
        services::addon_sync::{
            read_carrier_header,
            verify_carrier,
            ManifestFile,
        },
    };

    use super::{
        parse_health_targets,
        parse_manifest,
        verify_addon_file,
        AddonFolder,
        HttpTrackerAddon,
    };
    use crate::store::sha256_hex;

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "amtracker-addon-sync-{}-{}-{}",
            tag,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        crate::testdir::fresh(&dir).expect("create temp dir")
    }

    #[test]
    fn a_manifest_reads_back() {
        let body = format!(
            r#"{{"addon":"AuctiMoggerDatabase","version":"0.1.0","feedVersion":4,"files":[{{"path":"AuctiMoggerDatabase.toc","sha256":"{}","size":40}},{{"path":"Core/pack.lua","sha256":"{}","size":1200}}]}}"#,
            sha256_hex(b"toc"),
            sha256_hex(b"pack")
        );
        let manifest = parse_manifest(body.as_bytes()).unwrap();
        assert_eq!(manifest.addon, "AuctiMoggerDatabase");
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.feed_version, 4);
        assert_eq!(manifest.files.len(), 2);
        assert_eq!(manifest.files[1].path, "Core/pack.lua");
        assert_eq!(manifest.files[1].size, 1200);
        assert_eq!(manifest.files[1].sha256, sha256_hex(b"pack"));

        for broken in [
            "",
            "[]",
            r#"{"addon":"AuctiMoggerDatabase"}"#,
            r#"{"addon":"AuctiMoggerDatabase","version":"0.1.0","feedVersion":"four","files":[]}"#,
            r#"{"addon":"AuctiMoggerDatabase","version":"0.1.0","feedVersion":4,"files":[{"path":"x"}]}"#,
        ] {
            assert!(parse_manifest(broken.as_bytes()).is_err(), "{broken}");
        }
    }

    #[test]
    fn health_targets_read_in_order() {
        let body = r#"{"targets":[{"target":"beta","kinds":[]},{"target":"alpha","kinds":[{"kind":"commodities","latest":null,"ageSeconds":null}]}]}"#;
        assert_eq!(parse_health_targets(body.as_bytes()).unwrap(), vec!["beta", "alpha"]);
        assert!(parse_health_targets(b"<html>").is_err());
        assert!(parse_health_targets(br#"{"targets":"none"}"#).is_err());
    }

    #[test]
    fn the_client_refuses_a_base_without_a_scheme_or_host() {
        assert!(HttpTrackerAddon::new("http://127.0.0.1:8080", Duration::from_secs(5)).is_ok());
        assert!(HttpTrackerAddon::new(" https://tracker.example/ ", Duration::from_secs(5)).is_ok());
        for base in ["127.0.0.1:8080", "ftp://x", "http://", "https://", "", "tracker.example"] {
            let error = HttpTrackerAddon::new(base, Duration::from_secs(5)).unwrap_err();
            assert!(matches!(error, SyncError::Config(_)), "{base}: {error}");
        }
    }

    #[test]
    fn a_download_is_held_to_its_manifest_entry() {
        let ok =
            ManifestFile { path: "Core/pack.lua".into(), sha256: sha256_hex(b"pack"), size: 4 };
        assert_eq!(verify_addon_file(&ok, b"pack"), Ok(()));
        let upper = ManifestFile { sha256: sha256_hex(b"pack").to_ascii_uppercase(), ..ok.clone() };
        assert_eq!(verify_addon_file(&upper, b"pack"), Ok(()), "hex case does not matter");

        let error = verify_addon_file(&ok, b"puck").unwrap_err();
        assert_eq!(
            error,
            SyncError::Verify(
                "addon file 'Core/pack.lua' does not match its manifest sha256".into()
            )
        );
        let malformed = ManifestFile { sha256: "nothex".into(), ..ok };
        let error = verify_addon_file(&malformed, b"pack").unwrap_err();
        assert_eq!(
            error,
            SyncError::Verify(
                "manifest entry 'Core/pack.lua' carries no well-formed sha256".into()
            )
        );
    }

    #[test]
    fn the_folder_writes_and_reads_atomically_and_stays_inside_its_root() {
        use amtracker_core::ports::AddonFolderPort;

        let dir = temp_dir("folder");
        let root = dir.join("AuctiMoggerDatabase");
        let folder = AddonFolder::new(root.clone());
        assert_eq!(folder.read("Core/pack.lua").unwrap(), None, "nothing exists yet");

        folder.write("Core/pack.lua", b"pack").unwrap();
        assert_eq!(folder.read("Core/pack.lua").unwrap(), Some(b"pack".to_vec()));
        assert_eq!(std::fs::read(root.join("Core").join("pack.lua")).unwrap(), b"pack");

        // A second write replaces the file and leaves no temporary behind.
        folder.write("Core/pack.lua", b"pack v2").unwrap();
        assert_eq!(std::fs::read(root.join("Core").join("pack.lua")).unwrap(), b"pack v2");
        let leftovers: Vec<String> = std::fs::read_dir(root.join("Core"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["pack.lua".to_owned()]);

        for escape in ["../outside.lua", "/etc/passwd", "Core/../../x", "Core\\..\\x", ""] {
            assert!(folder.read(escape).is_err(), "read {escape}");
            assert!(folder.write(escape, b"x").is_err(), "write {escape}");
        }
        assert!(!root.join("Core").join("pack.lua.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// One market with a single commodity record: enough shape for a carrier.
    fn snapshot(tracker_version: &str, target: &str) -> DatabaseSnapshot {
        let mut spans = BTreeMap::new();
        spans.insert(
            Span::D7,
            MetricBlock {
                n: 5,
                min: 100,
                p05: 100,
                p25: 200,
                median: 300,
                p75: 400,
                p95: 500,
                max: 500,
                mad: 8,
                vwap: 310,
                qty_sum: 50,
                listing_count: 5,
                sale_rate: Some(0.5),
                sale_rate_180d: None,
                days_since_last_sale: None,
                closures_since_last_sale: None,
                qty_sold: None,
            },
        );
        DatabaseSnapshot {
            target: TargetId(target.into()),
            market: MarketMeta {
                region: Some("eu".into()),
                ruleset_id: None,
                house_id: Some(111),
                connected_realm_id: None,
                flavor: Flavor::default(),
            },
            tracker_version: tracker_version.into(),
            generated_at: 1_800_000_000,
            auctions_at: Some(1_799_999_000),
            commodities_at: None,
            entries: vec![DatabaseEntry {
                item: ItemRef::Item { item_id: 194_784 },
                category: Category::Commodity,
                record: ItemRecord {
                    spans,
                    live: None,
                    value: None,
                    sold_count: None,
                    expired_count: None,
                    supply_days: None,
                    low30: None,
                    low_rank_30d_bp: None,
                    lows: None,
                    vendor_price: None,
                    craft: None,
                    deal: None,
                },
            }],
        }
    }

    #[test]
    fn a_rendered_carrier_passes_the_verification_it_will_get() {
        let bytes =
            crate::feed::FeedCodec::carrier(&snapshot("0.1.0", "forever-eu-pvp-alliance")).unwrap();
        let header = verify_carrier(&bytes, "forever-eu-pvp-alliance", 1_800_000_300).unwrap();
        assert_eq!(header.version, 4);
        assert_eq!(header.tracker_version, "0.1.0");
        assert_eq!(header.target, "forever-eu-pvp-alliance");
        assert_eq!(header.generated_at, 1_800_000_000);

        // A header value the renderer has to escape reads back whole.
        let bytes = crate::feed::FeedCodec::carrier(&snapshot("0.1.0 \"x\"\\y", "us-4")).unwrap();
        let header = read_carrier_header(&bytes).unwrap();
        assert_eq!(header.tracker_version, "0.1.0 \"x\"\\y");
        assert_eq!(header.target, "us-4");

        let error = verify_carrier(&bytes, "other", 1_800_000_300).unwrap_err();
        assert_eq!(
            error,
            SyncError::Verify("the carrier is for market 'us-4' and not for 'other'".into())
        );
    }
}

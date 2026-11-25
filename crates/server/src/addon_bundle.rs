//! The addon bundle: every static file of `addon/AuctiMoggerDatabase/`
//! embedded in the server binary with `include_str!`, so the addon a tracker
//! serves always matches the feed format it serves (`addon/SPEC.md`, section
//! 6.7). `GET /api/addon/manifest` lists the files with their SHA-256 and
//! size, and `GET /api/addon/file/<path>` serves one of them. The drift test
//! (`tests/addon_bundle.rs`) fails when this table and the folder on disk
//! disagree in either direction.

use std::sync::OnceLock;

use amtracker_core::{
    domain::DatabaseSnapshot,
    services::addon_sync::ADDON_NAME,
};
use serde_json::{
    json,
    Value,
};

/// How long a cache may keep what it fetched: the files are embedded per
/// binary and their URLs carry no version, so every cache must ask again.
pub const CACHE_CONTROL: &str = "no-cache";

/// One embedded addon file.
#[derive(Debug, PartialEq, Eq)]
pub struct AddonFile {
    /// Path inside the addon folder, `/`-separated.
    pub path: &'static str,
    /// The file's bytes: every addon file is UTF-8 text.
    pub bytes: &'static str,
}

/// Embeds one file of `addon/AuctiMoggerDatabase/` under its own path.
macro_rules! addon_file {
    ($path:literal) => {
        AddonFile {
            path: $path,
            bytes: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../addon/AuctiMoggerDatabase/",
                $path
            )),
        }
    };
}

/// Every file of the addon, in the order its TOC loads them, the TOC first.
/// A new addon file is added here in the change that creates it: the drift
/// test fails until it is.
pub const FILES: &[AddonFile] = &[
    addon_file!("AuctiMoggerDatabase.toc"),
    addon_file!("Core/pack.lua"),
    addon_file!("Core/record.lua"),
    addon_file!("Core/market.lua"),
    addon_file!("Core/settings.lua"),
    addon_file!("Core/format.lua"),
    addon_file!("Core/resolve.lua"),
    addon_file!("Core/tooltip_lines.lua"),
    addon_file!("Core/anchor.lua"),
    addon_file!("Core/panel_rows.lua"),
    addon_file!("Adapters/market_files.lua"),
    addon_file!("Market1.lua"),
    addon_file!("Market2.lua"),
    addon_file!("Market3.lua"),
    addon_file!("Market4.lua"),
    addon_file!("Market5.lua"),
    addon_file!("Market6.lua"),
    addon_file!("Adapters/compat.lua"),
    addon_file!("Adapters/extra_tip.lua"),
    addon_file!("Adapters/tooltip.lua"),
    addon_file!("Adapters/options.lua"),
    addon_file!("Adapters/public_api.lua"),
    addon_file!("bootstrap.lua"),
];

/// The embedded file at `path`, when the bundle has it. Anything else — an
/// unknown name, a `..` segment, an absolute path — answers `None`: no
/// request reaches outside the bundle.
#[must_use]
pub fn lookup(path: &str) -> Option<&'static AddonFile> {
    FILES.iter().find(|file| file.path == path)
}

/// The content type a file is served with: Lua for the `.lua` files, plain
/// text for the TOC.
#[must_use]
pub fn content_type(path: &str) -> &'static str {
    let is_lua = std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("lua"));
    if is_lua {
        "text/x-lua; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    }
}

/// The addon's version, as its TOC declares it (`## Version:`).
#[must_use]
pub fn toc_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        lookup("AuctiMoggerDatabase.toc")
            .map_or("", |file| file.bytes)
            .lines()
            .find_map(|line| line.strip_prefix("## Version:"))
            .map_or_else(|| "0.0.0".to_owned(), |version| version.trim().to_owned())
    })
}

/// The manifest document of `GET /api/addon/manifest`, built once:
/// `{ "addon", "version", "feedVersion", "files": [ { "path", "sha256",
/// "size" } ] }`.
#[must_use]
pub fn manifest() -> &'static str {
    static MANIFEST: OnceLock<String> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        json!({
            "addon": ADDON_NAME,
            "version": toc_version(),
            "feedVersion": DatabaseSnapshot::FEED_VERSION,
            "files": FILES
                .iter()
                .map(|file| {
                    json!({
                        "path": file.path,
                        "sha256": sha256_hex(file.bytes.as_bytes()),
                        "size": file.bytes.len(),
                    })
                })
                .collect::<Vec<Value>>(),
        })
        .to_string()
    })
}

/// Lower-case hex SHA-256 of `bytes`.
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

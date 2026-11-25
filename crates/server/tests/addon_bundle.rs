//! The embedded addon bundle cannot drift from `addon/AuctiMoggerDatabase/`
//! (`addon/SPEC.md`, section 6.7): every file on disk is embedded, every
//! embedded file is on disk, and the bytes are identical — so a file cannot
//! appear on either side without the other, and an edit to any addon file
//! that is not rebuilt into the table fails here.

// Test walking may expect and index: the workspace layout is fixed, not hostile data.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{
    Path,
    PathBuf,
};

use amtracker_core::{
    domain::DatabaseSnapshot,
    services::addon_sync::{
        slot_path,
        ADDON_NAME,
        SLOT_COUNT,
    },
};
use amtracker_server::addon_bundle::{
    content_type,
    lookup,
    manifest,
    toc_version,
    AddonFile,
    FILES,
};

fn addon_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addon/AuctiMoggerDatabase")
}

/// Every file under `dir`, as `/`-separated paths relative to the addon folder.
fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read the addon folder") {
        let entry = entry.expect("addon dir entry");
        let path = entry.path();
        let name = path.file_name().and_then(|name| name.to_str()).expect("UTF-8 file name");
        let relative = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") };
        if path.is_dir() {
            walk(&path, &relative, out);
        } else {
            out.push(relative);
        }
    }
}

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

#[test]
fn the_embedded_table_and_the_folder_agree_in_both_directions() {
    let mut on_disk = Vec::new();
    walk(&addon_dir(), "", &mut on_disk);
    assert!(!on_disk.is_empty(), "no addon folder at {}", addon_dir().display());
    on_disk.sort_unstable();
    let mut embedded: Vec<&str> = FILES.iter().map(|file: &AddonFile| file.path).collect();
    embedded.sort_unstable();
    assert_eq!(
        embedded, on_disk,
        "every file on disk must be embedded and every embedded file must be on disk"
    );
}

#[test]
fn every_embedded_file_is_the_folder_s_file_byte_for_byte() {
    for file in FILES {
        let disk = std::fs::read(addon_dir().join(file.path)).expect("read the addon file");
        assert_eq!(disk, file.bytes.as_bytes(), "{}", file.path);
    }
}

#[test]
fn the_manifest_lists_every_file_with_its_hash_and_size() {
    let value: serde_json::Value = serde_json::from_str(manifest()).expect("the manifest is JSON");
    assert_eq!(value["addon"], ADDON_NAME);
    assert_eq!(value["version"], toc_version());
    assert_eq!(value["feedVersion"], DatabaseSnapshot::FEED_VERSION);
    let files = value["files"].as_array().expect("files array");
    assert_eq!(files.len(), FILES.len());
    for (entry, file) in files.iter().zip(FILES) {
        assert_eq!(entry["path"], file.path, "the manifest keeps the table's order");
        assert_eq!(entry["sha256"], sha256_hex(file.bytes.as_bytes()), "{}", file.path);
        assert_eq!(entry["size"], file.bytes.len(), "{}", file.path);
    }

    // And it is exactly what the sync's own client reads back.
    let parsed = amtracker_infra::addon_sync::parse_manifest(manifest().as_bytes())
        .expect("the sync client parses the served manifest");
    assert_eq!(parsed.addon, ADDON_NAME);
    assert_eq!(parsed.feed_version, DatabaseSnapshot::FEED_VERSION);
    assert_eq!(parsed.files.len(), FILES.len());
}

#[test]
fn every_file_the_toc_loads_is_in_the_bundle() {
    let toc = lookup("AuctiMoggerDatabase.toc").expect("the TOC").bytes;
    for line in toc.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        assert!(lookup(line).is_some(), "the TOC loads {line} and the bundle lacks it");
    }
}

#[test]
fn the_toc_version_is_the_addon_s_own_version() {
    let api = lookup("Adapters/public_api.lua").expect("the public API").bytes;
    let version = api
        .lines()
        .find_map(|line| line.strip_prefix("A.VERSION = "))
        .expect("A.VERSION is declared");
    assert_eq!(version, format!("\"{}\"", toc_version()));
}

#[test]
fn the_slots_are_the_placeholders() {
    for slot in 1..=SLOT_COUNT {
        let file = lookup(&slot_path(slot)).unwrap_or_else(|| panic!("slot {slot} is embedded"));
        assert!(
            file.bytes.starts_with(&format!("-- AuctiMogger Database market slot {slot} (empty).")),
            "{}",
            file.path
        );
    }
}

#[test]
fn lookup_serves_only_what_is_embedded() {
    let toc = lookup("AuctiMoggerDatabase.toc").expect("the TOC");
    assert_eq!(content_type(toc.path), "text/plain; charset=utf-8");
    assert_eq!(content_type("Core/pack.lua"), "text/x-lua; charset=utf-8");
    for path in [
        "",
        "Core/",
        "/AuctiMoggerDatabase.toc",
        "../Cargo.toml",
        "Core/../bootstrap.lua",
        "Core//pack.lua",
        "core/pack.lua",
        "Market7.lua",
        "nope.lua",
    ] {
        assert_eq!(lookup(path), None, "{path}");
    }
}

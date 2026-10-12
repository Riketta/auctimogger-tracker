//! Hexagonal boundary enforcement for the core crate (CI gate).
//!
//! 1. `crates/core/Cargo.toml` must have NO `[dependencies]` section at all —
//!    the compile-time guarantee that the core cannot touch infrastructure.
//! 2. Core sources must not `use` any infrastructure crate name.

// Test walking may expect: the workspace layout is fixed, not hostile data.
#![allow(clippy::expect_used)]

use std::path::{
    Path,
    PathBuf,
};

const FORBIDDEN_CRATES: [&str; 7] =
    ["reqwest", "rusqlite", "axum", "tokio", "serde", "toml", "clap"];

#[test]
fn core_manifest_has_no_dependencies_section() {
    let manifest = include_str!("../Cargo.toml");
    assert!(
        !manifest.contains("[dependencies]"),
        "core must stay dependency-free; found a [dependencies] section"
    );
}

#[test]
fn core_sources_reference_no_infrastructure() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut stack: Vec<PathBuf> = vec![src];
    let mut scanned = 0usize;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read core src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read core source");
            for line in text.lines() {
                let trimmed = line.trim_start();
                if let Some(rest) = trimmed.strip_prefix("use ") {
                    for forbidden in FORBIDDEN_CRATES {
                        assert!(
                            !rest.starts_with(forbidden),
                            "{} references infrastructure crate `{}`: {line}",
                            path.display(),
                            forbidden
                        );
                    }
                }
            }
            scanned += 1;
        }
    }
    assert!(scanned >= 8, "expected to scan the full core sources, saw {scanned}");
}

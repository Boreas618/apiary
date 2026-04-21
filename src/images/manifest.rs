//! Optional JSON manifest for the image-to-layers map.
//!
//! Format: `{ "image_name": ["/path/to/layer1", "/path/to/layer2", ...] }`
//!
//! The manifest is **not** the source of truth — the Docker daemon is.
//! It exists for debugging/inspection and to optionally speed up same-node
//! restarts by recording which images have been extracted.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

pub type ImageLayersMap = BTreeMap<String, Vec<String>>;

/// Read an existing manifest from disk, or return an empty map if missing.
pub fn read_manifest(path: &Path) -> Result<ImageLayersMap> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Write (or overwrite) the manifest to disk.
pub fn write_manifest(path: &Path, map: &ImageLayersMap) -> Result<()> {
    let json =
        serde_json::to_string_pretty(map).context("serialize image layers manifest")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json).with_context(|| format!("write {}", path.display()))
}

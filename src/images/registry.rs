//! In-memory image registry built at daemon startup.
//!
//! Resolves Docker image names to ordered lists of local layer directory
//! paths.  Built by [`ImageRegistry::ensure_all`], which drives layer
//! extraction for any images missing from the local cache.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use tracing::info;

use super::extractor::LayerExtractor;
use super::manifest;

/// In-memory map of image name → extracted layer directory paths.
pub struct ImageRegistry {
    images: BTreeMap<String, Vec<PathBuf>>,
}

impl ImageRegistry {
    /// Build the registry, ensuring every image has its layers locally.
    ///
    /// For each image in `image_names`, calls `docker inspect` to discover
    /// layer diff IDs, then extracts any missing layers to `layers_dir`.
    pub fn ensure_all(
        image_names: &[String],
        layers_dir: &Path,
        docker_bin: &str,
    ) -> Result<Self> {
        if image_names.is_empty() {
            bail!("no images specified in config");
        }

        let extractor = LayerExtractor::new(layers_dir.to_path_buf(), docker_bin.to_string())?;
        let mut images = BTreeMap::new();
        let total = image_names.len();

        for (i, name) in image_names.iter().enumerate() {
            info!(
                "[{}/{}] ensuring layers for {}",
                i + 1,
                total,
                name,
            );
            let paths = extractor.ensure_layers(name)?;
            images.insert(name.clone(), paths);
        }

        let manifest_path = layers_dir.join("image_layers_map.json");
        let map: manifest::ImageLayersMap = images
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
                )
            })
            .collect();
        if let Err(e) = manifest::write_manifest(&manifest_path, &map) {
            tracing::warn!("failed to write image manifest (non-fatal): {e}");
        } else {
            info!(
                "wrote image_layers_map.json: {} images under {}",
                map.len(),
                manifest_path.display(),
            );
        }

        info!(
            "image registry ready: {} images",
            images.len(),
        );

        Ok(Self { images })
    }

    /// Build a registry directly from pre-built entries (for tests).
    pub fn new_with_entries(images: BTreeMap<String, Vec<PathBuf>>) -> Self {
        Self { images }
    }

    /// Resolve an image name to its ordered layer paths (base first).
    pub fn resolve(&self, name: &str) -> Option<&[PathBuf]> {
        self.images.get(name).map(|v| v.as_slice())
    }

    /// Iterate over all registered image names.
    pub fn image_names(&self) -> impl Iterator<Item = &str> {
        self.images.keys().map(|s| s.as_str())
    }

    /// Number of registered images.
    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }
}

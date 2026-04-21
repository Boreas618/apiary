//! Mutable in-memory image registry.
//!
//! Maps Docker image names to the ordered list of local layer directories
//! that the daemon will hand to OverlayFS at session creation. Always
//! starts empty: images are registered at runtime via [`ImageLoader`],
//! which drives layer extraction and then calls [`ImageRegistry::insert`].
//!
//! Every mutation rewrites `image_layers_map.json` in `layers_dir` so
//! external tools can observe the current registry state.
//!
//! [`ImageLoader`]: crate::images::ImageLoader

use std::collections::BTreeMap;
use std::path::PathBuf;

use parking_lot::RwLock;

use super::manifest;

/// In-memory map of image name → extracted layer directory paths.
///
/// Interior-mutable so the daemon can register new images at runtime
/// without requiring a restart. All mutations also rewrite the on-disk
/// manifest at `{layers_dir}/image_layers_map.json` (best effort).
pub struct ImageRegistry {
    images: RwLock<BTreeMap<String, Vec<PathBuf>>>,
    layers_dir: PathBuf,
}

impl ImageRegistry {
    /// Build an empty registry. The daemon always starts in this state;
    /// images are added via [`Self::insert`] after layer extraction.
    pub fn new(layers_dir: impl Into<PathBuf>) -> Self {
        Self {
            images: RwLock::new(BTreeMap::new()),
            layers_dir: layers_dir.into(),
        }
    }

    /// Insert (or replace) `name` with its ordered layer paths and rewrite
    /// the on-disk manifest. Returns the previous entry, if any.
    pub fn insert(&self, name: String, layers: Vec<PathBuf>) -> Option<Vec<PathBuf>> {
        let prev = {
            let mut map = self.images.write();
            map.insert(name, layers)
        };
        self.write_manifest_best_effort();
        prev
    }

    /// Remove `name` from the registry and rewrite the on-disk manifest.
    /// Returns the previous entry, if any.
    pub fn remove(&self, name: &str) -> Option<Vec<PathBuf>> {
        let prev = {
            let mut map = self.images.write();
            map.remove(name)
        };
        if prev.is_some() {
            self.write_manifest_best_effort();
        }
        prev
    }

    /// True if `name` is currently registered.
    pub fn contains(&self, name: &str) -> bool {
        self.images.read().contains_key(name)
    }

    /// Resolve an image name to a clone of its ordered layer paths
    /// (base first, topmost last).
    pub fn resolve(&self, name: &str) -> Option<Vec<PathBuf>> {
        self.images.read().get(name).cloned()
    }

    /// Snapshot of all currently registered image names (sorted).
    pub fn list(&self) -> Vec<String> {
        self.images.read().keys().cloned().collect()
    }

    /// Number of registered images.
    pub fn len(&self) -> usize {
        self.images.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.read().is_empty()
    }

    /// Path to the on-disk image-layers manifest.
    pub fn manifest_path(&self) -> PathBuf {
        self.layers_dir.join("image_layers_map.json")
    }

    fn write_manifest_best_effort(&self) {
        let map: manifest::ImageLayersMap = {
            let images = self.images.read();
            images
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
                    )
                })
                .collect()
        };
        let path = self.manifest_path();
        if let Err(error) = manifest::write_manifest(&path, &map) {
            tracing::warn!(
                path = %path.display(),
                %error,
                "failed to write image manifest (non-fatal)",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn empty_registry_starts_clean() {
        let tmp = tempdir().unwrap();
        let reg = ImageRegistry::new(tmp.path());
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.list().is_empty());
        assert!(!reg.contains("ubuntu:22.04"));
        assert!(reg.resolve("ubuntu:22.04").is_none());
    }

    #[test]
    fn insert_then_resolve_round_trip() {
        let tmp = tempdir().unwrap();
        let reg = ImageRegistry::new(tmp.path());
        let layers = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let prev = reg.insert("img:latest".to_string(), layers.clone());
        assert!(prev.is_none());

        assert!(reg.contains("img:latest"));
        assert_eq!(reg.resolve("img:latest"), Some(layers.clone()));
        assert_eq!(reg.list(), vec!["img:latest"]);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn insert_overwrites_and_returns_prev() {
        let tmp = tempdir().unwrap();
        let reg = ImageRegistry::new(tmp.path());
        let v1 = vec![PathBuf::from("/a")];
        let v2 = vec![PathBuf::from("/b"), PathBuf::from("/c")];
        reg.insert("img".to_string(), v1.clone());
        let prev = reg.insert("img".to_string(), v2.clone());
        assert_eq!(prev, Some(v1));
        assert_eq!(reg.resolve("img"), Some(v2));
    }

    #[test]
    fn remove_returns_prev_and_drops_entry() {
        let tmp = tempdir().unwrap();
        let reg = ImageRegistry::new(tmp.path());
        let layers = vec![PathBuf::from("/a")];
        reg.insert("img".to_string(), layers.clone());

        let removed = reg.remove("img");
        assert_eq!(removed, Some(layers));
        assert!(!reg.contains("img"));
        assert!(reg.remove("img").is_none());
    }

    #[test]
    fn manifest_is_rewritten_on_mutation() {
        let tmp = tempdir().unwrap();
        let reg = ImageRegistry::new(tmp.path());

        reg.insert("img".to_string(), vec![PathBuf::from("/a")]);
        let manifest_path = reg.manifest_path();
        assert!(manifest_path.exists(), "manifest should be written on insert");

        let map = manifest::read_manifest(&manifest_path).unwrap();
        assert_eq!(map.get("img").map(|v| v.as_slice()), Some(&["/a".to_string()][..]));

        reg.remove("img");
        let map = manifest::read_manifest(&manifest_path).unwrap();
        assert!(map.is_empty(), "manifest should be empty after remove");
    }
}

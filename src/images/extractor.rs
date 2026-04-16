//! Docker image layer extraction with content-addressable caching.
//!
//! Preserves Docker's layer structure by using `docker save` (not `docker
//! export`), extracting each layer exactly once into a shared cache keyed
//! by the layer's SHA-256 diff ID.  Docker whiteout files (`.wh.*`) are
//! converted to OverlayFS format during extraction so the layer directories
//! can be passed directly as multiple `lowerdir` entries to OverlayFS.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;
use tracing::{info, warn};

/// Manages a content-addressable cache of Docker image layers.
///
/// Layers are stored once in `{layers_dir}/{diff_id_hex}/` and shared
/// across all images that contain them.  [`ensure_layers`] returns an
/// ordered list of layer directories (base first, topmost last) suitable
/// for OverlayFS multi-lowerdir.
///
/// [`ensure_layers`]: LayerExtractor::ensure_layers
pub struct LayerExtractor {
    layers_dir: PathBuf,
    docker: String,
}

impl LayerExtractor {
    pub fn new(layers_dir: PathBuf, docker: String) -> Result<Self> {
        fs::create_dir_all(&layers_dir)?;
        Ok(Self { layers_dir, docker })
    }

    /// Return the local directory for a given diff ID.
    fn layer_path(&self, diff_id: &str) -> PathBuf {
        let hex = diff_id.strip_prefix("sha256:").unwrap_or(diff_id);
        self.layers_dir.join(hex)
    }

    fn layer_cached(&self, diff_id: &str) -> Result<bool> {
        let p = self.layer_path(diff_id);
        if !p.is_dir() {
            return Ok(false);
        }
        Ok(fs::read_dir(&p)?.next().is_some())
    }

    // ------------------------------------------------------------------
    // Docker CLI helpers
    // ------------------------------------------------------------------

    /// Return the ordered list of layer diff IDs via `docker inspect`.
    pub fn docker_inspect_layers(&self, image: &str) -> Result<Vec<String>> {
        let out = Command::new(&self.docker)
            .args(["inspect", "--format", "{{json .RootFS.Layers}}", image])
            .output()
            .with_context(|| format!("{} inspect {}", self.docker, image))?;
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr);
            bail!("docker inspect failed for {image}: {msg}");
        }
        let layers: Vec<String> = serde_json::from_slice(&out.stdout)
            .with_context(|| format!("parse RootFS.Layers for {image}"))?;
        if layers.is_empty() {
            bail!("docker inspect returned no layers for {image:?}");
        }
        Ok(layers)
    }

    // ------------------------------------------------------------------
    // Public entry point
    // ------------------------------------------------------------------

    /// Ensure all layers for `image` are extracted locally.
    ///
    /// Returns an ordered `Vec<PathBuf>` (base first, topmost last).
    /// Layers already present in the cache are skipped.
    pub fn ensure_layers(&self, image: &str) -> Result<Vec<PathBuf>> {
        let diff_ids = self.docker_inspect_layers(image)?;
        let missing: Vec<_> = diff_ids
            .iter()
            .filter(|d| !self.layer_cached(d).unwrap_or(false))
            .cloned()
            .collect();
        if !missing.is_empty() {
            info!(
                "{}/{} layers missing for {} — running docker save",
                missing.len(),
                diff_ids.len(),
                image,
            );
            self.extract_layers_from_save(image, &diff_ids)?;
        } else {
            info!(
                "all {} layers cached for {}",
                diff_ids.len(),
                image,
            );
        }
        diff_ids
            .iter()
            .map(|d| {
                let p = self.layer_path(d);
                p.canonicalize().or_else(|_| Ok(p))
            })
            .collect()
    }

    // ------------------------------------------------------------------
    // Layer extraction
    // ------------------------------------------------------------------

    fn extract_layers_from_save(
        &self,
        image: &str,
        diff_ids: &[String],
    ) -> Result<()> {
        let archive_path = self
            .layers_dir
            .join(format!(".tmp-save-{}.tar", uuid::Uuid::new_v4()));
        let t_image = Instant::now();
        info!("docker save {image} -> {}", archive_path.display());
        let t_save = Instant::now();
        let status = Command::new(&self.docker)
            .args(["save", "-o"])
            .arg(&archive_path)
            .arg(image)
            .status()
            .with_context(|| format!("docker save {image}"))?;
        if !status.success() {
            let _ = fs::remove_file(&archive_path);
            bail!("docker save failed for {image}");
        }
        info!(
            "docker save finished for {image} in {:?}",
            t_save.elapsed(),
        );

        let result = (|| -> Result<()> {
            let layer_tar_paths = read_manifest_layer_paths(&archive_path)?;

            if layer_tar_paths.len() != diff_ids.len() {
                bail!(
                    "layer count mismatch: manifest {} vs inspect {}",
                    layer_tar_paths.len(),
                    diff_ids.len(),
                );
            }

            let t_extract = Instant::now();
            for (idx, layer_tar_path) in layer_tar_paths.iter().enumerate() {
                let expected = &diff_ids[idx];
                if self.layer_cached(expected)? {
                    continue;
                }
                let mut found = false;
                let archive_file = File::open(&archive_path)?;
                let mut archive = Archive::new(archive_file);
                for entry in archive.entries()? {
                    let mut entry = entry?;
                    if entry.path()?.to_string_lossy() != *layer_tar_path {
                        continue;
                    }
                    self.extract_single_layer(
                        &mut entry,
                        expected,
                        idx + 1,
                        layer_tar_paths.len(),
                    )?;
                    found = true;
                    break;
                }
                if !found {
                    bail!("missing layer path {layer_tar_path} in docker save archive");
                }
            }
            info!(
                "layer extract finished for {image} in {:?} (save+extract total {:?})",
                t_extract.elapsed(),
                t_image.elapsed(),
            );
            Ok(())
        })();

        let _ = fs::remove_file(&archive_path);
        result
    }

    fn extract_single_layer<R: Read>(
        &self,
        layer_read: &mut R,
        expected_diff_id: &str,
        idx: usize,
        total: usize,
    ) -> Result<()> {
        let tmp_layer = self
            .layers_dir
            .join(format!(".tmp-layer-{}.tar", uuid::Uuid::new_v4()));
        let layer_dir = self.layer_path(expected_diff_id);

        let res = (|| -> Result<()> {
            let t_layer = Instant::now();
            let mut hasher = Sha256::new();
            let mut tmp = File::create(&tmp_layer)?;
            let mut buf = [0u8; 65536];
            loop {
                let n = layer_read.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                tmp.write_all(&buf[..n])?;
            }
            drop(tmp);

            let digest = format!("sha256:{:x}", hasher.finalize());
            if digest != expected_diff_id {
                bail!(
                    "layer {idx} hash mismatch: expected {expected_diff_id}, got {digest}"
                );
            }

            fs::create_dir_all(&layer_dir)?;
            extract_layer_tar_with_whiteouts(&tmp_layer, &layer_dir)?;

            info!(
                "extracted layer {idx}/{total} in {:?}: {}…",
                t_layer.elapsed(),
                &expected_diff_id[..expected_diff_id.len().min(24)],
            );
            Ok(())
        })();

        if res.is_err() {
            let _ = fs::remove_dir_all(&layer_dir);
        }
        let _ = fs::remove_file(&tmp_layer);
        res
    }
}

// ---------------------------------------------------------------------------
// Manifest reading from docker-save archive
// ---------------------------------------------------------------------------

fn read_manifest_layer_paths(archive_path: &Path) -> Result<Vec<String>> {
    let archive_file = File::open(archive_path)?;
    let mut archive = Archive::new(archive_file);
    let mut manifest_bytes = Vec::new();
    let mut found = false;
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.to_string_lossy() != "manifest.json" {
            continue;
        }
        entry.read_to_end(&mut manifest_bytes)?;
        found = true;
        break;
    }
    if !found {
        bail!("docker save archive missing manifest.json");
    }
    let manifest: Vec<serde_json::Value> =
        serde_json::from_slice(&manifest_bytes).context("parse manifest.json")?;
    let layer_tar_paths: Vec<String> = manifest
        .first()
        .and_then(|v| v.get("Layers"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("manifest.json missing Layers"))?
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    Ok(layer_tar_paths)
}

// ---------------------------------------------------------------------------
// Tar extraction with Docker whiteout → OverlayFS conversion
// ---------------------------------------------------------------------------

fn extract_layer_tar_with_whiteouts(layer_tar_path: &Path, dest_dir: &Path) -> Result<()> {
    let mut file = File::open(layer_tar_path)?;
    let mut magic = [0u8; 2];
    std::io::Read::read_exact(&mut file, &mut magic)?;
    file.seek(SeekFrom::Start(0))?;
    if magic == [0x1f, 0x8b] {
        let dec = GzDecoder::new(file);
        let mut archive = Archive::new(dec);
        extract_archive_whiteouts(&mut archive, dest_dir)?;
    } else {
        let mut archive = Archive::new(file);
        extract_archive_whiteouts(&mut archive, dest_dir)?;
    }
    Ok(())
}

fn extract_archive_whiteouts<R: Read>(archive: &mut Archive<R>, dest_dir: &Path) -> Result<()> {
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let path_str = path.to_string_lossy();
        let basename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let dirname = path.parent();

        if basename == ".wh..wh..opq" {
            let parent_dir =
                dirname.map_or_else(|| dest_dir.to_path_buf(), |d| dest_dir.join(d));
            fs::create_dir_all(&parent_dir)?;
            set_opaque_xattr(&parent_dir);
        } else if let Some(rest) = basename.strip_prefix(".wh.") {
            let parent =
                dirname.map_or_else(|| dest_dir.to_path_buf(), |d| dest_dir.join(d));
            fs::create_dir_all(&parent)?;
            let whiteout_path = parent.join(rest);
            create_whiteout_device(&whiteout_path)?;
        } else {
            entry.unpack_in(dest_dir).with_context(|| {
                format!("unpack {} into {}", path_str, dest_dir.display())
            })?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// OverlayFS whiteout helpers
// ---------------------------------------------------------------------------

fn set_opaque_xattr(dir_path: &Path) {
    match xattr::set(dir_path, "user.overlay.opaque", b"y") {
        Ok(()) => {}
        Err(e) => {
            warn!(
                "failed to set user.overlay.opaque on {}: {e}",
                dir_path.display(),
            );
        }
    }
}

fn create_whiteout_device(path: &Path) -> Result<()> {
    if path.exists() || path.is_symlink() {
        let _ = fs::remove_file(path);
    }
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| anyhow!("path contains NUL: {}", path.display()))?;
    let dev = libc::makedev(0, 0);
    // SAFETY: libc mknod on valid NUL-terminated path
    let rc = unsafe { libc::mknod(c_path.as_ptr(), libc::S_IFCHR | 0o666, dev) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    let errno = err.raw_os_error();
    if errno != Some(libc::EPERM) && errno != Some(libc::EACCES) {
        return Err(err).with_context(|| format!("mknod whiteout {}", path.display()));
    }

    let status = Command::new("unshare")
        .args(["-r", "sh", "-c", "mknod \"$1\" c 0 0", "_"])
        .arg(path)
        .output()?;
    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        warn!(
            "cannot create whiteout device at {} (mknod and unshare failed: {})",
            path.display(),
            stderr.trim(),
        );
    }
    Ok(())
}

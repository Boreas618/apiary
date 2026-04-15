//! Phase 1 for SWE-bench in an Apiary-style Linux environment: load a dataset,
//! `docker pull` each unique evaluation image, extract Docker layers into
//! `{rootfs_cache_dir}/.layers/{sha256-hex}/` (same layout as Python
//! `apiary_swebench.rootfs.RootfsManager`), and write a JSON map
//! `{image: [layer paths…]}` to `{rootfs_cache_dir}/.layers/image_layers_map.json`.
//!
//! **Batching:** `--batch-size N` and `--batch-id K` (0-based) restrict work to the *K*-th
//! block of `N` unique images (sorted). Use `--batch-size 0` for the full set. With batching,
//! existing `image_layers_map.json` is loaded and merged so runs can be split across machines
//! or time without losing earlier entries.
//!
//! Requires: `docker` CLI, a writable cache directory (default `/tmp/apiary_rootfs`),
//! and network access for HuggingFace unless `--dataset` is a local JSON/JSONL path.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::StatusCode;
use arrow_array::{Array, ArrayRef, LargeStringArray, StringArray};
use clap::Parser;
use flate2::read::GzDecoder;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tar::Archive;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Dataset loading (local JSON / JSONL, or HuggingFace parquet via REST API)
// ---------------------------------------------------------------------------

const DATASET_ALIASES: &[(&str, &str)] = &[
    ("full", "princeton-nlp/SWE-bench"),
    ("verified", "princeton-nlp/SWE-bench_Verified"),
    ("lite", "princeton-nlp/SWE-bench_Lite"),
    ("multimodal", "princeton-nlp/SWE-bench_Multimodal"),
    ("multilingual", "swe-bench/SWE-Bench_Multilingual"),
];

#[derive(Debug, Deserialize)]
struct HfDatasetApi {
    id: String,
    siblings: Vec<HfSibling>,
}

#[derive(Debug, Deserialize)]
struct HfSibling {
    rfilename: String,
}

fn resolve_hf_repo_id(dataset: &str) -> String {
    DATASET_ALIASES
        .iter()
        .find(|(k, _)| *k == dataset)
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| dataset.to_string())
}

fn get_docker_image(instance: &Value) -> Result<String> {
    if let Some(s) = instance.get("image_name").and_then(|v| v.as_str()) {
        if !s.is_empty() {
            return Ok(s.to_string());
        }
    }
    if let Some(s) = instance.get("docker_image").and_then(|v| v.as_str()) {
        if !s.is_empty() {
            return Ok(s.to_string());
        }
    }
    let iid = instance
        .get("instance_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("instance missing instance_id"))?;
    let id_compat = iid.replace("__", "_1776_");
    Ok(format!(
        "docker.io/swebench/sweb.eval.x86_64.{id_compat}:latest"
    )
    .to_lowercase())
}

fn load_instances_local(path: &Path) -> Result<Vec<Value>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("read dataset file {}", path.display()))?;
    let trimmed = text.trim();
    if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
        let mut out = Vec::new();
        for line in trimmed.lines() {
            if line.is_empty() {
                continue;
            }
            out.push(serde_json::from_str(line)?);
        }
        return Ok(out);
    }
    if trimmed.starts_with('[') {
        return Ok(serde_json::from_str(trimmed)?);
    }
    let obj: BTreeMap<String, Value> = serde_json::from_str(trimmed)?;
    Ok(obj.into_values().collect())
}

fn parquet_paths_for_split(siblings: &[HfSibling], split: &str) -> Vec<String> {
    let prefix = format!("data/{split}-");
    let mut paths: Vec<String> = siblings
        .iter()
        .filter(|s| {
            s.rfilename.starts_with(&prefix) && s.rfilename.ends_with(".parquet")
        })
        .map(|s| s.rfilename.clone())
        .collect();
    paths.sort();
    paths
}

fn col_as_strings(col: &ArrayRef) -> Result<Vec<Option<String>>> {
    if let Some(a) = col.as_any().downcast_ref::<StringArray>() {
        return Ok((0..a.len())
            .map(|i| {
                if a.is_null(i) {
                    None
                } else {
                    Some(a.value(i).to_string())
                }
            })
            .collect());
    }
    if let Some(a) = col.as_any().downcast_ref::<LargeStringArray>() {
        return Ok((0..a.len())
            .map(|i| {
                if a.is_null(i) {
                    None
                } else {
                    Some(a.value(i).to_string())
                }
            })
            .collect());
    }
    bail!("unsupported Arrow string type for column")
}

fn read_parquet_instances(path: &Path) -> Result<Vec<Value>> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(f)
        .with_context(|| format!("parquet {}", path.display()))?;
    let schema = builder.schema().clone();
    let inst_idx = schema
        .fields()
        .iter()
        .position(|f| f.name() == "instance_id")
        .ok_or_else(|| anyhow!("parquet missing instance_id column"))?;
    let img_name_idx = schema.fields().iter().position(|f| f.name() == "image_name");
    let docker_img_idx = schema.fields().iter().position(|f| f.name() == "docker_image");

    let reader = builder.build()?;
    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch.with_context(|| format!("read batch {}", path.display()))?;
        let n = batch.num_rows();
        let inst_col = batch.column(inst_idx);
        let inst_vals = col_as_strings(inst_col)?;
        let img_name_vals = img_name_idx.map(|i| col_as_strings(batch.column(i))).transpose()?;
        let docker_vals = docker_img_idx.map(|i| col_as_strings(batch.column(i))).transpose()?;

        for i in 0..n {
            let mut row = json!({
                "instance_id": inst_vals.get(i).and_then(|x| x.clone()).unwrap_or_default(),
            });
            if let Some(ref names) = img_name_vals {
                if let Some(Some(ref s)) = names.get(i) {
                    row["image_name"] = json!(s);
                }
            }
            if let Some(ref dockers) = docker_vals {
                if let Some(Some(ref s)) = dockers.get(i) {
                    row["docker_image"] = json!(s);
                }
            }
            rows.push(row);
        }
    }
    Ok(rows)
}

async fn load_instances_hf(
    client: &reqwest::Client,
    dataset: &str,
    split: &str,
    hf_token: Option<&str>,
) -> Result<Vec<Value>> {
    let repo = resolve_hf_repo_id(dataset);
    let mut api = client.get(format!("https://huggingface.co/api/datasets/{repo}"));
    if let Some(t) = hf_token {
        api = api.bearer_auth(t);
    }
    let resp = api.send().await.context("HF datasets API request")?;
    if resp.status() == StatusCode::UNAUTHORIZED {
        bail!(
            "HuggingFace returned 401 for dataset `{repo}`. \
             Set `HF_TOKEN` or pass `--hf-token` with a token that can access this repo, \
             and on huggingface.co accept any dataset terms / access request if required. \
             Alternatively use a local JSON/JSONL path for `--dataset`."
        );
    }
    let resp = resp
        .error_for_status()
        .context("HF datasets API status")?;
    let meta: HfDatasetApi = resp.json().await.context("HF datasets API json")?;
    let canonical_id = meta.id.clone();
    let paths = parquet_paths_for_split(&meta.siblings, split);
    if paths.is_empty() {
        bail!(
            "no parquet shards for split {:?} under data/ in {}; check split name",
            split,
            canonical_id
        );
    }

    let tmp_dir = tempfile::tempdir().context("tempdir for parquet")?;
    let tmp_path = tmp_dir.path().to_path_buf();
    let mut all = Vec::new();
    for rel in paths {
        let url = format!(
            "https://huggingface.co/datasets/{canonical_id}/resolve/main/{rel}"
        );
        let mut req = client.get(&url);
        if let Some(t) = hf_token {
            req = req.bearer_auth(t);
        }
        let bytes = req
            .send()
            .await
            .with_context(|| format!("download {url}"))?
            .error_for_status()
            .with_context(|| format!("download status {url}"))?
            .bytes()
            .await
            .with_context(|| format!("read body {url}"))?;
        let dest = tmp_path.join(rel.replace('/', "_"));
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(&dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
        all.extend(read_parquet_instances(&dest)?);
    }
    let _keep = tmp_dir;
    Ok(all)
}

async fn load_instances(
    client: &reqwest::Client,
    dataset: &str,
    split: &str,
    hf_token: Option<&str>,
) -> Result<Vec<Value>> {
    let p = Path::new(dataset);
    if p.exists() {
        return load_instances_local(p);
    }
    load_instances_hf(client, dataset, split, hf_token).await
}

// ---------------------------------------------------------------------------
// Docker layer cache (matches apiary_swebench/rootfs.py)
// ---------------------------------------------------------------------------

struct RootfsManager {
    cache_dir: PathBuf,
    layers_dir: PathBuf,
    docker: String,
}

impl RootfsManager {
    fn new(cache_dir: PathBuf, docker: String) -> Result<Self> {
        fs::create_dir_all(&cache_dir)?;
        let layers_dir = cache_dir.join(".layers");
        fs::create_dir_all(&layers_dir)?;
        Ok(Self {
            cache_dir,
            layers_dir,
            docker,
        })
    }

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

    fn docker_inspect_layers(&self, image: &str) -> Result<Vec<String>> {
        let out = Command::new(&self.docker)
            .args([
                "inspect",
                "--format",
                "{{json .RootFS.Layers}}",
                image,
            ])
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

    fn ensure_layers(&self, image: &str, batch_idx: usize, batch_total: usize) -> Result<Vec<PathBuf>> {
        let diff_ids = self.docker_inspect_layers(image)?;
        let missing: Vec<_> = diff_ids
            .iter()
            .filter(|d| !self.layer_cached(d).unwrap_or(false))
            .cloned()
            .collect();
        if !missing.is_empty() {
            info!(
                "[batch {batch_idx}/{batch_total}] {}/{} layers missing for {} — docker save",
                missing.len(),
                diff_ids.len(),
                image
            );
            self.extract_layers_from_save(image, &diff_ids, batch_idx, batch_total)?;
        } else {
            info!(
                "[batch {batch_idx}/{batch_total}] all {} layers cached for {}",
                diff_ids.len(),
                image
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

    fn extract_layers_from_save(
        &self,
        image: &str,
        diff_ids: &[String],
        batch_idx: usize,
        batch_total: usize,
    ) -> Result<()> {
        let archive_path = self
            .cache_dir
            .join(format!(".tmp-save-{}.tar", uuid::Uuid::new_v4()));
        let t_image = Instant::now();
        info!(
            "[batch {batch_idx}/{batch_total}] docker save {image} -> {}",
            archive_path.display()
        );
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
            "[batch {batch_idx}/{batch_total}] docker save finished for {image} in {:?}",
            t_save.elapsed()
        );

        let result = (|| -> Result<()> {
            let layer_tar_paths = read_manifest_layer_paths(&archive_path)?;

            if layer_tar_paths.len() != diff_ids.len() {
                bail!(
                    "layer count mismatch: manifest {} vs inspect {}",
                    layer_tar_paths.len(),
                    diff_ids.len()
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
                    self.extract_single_layer_from_read(
                        &mut entry,
                        expected,
                        idx + 1,
                        layer_tar_paths.len(),
                        batch_idx,
                        batch_total,
                    )?;
                    found = true;
                    break;
                }
                if !found {
                    bail!("missing layer path {layer_tar_path} in docker save archive");
                }
            }
            info!(
                "[batch {batch_idx}/{batch_total}] layer extract finished for {image} in {:?} (save+extract total {:?})",
                t_extract.elapsed(),
                t_image.elapsed()
            );
            Ok(())
        })();

        let _ = fs::remove_file(&archive_path);
        result
    }

    fn extract_single_layer_from_read<R: Read>(
        &self,
        layer_read: &mut R,
        expected_diff_id: &str,
        idx: usize,
        total: usize,
        batch_idx: usize,
        batch_total: usize,
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
                "[batch {batch_idx}/{batch_total}] extracted layer {idx}/{total} in {:?}: {}…",
                t_layer.elapsed(),
                &expected_diff_id[..expected_diff_id.len().min(24)]
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
    let manifest: Vec<Value> =
        serde_json::from_slice(&manifest_bytes).context("parse manifest.json")?;
    let layer_tar_paths: Vec<String> = manifest
        .get(0)
        .and_then(|v| v.get("Layers"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("manifest.json missing Layers"))?
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    Ok(layer_tar_paths)
}

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
        let basename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let dirname = path.parent();

        if basename == ".wh..wh..opq" {
            let parent_dir = dirname.map_or_else(|| dest_dir.to_path_buf(), |d| dest_dir.join(d));
            fs::create_dir_all(&parent_dir)?;
            set_opaque_xattr(&parent_dir);
        } else if let Some(rest) = basename.strip_prefix(".wh.") {
            let parent = dirname.map_or_else(|| dest_dir.to_path_buf(), |d| dest_dir.join(d));
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

fn set_opaque_xattr(dir_path: &Path) {
    match xattr::set(dir_path, "user.overlay.opaque", b"y") {
        Ok(()) => {}
        Err(e) => {
            warn!(
                "failed to set user.overlay.opaque on {}: {e}",
                dir_path.display()
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
            stderr.trim()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "swebench_prefetch_layers",
    about = "SWE-bench Phase 1: pull Docker images and extract shared layer cache + image_layers_map.json"
)]
struct Args {
    /// Dataset alias (lite, full, verified, …), HuggingFace id, or path to JSON/JSONL
    #[arg(long, default_value = "lite")]
    dataset: String,

    /// HuggingFace split (ignored for local files)
    #[arg(long, default_value = "test")]
    split: String,

    /// Use SWE-bench Lite `dev` split (~23 instances), same default split as run_swebench.py
    #[arg(long)]
    lite_dev: bool,

    /// Same as run_swebench.py --rootfs_cache_dir; layers live under `<dir>/.layers/`
    #[arg(long, default_value = "/tmp/apiary_rootfs")]
    rootfs_cache_dir: PathBuf,

    /// Concurrent `docker pull` processes
    #[arg(long, default_value_t = 8)]
    max_workers: usize,

    /// Docker CLI binary
    #[arg(long, default_value = "docker")]
    docker: String,

    /// Print unique image names and exit
    #[arg(long)]
    list_only: bool,

    /// Write one image per line (deduplicated)
    #[arg(long)]
    write_list: Option<PathBuf>,

    /// HuggingFace token (optional; also reads env HF_TOKEN)
    #[arg(long, env = "HF_TOKEN")]
    hf_token: Option<String>,

    /// Process only one batch of this many **unique** images (sorted order). `0` = entire dataset.
    #[arg(long, default_value_t = 0)]
    batch_size: usize,

    /// Which batch to run (0-based). Only used when `batch_size > 0`.
    #[arg(long, default_value_t = 0)]
    batch_id: usize,
}

async fn docker_pull(bin: &str, image: &str) -> Result<()> {
    let out = tokio::process::Command::new(bin)
        .args(["pull", image])
        .output()
        .await
        .with_context(|| format!("spawn {bin} pull {image}"))?;
    if out.status.success() {
        return Ok(());
    }
    let tail = String::from_utf8_lossy(&out.stderr);
    let tail = tail.chars().rev().take(2000).collect::<String>();
    let tail: String = tail.chars().rev().collect();
    bail!("docker pull failed for {image}: {tail}");
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = Args::parse();
    if args.lite_dev {
        args.dataset = "lite".into();
        args.split = "dev".into();
    }

    let hf_token = args.hf_token.clone();
    let client = reqwest::Client::builder()
        .user_agent("apiary/swebench_prefetch_layers")
        .build()
        .context("reqwest client")?;

    info!("[Phase 1] loading dataset …");
    let instances = load_instances(&client, &args.dataset, &args.split, hf_token.as_deref()).await?;
    if instances.is_empty() {
        bail!("no instances loaded");
    }

    let mut images = BTreeSet::new();
    for inst in &instances {
        images.insert(get_docker_image(inst)?);
    }
    let mut images: Vec<String> = images.into_iter().collect();
    let total_unique = images.len();

    if args.batch_size > 0 {
        let start = args.batch_id.saturating_mul(args.batch_size);
        if start >= images.len() {
            let num_batches = (images.len() + args.batch_size - 1) / args.batch_size;
            bail!(
                "batch slice is empty: batch_id={} batch_size={} but only {} unique images \
                 (use batch_id in 0..{} for non-empty batches)",
                args.batch_id,
                args.batch_size,
                images.len(),
                num_batches
            );
        }
        let end = (start + args.batch_size).min(images.len());
        images = images[start..end].to_vec();
        info!(
            "batch mode: batch_id={} batch_size={} → images [{}..{}) of {} unique",
            args.batch_id,
            args.batch_size,
            start,
            end,
            total_unique
        );
    }

    info!(
        "{} instances → {} unique images (dataset={} split={}){}",
        instances.len(),
        total_unique,
        args.dataset,
        args.split,
        if args.batch_size > 0 {
            format!("; this run processes {}", images.len())
        } else {
            String::new()
        }
    );

    if let Some(ref p) = args.write_list {
        fs::write(p, format!("{}\n", images.join("\n")))?;
        info!("wrote image list to {}", p.display());
    }

    if args.list_only {
        for img in &images {
            println!("{img}");
        }
        return Ok(());
    }

    let sem = std::sync::Arc::new(Semaphore::new(args.max_workers.max(1)));
    let mut set = tokio::task::JoinSet::new();
    let docker_bin = args.docker.clone();
    for img in images.clone() {
        let bin = docker_bin.clone();
        let permit_owner = sem.clone();
        set.spawn(async move {
            let _permit = permit_owner
                .acquire_owned()
                .await
                .map_err(|e| anyhow!("semaphore: {e}"))?;
            docker_pull(&bin, &img).await.map(|_| img)
        });
    }

    let mut failed = Vec::new();
    while let Some(res) = set.join_next().await {
        match res {
            Ok(Ok(img)) => info!("pulled {img}"),
            Ok(Err(e)) => {
                error!("{e:#}");
                failed.push(e.to_string());
            }
            Err(e) => failed.push(format!("task join: {e}")),
        }
    }
    if !failed.is_empty() {
        bail!("{} docker pull task(s) failed", failed.len());
    }

    info!("[Phase 1] extracting image layers …");
    let mgr = RootfsManager::new(args.rootfs_cache_dir.clone(), args.docker.clone())?;
    let manifest_path = mgr.layers_dir.join("image_layers_map.json");
    let mut rootfs_map: BTreeMap<String, Vec<String>> = if manifest_path.exists() {
        let text = fs::read_to_string(&manifest_path)
            .with_context(|| format!("read {}", manifest_path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parse {}", manifest_path.display()))?
    } else {
        BTreeMap::new()
    };
    let n_this_run = images.len();
    let batch_total = n_this_run;
    for (i, img) in images.iter().enumerate() {
        let paths = mgr.ensure_layers(img, i + 1, batch_total)?;
        rootfs_map.insert(
            img.clone(),
            paths.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
        );
    }

    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&rootfs_map).context("serialize manifest")?,
    )
    .with_context(|| format!("write {}", manifest_path.display()))?;
    info!(
        "wrote image_layers_map.json: {} image entries total ({} this run) under {}",
        rootfs_map.len(),
        n_this_run,
        manifest_path.display()
    );

    Ok(())
}

//! Async image loader: pull + extract + register, with per-image dedupe.
//!
//! The daemon never pre-loads any image. Clients hit `POST /api/v1/images`
//! at runtime; that path enters [`ImageLoader::load_one`] for each name
//! requested. The loader:
//!
//! 1. Short-circuits if the image is already registered (`AlreadyPresent`).
//! 2. Deduplicates concurrent requests for the same image via a
//!    [`tokio::sync::Notify`] map — only one underlying extraction runs.
//! 3. Bounds total extraction parallelism with a [`Semaphore`] sized from
//!    `LayerCacheConfig.pull_concurrency`.
//! 4. Runs the synchronous `docker pull` + `docker save` + tar-extract
//!    pipeline on `tokio::task::spawn_blocking` so the async runtime is
//!    never blocked.
//!
//! On success, the registry is updated atomically and the on-disk
//! manifest is rewritten.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{Notify, Semaphore};
use tracing::{debug, info, warn};

use super::extractor::LayerExtractor;
use super::registry::ImageRegistry;

/// Coarse-grained progress signal for clients tracking long loads.
#[derive(Debug, Clone)]
pub enum ImageStage {
    /// Waiting for the load semaphore.
    Queued,
    /// `docker pull` in progress (inspect-then-pull-if-missing).
    Pulling,
    /// `docker save` finished; per-layer tar extraction in progress.
    Extracting {
        /// Number of layers extracted so far.
        layers_done: usize,
        /// Total number of layers in the image.
        layers_total: usize,
    },
    /// Successful insert into the registry.
    Done,
    /// Terminal failure with a human-readable reason.
    Failed(String),
}

/// Outcome of a single [`ImageLoader::load_one`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    /// Image was already in the registry; no work was performed.
    AlreadyPresent,
    /// Image was successfully pulled, extracted, and registered.
    Loaded,
    /// Load failed; caller should treat the registry as unchanged.
    Failed(String),
}

/// Async wrapper around [`LayerExtractor`] with dedupe + concurrency bounds.
///
/// Cloneable; the inner state (semaphore, in-flight map, registry) is
/// shared across clones via `Arc`.
#[derive(Clone)]
pub struct ImageLoader {
    inner: Arc<Inner>,
}

struct Inner {
    extractor: Arc<LayerExtractor>,
    registry: Arc<ImageRegistry>,
    docker_bin: String,
    in_flight: Mutex<HashMap<String, Arc<InFlight>>>,
    semaphore: Arc<Semaphore>,
}

struct InFlight {
    notify: Notify,
}

impl ImageLoader {
    /// Build a new loader bound to `registry` and `extractor`.
    ///
    /// `docker_bin` is the Docker CLI path used for `pull`/`inspect`
    /// (typically `"docker"`). `concurrency` caps the number of
    /// simultaneous load operations.
    pub fn new(
        registry: Arc<ImageRegistry>,
        extractor: Arc<LayerExtractor>,
        docker_bin: String,
        concurrency: usize,
    ) -> Self {
        let concurrency = concurrency.max(1);
        Self {
            inner: Arc::new(Inner {
                extractor,
                registry,
                docker_bin,
                in_flight: Mutex::new(HashMap::new()),
                semaphore: Arc::new(Semaphore::new(concurrency)),
            }),
        }
    }

    /// Load a single image, deduplicating against any in-flight load of
    /// the same name. The `progress` callback fires on stage transitions.
    pub async fn load_one<F>(&self, name: &str, progress: F) -> LoadOutcome
    where
        F: Fn(ImageStage) + Send + Sync + 'static,
    {
        if self.inner.registry.contains(name) {
            progress(ImageStage::Done);
            return LoadOutcome::AlreadyPresent;
        }

        // Either join an in-flight load or claim ownership.
        let (in_flight, is_owner) = {
            let mut in_flight = self.inner.in_flight.lock();
            if let Some(existing) = in_flight.get(name).cloned() {
                (existing, false)
            } else {
                let entry = Arc::new(InFlight {
                    notify: Notify::new(),
                });
                in_flight.insert(name.to_string(), entry.clone());
                (entry, true)
            }
        };

        if !is_owner {
            // Wait for the owner to finish, then re-check the registry.
            in_flight.notify.notified().await;
            return if self.inner.registry.contains(name) {
                progress(ImageStage::Done);
                LoadOutcome::AlreadyPresent
            } else {
                LoadOutcome::Failed("primary load failed".to_string())
            };
        }

        // We own the load. Make sure waiters are released no matter how
        // we exit.
        let result = self.run_load(name, &progress).await;

        {
            let mut in_flight = self.inner.in_flight.lock();
            in_flight.remove(name);
        }
        in_flight.notify.notify_waiters();

        result
    }

    async fn run_load<F>(&self, name: &str, progress: &F) -> LoadOutcome
    where
        F: Fn(ImageStage) + Send + Sync + 'static,
    {
        progress(ImageStage::Queued);
        let permit = self
            .inner
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("loader semaphore must not be closed");

        // Stage 1: pull-if-missing on the blocking pool.
        progress(ImageStage::Pulling);
        let docker_bin = self.inner.docker_bin.clone();
        let pull_name = name.to_string();
        let pull_result = tokio::task::spawn_blocking(move || pull_if_missing(&docker_bin, &pull_name))
            .await;

        match pull_result {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => {
                drop(permit);
                let msg = format!("docker pull failed: {reason}");
                warn!(image = %name, error = %msg, "image load failed during pull");
                progress(ImageStage::Failed(msg.clone()));
                return LoadOutcome::Failed(msg);
            }
            Err(join_err) => {
                drop(permit);
                let msg = format!("pull task panicked: {join_err}");
                warn!(image = %name, error = %msg, "image load failed during pull");
                progress(ImageStage::Failed(msg.clone()));
                return LoadOutcome::Failed(msg);
            }
        }

        // Stage 2: discover layer count for progress reporting.
        let extractor = self.inner.extractor.clone();
        let inspect_name = name.to_string();
        let inspect_result =
            tokio::task::spawn_blocking(move || extractor.docker_inspect_layers(&inspect_name))
                .await;
        let layers_total = match inspect_result {
            Ok(Ok(layer_ids)) => layer_ids.len(),
            Ok(Err(_)) => 0,
            Err(_) => 0,
        };
        progress(ImageStage::Extracting {
            layers_done: 0,
            layers_total,
        });

        // Stage 3: extract layers (the heavy lifting). The current
        // extractor doesn't expose per-layer streaming yet; we report
        // a single jump to "all done" once the call returns.
        let extractor = self.inner.extractor.clone();
        let extract_name = name.to_string();
        let extract_result =
            tokio::task::spawn_blocking(move || extractor.ensure_layers(&extract_name)).await;

        let layers = match extract_result {
            Ok(Ok(layers)) => layers,
            Ok(Err(reason)) => {
                drop(permit);
                let msg = format!("layer extract failed: {reason}");
                warn!(image = %name, error = %msg, "image load failed during extract");
                progress(ImageStage::Failed(msg.clone()));
                return LoadOutcome::Failed(msg);
            }
            Err(join_err) => {
                drop(permit);
                let msg = format!("extract task panicked: {join_err}");
                warn!(image = %name, error = %msg, "image load failed during extract");
                progress(ImageStage::Failed(msg.clone()));
                return LoadOutcome::Failed(msg);
            }
        };

        let layers_count = layers.len();
        self.inner.registry.insert(name.to_string(), layers);
        drop(permit);

        info!(image = %name, layers = layers_count, "image registered");
        progress(ImageStage::Extracting {
            layers_done: layers_count,
            layers_total: layers_count.max(layers_total),
        });
        progress(ImageStage::Done);
        LoadOutcome::Loaded
    }
}

/// Run `docker inspect` to check if `image` is locally available; if not,
/// run `docker pull`. Errors propagate the pull stderr.
fn pull_if_missing(docker_bin: &str, image: &str) -> Result<(), String> {
    let inspect = Command::new(docker_bin)
        .args(["inspect", "--format", "{{.Id}}", image])
        .output();
    if let Ok(out) = inspect {
        if out.status.success() {
            debug!(image = %image, "image already present locally; skipping docker pull");
            return Ok(());
        }
    }

    info!(image = %image, "pulling missing image");
    let pull = Command::new(docker_bin).args(["pull", image]).output();
    match pull {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(stderr.trim().to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::extractor::LayerExtractor;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    fn build_test_loader() -> (ImageLoader, Arc<ImageRegistry>) {
        let tmp = tempdir().unwrap();
        let registry = Arc::new(ImageRegistry::new(tmp.path()));
        let extractor = Arc::new(
            LayerExtractor::new(tmp.path().to_path_buf(), "docker-not-installed".to_string())
                .expect("layer extractor should construct"),
        );
        let loader = ImageLoader::new(registry.clone(), extractor, "docker-not-installed".into(), 4);
        // Keep the temp dir alive by leaking the handle — the test scope
        // is short-lived and we only need the path for registry/manifest.
        std::mem::forget(tmp);
        (loader, registry)
    }

    #[tokio::test]
    async fn already_present_short_circuits() {
        let (loader, registry) = build_test_loader();
        registry.insert(
            "ubuntu:22.04".to_string(),
            vec![PathBuf::from("/some/layer")],
        );

        let outcome = loader.load_one("ubuntu:22.04", |_| {}).await;
        assert_eq!(outcome, LoadOutcome::AlreadyPresent);
    }

    #[tokio::test]
    async fn already_present_emits_done_stage() {
        let (loader, registry) = build_test_loader();
        registry.insert("img".to_string(), vec![PathBuf::from("/a")]);

        let done = Arc::new(AtomicUsize::new(0));
        let done_count = done.clone();
        let outcome = loader
            .load_one("img", move |stage| {
                if matches!(stage, ImageStage::Done) {
                    done_count.fetch_add(1, Ordering::Relaxed);
                }
            })
            .await;
        assert_eq!(outcome, LoadOutcome::AlreadyPresent);
        assert_eq!(done.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn pull_failure_yields_failed_outcome() {
        // No real docker binary on the test path → `docker inspect` and
        // `docker pull` both fail, so the loader must return Failed.
        let (loader, _) = build_test_loader();
        let outcome = loader.load_one("definitely-not-a-real-image:tag", |_| {}).await;
        match outcome {
            LoadOutcome::Failed(msg) => {
                assert!(
                    msg.contains("docker pull failed"),
                    "expected pull-failure message, got: {msg}",
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn concurrent_loads_dedupe() {
        // Two concurrent load_one calls for the same name share a single
        // primary load. Both end up Failed (because docker is fake), but
        // the secondary observes "primary load failed" — proving it
        // waited on the primary's Notify rather than starting its own
        // pipeline.
        let (loader, _) = build_test_loader();
        let l1 = loader.clone();
        let l2 = loader.clone();
        let h1 = tokio::spawn(async move { l1.load_one("ghost:tag", |_| {}).await });
        let h2 = tokio::spawn(async move { l2.load_one("ghost:tag", |_| {}).await });
        let (r1, r2) = tokio::join!(h1, h2);
        let r1 = r1.unwrap();
        let r2 = r2.unwrap();

        let primary_failed = matches!(&r1, LoadOutcome::Failed(m) if m.contains("docker pull failed"))
            || matches!(&r2, LoadOutcome::Failed(m) if m.contains("docker pull failed"));
        let secondary_waited = matches!(&r1, LoadOutcome::Failed(m) if m.contains("primary load failed"))
            || matches!(&r2, LoadOutcome::Failed(m) if m.contains("primary load failed"));
        assert!(
            primary_failed && secondary_waited,
            "expected one primary failure and one secondary waiter; got {r1:?} and {r2:?}",
        );
    }
}

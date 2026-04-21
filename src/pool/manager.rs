//! Sandbox pool manager.
//!
//! Every session creates a dedicated sandbox for the requested image and
//! destroys it on close. `max_sandboxes` acts as a hard concurrency cap.
//!
//! The pool also owns the runtime image registry and an [`ImageLoader`]
//! / [`ImageJobs`] tracker so clients can register images via HTTP after
//! the daemon starts.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use thiserror::Error;

use super::image_jobs::ImageJobs;
use super::session::SessionHandle;
use crate::config::PoolConfig;
use crate::images::{ImageLoader, ImageRegistry, LayerExtractor};
use crate::sandbox::monitor::ProcessMonitor;
use crate::sandbox::{cgroup, Sandbox, SandboxError, SandboxState};

/// Maximum time to wait for busy sandboxes to finish during shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors that can occur during pool operations.
#[derive(Debug, Error)]
pub enum PoolError {
    #[error("pool initialization failed: {0}")]
    InitFailed(String),

    #[error("pool at capacity ({0} sandboxes)")]
    AtCapacity(usize),

    #[error("sandbox error: {0}")]
    SandboxError(#[from] SandboxError),

    #[error("pool is shutting down")]
    ShuttingDown,

    #[error("task execution failed: {0}")]
    ExecutionFailed(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("image not registered: {0}")]
    UnknownImage(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Status of the pool.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PoolStatus {
    pub total: usize,
    pub busy: usize,
    pub error: usize,
    pub max_sandboxes: usize,
    pub tasks_executed: u64,
    pub tasks_succeeded: u64,
    pub tasks_failed: u64,
    pub avg_task_duration_ms: u64,
    pub registered_images: usize,
}

/// A sandbox pool that manages multiple sandboxes for task execution.
#[derive(Clone)]
pub struct Pool {
    pub(super) config: Arc<PoolConfig>,
    pub(super) sandboxes: Arc<RwLock<HashMap<String, Arc<Sandbox>>>>,
    pub(super) sessions: Arc<RwLock<HashMap<String, SessionHandle>>>,
    pub(super) shutdown: Arc<AtomicBool>,
    pub(super) next_sandbox_id: Arc<AtomicUsize>,
    /// Shared process monitor for resource enforcement when cgroups are
    /// unavailable. Created once during pool init and injected into every
    /// sandbox so a single background task polls all tracked processes.
    pub(super) process_monitor: Option<ProcessMonitor>,
    /// Runtime image registry. Always starts empty; clients register
    /// images via the HTTP API.
    pub(super) image_registry: Arc<ImageRegistry>,
    /// Async pull+extract pipeline backed by the registry above.
    pub(super) image_loader: ImageLoader,
    /// Tracker for in-flight and historical image-load jobs.
    pub(super) image_jobs: ImageJobs,
}

impl Pool {
    pub async fn new(config: PoolConfig) -> Result<Self, PoolError> {
        // The layer cache directory must exist before extraction can run.
        std::fs::create_dir_all(&config.image_cache.layers_dir)
            .map_err(|e| PoolError::InitFailed(format!("create layers_dir: {e}")))?;

        let monitor = if !cgroup::has_delegated_cgroup() {
            tracing::info!(
                "cgroups unavailable; starting shared process monitor for resource enforcement"
            );
            Some(ProcessMonitor::spawn())
        } else {
            None
        };

        // Remove stale xattr rootfs caches from a previous run.
        if let Ok(entries) = std::fs::read_dir(&config.overlay_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with(".rootfs-cache") {
                    tracing::info!(
                        path = %entry.path().display(),
                        "removing stale rootfs cache from previous run"
                    );
                    if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                        tracing::warn!(
                            path = %entry.path().display(),
                            error = %e,
                            "failed to remove rootfs cache; will attempt to overwrite"
                        );
                    }
                }
            }
        }

        let registry = Arc::new(ImageRegistry::new(&config.image_cache.layers_dir));
        let extractor = Arc::new(
            LayerExtractor::new(
                config.image_cache.layers_dir.clone(),
                config.image_cache.docker.clone(),
            )
            .map_err(|e| PoolError::InitFailed(e.to_string()))?,
        );
        let loader = ImageLoader::new(
            registry.clone(),
            extractor,
            config.image_cache.docker.clone(),
            config.image_cache.pull_concurrency,
        );
        let jobs = ImageJobs::new(loader.clone(), registry.clone());

        let pool = Self {
            config: Arc::new(config),
            sandboxes: Arc::new(RwLock::new(HashMap::new())),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
            next_sandbox_id: Arc::new(AtomicUsize::new(0)),
            process_monitor: monitor,
            image_registry: registry,
            image_loader: loader,
            image_jobs: jobs,
        };

        tracing::info!(
            "pool ready (max_sandboxes={}, registry empty - register images via API)",
            pool.config.max_sandboxes,
        );

        Ok(pool)
    }

    /// Borrow the runtime image registry (callers can list/check images).
    pub fn image_registry(&self) -> &Arc<ImageRegistry> {
        &self.image_registry
    }

    /// Borrow the image loader (drives pull + extract on demand).
    pub fn image_loader(&self) -> &ImageLoader {
        &self.image_loader
    }

    /// Borrow the image-jobs tracker (HTTP layer uses this for register/poll).
    pub fn image_jobs(&self) -> &ImageJobs {
        &self.image_jobs
    }

    /// Create a sandbox for a named image from the registry.
    ///
    /// Returns `PoolError::AtCapacity` if `max_sandboxes` has been reached,
    /// or `PoolError::UnknownImage` if the image is not registered.
    pub(super) async fn create_sandbox_for_image(
        &self,
        image_name: &str,
    ) -> Result<Arc<Sandbox>, PoolError> {
        {
            let count = self.sandboxes.read().len();
            if count >= self.config.max_sandboxes {
                return Err(PoolError::AtCapacity(self.config.max_sandboxes));
            }
        }

        let layers = self
            .image_registry
            .resolve(image_name)
            .ok_or_else(|| PoolError::UnknownImage(image_name.to_string()))?;

        let idx = self.next_sandbox_id.fetch_add(1, Ordering::Relaxed);
        let sandbox_id = format!("sandbox-{idx}");

        tracing::debug!(
            "creating sandbox {sandbox_id} for image {image_name} ({} layers)",
            layers.len(),
        );

        let mut sandbox = Sandbox::new(sandbox_id.clone(), &self.config)
            .map_err(|e| PoolError::InitFailed(e.to_string()))?;

        if let Some(ref monitor) = self.process_monitor {
            sandbox.set_process_monitor(monitor.clone());
        }

        sandbox
            .initialize(&layers, &self.config.overlay_driver)
            .await
            .map_err(|e| PoolError::InitFailed(e.to_string()))?;

        let sandbox = Arc::new(sandbox);
        self.sandboxes
            .write()
            .insert(sandbox_id, sandbox.clone());

        Ok(sandbox)
    }

    /// Remove a sandbox from the pool entirely (cleanup + deregister).
    pub(super) fn remove_sandbox(&self, sandbox_id: &str) {
        if let Some(sandbox) = self.sandboxes.write().remove(sandbox_id) {
            if let Err(error) = sandbox.cleanup() {
                tracing::debug!(
                    %error,
                    sandbox_id = %sandbox.id(),
                    "failed to cleanup sandbox while removing it from the pool"
                );
            }
        }
    }

    pub fn status(&self) -> PoolStatus {
        let sandboxes = self.sandboxes.read();

        let mut busy = 0;
        let mut error = 0;
        let mut tasks_executed = 0_u64;
        let mut tasks_succeeded = 0_u64;
        let mut tasks_failed = 0_u64;
        let mut total_duration_ms = 0_u64;

        for sandbox in sandboxes.values() {
            match sandbox.state() {
                SandboxState::Running { .. } => busy += 1,
                SandboxState::Error(_) => error += 1,
                _ => {}
            }

            let stats = sandbox.stats();
            tasks_executed += stats.tasks_executed.load(Ordering::Relaxed);
            tasks_succeeded += stats.successful_tasks.load(Ordering::Relaxed);
            tasks_failed += stats.failed_tasks.load(Ordering::Relaxed);
            total_duration_ms += stats.total_execution_time_ms.load(Ordering::Relaxed);
        }

        let avg_duration = if tasks_executed > 0 {
            total_duration_ms / tasks_executed
        } else {
            0
        };

        PoolStatus {
            total: sandboxes.len(),
            busy,
            error,
            max_sandboxes: self.config.max_sandboxes,
            tasks_executed,
            tasks_succeeded,
            tasks_failed,
            avg_task_duration_ms: avg_duration,
            registered_images: self.image_registry.len(),
        }
    }

    pub fn config(&self) -> &PoolConfig {
        self.config.as_ref()
    }

    pub async fn shutdown(&self) {
        tracing::info!("Shutting down pool...");
        self.shutdown.store(true, Ordering::Relaxed);

        // Cancel any in-flight image-load jobs first so the worker tasks
        // stop racing with sandbox teardown.
        self.image_jobs.shutdown().await;

        let session_ids: Vec<String> = self.sessions.read().keys().cloned().collect();
        for session_id in session_ids {
            if let Err(error) = self.close_session(&session_id).await {
                tracing::error!(
                    %error,
                    session_id = %session_id,
                    "failed to close session during shutdown"
                );
            }
        }

        let start = Instant::now();
        while start.elapsed() < SHUTDOWN_TIMEOUT {
            if self.status().busy == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        if let Some(ref monitor) = self.process_monitor {
            monitor.shutdown().await;
        }

        let sandboxes = self.sandboxes.read();
        for sandbox in sandboxes.values() {
            if let Err(error) = sandbox.cleanup() {
                tracing::error!(
                    %error,
                    sandbox_id = %sandbox.id(),
                    "failed to cleanup sandbox during shutdown"
                );
            }
        }

        tracing::info!("Pool shutdown complete");
    }
}

// NOTE: Pool is Clone (all Arc fields) so Drop must NOT set the shared
// shutdown flag — axum clones the state for every request and drops it
// when the handler returns, which would poison the pool after the first
// request.  Use pool.shutdown().await for explicit teardown instead.

//! Sandbox pool manager with auto-scaling.
//!
//! The pool starts with `min_sandboxes` pre-created instances and scales up
//! on-demand (up to `max_sandboxes`) when a session request arrives and no
//! idle sandbox is available. A background task periodically reclaims
//! sandboxes that have been idle longer than `idle_timeout`, shrinking the
//! pool back toward `min_sandboxes`.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use thiserror::Error;
use tokio::sync::Notify;

use super::session::SessionHandle;
use crate::config::PoolConfig;
use crate::sandbox::monitor::ProcessMonitor;
use crate::sandbox::{cgroup, Sandbox, SandboxError, SandboxState};

/// When the pool is at max capacity, how long `create_session` waits for an
/// idle sandbox before giving up.
pub(super) const SANDBOX_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the background scaler wakes up.
pub(super) const SCALER_INTERVAL: Duration = Duration::from_secs(10);

/// Maximum time to wait for busy sandboxes to finish during shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors that can occur during pool operations.
#[derive(Debug, Error)]
pub enum PoolError {
    #[error("pool initialization failed: {0}")]
    InitFailed(String),

    #[error("no idle sandbox available (timed out after {0}s)")]
    NoIdleSandbox(u64),

    #[error("sandbox error: {0}")]
    SandboxError(#[from] SandboxError),

    #[error("pool is shutting down")]
    ShuttingDown,

    #[error("task execution failed: {0}")]
    ExecutionFailed(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Status of the pool.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PoolStatus {
    pub total: usize,
    pub idle: usize,
    pub busy: usize,
    pub reserved: usize,
    pub error: usize,
    pub min_sandboxes: usize,
    pub max_sandboxes: usize,
    pub tasks_executed: u64,
    pub tasks_succeeded: u64,
    pub tasks_failed: u64,
    pub avg_task_duration_ms: u64,
}

/// A sandbox pool that manages multiple sandboxes for task execution.
///
/// Scales between `config.min_sandboxes` and `config.max_sandboxes`
/// automatically based on demand.
#[derive(Clone)]
pub struct Pool {
    pub(super) config: Arc<PoolConfig>,
    pub(super) sandboxes: Arc<RwLock<HashMap<String, Arc<Sandbox>>>>,
    pub(super) idle_queue: Arc<Mutex<VecDeque<String>>>,
    pub(super) idle_notify: Arc<Notify>,
    pub(super) sessions: Arc<RwLock<HashMap<String, SessionHandle>>>,
    pub(super) shutdown: Arc<AtomicBool>,
    pub(super) next_sandbox_id: Arc<AtomicUsize>,
    pub(super) last_scale_event: Arc<Mutex<Instant>>,
    pub(super) idle_since: Arc<RwLock<HashMap<String, Instant>>>,
    /// Shared process monitor for resource enforcement when cgroups are
    /// unavailable. Created once during pool init and injected into every
    /// sandbox so a single background task polls all tracked processes.
    pub(super) process_monitor: Option<ProcessMonitor>,
}

impl Pool {
    pub async fn new(config: PoolConfig) -> Result<Self, PoolError> {
        let monitor = if !cgroup::has_delegated_cgroup() {
            tracing::info!(
                "cgroups unavailable; starting shared process monitor for resource enforcement"
            );
            Some(ProcessMonitor::spawn())
        } else {
            None
        };

        let pool = Self {
            config: Arc::new(config),
            sandboxes: Arc::new(RwLock::new(HashMap::new())),
            idle_queue: Arc::new(Mutex::new(VecDeque::new())),
            idle_notify: Arc::new(Notify::new()),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
            next_sandbox_id: Arc::new(AtomicUsize::new(0)),
            last_scale_event: Arc::new(Mutex::new(Instant::now())),
            idle_since: Arc::new(RwLock::new(HashMap::new())),
            process_monitor: monitor,
        };

        pool.initialize_sandboxes().await?;
        pool.spawn_scaler_task();

        Ok(pool)
    }

    /// Create the initial `min_sandboxes` pool members.
    async fn initialize_sandboxes(&self) -> Result<(), PoolError> {
        tracing::info!("Initializing {} sandboxes...", self.config.min_sandboxes);

        let now = Instant::now();
        for _ in 0..self.config.min_sandboxes {
            let sandbox = self.create_sandbox().await?;
            self.store_idle_sandbox(sandbox, now);
        }

        self.idle_notify.notify_waiters();
        tracing::info!(
            "Pool initialized with {} sandboxes (min={}, max={})",
            self.config.min_sandboxes,
            self.config.min_sandboxes,
            self.config.max_sandboxes,
        );
        Ok(())
    }

    /// Allocate a unique ID and create + initialize a sandbox.
    pub(super) async fn create_sandbox(&self) -> Result<Arc<Sandbox>, PoolError> {
        let idx = self.next_sandbox_id.fetch_add(1, Ordering::Relaxed);
        let sandbox_id = format!("sandbox-{idx}");
        self.create_sandbox_with_id(sandbox_id).await
    }

    pub(super) async fn create_sandbox_with_id(
        &self,
        sandbox_id: String,
    ) -> Result<Arc<Sandbox>, PoolError> {
        tracing::debug!("Creating sandbox: {sandbox_id}");

        let mut sandbox = Sandbox::new(sandbox_id.clone(), &self.config)
            .map_err(|e| PoolError::InitFailed(e.to_string()))?;

        if let Some(ref monitor) = self.process_monitor {
            sandbox.set_process_monitor(monitor.clone());
        }

        sandbox
            .initialize(&self.config.base_image, &self.config.overlay_driver)
            .await
            .map_err(|e| PoolError::InitFailed(e.to_string()))?;

        Ok(Arc::new(sandbox))
    }

    /// Create a sandbox with a custom rootfs (for per-session base images).
    /// The sandbox is tracked in the pool but NOT added to the idle queue.
    pub(super) async fn create_sandbox_with_rootfs(
        &self,
        base_image: &Path,
    ) -> Result<Arc<Sandbox>, PoolError> {
        let idx = self.next_sandbox_id.fetch_add(1, Ordering::Relaxed);
        let sandbox_id = format!("sandbox-custom-{idx}");

        tracing::debug!("Creating custom-rootfs sandbox: {sandbox_id} with base_image={}", base_image.display());

        let mut sandbox = Sandbox::new(sandbox_id.clone(), &self.config)
            .map_err(|e| PoolError::InitFailed(e.to_string()))?;

        if let Some(ref monitor) = self.process_monitor {
            sandbox.set_process_monitor(monitor.clone());
        }

        sandbox
            .initialize(base_image, &self.config.overlay_driver)
            .await
            .map_err(|e| PoolError::InitFailed(e.to_string()))?;

        let sandbox = Arc::new(sandbox);
        self.sandboxes
            .write()
            .insert(sandbox_id, sandbox.clone());

        Ok(sandbox)
    }

    pub(super) fn store_idle_sandbox(&self, sandbox: Arc<Sandbox>, idle_since: Instant) {
        let sandbox_id = sandbox.id().to_string();
        self.sandboxes.write().insert(sandbox_id.clone(), sandbox);
        self.idle_since
            .write()
            .insert(sandbox_id.clone(), idle_since);
        self.idle_queue.lock().push_back(sandbox_id);
        self.idle_notify.notify_one();
    }

    pub fn status(&self) -> PoolStatus {
        let sandboxes = self.sandboxes.read();
        let idle_count = self.idle_queue.lock().len();

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

        let total = sandboxes.len();

        PoolStatus {
            total,
            idle: idle_count,
            busy,
            reserved: total.saturating_sub(idle_count + busy + error),
            error,
            min_sandboxes: self.config.min_sandboxes,
            max_sandboxes: self.config.max_sandboxes,
            tasks_executed,
            tasks_succeeded,
            tasks_failed,
            avg_task_duration_ms: avg_duration,
        }
    }

    pub fn config(&self) -> &PoolConfig {
        self.config.as_ref()
    }

    pub async fn shutdown(&self) {
        tracing::info!("Shutting down pool...");
        self.shutdown.store(true, Ordering::Relaxed);
        self.idle_notify.notify_waiters();

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

#[cfg(test)]
mod tests {
    use super::PoolStatus;

    #[test]
    fn pool_status_totals_stay_consistent() {
        let status = PoolStatus {
            total: 12,
            idle: 8,
            busy: 2,
            reserved: 2,
            error: 0,
            min_sandboxes: 10,
            max_sandboxes: 40,
            tasks_executed: 100,
            tasks_succeeded: 95,
            tasks_failed: 5,
            avg_task_duration_ms: 500,
        };

        assert_eq!(
            status.total,
            status.idle + status.busy + status.reserved + status.error
        );
        assert!(status.busy <= status.total);
        assert!(status.min_sandboxes <= status.max_sandboxes);
    }
}

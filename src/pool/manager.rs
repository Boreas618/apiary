//! Sandbox pool manager with auto-scaling.
//!
//! The pool starts with `min_sandboxes` pre-created instances and scales up
//! on-demand (up to `max_sandboxes`) when a session request arrives and no
//! idle sandbox is available.  A background task periodically reclaims
//! sandboxes that have been idle longer than `idle_timeout`, shrinking the
//! pool back toward `min_sandboxes`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use uuid::Uuid;

use crate::config::PoolConfig;
use crate::sandbox::{Sandbox, SandboxError, SandboxState};
use crate::task::{Task, TaskResult};

/// When the pool is at max capacity, how long `create_session` waits for an
/// idle sandbox before giving up.
const SANDBOX_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the background scaler wakes up.
const SCALER_INTERVAL: Duration = Duration::from_secs(10);

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

/// Options for creating a persistent session.
#[derive(Debug, Clone, Default)]
pub struct SessionOptions {
    /// Default working directory for tasks executed in this session.
    pub working_dir: Option<PathBuf>,
}

impl SessionOptions {
    /// Set the session working directory.
    pub fn working_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(path.into());
        self
    }
}

#[derive(Clone)]
struct SessionHandle {
    sandbox_id: String,
    working_dir: PathBuf,
    execution_lock: Arc<AsyncMutex<()>>,
}

fn normalize_session_working_dir(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new("/").join(path)
    }
}

fn resolve_task_working_dir(
    session_working_dir: &Path,
    task_working_dir: Option<&Path>,
) -> PathBuf {
    match task_working_dir {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => session_working_dir.join(path),
        None => session_working_dir.to_path_buf(),
    }
}

/// A sandbox pool that manages multiple sandboxes for task execution.
///
/// Scales between `config.min_sandboxes` and `config.max_sandboxes`
/// automatically based on demand.
#[derive(Clone)]
pub struct Pool {
    config: Arc<PoolConfig>,
    sandboxes: Arc<RwLock<HashMap<String, Arc<Sandbox>>>>,
    idle_queue: Arc<Mutex<VecDeque<String>>>,
    idle_notify: Arc<Notify>,
    sessions: Arc<RwLock<HashMap<String, SessionHandle>>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    next_sandbox_id: Arc<AtomicUsize>,
    last_scale_event: Arc<Mutex<Instant>>,
    idle_since: Arc<RwLock<HashMap<String, Instant>>>,
}

impl Pool {
    pub async fn new(config: PoolConfig) -> Result<Self, PoolError> {
        let config = Arc::new(config);
        let sandboxes = Arc::new(RwLock::new(HashMap::new()));
        let idle_queue = Arc::new(Mutex::new(VecDeque::new()));
        let idle_notify = Arc::new(Notify::new());
        let sessions = Arc::new(RwLock::new(HashMap::new()));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let next_sandbox_id = Arc::new(AtomicUsize::new(0));
        let last_scale_event = Arc::new(Mutex::new(Instant::now()));
        let idle_since = Arc::new(RwLock::new(HashMap::new()));

        let pool = Self {
            config,
            sandboxes,
            idle_queue,
            idle_notify,
            sessions,
            shutdown,
            next_sandbox_id,
            last_scale_event,
            idle_since,
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
            let id = sandbox.id().to_string();
            self.sandboxes.write().insert(id.clone(), Arc::new(sandbox));
            self.idle_since.write().insert(id.clone(), now);
            self.idle_queue.lock().push_back(id);
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
    async fn create_sandbox(&self) -> Result<Sandbox, PoolError> {
        let idx = self.next_sandbox_id.fetch_add(1, Ordering::Relaxed);
        let sandbox_id = format!("sandbox-{idx}");
        tracing::debug!("Creating sandbox: {sandbox_id}");

        let mut sandbox = Sandbox::new(sandbox_id, &self.config)
            .map_err(|e| PoolError::InitFailed(e.to_string()))?;

        sandbox
            .initialize(&self.config.base_image, &self.config.overlay_driver)
            .await
            .map_err(|e| PoolError::InitFailed(e.to_string()))?;

        Ok(sandbox)
    }

    /// Try to pop an idle sandbox, or scale up if possible, or wait.
    ///
    /// 1. Pop from `idle_queue` -> done.
    /// 2. If `total < max_sandboxes` -> create a new sandbox inline and also
    ///    spawn background creation of `scale_up_step - 1` more.
    /// 3. If at capacity -> wait up to [`SANDBOX_ACQUIRE_TIMEOUT`].
    async fn acquire_sandbox(&self) -> Result<Arc<Sandbox>, PoolError> {
        // Fast path: grab from idle queue.
        if let Some(sb) = self.try_pop_idle() {
            return Ok(sb);
        }

        // Scale-up path: create on-demand if below max.
        let total = self.sandboxes.read().len();
        if total < self.config.max_sandboxes {
            let sandbox = self.create_sandbox().await?;
            let id = sandbox.id().to_string();
            let sandbox = Arc::new(sandbox);
            self.sandboxes.write().insert(id.clone(), sandbox.clone());

            *self.last_scale_event.lock() = Instant::now();
            tracing::info!(
                sandbox_id = %id,
                total = total + 1,
                "scaled up: created sandbox on demand"
            );

            // Pre-warm: spawn async creation of more sandboxes (up to step-1
            // additional, capped at max_sandboxes).
            let extra = (self.config.scale_up_step.saturating_sub(1))
                .min(self.config.max_sandboxes.saturating_sub(total + 1));
            if extra > 0 {
                let pool = self.clone();
                tokio::spawn(async move {
                    for _ in 0..extra {
                        if pool.shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        let current = pool.sandboxes.read().len();
                        if current >= pool.config.max_sandboxes {
                            break;
                        }
                        match pool.create_sandbox().await {
                            Ok(sb) => {
                                let sb_id = sb.id().to_string();
                                let sb = Arc::new(sb);
                                pool.sandboxes.write().insert(sb_id.clone(), sb);
                                pool.idle_since
                                    .write()
                                    .insert(sb_id.clone(), Instant::now());
                                pool.idle_queue.lock().push_back(sb_id.clone());
                                pool.idle_notify.notify_one();
                                tracing::info!(
                                    sandbox_id = %sb_id,
                                    total = pool.sandboxes.read().len(),
                                    "scaled up: pre-warmed sandbox"
                                );
                            }
                            Err(e) => {
                                tracing::error!("failed to pre-warm sandbox: {e}");
                                break;
                            }
                        }
                    }
                });
            }

            return Ok(sandbox);
        }

        // At capacity: wait for an idle sandbox with timeout.
        tracing::debug!(
            "pool at max capacity ({}), waiting for idle sandbox",
            self.config.max_sandboxes
        );
        let deadline = tokio::time::Instant::now() + SANDBOX_ACQUIRE_TIMEOUT;

        loop {
            if let Some(sb) = self.try_pop_idle() {
                return Ok(sb);
            }

            if self.shutdown.load(Ordering::Relaxed) {
                return Err(PoolError::ShuttingDown);
            }

            let notified = self.idle_notify.notified();
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(PoolError::NoIdleSandbox(SANDBOX_ACQUIRE_TIMEOUT.as_secs()));
            }

            match tokio::time::timeout(remaining, notified).await {
                Ok(()) => continue,
                Err(_) => {
                    return Err(PoolError::NoIdleSandbox(SANDBOX_ACQUIRE_TIMEOUT.as_secs()));
                }
            }
        }
    }

    /// Pop one sandbox from the idle queue (non-blocking).
    fn try_pop_idle(&self) -> Option<Arc<Sandbox>> {
        loop {
            let id = self.idle_queue.lock().pop_front()?;
            self.idle_since.write().remove(&id);
            if let Some(sb) = self.sandboxes.read().get(&id).cloned() {
                return Some(sb);
            }
            // ID was stale (sandbox removed); try again.
        }
    }

    /// Return a sandbox to the idle pool.
    fn return_to_idle(&self, sandbox_id: &str) {
        self.idle_since
            .write()
            .insert(sandbox_id.to_string(), Instant::now());
        self.idle_queue.lock().push_back(sandbox_id.to_string());
        self.idle_notify.notify_one();
    }

    // ------------------------------------------------------------------
    // Session API
    // ------------------------------------------------------------------

    /// Create a persistent session bound to a single sandbox.
    ///
    /// If no idle sandbox is available the pool scales up automatically (up to
    /// `max_sandboxes`). Only when the hard cap is reached does the call block
    /// with a timeout.
    pub async fn create_session(
        &self,
        options: SessionOptions,
    ) -> Result<String, PoolError> {
        if self.shutdown.load(Ordering::Relaxed) {
            return Err(PoolError::ShuttingDown);
        }

        let sandbox = self.acquire_sandbox().await?;
        let working_dir = normalize_session_working_dir(
            options
                .working_dir
                .as_deref()
                .unwrap_or(self.config.default_workdir.as_path()),
        );

        let session_id = Uuid::new_v4().to_string();

        self.sessions.write().insert(
            session_id.clone(),
            SessionHandle {
                sandbox_id: sandbox.id().to_string(),
                working_dir: working_dir.clone(),
                execution_lock: Arc::new(AsyncMutex::new(())),
            },
        );

        tracing::info!(
            session_id = %session_id,
            sandbox_id = %sandbox.id(),
            working_dir = %working_dir.display(),
            "session created"
        );
        Ok(session_id)
    }

    /// Execute a task inside a persistent session.
    pub async fn execute_in_session(
        &self,
        session_id: &str,
        task: Task,
    ) -> Result<TaskResult, PoolError> {
        if self.shutdown.load(Ordering::Relaxed) {
            return Err(PoolError::ShuttingDown);
        }

        let session = {
            let sessions = self.sessions.read();
            sessions
                .get(session_id)
                .cloned()
                .ok_or_else(|| PoolError::SessionNotFound(session_id.to_string()))?
        };

        let sandbox = {
            let sandboxes = self.sandboxes.read();
            sandboxes.get(&session.sandbox_id).cloned().ok_or_else(|| {
                PoolError::ExecutionFailed(format!(
                    "session {session_id} is bound to missing sandbox {}",
                    session.sandbox_id
                ))
            })?
        };

        let _execution_guard = session.execution_lock.lock().await;

        // Prevent races with close_session: if the session disappeared while
        // waiting for the lock, do not run the task.
        {
            let sessions = self.sessions.read();
            match sessions.get(session_id) {
                Some(current) if current.sandbox_id == session.sandbox_id => {}
                _ => return Err(PoolError::SessionNotFound(session_id.to_string())),
            }
        }

        let mut task = task;
        let resolved_working_dir =
            resolve_task_working_dir(&session.working_dir, task.working_dir.as_deref());
        task.working_dir = Some(resolved_working_dir);

        sandbox.execute(task).await.map_err(PoolError::SandboxError)
    }

    /// Close a persistent session, reset its sandbox, and return it to the idle pool.
    pub async fn close_session(&self, session_id: &str) -> Result<(), PoolError> {
        let session = {
            let mut sessions = self.sessions.write();
            sessions
                .remove(session_id)
                .ok_or_else(|| PoolError::SessionNotFound(session_id.to_string()))?
        };

        // Wait for any in-flight session execution to complete before reset.
        let _execution_guard = session.execution_lock.lock().await;

        let sandbox = {
            let sandboxes = self.sandboxes.read();
            sandboxes.get(&session.sandbox_id).cloned().ok_or_else(|| {
                PoolError::ExecutionFailed(format!(
                    "session {session_id} is bound to missing sandbox {}",
                    session.sandbox_id
                ))
            })?
        };

        match sandbox.reset().await {
            Ok(()) => {
                self.return_to_idle(&session.sandbox_id);
                tracing::info!(
                    session_id = %session_id,
                    sandbox_id = %session.sandbox_id,
                    "session closed"
                );
                Ok(())
            }
            Err(error) => {
                tracing::error!(
                    %error,
                    session_id = %session_id,
                    sandbox_id = %session.sandbox_id,
                    "session sandbox reset failed; replacing"
                );
                self.sandboxes.write().remove(&session.sandbox_id);
                drop(sandbox);

                self.replace_sandbox(&session.sandbox_id).await?;
                Ok(())
            }
        }
    }

    /// Run a task in an ephemeral session: create a session, execute the
    /// task, then close the session regardless of outcome.
    ///
    /// This is a convenience wrapper around [`create_session`] +
    /// [`execute_in_session`] + [`close_session`] that handles the
    /// lifecycle and error priority automatically.
    pub async fn run_task(
        &self,
        task: Task,
        options: SessionOptions,
    ) -> Result<TaskResult, PoolError> {
        let session_id = self.create_session(options).await?;
        let exec_result = self.execute_in_session(&session_id, task).await;
        let close_result = self.close_session(&session_id).await;

        match (exec_result, close_result) {
            (Ok(result), Ok(())) => Ok(result),
            (Ok(_), Err(close_err)) => Err(close_err),
            (Err(exec_err), Ok(())) => Err(exec_err),
            (Err(exec_err), Err(close_err)) => {
                tracing::error!(
                    %close_err,
                    session_id = %session_id,
                    "failed to close session after task error"
                );
                Err(exec_err)
            }
        }
    }

    // ------------------------------------------------------------------
    // Status / config
    // ------------------------------------------------------------------

    pub fn status(&self) -> PoolStatus {
        let sandboxes = self.sandboxes.read();
        let idle_count = self.idle_queue.lock().len();
        let reserved_count = self.sessions.read().len();

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
            idle: idle_count,
            busy,
            reserved: reserved_count,
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
        &self.config
    }

    pub async fn shutdown(&self) {
        tracing::info!("Shutting down pool...");
        self.shutdown.store(true, Ordering::Relaxed);
        self.idle_notify.notify_waiters();

        let session_ids: Vec<String> = self.sessions.read().keys().cloned().collect();
        for session_id in session_ids {
            if let Err(error) = self.close_session(&session_id).await {
                tracing::error!(%error, session_id = %session_id, "failed to close session during shutdown");
            }
        }

        let timeout = Duration::from_secs(30);
        let start = Instant::now();

        while start.elapsed() < timeout {
            let status = self.status();
            if status.busy == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let sandboxes = self.sandboxes.read();
        for sandbox in sandboxes.values() {
            let _ = sandbox.cleanup();
        }

        tracing::info!("Pool shutdown complete");
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Create a replacement sandbox after a failed reset, keeping the same ID.
    async fn replace_sandbox(&self, sandbox_id: &str) -> Result<(), PoolError> {
        tracing::info!(sandbox_id = %sandbox_id, "creating replacement sandbox");

        let mut sandbox = Sandbox::new(sandbox_id.to_string(), &self.config)
            .map_err(|e| PoolError::InitFailed(format!("replacement creation failed: {e}")))?;

        sandbox
            .initialize(&self.config.base_image, &self.config.overlay_driver)
            .await
            .map_err(|e| PoolError::InitFailed(format!("replacement init failed: {e}")))?;

        let sandbox = Arc::new(sandbox);
        self.sandboxes
            .write()
            .insert(sandbox_id.to_string(), sandbox);
        self.return_to_idle(sandbox_id);

        tracing::info!(sandbox_id = %sandbox_id, "replacement sandbox ready");
        Ok(())
    }

    /// Remove a sandbox from the pool entirely (cleanup + deregister).
    fn remove_sandbox(&self, sandbox_id: &str) {
        if let Some(sb) = self.sandboxes.write().remove(sandbox_id) {
            let _ = sb.cleanup();
        }
        self.idle_since.write().remove(sandbox_id);
    }

    // ------------------------------------------------------------------
    // Background scaler
    // ------------------------------------------------------------------

    /// Spawn a background tokio task that periodically:
    ///   - Scales down idle sandboxes beyond `min_sandboxes` that have exceeded
    ///     `idle_timeout`.
    ///   - Pro-actively scales up when idle count hits 0 and pool has capacity.
    fn spawn_scaler_task(&self) {
        let pool = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SCALER_INTERVAL).await;

                if pool.shutdown.load(Ordering::Relaxed) {
                    tracing::debug!("scaler: shutdown detected, exiting");
                    break;
                }

                pool.scaler_tick().await;
            }
        });
    }

    fn cooldown_elapsed(&self) -> bool {
        self.last_scale_event.lock().elapsed() >= self.config.cooldown
    }

    async fn scaler_tick(&self) {
        let total = self.sandboxes.read().len();
        let idle_count = self.idle_queue.lock().len();

        if total > self.config.min_sandboxes && idle_count > 0 && self.cooldown_elapsed() {
            self.try_scale_down(total);
        }

        if idle_count == 0 && total < self.config.max_sandboxes && self.cooldown_elapsed() {
            self.try_proactive_scale_up(total).await;
        }
    }

    /// Remove the sandbox that has been idle longest, provided it has exceeded
    /// `idle_timeout` and the pool would still be at or above `min_sandboxes`.
    fn try_scale_down(&self, total: usize) {
        if total <= self.config.min_sandboxes {
            return;
        }

        let now = Instant::now();
        let idle_timeout = self.config.idle_timeout;

        // Find the sandbox with the oldest idle timestamp that exceeds the
        // timeout.  We scan the idle_queue rather than idle_since so we only
        // consider truly idle (non-reserved) sandboxes.
        let mut idle_queue = self.idle_queue.lock();
        let mut oldest_idx: Option<usize> = None;
        let mut oldest_time = now;

        {
            let idle_since = self.idle_since.read();
            for (i, id) in idle_queue.iter().enumerate() {
                if let Some(&ts) = idle_since.get(id) {
                    if now.duration_since(ts) >= idle_timeout && ts < oldest_time {
                        oldest_idx = Some(i);
                        oldest_time = ts;
                    }
                }
            }
        }

        if let Some(idx) = oldest_idx {
            if let Some(id) = idle_queue.remove(idx) {
                drop(idle_queue);

                self.remove_sandbox(&id);
                *self.last_scale_event.lock() = Instant::now();

                tracing::info!(
                    sandbox_id = %id,
                    total = self.sandboxes.read().len(),
                    "scaled down: removed idle sandbox"
                );
            }
        }
    }

    /// Pre-create one sandbox so the next `create_session` doesn't have to
    /// wait for sandbox initialisation.
    async fn try_proactive_scale_up(&self, total: usize) {
        if total >= self.config.max_sandboxes {
            return;
        }

        match self.create_sandbox().await {
            Ok(sb) => {
                let id = sb.id().to_string();
                self.sandboxes.write().insert(id.clone(), Arc::new(sb));
                self.return_to_idle(&id);
                *self.last_scale_event.lock() = Instant::now();

                tracing::info!(
                    sandbox_id = %id,
                    total = self.sandboxes.read().len(),
                    "scaled up: proactive warm-up"
                );
            }
            Err(e) => {
                tracing::error!("proactive scale-up failed: {e}");
            }
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_pool_status_default() {
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

    #[test]
    fn test_normalize_session_working_dir_makes_paths_absolute() {
        assert_eq!(
            normalize_session_working_dir(Path::new("/workspace/project")),
            PathBuf::from("/workspace/project")
        );
        assert_eq!(
            normalize_session_working_dir(Path::new("workspace/project")),
            PathBuf::from("/workspace/project")
        );
    }

    #[test]
    fn test_resolve_task_working_dir_prefers_task_override() {
        let session_working_dir = Path::new("/workspace/project");

        assert_eq!(
            resolve_task_working_dir(session_working_dir, None),
            PathBuf::from("/workspace/project")
        );
        assert_eq!(
            resolve_task_working_dir(session_working_dir, Some(Path::new("src"))),
            PathBuf::from("/workspace/project/src")
        );
        assert_eq!(
            resolve_task_working_dir(session_working_dir, Some(Path::new("/tmp"))),
            PathBuf::from("/tmp")
        );
    }
}

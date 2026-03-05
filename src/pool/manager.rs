//! Sandbox pool manager implementation.
//!
//! The pool manager maintains a pre-created set of sandboxes and
//! assigns them to persistent client sessions.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use thiserror::Error;
use tokio::sync::{mpsc, Mutex as AsyncMutex, Notify};
use uuid::Uuid;

use crate::config::PoolConfig;
use crate::sandbox::{Sandbox, SandboxError, SandboxState};
use crate::task::{Task, TaskOutputEvent, TaskResult};

/// Maximum time session creation waits for an idle sandbox.
const SANDBOX_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(60);

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
#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub total: usize,
    pub idle: usize,
    pub busy: usize,
    pub reserved: usize,
    pub error: usize,
    pub tasks_executed: u64,
    pub tasks_succeeded: u64,
    pub tasks_failed: u64,
    pub avg_task_duration_ms: u64,
}

#[derive(Clone)]
struct SessionHandle {
    sandbox_id: String,
    execution_lock: Arc<AsyncMutex<()>>,
}

/// A sandbox pool that manages multiple sandboxes for task execution.
pub struct Pool {
    config: Arc<PoolConfig>,
    sandboxes: Arc<RwLock<HashMap<String, Arc<Sandbox>>>>,
    idle_queue: Arc<Mutex<Vec<String>>>,
    idle_notify: Arc<Notify>,
    sessions: Arc<RwLock<HashMap<String, SessionHandle>>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl Pool {
    pub async fn new(config: PoolConfig) -> Result<Self, PoolError> {
        let config = Arc::new(config);
        let sandboxes = Arc::new(RwLock::new(HashMap::new()));
        let idle_queue = Arc::new(Mutex::new(Vec::new()));
        let idle_notify = Arc::new(Notify::new());
        let sessions = Arc::new(RwLock::new(HashMap::new()));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let pool = Self {
            config: config.clone(),
            sandboxes: sandboxes.clone(),
            idle_queue: idle_queue.clone(),
            idle_notify: idle_notify.clone(),
            sessions: sessions.clone(),
            shutdown: shutdown.clone(),
        };

        pool.initialize_sandboxes().await?;

        Ok(pool)
    }

    async fn initialize_sandboxes(&self) -> Result<(), PoolError> {
        tracing::info!("Initializing {} sandboxes...", self.config.pool_size);

        for i in 0..self.config.pool_size {
            let sandbox_id = format!("sandbox-{i}");
            tracing::debug!("Creating sandbox: {sandbox_id}");

            let mut sandbox = Sandbox::new(sandbox_id.clone(), &self.config)
                .map_err(|e| PoolError::InitFailed(e.to_string()))?;

            sandbox
                .initialize(&self.config.base_image, &self.config.overlay_driver)
                .await
                .map_err(|e| PoolError::InitFailed(e.to_string()))?;

            let sandbox = Arc::new(sandbox);
            self.sandboxes.write().insert(sandbox_id.clone(), sandbox);
            self.idle_queue.lock().push(sandbox_id);
        }

        self.idle_notify.notify_waiters();
        tracing::info!("Pool initialized with {} sandboxes", self.config.pool_size);
        Ok(())
    }

    /// Create a persistent session bound to a single sandbox.
    ///
    /// Tasks executed through this session keep filesystem changes until
    /// [`Pool::close_session`] is called.
    pub async fn create_session(&self) -> Result<String, PoolError> {
        if self.shutdown.load(Ordering::Relaxed) {
            return Err(PoolError::ShuttingDown);
        }

        let sandbox = acquire_idle_sandbox(
            &self.sandboxes,
            &self.idle_queue,
            &self.idle_notify,
            &self.shutdown,
        )
        .await
        .ok_or_else(|| {
            if self.shutdown.load(Ordering::Relaxed) {
                PoolError::ShuttingDown
            } else {
                PoolError::NoIdleSandbox(SANDBOX_ACQUIRE_TIMEOUT.as_secs())
            }
        })?;

        let session_id = loop {
            let id = Uuid::new_v4().to_string();
            if !self.sessions.read().contains_key(&id) {
                break id;
            }
        };

        self.sessions.write().insert(
            session_id.clone(),
            SessionHandle {
                sandbox_id: sandbox.id().to_string(),
                execution_lock: Arc::new(AsyncMutex::new(())),
            },
        );

        tracing::info!(session_id = %session_id, sandbox_id = %sandbox.id(), "session created");
        Ok(session_id)
    }

    /// Execute a task inside a persistent session.
    pub async fn execute_in_session(
        &self,
        session_id: &str,
        task: Task,
    ) -> Result<TaskResult, PoolError> {
        self.execute_in_session_with_events(session_id, task, None)
            .await
    }

    /// Execute a task inside a persistent session and optionally stream output events.
    pub async fn execute_in_session_with_events(
        &self,
        session_id: &str,
        task: Task,
        output_events: Option<mpsc::UnboundedSender<TaskOutputEvent>>,
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

        sandbox
            .execute_with_events(task, output_events)
            .await
            .map_err(PoolError::SandboxError)
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
                self.idle_queue.lock().push(session.sandbox_id.clone());
                self.idle_notify.notify_one();
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

                replace_sandbox(
                    &session.sandbox_id,
                    &self.config,
                    &self.sandboxes,
                    &self.idle_queue,
                    &self.idle_notify,
                )
                .await?;
                Ok(())
            }
        }
    }

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

        // Proactively close all sessions so their sandboxes are reset before
        // final cleanup. This keeps shutdown behavior deterministic in
        // session-only mode.
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

    pub async fn wait_for_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.idle_notify.notified();

            if !self.idle_queue.lock().is_empty() {
                return true;
            }

            let now = Instant::now();
            if now >= deadline {
                return false;
            }

            let wait_duration = deadline.saturating_duration_since(now);
            if tokio::time::timeout(wait_duration, notified).await.is_err() {
                return false;
            }
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Wait for an idle sandbox, blocking up to [`SANDBOX_ACQUIRE_TIMEOUT`].
async fn acquire_idle_sandbox(
    sandboxes: &RwLock<HashMap<String, Arc<Sandbox>>>,
    idle_queue: &Mutex<Vec<String>>,
    idle_notify: &Notify,
    shutdown: &std::sync::atomic::AtomicBool,
) -> Option<Arc<Sandbox>> {
    let deadline = tokio::time::Instant::now() + SANDBOX_ACQUIRE_TIMEOUT;

    loop {
        if let Some(id) = idle_queue.lock().pop() {
            if let Some(sb) = sandboxes.read().get(&id).cloned() {
                return Some(sb);
            }
            continue;
        }

        if shutdown.load(Ordering::Relaxed) {
            return None;
        }

        let notified = idle_notify.notified();
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }

        match tokio::time::timeout(remaining, notified).await {
            Ok(()) => continue,
            Err(_) => return None,
        }
    }
}

/// Create a replacement sandbox after a failed reset.
async fn replace_sandbox(
    sandbox_id: &str,
    config: &PoolConfig,
    sandboxes: &RwLock<HashMap<String, Arc<Sandbox>>>,
    idle_queue: &Mutex<Vec<String>>,
    idle_notify: &Notify,
) -> Result<(), PoolError> {
    tracing::info!(sandbox_id = %sandbox_id, "creating replacement sandbox");

    let mut sandbox = Sandbox::new(sandbox_id.to_string(), config)
        .map_err(|e| PoolError::InitFailed(format!("replacement creation failed: {e}")))?;

    sandbox
        .initialize(&config.base_image, &config.overlay_driver)
        .await
        .map_err(|e| PoolError::InitFailed(format!("replacement init failed: {e}")))?;

    let sandbox = Arc::new(sandbox);
    sandboxes.write().insert(sandbox_id.to_string(), sandbox);
    idle_queue.lock().push(sandbox_id.to_string());
    idle_notify.notify_one();

    tracing::info!(sandbox_id = %sandbox_id, "replacement sandbox ready");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_status_default() {
        let status = PoolStatus {
            total: 10,
            idle: 8,
            busy: 2,
            reserved: 2,
            error: 0,
            tasks_executed: 100,
            tasks_succeeded: 95,
            tasks_failed: 5,
            avg_task_duration_ms: 500,
        };

        assert_eq!(status.total, status.idle + status.reserved + status.error);
        assert!(status.busy <= status.total);
    }
}

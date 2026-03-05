//! Sandbox pool manager implementation.
//!
//! The pool manager maintains a pre-created set of sandboxes and
//! distributes tasks to idle sandboxes for execution.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Notify};

use crate::config::PoolConfig;
use crate::sandbox::{Sandbox, SandboxError, SandboxState};
use crate::task::{Task, TaskOutputEvent, TaskResult};

/// Maximum time a task will wait for an idle sandbox before giving up.
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

    #[error("task submission failed: {0}")]
    TaskSubmissionFailed(String),

    #[error("pool is shutting down")]
    ShuttingDown,

    #[error("task execution failed: {0}")]
    ExecutionFailed(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Status of the pool.
#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub total: usize,
    pub idle: usize,
    pub busy: usize,
    pub error: usize,
    pub tasks_executed: u64,
    pub tasks_succeeded: u64,
    pub tasks_failed: u64,
    pub avg_task_duration_ms: u64,
}

struct TaskRequest {
    task: Task,
    response: oneshot::Sender<Result<TaskResult, PoolError>>,
    output_events: Option<mpsc::UnboundedSender<TaskOutputEvent>>,
}

/// A sandbox pool that manages multiple sandboxes for task execution.
pub struct Pool {
    config: Arc<PoolConfig>,
    sandboxes: Arc<RwLock<HashMap<String, Arc<Sandbox>>>>,
    idle_queue: Arc<Mutex<Vec<String>>>,
    idle_notify: Arc<Notify>,
    task_tx: mpsc::Sender<TaskRequest>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl Pool {
    pub async fn new(config: PoolConfig) -> Result<Self, PoolError> {
        let config = Arc::new(config);
        let sandboxes = Arc::new(RwLock::new(HashMap::new()));
        let idle_queue = Arc::new(Mutex::new(Vec::new()));
        let idle_notify = Arc::new(Notify::new());
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let (task_tx, task_rx) = mpsc::channel::<TaskRequest>(config.pool_size * 2);

        let pool = Self {
            config: config.clone(),
            sandboxes: sandboxes.clone(),
            idle_queue: idle_queue.clone(),
            idle_notify: idle_notify.clone(),
            task_tx,
            shutdown: shutdown.clone(),
        };

        pool.initialize_sandboxes().await?;

        let d_sandboxes = sandboxes.clone();
        let d_idle_queue = idle_queue.clone();
        let d_idle_notify = idle_notify.clone();
        let d_shutdown = shutdown.clone();
        let d_config = config.clone();

        tokio::spawn(async move {
            task_dispatcher(
                task_rx,
                d_sandboxes,
                d_idle_queue,
                d_idle_notify,
                d_shutdown,
                d_config,
            )
            .await;
        });

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

    pub async fn execute(&self, task: Task) -> Result<TaskResult, PoolError> {
        if self.shutdown.load(Ordering::Relaxed) {
            return Err(PoolError::ShuttingDown);
        }

        let (response_tx, response_rx) = oneshot::channel();
        let request = TaskRequest {
            task,
            response: response_tx,
            output_events: None,
        };

        self.task_tx
            .send(request)
            .await
            .map_err(|_| PoolError::TaskSubmissionFailed("channel closed".to_string()))?;

        response_rx
            .await
            .map_err(|_| PoolError::ExecutionFailed("response channel dropped".to_string()))?
    }

    pub async fn execute_with_events(
        &self,
        task: Task,
        output_events: mpsc::UnboundedSender<TaskOutputEvent>,
    ) -> Result<TaskResult, PoolError> {
        if self.shutdown.load(Ordering::Relaxed) {
            return Err(PoolError::ShuttingDown);
        }

        let (response_tx, response_rx) = oneshot::channel();
        let request = TaskRequest {
            task,
            response: response_tx,
            output_events: Some(output_events),
        };

        self.task_tx
            .send(request)
            .await
            .map_err(|_| PoolError::TaskSubmissionFailed("channel closed".to_string()))?;

        response_rx
            .await
            .map_err(|_| PoolError::ExecutionFailed("response channel dropped".to_string()))?
    }

    pub async fn execute_batch(&self, tasks: Vec<Task>) -> Vec<Result<TaskResult, PoolError>> {
        let futures: Vec<_> = tasks.into_iter().map(|task| self.execute(task)).collect();
        futures::future::join_all(futures).await
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

        PoolStatus {
            total: sandboxes.len(),
            idle: idle_count,
            busy,
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

// ---------------------------------------------------------------------------
// Task dispatcher with backpressure and sandbox recovery
// ---------------------------------------------------------------------------

async fn task_dispatcher(
    mut task_rx: mpsc::Receiver<TaskRequest>,
    sandboxes: Arc<RwLock<HashMap<String, Arc<Sandbox>>>>,
    idle_queue: Arc<Mutex<Vec<String>>>,
    idle_notify: Arc<Notify>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    config: Arc<PoolConfig>,
) {
    while let Some(request) = task_rx.recv().await {
        if shutdown.load(Ordering::Relaxed) {
            let _ = request.response.send(Err(PoolError::ShuttingDown));
            continue;
        }

        // Spawn each request as its own task so the dispatcher loop
        // is never blocked; each spawned task waits for an idle sandbox.
        let sandboxes = sandboxes.clone();
        let idle_queue = idle_queue.clone();
        let idle_notify = idle_notify.clone();
        let shutdown = shutdown.clone();
        let config = config.clone();

        tokio::spawn(async move {
            let sandbox = acquire_idle_sandbox(
                &sandboxes,
                &idle_queue,
                &idle_notify,
                &shutdown,
            )
            .await;

            match sandbox {
                Some(sandbox) => {
                    let sandbox_id = sandbox.id().to_string();
                    let TaskRequest {
                        task,
                        response,
                        output_events,
                    } = request;

                    let result = sandbox.execute_with_events(task, output_events).await;

                    match sandbox.reset().await {
                        Ok(()) => {
                            idle_queue.lock().push(sandbox_id);
                            idle_notify.notify_one();
                        }
                        Err(error) => {
                            tracing::error!(
                                %error,
                                sandbox_id = %sandbox_id,
                                "sandbox reset failed; replacing"
                            );
                            sandboxes.write().remove(&sandbox_id);
                            drop(sandbox);

                            if let Err(e) = replace_sandbox(
                                &sandbox_id,
                                &config,
                                &sandboxes,
                                &idle_queue,
                                &idle_notify,
                            )
                            .await
                            {
                                tracing::error!(
                                    %e,
                                    sandbox_id = %sandbox_id,
                                    "sandbox replacement failed; pool capacity reduced"
                                );
                            }
                        }
                    }

                    let _ = response.send(result.map_err(PoolError::SandboxError));
                }
                None => {
                    let err = if shutdown.load(Ordering::Relaxed) {
                        PoolError::ShuttingDown
                    } else {
                        PoolError::NoIdleSandbox(SANDBOX_ACQUIRE_TIMEOUT.as_secs())
                    };
                    let _ = request.response.send(Err(err));
                }
            }
        });
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
    sandboxes
        .write()
        .insert(sandbox_id.to_string(), sandbox);
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
            error: 0,
            tasks_executed: 100,
            tasks_succeeded: 95,
            tasks_failed: 5,
            avg_task_duration_ms: 500,
        };

        assert_eq!(status.total, status.idle + status.busy + status.error);
    }
}

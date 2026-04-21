use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use super::history::{dump_session_history, record_execution, ExecutionRecord};
use super::manager::{Pool, PoolError};
use crate::task::{Task, TaskResult};

/// Options for creating a persistent session.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Working directory for tasks executed in this session (required).
    pub working_dir: PathBuf,
    /// Docker image name from the registry (required).
    pub image: String,
}

impl SessionOptions {
    pub fn new(image: impl Into<String>, working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            image: image.into(),
        }
    }
}

#[derive(Clone)]
pub(super) struct SessionHandle {
    pub(super) sandbox_id: String,
    pub(super) working_dir: PathBuf,
    pub(super) execution_lock: Arc<AsyncMutex<()>>,
    /// Per-session execution history (append-only under execution_lock).
    pub(super) history: Arc<Mutex<Vec<ExecutionRecord>>>,
    /// ISO 8601 timestamp of session creation.
    pub(super) created_at: String,
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

impl Pool {
    /// Create a persistent session bound to a dedicated sandbox.
    ///
    /// A new sandbox is created for the requested image. Returns
    /// `PoolError::AtCapacity` when `max_sandboxes` has been reached.
    pub async fn create_session(&self, options: SessionOptions) -> Result<String, PoolError> {
        if self.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(PoolError::ShuttingDown);
        }

        let sandbox = self.create_sandbox_for_image(&options.image).await?;

        let working_dir = normalize_session_working_dir(&options.working_dir);

        let session_id = Uuid::new_v4().to_string();
        self.sessions.write().insert(
            session_id.clone(),
            SessionHandle {
                sandbox_id: sandbox.id().to_string(),
                working_dir: working_dir.clone(),
                execution_lock: Arc::new(AsyncMutex::new(())),
                history: Arc::new(Mutex::new(Vec::new())),
                created_at: chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            },
        );

        tracing::info!(
            session_id = %session_id,
            sandbox_id = %sandbox.id(),
            image = %options.image,
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
        if self.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
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

        let result = sandbox
            .execute(task.clone())
            .await
            .map_err(PoolError::SandboxError)?;

        if self.config.session_log_dir.is_some() {
            let mut history = session.history.lock();
            let seq = history.len();
            history.push(record_execution(seq, &task, &result));
        }

        Ok(result)
    }

    /// Close a persistent session and destroy its sandbox.
    pub async fn close_session(&self, session_id: &str) -> Result<(), PoolError> {
        let session = {
            let mut sessions = self.sessions.write();
            sessions
                .remove(session_id)
                .ok_or_else(|| PoolError::SessionNotFound(session_id.to_string()))?
        };

        // Wait for any in-flight session execution to complete.
        let _execution_guard = session.execution_lock.lock().await;

        if let Some(ref log_dir) = self.config.session_log_dir {
            let records: Vec<ExecutionRecord> = session.history.lock().drain(..).collect();
            if !records.is_empty() {
                match dump_session_history(
                    log_dir,
                    session_id,
                    &session.sandbox_id,
                    &session.working_dir,
                    &session.created_at,
                    records,
                ) {
                    Ok(path) => {
                        tracing::info!(
                            session_id = %session_id,
                            path = %path.display(),
                            "session history dumped"
                        );
                    }
                    Err(error) => {
                        tracing::error!(
                            %error,
                            session_id = %session_id,
                            "failed to dump session history"
                        );
                    }
                }
            }
        }

        self.remove_sandbox(&session.sandbox_id);
        tracing::info!(
            session_id = %session_id,
            sandbox_id = %session.sandbox_id,
            "session closed (sandbox destroyed)"
        );
        Ok(())
    }

    /// Run a task in an ephemeral session: create a session, execute the
    /// task, then close the session regardless of outcome.
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
}

#[cfg(test)]
mod tests {
    use super::{normalize_session_working_dir, resolve_task_working_dir};
    use std::path::{Path, PathBuf};

    #[test]
    fn normalize_session_working_dir_makes_paths_absolute() {
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
    fn resolve_task_working_dir_prefers_task_override() {
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

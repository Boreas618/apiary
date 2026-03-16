use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use super::manager::{Pool, PoolError};
use crate::task::{Task, TaskResult};

/// Options for creating a persistent session.
#[derive(Debug, Clone, Default)]
pub struct SessionOptions {
    /// Default working directory for tasks executed in this session.
    pub working_dir: Option<PathBuf>,
    /// Optional per-session base image (rootfs) path. When set, a dedicated
    /// sandbox is created using this rootfs instead of the pool's default.
    /// The sandbox is destroyed when the session is closed (not returned to pool).
    pub base_image: Option<PathBuf>,
}

impl SessionOptions {
    /// Set the session working directory.
    pub fn working_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(path.into());
        self
    }

    /// Set a custom base image (rootfs) for this session.
    /// The session will get a dedicated sandbox with this rootfs,
    /// which is destroyed (not pooled) when the session closes.
    pub fn base_image(mut self, path: impl Into<PathBuf>) -> Self {
        self.base_image = Some(path.into());
        self
    }
}

#[derive(Clone)]
pub(super) struct SessionHandle {
    pub(super) sandbox_id: String,
    pub(super) working_dir: PathBuf,
    pub(super) execution_lock: Arc<AsyncMutex<()>>,
    /// When true, the sandbox was created with a per-session rootfs and
    /// must be destroyed (not returned to the idle pool) on session close.
    pub(super) custom_rootfs: bool,
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
    /// Create a persistent session bound to a single sandbox.
    ///
    /// If no idle sandbox is available the pool scales up automatically (up to
    /// `max_sandboxes`). Only when the hard cap is reached does the call block
    /// with a timeout.
    pub async fn create_session(&self, options: SessionOptions) -> Result<String, PoolError> {
        if self.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(PoolError::ShuttingDown);
        }

        let (sandbox, custom_rootfs) = if let Some(ref base_image) = options.base_image {
            let sandbox = self.create_sandbox_with_rootfs(base_image).await?;
            (sandbox, true)
        } else {
            let sandbox = self.acquire_sandbox().await?;
            (sandbox, false)
        };

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
                custom_rootfs,
            },
        );

        tracing::info!(
            session_id = %session_id,
            sandbox_id = %sandbox.id(),
            working_dir = %working_dir.display(),
            custom_rootfs = custom_rootfs,
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

        if session.custom_rootfs {
            // Custom rootfs sandboxes are destroyed, not returned to pool
            self.remove_sandbox(&session.sandbox_id);
            tracing::info!(
                session_id = %session_id,
                sandbox_id = %session.sandbox_id,
                "session closed (custom rootfs sandbox destroyed)"
            );
            Ok(())
        } else {
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

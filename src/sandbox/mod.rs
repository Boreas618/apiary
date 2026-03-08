//! Sandbox implementation with namespace isolation.

pub mod cgroup;
mod mounts;
pub mod namespace;
mod output;
pub mod overlay;
mod process;
pub mod seccomp;

use crate::config::{OverlayDriver, PoolConfig, ResourceLimits, SeccompPolicy};
use crate::task::{MountSpec, Task, TaskResult};
use mounts::overlay_base_dir;
use overlay::ActiveOverlay;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use self::output::{append_capped, read_output_stream};
use self::process::configure_task_process_linux;

/// Errors that can occur during sandbox operations.
#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("failed to create namespace: {0}")]
    NamespaceCreation(String),

    #[error("failed to setup overlay filesystem: {0}")]
    OverlaySetup(String),

    #[error("failed to apply seccomp filter: {0}")]
    SeccompFilter(String),

    #[error("failed to setup cgroup: {0}")]
    CgroupSetup(String),

    #[error("sandbox is not in idle state")]
    NotIdle,

    #[error("failed to spawn sandbox process: {0}")]
    SpawnFailed(String),

    #[error("sandbox execution failed: {0}")]
    ExecutionFailed(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("system error: {0}")]
    System(#[from] nix::Error),
}

impl SandboxError {
    /// Whether this error indicates the sandbox infrastructure itself is broken
    /// (as opposed to a task-level failure like timeout or output capture issue).
    pub fn is_sandbox_broken(&self) -> bool {
        matches!(
            self,
            Self::SpawnFailed(_)
                | Self::NamespaceCreation(_)
                | Self::OverlaySetup(_)
                | Self::CgroupSetup(_)
                | Self::SeccompFilter(_)
        )
    }
}

/// State of a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxState {
    Creating,
    Idle,
    Running { task_id: String },
    Resetting,
    Error(String),
}

/// A single sandbox instance with namespace isolation.
pub struct Sandbox {
    id: String,
    state: Mutex<SandboxState>,
    root_path: PathBuf,
    upper_path: PathBuf,
    work_path: PathBuf,
    init_pid: Mutex<Option<nix::unistd::Pid>>,
    resource_limits: ResourceLimits,
    enable_seccomp: bool,
    seccomp_policy: SeccompPolicy,
    cgroup_path: Option<PathBuf>,
    active_overlay: Mutex<Option<ActiveOverlay>>,
    stats: SandboxStats,
}

/// Statistics for a sandbox.
#[derive(Debug, Default)]
pub struct SandboxStats {
    pub tasks_executed: AtomicU64,
    pub total_execution_time_ms: AtomicU64,
    pub successful_tasks: AtomicU64,
    pub failed_tasks: AtomicU64,
    pub timed_out_tasks: AtomicU64,
}

impl Sandbox {
    /// Create a new sandbox.
    pub fn new(id: String, config: &PoolConfig) -> Result<Self, SandboxError> {
        let overlay_base = config.overlay_dir.join(&id);
        let upper_path = overlay_base.join("upper");
        let work_path = overlay_base.join("work");
        let root_path = overlay_base.join("merged");

        std::fs::create_dir_all(&upper_path)?;
        std::fs::create_dir_all(&work_path)?;
        std::fs::create_dir_all(&root_path)?;

        Ok(Self {
            id,
            state: Mutex::new(SandboxState::Creating),
            root_path,
            upper_path,
            work_path,
            init_pid: Mutex::new(None),
            resource_limits: config.resource_limits.clone(),
            enable_seccomp: config.enable_seccomp,
            seccomp_policy: config.seccomp_policy.clone(),
            cgroup_path: None,
            active_overlay: Mutex::new(None),
            stats: SandboxStats::default(),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn state(&self) -> SandboxState {
        self.state.lock().clone()
    }

    fn set_state(&self, state: SandboxState) {
        *self.state.lock() = state;
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// Initialize the sandbox.
    pub async fn initialize(
        &mut self,
        base_image: &Path,
        driver: &OverlayDriver,
    ) -> Result<(), SandboxError> {
        self.set_state(SandboxState::Creating);

        let base_image = base_image.canonicalize().map_err(|e| {
            SandboxError::OverlaySetup(format!(
                "base image not found at {}: {e}",
                base_image.display()
            ))
        })?;

        let active = overlay::setup_overlay(
            &self.root_path,
            &self.upper_path,
            &self.work_path,
            &base_image,
            driver,
        )?;
        *self.active_overlay.lock() = Some(active);

        match cgroup::setup_cgroup(&self.id, &self.resource_limits) {
            Ok(path) => {
                self.cgroup_path = Some(path);
            }
            Err(e) => {
                tracing::warn!("Failed to setup cgroup (may need root or delegation): {e}");
            }
        }

        self.set_state(SandboxState::Idle);
        Ok(())
    }

    /// Execute a task in this sandbox.
    pub async fn execute(&self, task: Task) -> Result<TaskResult, SandboxError> {
        {
            let mut state = self.state.lock();
            if *state != SandboxState::Idle {
                return Err(SandboxError::NotIdle);
            }
            *state = SandboxState::Running {
                task_id: task.id.clone(),
            };
        }

        let start = Instant::now();
        let result = self.run_task_inner(&task).await;
        let duration = start.elapsed();

        self.stats.tasks_executed.fetch_add(1, Ordering::Relaxed);
        self.stats
            .total_execution_time_ms
            .fetch_add(duration.as_millis() as u64, Ordering::Relaxed);

        match &result {
            Ok(r) if r.timed_out => {
                self.stats.timed_out_tasks.fetch_add(1, Ordering::Relaxed);
            }
            Ok(r) if r.exit_code == 0 => {
                self.stats.successful_tasks.fetch_add(1, Ordering::Relaxed);
            }
            Ok(_) => {
                self.stats.failed_tasks.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.stats.failed_tasks.fetch_add(1, Ordering::Relaxed);
            }
        }

        match &result {
            Err(e) if e.is_sandbox_broken() => {
                self.set_state(SandboxState::Error(e.to_string()));
            }
            _ => {
                self.set_state(SandboxState::Idle);
            }
        }
        result
    }

    async fn run_task_inner(&self, task: &Task) -> Result<TaskResult, SandboxError> {
        use tokio::process::Command;
        use tokio::time::timeout;

        let shell = task
            .command
            .first()
            .cloned()
            .unwrap_or_else(|| "/bin/sh".to_string());
        let args: Vec<&str> = task.command.iter().skip(1).map(|s| s.as_str()).collect();

        let root = self.root_path.clone();
        let workdir = task
            .working_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("/workspace"));
        let cgroup_path = self.cgroup_path.clone();
        let enable_seccomp = self.enable_seccomp;
        let seccomp_policy = self.seccomp_policy.clone();
        let task_uid = task.uid;
        let task_gid = task.gid;
        let writable_mounts = task.writable_mounts.clone();
        let readonly_mounts = task.readonly_mounts.clone();

        let need_stdout_pipe = task.capture_stdout;
        let need_stderr_pipe = task.capture_stderr;

        let mut cmd = Command::new(&shell);
        cmd.args(&args)
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("HOME", "/root")
            .env("TERM", "xterm-256color")
            .envs(&task.env)
            .stdout(if need_stdout_pipe {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stderr(if need_stderr_pipe {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            });
        if task.stdin.is_some() {
            cmd.stdin(std::process::Stdio::piped());
        }

        unsafe {
            cmd.pre_exec(move || {
                configure_task_process_linux(
                    &root,
                    &workdir,
                    cgroup_path.as_deref(),
                    enable_seccomp,
                    &seccomp_policy,
                    task_uid,
                    task_gid,
                    &writable_mounts,
                    &readonly_mounts,
                )
            });
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| SandboxError::SpawnFailed(format!("failed to spawn process: {e}")))?;
        if let Some(pid) = child.id() {
            *self.init_pid.lock() = Some(nix::unistd::Pid::from_raw(pid as i32));
        }
        struct RunningPidGuard<'a> {
            slot: &'a Mutex<Option<nix::unistd::Pid>>,
        }
        impl Drop for RunningPidGuard<'_> {
            fn drop(&mut self) {
                *self.slot.lock() = None;
            }
        }
        let _running_pid_guard = RunningPidGuard {
            slot: &self.init_pid,
        };

        if let Some(stdin_data) = task.stdin.clone() {
            if let Some(mut stdin) = child.stdin.take() {
                tokio::spawn(async move {
                    if let Err(error) = stdin.write_all(&stdin_data).await {
                        tracing::debug!(%error, "failed to write task stdin");
                        return;
                    }
                    if let Err(error) = stdin.shutdown().await {
                        tracing::debug!(%error, "failed to close task stdin");
                    }
                });
            }
        }

        let stdout_handle = tokio::spawn(read_output_stream(
            child.stdout.take(),
            task.capture_stdout,
            task.max_output_size,
        ));
        let stderr_handle = tokio::spawn(read_output_stream(
            child.stderr.take(),
            task.capture_stderr,
            task.max_output_size,
        ));

        let task_start = Instant::now();
        let wait_result = timeout(task.timeout, child.wait()).await;
        let task_duration = task_start.elapsed();

        let (exit_code, timed_out) = match wait_result {
            Ok(Ok(status)) => (status.code().unwrap_or(-1), false),
            Ok(Err(e)) => return Err(SandboxError::ExecutionFailed(e.to_string())),
            Err(_) => {
                if let Some(cgroup_path) = &self.cgroup_path {
                    log_best_effort(
                        "kill timed-out task cgroup",
                        cgroup::kill_cgroup_processes(cgroup_path),
                    );
                }
                if let Some(pid) = child.id() {
                    log_best_effort(
                        "kill timed-out task process group",
                        nix::sys::signal::killpg(
                            nix::unistd::Pid::from_raw(pid as i32),
                            nix::sys::signal::Signal::SIGKILL,
                        ),
                    );
                }
                if let Err(error) = child.kill().await {
                    tracing::debug!(%error, "failed to kill timed-out child process");
                }
                if let Err(error) = child.wait().await {
                    tracing::debug!(%error, "failed to reap timed-out child process");
                }
                (-1, true)
            }
        };

        let mut stdout = match stdout_handle.await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(SandboxError::Io(e)),
            Err(e) => {
                return Err(SandboxError::ExecutionFailed(format!(
                    "stdout task join failed: {e}"
                )));
            }
        };
        let mut stderr = match stderr_handle.await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(SandboxError::Io(e)),
            Err(e) => {
                return Err(SandboxError::ExecutionFailed(format!(
                    "stderr task join failed: {e}"
                )));
            }
        };

        if timed_out {
            let timeout_msg = b"Task timed out\n";
            if task.capture_stderr {
                append_capped(&mut stderr, timeout_msg, task.max_output_size);
            }
        }

        Ok(TaskResult {
            task_id: task.id.clone(),
            exit_code,
            stdout,
            stderr,
            duration: task_duration,
            timed_out,
        })
    }

    /// Reset the sandbox to a clean state.
    pub async fn reset(&self) -> Result<(), SandboxError> {
        self.set_state(SandboxState::Resetting);

        if let Some(pid) = self.init_pid.lock().take() {
            log_best_effort(
                "kill running sandbox process group during reset",
                nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL),
            );
            log_best_effort(
                "kill running sandbox process during reset",
                nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL),
            );
            log_best_effort(
                "reap running sandbox process during reset",
                nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)),
            );
        }

        if let Some(ref cgroup_path) = self.cgroup_path {
            cgroup::kill_cgroup_processes(cgroup_path)?;
            cgroup::reset_cgroup(cgroup_path)?;
        }

        if self.upper_path.exists() {
            overlay::clear_upper_layer(&self.upper_path)?;
        }

        self.set_state(SandboxState::Idle);
        Ok(())
    }

    /// Clean up the sandbox resources.
    pub fn cleanup(&self) -> Result<(), SandboxError> {
        if let Some(ref active) = *self.active_overlay.lock() {
            if self.root_path.exists() {
                log_best_effort(
                    "unmount sandbox overlay during cleanup",
                    overlay::unmount_overlay(&self.root_path, active),
                );
            }
        }

        if let Some(ref cgroup_path) = self.cgroup_path {
            log_best_effort(
                "remove sandbox cgroup during cleanup",
                cgroup::remove_cgroup(cgroup_path),
            );
        }

        match overlay_base_dir(&self.root_path) {
            Some(overlay_base) => {
                if let Err(error) = std::fs::remove_dir_all(overlay_base) {
                    tracing::debug!(
                        %error,
                        overlay_base = %overlay_base.display(),
                        "failed to remove sandbox overlay base"
                    );
                }
            }
            None => {
                tracing::warn!(
                    root_path = %self.root_path.display(),
                    "skipping sandbox cleanup because the overlay base is unsafe"
                );
            }
        }

        Ok(())
    }

    pub fn stats(&self) -> &SandboxStats {
        &self.stats
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn log_best_effort<T, E>(action: &str, result: Result<T, E>)
where
    E: std::fmt::Display,
{
    if let Err(error) = result {
        tracing::debug!(%error, action, "best-effort sandbox cleanup failed");
    }
}

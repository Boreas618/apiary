//! Sandbox implementation with namespace isolation.

pub mod cgroup;
pub mod namespace;
pub mod overlay;
pub mod seccomp;

use crate::config::{PoolConfig, ResourceLimits, SeccompPolicy};
use crate::task::{MountSpec, Task, TaskResult};
use overlay::ActiveOverlay;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

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

    #[error("sandbox execution timed out")]
    Timeout,

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
        base_image: &PathBuf,
        driver: &overlay::OverlayDriver,
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
            Ok(r) if r.exit_code == 0 => {
                self.stats.successful_tasks.fetch_add(1, Ordering::Relaxed);
            }
            Ok(_) => {
                self.stats.failed_tasks.fetch_add(1, Ordering::Relaxed);
            }
            Err(SandboxError::Timeout) => {
                self.stats.timed_out_tasks.fetch_add(1, Ordering::Relaxed);
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
                    let _ = stdin.write_all(&stdin_data).await;
                    let _ = stdin.shutdown().await;
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
                    let _ = cgroup::kill_cgroup_processes(cgroup_path);
                }
                if let Some(pid) = child.id() {
                    let _ = nix::sys::signal::killpg(
                        nix::unistd::Pid::from_raw(pid as i32),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
                let _ = child.kill().await;
                let _ = child.wait().await;
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
            let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
            let _ = nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
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
                let _ = overlay::unmount_overlay(&self.root_path, active);
            }
        }

        if let Some(ref cgroup_path) = self.cgroup_path {
            let _ = cgroup::remove_cgroup(cgroup_path);
        }

        let overlay_base = self.root_path.parent().unwrap_or(&self.root_path);
        let _ = std::fs::remove_dir_all(overlay_base);

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

/// Write a message to stderr using async-signal-safe `libc::write`.
/// Safe to call from a `pre_exec` (post-fork, pre-exec) context.
fn write_stderr_safe(msg: &[u8]) {
    unsafe {
        libc::write(libc::STDERR_FILENO, msg.as_ptr() as *const libc::c_void, msg.len());
    }
}

fn configure_task_process_linux(
    root: &Path,
    workdir: &Path,
    cgroup_path: Option<&Path>,
    enable_seccomp: bool,
    seccomp_policy: &SeccompPolicy,
    uid: Option<u32>,
    gid: Option<u32>,
    writable_mounts: &[MountSpec],
    readonly_mounts: &[MountSpec],
) -> std::io::Result<()> {
    // NOTE: This function runs in a forked child before exec().
    // Only async-signal-safe functions may be called. In particular,
    // tracing/log macros, heap allocation, and lock acquisition are
    // forbidden. Use write_stderr_safe() for diagnostics.
    use nix::mount::{mount, umount2, MntFlags, MsFlags};
    use nix::sched::{unshare, CloneFlags};

    if unsafe { libc::setpgid(0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    if let Some(cgroup_path) = cgroup_path {
        if cgroup::add_process_to_cgroup(cgroup_path, std::process::id()).is_err() {
            write_stderr_safe(b"[apiary] warning: failed to attach process to cgroup\n");
        }
    }

    unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_NEWUTS)
        .map_err(|e| std::io::Error::other(format!("failed to unshare task namespaces: {e}")))?;
    namespace::make_mount_private().map_err(sandbox_error_to_io)?;

    mount(
        Some(root),
        root,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| std::io::Error::other(format!("failed to bind mount new root: {e}")))?;

    apply_task_mounts(root, writable_mounts, readonly_mounts)?;

    if overlay::setup_dev_mounts(root).is_err() {
        write_stderr_safe(b"[apiary] warning: failed to setup /dev; continuing\n");
    }

    let put_old = root.join(".old_root");
    std::fs::create_dir_all(&put_old)?;
    namespace::pivot_root(root, &put_old).map_err(sandbox_error_to_io)?;
    umount2("/.old_root", MntFlags::MNT_DETACH)
        .map_err(|e| std::io::Error::other(format!("failed to unmount old root: {e}")))?;
    let _ = std::fs::remove_dir_all("/.old_root");

    if overlay::setup_post_pivot_mounts(Path::new("/")).is_err() {
        write_stderr_safe(b"[apiary] warning: failed to mount pseudo-filesystems; continuing\n");
    }

    let effective_workdir = if workdir.is_absolute() {
        workdir.to_path_buf()
    } else {
        Path::new("/").join(workdir)
    };
    std::fs::create_dir_all(&effective_workdir)?;
    std::env::set_current_dir(&effective_workdir)?;

    if let Some(gid) = gid {
        if unsafe { libc::setgid(gid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    if let Some(uid) = uid {
        if unsafe { libc::setuid(uid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    if enable_seccomp {
        if seccomp::set_no_new_privs().is_err() {
            write_stderr_safe(
                b"[apiary] warning: failed to set no_new_privs; continuing without seccomp\n",
            );
        } else if seccomp::apply_seccomp_filter(seccomp_policy).is_err() {
            write_stderr_safe(b"[apiary] warning: failed to apply seccomp filter; continuing\n");
        }
    }

    Ok(())
}

fn apply_task_mounts(
    root: &Path,
    writable_mounts: &[MountSpec],
    readonly_mounts: &[MountSpec],
) -> std::io::Result<()> {
    for spec in writable_mounts {
        bind_mount_spec(root, spec, false)?;
    }
    for spec in readonly_mounts {
        bind_mount_spec(root, spec, true)?;
    }
    Ok(())
}

fn bind_mount_spec(root: &Path, spec: &MountSpec, readonly: bool) -> std::io::Result<()> {
    use nix::mount::{mount, MsFlags};

    if !spec.source.exists() {
        return Err(std::io::Error::other(format!(
            "mount source does not exist: {}",
            spec.source.display()
        )));
    }

    let target = root.join(spec.dest.strip_prefix("/").unwrap_or(&spec.dest));
    std::fs::create_dir_all(&target)?;

    mount(
        Some(&spec.source),
        &target,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| {
        std::io::Error::other(format!(
            "failed to bind mount {} -> {}: {e}",
            spec.source.display(),
            spec.dest.display()
        ))
    })?;

    if readonly {
        mount(
            None::<&str>,
            &target,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(|e| {
            std::io::Error::other(format!(
                "failed to remount {} as read-only: {e}",
                spec.dest.display()
            ))
        })?;
    }

    Ok(())
}

fn sandbox_error_to_io(error: SandboxError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

async fn read_output_stream<R>(
    reader: Option<R>,
    capture: bool,
    max_output_size: usize,
) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return Ok(Vec::new());
    };

    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        if capture && !truncated {
            let chunk = &buffer[..read];
            let available = max_output_size.saturating_sub(captured.len());
            if available == 0 {
                truncated = true;
                continue;
            }

            let to_copy = available.min(chunk.len());
            captured.extend_from_slice(&chunk[..to_copy]);
            if to_copy < chunk.len() {
                truncated = true;
            }
        }
    }

    Ok(captured)
}

fn append_capped(target: &mut Vec<u8>, chunk: &[u8], max_output_size: usize) {
    let available = max_output_size.saturating_sub(target.len());
    if available == 0 {
        return;
    }

    let to_copy = available.min(chunk.len());
    target.extend_from_slice(&chunk[..to_copy]);
}

//! Sandbox implementation with namespace isolation.

pub mod cgroup;
pub mod monitor;
mod mounts;
pub mod namespace;
mod output;
pub mod overlay;
mod process;
pub mod rlimits;
#[cfg(target_os = "linux")]
pub mod seccomp;

use crate::config::{OverlayDriver, PoolConfig, ResourceLimits, SeccompPolicy};
use crate::task::{MountSpec, Task, TaskResult};
use monitor::ProcessMonitor;
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
    seccomp_policy: SeccompPolicy,
    cgroup_path: Option<PathBuf>,
    active_overlay: Mutex<Option<ActiveOverlay>>,
    stats: SandboxStats,
    process_monitor: Option<ProcessMonitor>,
    /// Bind-mount daemon `/etc/resolv.conf` into the sandbox for each task.
    mount_host_resolv_conf: bool,
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

const RESOLV_CONF_PATH: &str = "/etc/resolv.conf";

/// Prepend a read-only bind of the daemon's `resolv.conf` when enabled.
fn merge_host_resolv_readonly_mounts(
    mount_host_resolv: bool,
    task_readonly: &[MountSpec],
) -> Vec<MountSpec> {
    if !mount_host_resolv {
        return task_readonly.to_vec();
    }
    let source = Path::new(RESOLV_CONF_PATH);
    if !source.is_file() {
        tracing::debug!(
            path = RESOLV_CONF_PATH,
            "mount_host_resolv_conf: source missing; not adding resolv.conf mount"
        );
        return task_readonly.to_vec();
    }
    let dest = Path::new(RESOLV_CONF_PATH);
    if task_readonly.iter().any(|m| m.dest.as_path() == dest) {
        return task_readonly.to_vec();
    }
    let mut out = Vec::with_capacity(task_readonly.len() + 1);
    out.push(MountSpec {
        source: source.to_path_buf(),
        dest: dest.to_path_buf(),
    });
    out.extend(task_readonly.iter().cloned());
    out
}

/// Resolve one overlay lower directory for [`Sandbox::initialize`].
///
/// With `session_layers_base` (per-session `base_image`): `rootfs_layers_dir.join(path)`
/// then canonicalize. If `path` is absolute, [`Path::join`] keeps that absolute path
/// (same as passing a full lower dir). Without a base, require `path` to canonicalize as given.
fn resolve_lower_dir(
    p: &Path,
    session_layers_base: Option<&Path>,
) -> Result<PathBuf, SandboxError> {
    let candidate = match session_layers_base {
        Some(base) => base.join(p),
        None => p.to_path_buf(),
    };
    candidate.canonicalize().map_err(|e| {
        SandboxError::OverlaySetup(format!(
            "lower dir not found at {} (resolved as {}): {e}",
            p.display(),
            candidate.display()
        ))
    })
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
            seccomp_policy: config.seccomp_policy.clone(),
            cgroup_path: None,
            active_overlay: Mutex::new(None),
            stats: SandboxStats::default(),
            process_monitor: None,
            mount_host_resolv_conf: config.mount_host_resolv_conf,
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

    /// Attach a shared process monitor (typically created by the Pool).
    /// When set before `initialize()`, this monitor is used instead of
    /// spawning a per-sandbox monitor when cgroups are unavailable.
    pub fn set_process_monitor(&mut self, monitor: ProcessMonitor) {
        self.process_monitor = Some(monitor);
    }

    /// Initialize the sandbox.
    ///
    /// `lower_dirs` lists layer directories in bottom-to-top order (base
    /// first, topmost last).  A single-element slice is the common case
    /// for pool-default sandboxes backed by a flat rootfs.
    ///
    /// `session_layers_base`: when `Some`, each session `base_image` path is
    /// `join`ed to this directory (typically `/tmp/apiary_rootfs/.layers`) and
    /// then canonicalized.  Use `None` for the pool default rootfs path only.
    pub async fn initialize(
        &mut self,
        lower_dirs: &[PathBuf],
        driver: &OverlayDriver,
        session_layers_base: Option<&Path>,
    ) -> Result<(), SandboxError> {
        self.set_state(SandboxState::Creating);

        let canonical_lowers: Vec<PathBuf> = lower_dirs
            .iter()
            .map(|p| resolve_lower_dir(p, session_layers_base))
            .collect::<Result<_, _>>()?;

        let active = overlay::setup_overlay(
            &self.root_path,
            &self.upper_path,
            &self.work_path,
            &canonical_lowers,
            driver,
        )?;
        *self.active_overlay.lock() = Some(active);

        match cgroup::setup_cgroup(&self.id, &self.resource_limits) {
            Ok(path) => {
                self.cgroup_path = Some(path);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to setup cgroup (falling back to rlimits + process monitor): {e}"
                );
                if self.process_monitor.is_none() {
                    self.process_monitor = Some(ProcessMonitor::spawn());
                    tracing::info!(
                        sandbox_id = %self.id,
                        "started process monitor as cgroup fallback"
                    );
                }
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
        let seccomp_policy = self.seccomp_policy.clone();
        let resource_limits = self.resource_limits.clone();
        let task_timeout = task.timeout;
        let task_uid = task.uid;
        let task_gid = task.gid;
        let writable_mounts = task.writable_mounts.clone();
        let readonly_mounts = merge_host_resolv_readonly_mounts(
            self.mount_host_resolv_conf,
            &task.readonly_mounts,
        );

        let need_stdout_pipe = task.capture_stdout;
        let need_stderr_pipe = task.capture_stderr;

        tracing::debug!(
            sandbox_id = %self.id,
            task_id = %task.id,
            shell = %shell,
            args = ?args,
            root = %root.display(),
            workdir = %workdir.display(),
            has_cgroup = cgroup_path.is_some(),
            seccomp_policy = ?seccomp_policy,
            uid = ?task_uid,
            gid = ?task_gid,
            writable_mounts = writable_mounts.len(),
            readonly_mounts = readonly_mounts.len(),
            timeout_secs = task_timeout.as_secs(),
            "spawning sandbox process"
        );

        // Parent-side stat check: verify the overlay is still functional right
        // before we fork. If this fails, the overlay broke AFTER initialization.
        let probe_path = self.root_path.join("bin/sh");
        match std::fs::metadata(&probe_path) {
            Ok(meta) => {
                use std::os::unix::fs::MetadataExt;
                tracing::debug!(
                    sandbox_id = %self.id,
                    path = %probe_path.display(),
                    mode = format_args!("{:#o}", meta.mode()),
                    size = meta.len(),
                    "parent-side overlay probe: OK"
                );
            }
            Err(e) => {
                tracing::error!(
                    sandbox_id = %self.id,
                    path = %probe_path.display(),
                    error = %e,
                    raw_errno = ?e.raw_os_error(),
                    "parent-side overlay probe: FAILED — overlay is broken BEFORE fork"
                );
                log_mount_diagnostics(&self.root_path);
            }
        }

        // Dup the parent's stderr fd so the child can write pre_exec
        // breadcrumbs (step-by-step progress, stat probes, exec target
        // diagnostics) directly to the terminal log, bypassing the captured
        // pipe. Only enabled when APIARY_DEBUG is set — in normal operation
        // pre_exec is silent and errors are reported via the spawn() return.
        let debug_fd = if std::env::var_os("APIARY_DEBUG").is_some() {
            let fd = unsafe { libc::dup(libc::STDERR_FILENO) };
            if fd < 0 {
                tracing::warn!("APIARY_DEBUG set but failed to dup stderr for pre_exec debug output");
            }
            fd
        } else {
            -1
        };

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

        let shell_for_preexec = shell.clone();
        unsafe {
            cmd.pre_exec(move || {
                let result = configure_task_process_linux(
                    &root,
                    &workdir,
                    cgroup_path.as_deref(),
                    &seccomp_policy,
                    &resource_limits,
                    task_timeout,
                    task_uid,
                    task_gid,
                    &writable_mounts,
                    &readonly_mounts,
                    debug_fd,
                    &shell_for_preexec,
                );
                if debug_fd >= 0 {
                    libc::close(debug_fd);
                }
                result
            });
        }

        let spawn_result = cmd.spawn();

        // Close the parent's copy of debug_fd (child got its own copy via fork)
        if debug_fd >= 0 {
            unsafe { libc::close(debug_fd) };
        }

        let mut child = spawn_result.map_err(|e| {
            let raw_errno = e.raw_os_error();
            let error_msg = e.to_string();
            let has_step_prefix = error_msg.contains("step ");

            tracing::error!(
                sandbox_id = %self.id,
                task_id = %task.id,
                error = %e,
                raw_errno = ?raw_errno,
                error_kind = ?e.kind(),
                shell = %shell,
                root = %self.root_path.display(),
                has_step_prefix,
                "sandbox process spawn failed"
            );

            if has_step_prefix {
                tracing::error!(
                    sandbox_id = %self.id,
                    "pre_exec failed at the step indicated above"
                );
            } else {
                tracing::error!(
                    sandbox_id = %self.id,
                    shell = %shell,
                    "spawn failure has no step prefix from pre_exec. Two possibilities:\n\
                     (A) fork/clone itself failed (pre_exec never ran):\n\
                         - Container seccomp blocks clone/clone3\n\
                         - PID/namespace limit reached\n\
                     (B) pre_exec succeeded but execvp(\"{shell}\") failed:\n\
                         - The binary does not exist inside the sandbox rootfs\n\
                         - The overlayfs does not support exec after pivot_root (EOPNOTSUPP)\n\
                         - The binary requires a missing shared library (ENOENT)\n\
                     Set APIARY_DEBUG=1 to enable detailed pre_exec breadcrumbs on stderr.\n\
                     If breadcrumbs show 'configuration complete', the issue is (B)."
                );
                log_spawn_diagnostics();
            }

            SandboxError::SpawnFailed(format!("failed to spawn process: {e}"))
        })?;
        let child_pid = child.id();
        if let Some(pid) = child_pid {
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

        if let (Some(monitor), Some(pid)) = (&self.process_monitor, child_pid) {
            let limits = monitor::limits_from_config(&self.resource_limits);
            monitor.register(pid, pid, limits).await;
        }

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

        if let (Some(monitor), Some(pid)) = (&self.process_monitor, child_pid) {
            monitor.unregister(pid).await;
        }

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

/// Collect and log mount/filesystem diagnostics when the overlay probe fails.
fn log_mount_diagnostics(root_path: &Path) {
    // Check if the mount point directory exists
    match std::fs::metadata(root_path) {
        Ok(meta) => {
            use std::os::unix::fs::MetadataExt;
            tracing::error!(
                path = %root_path.display(),
                mode = format_args!("{:#o}", meta.mode()),
                ino = meta.ino(),
                dev = format_args!("{:#x}", meta.dev()),
                "mount diagnostics: root_path stat OK (directory exists)"
            );
        }
        Err(e) => {
            tracing::error!(
                path = %root_path.display(),
                error = %e,
                "mount diagnostics: root_path stat FAILED (mount point gone?)"
            );
        }
    }

    // List directory contents (readdir might work even if stat on files fails)
    match std::fs::read_dir(root_path) {
        Ok(entries) => {
            let names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .take(20)
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            tracing::error!(
                entries = ?names,
                "mount diagnostics: readdir OK"
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "mount diagnostics: readdir FAILED");
        }
    }

    // Check /proc/self/mountinfo for relevant mounts
    if let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") {
        let root_str = root_path.to_string_lossy();
        // Also check the parent (overlay base dir)
        let parent_str = root_path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let relevant: Vec<&str> = mounts
            .lines()
            .filter(|l| l.contains(root_str.as_ref()) || l.contains(&parent_str))
            .collect();
        if relevant.is_empty() {
            tracing::error!(
                path = %root_path.display(),
                "mount diagnostics: NO mount found for this path in /proc/self/mountinfo!"
            );
        } else {
            for line in &relevant {
                tracing::error!(mount_entry = %line, "mount diagnostics: mountinfo");
            }
        }
    }

    // Check if fuse-overlayfs processes are running
    if let Ok(output) = std::process::Command::new("pgrep")
        .args(["-a", "fuse-overlayfs"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            tracing::error!(
                "mount diagnostics: NO fuse-overlayfs processes running! Daemon likely crashed."
            );
        } else {
            for line in stdout.trim().lines() {
                tracing::error!(process = %line, "mount diagnostics: fuse-overlayfs process");
            }
        }
    }

    // statfs on the mount point to check filesystem type
    let c_path =
        std::ffi::CString::new(root_path.to_string_lossy().as_bytes()).ok();
    if let Some(ref c_path) = c_path {
        let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::statfs(c_path.as_ptr(), &mut buf) };
        if ret == 0 {
            let fs_name = match buf.f_type {
                0x61756673 => "aufs",
                0xEF53 => "ext2/ext3/ext4",
                0x794c7630 => "overlayfs",
                0x01021994 => "tmpfs",
                0x9123683e => "btrfs",
                0x58465342 => "xfs",
                0x65735546 => "fuse",
                0x6969 => "nfs",
                0xBD00BD0 => "lustre",
                _ => "unknown",
            };
            tracing::error!(
                f_type = format_args!("{:#x}", buf.f_type),
                fs_name,
                "mount diagnostics: statfs on root_path"
            );
        } else {
            let err = std::io::Error::last_os_error();
            tracing::error!(error = %err, "mount diagnostics: statfs FAILED");
        }
    }
}

/// Collect and log system diagnostic info to help debug spawn failures.
fn log_spawn_diagnostics() {
    let uid = nix::unistd::Uid::current();
    let euid = nix::unistd::Uid::effective();
    let pid = std::process::id();

    tracing::error!(
        uid = uid.as_raw(),
        euid = euid.as_raw(),
        pid,
        "spawn diagnostics: process identity"
    );

    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        let interesting: Vec<&str> = status
            .lines()
            .filter(|l| {
                l.starts_with("NStgid:")
                    || l.starts_with("NSpid:")
                    || l.starts_with("NSpgid:")
                    || l.starts_with("NSsid:")
                    || l.starts_with("CapEff:")
                    || l.starts_with("CapPrm:")
                    || l.starts_with("CapBnd:")
                    || l.starts_with("Seccomp:")
                    || l.starts_with("NoNewPrivs:")
                    || l.starts_with("Threads:")
            })
            .collect();
        tracing::error!(
            proc_status = ?interesting,
            "spawn diagnostics: /proc/self/status (namespace/capability/seccomp fields)"
        );
    }

    if let Ok(cgroup) = std::fs::read_to_string("/proc/self/cgroup") {
        tracing::error!(
            cgroup = %cgroup.trim(),
            "spawn diagnostics: /proc/self/cgroup"
        );
    }

    if let Ok(kernel) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        tracing::error!(kernel = %kernel.trim(), "spawn diagnostics: kernel version");
    }

    if let Ok(userns_clone) =
        std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
    {
        tracing::error!(
            value = %userns_clone.trim(),
            "spawn diagnostics: unprivileged_userns_clone"
        );
    }

    if let Ok(max_user_ns) = std::fs::read_to_string("/proc/sys/user/max_user_namespaces") {
        tracing::error!(
            value = %max_user_ns.trim(),
            "spawn diagnostics: max_user_namespaces"
        );
    }

    if let Ok(max_mnt_ns) = std::fs::read_to_string("/proc/sys/user/max_mnt_namespaces") {
        tracing::error!(
            value = %max_mnt_ns.trim(),
            "spawn diagnostics: max_mnt_namespaces"
        );
    }
}

#[cfg(test)]
mod resolv_mount_tests {
    use super::{merge_host_resolv_readonly_mounts, MountSpec};
    use std::path::PathBuf;

    #[test]
    fn merge_disabled_leaves_task_mounts_unchanged() {
        let task = vec![MountSpec {
            source: PathBuf::from("/host/a"),
            dest: PathBuf::from("/a"),
        }];
        let merged = merge_host_resolv_readonly_mounts(false, &task);
        assert_eq!(merged, task);
    }

    #[test]
    fn merge_skips_when_task_already_mounts_resolv() {
        let task = vec![MountSpec {
            source: PathBuf::from("/custom/resolv.conf"),
            dest: PathBuf::from("/etc/resolv.conf"),
        }];
        let merged = merge_host_resolv_readonly_mounts(true, &task);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, PathBuf::from("/custom/resolv.conf"));
    }

    #[test]
    fn merge_prepends_resolv_when_enabled_and_source_exists() {
        if !std::path::Path::new("/etc/resolv.conf").is_file() {
            return;
        }
        let task = vec![MountSpec {
            source: PathBuf::from("/host/extra"),
            dest: PathBuf::from("/extra"),
        }];
        let merged = merge_host_resolv_readonly_mounts(true, &task);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].dest, PathBuf::from("/etc/resolv.conf"));
        assert_eq!(merged[0].source, PathBuf::from("/etc/resolv.conf"));
        assert_eq!(merged[1].dest, PathBuf::from("/extra"));
    }
}

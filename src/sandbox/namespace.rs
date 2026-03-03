//! Linux namespace management for sandbox isolation.
//!
//! This module provides functions to create and manage Linux namespaces
//! (User, Mount, PID) for sandbox isolation. It supports rootless operation
//! using user namespaces.
//!
//! Note: This module is only functional on Linux.

use super::SandboxError;

/// Configuration for namespace creation.
#[derive(Debug, Clone)]
pub struct NamespaceConfig {
    /// Create a new user namespace (required for rootless).
    pub user_ns: bool,
    /// Create a new mount namespace.
    pub mount_ns: bool,
    /// Create a new PID namespace.
    pub pid_ns: bool,
    /// Create a new network namespace.
    pub net_ns: bool,
    /// Create a new UTS namespace (hostname).
    pub uts_ns: bool,
    /// Create a new IPC namespace.
    pub ipc_ns: bool,
    /// UID to map inside the user namespace (maps to current UID outside).
    pub inner_uid: u32,
    /// GID to map inside the user namespace (maps to current GID outside).
    pub inner_gid: u32,
}

impl Default for NamespaceConfig {
    fn default() -> Self {
        Self {
            user_ns: true,
            mount_ns: true,
            pid_ns: true,
            net_ns: false, // Shared host network by default
            uts_ns: false,
            ipc_ns: false,
            inner_uid: 0, // Map to root inside the namespace
            inner_gid: 0,
        }
    }
}

/// File descriptors for namespaces.
#[derive(Debug, Default)]
pub struct NamespaceFds {
    pub user_ns: Option<std::os::fd::OwnedFd>,
    pub mount_ns: Option<std::os::fd::OwnedFd>,
    pub pid_ns: Option<std::os::fd::OwnedFd>,
    pub net_ns: Option<std::os::fd::OwnedFd>,
}

/// Returns the real UID from before entering the user namespace.
/// Falls back to `Uid::current()` if rootless mode was never entered.
pub fn original_uid() -> u32 {
    #[cfg(target_os = "linux")]
    {
        let stored = ORIGINAL_UID.load(std::sync::atomic::Ordering::Relaxed);
        if stored != u32::MAX {
            return stored;
        }
        nix::unistd::Uid::current().as_raw()
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Returns true if the process entered rootless mode via `enter_rootless_mode()`,
/// meaning it is running inside a user namespace with a non-root original UID.
pub fn is_rootless_mode() -> bool {
    #[cfg(target_os = "linux")]
    {
        let stored = ORIGINAL_UID.load(std::sync::atomic::Ordering::Relaxed);
        stored != u32::MAX && stored != 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
static ORIGINAL_UID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);

// Linux-specific implementations
#[cfg(target_os = "linux")]
mod linux_impl {
    use super::*;
    use nix::sched::{unshare, CloneFlags};
    use nix::sys::wait::waitpid;
    use nix::unistd::{fork, ForkResult, Gid, Pid, Uid, User};
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;
    use std::path::Path;
    use std::process::Command;

    #[derive(Debug, Clone, Copy)]
    enum IdMapKind {
        Uid,
        Gid,
    }

    impl IdMapKind {
        fn map_name(self) -> &'static str {
            match self {
                Self::Uid => "uid",
                Self::Gid => "gid",
            }
        }

        fn helper_binary(self) -> &'static str {
            match self {
                Self::Uid => "newuidmap",
                Self::Gid => "newgidmap",
            }
        }

        fn subid_file(self) -> &'static str {
            match self {
                Self::Uid => "/etc/subuid",
                Self::Gid => "/etc/subgid",
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct SubordinateRange {
        outer_start: u32,
        count: u32,
    }

    #[derive(Debug, Clone, Copy)]
    struct IdMapEntry {
        inside: u32,
        outside: u32,
        count: u32,
    }

    /// Create namespaces for the current process (in-place).
    pub fn create_namespaces(config: &NamespaceConfig) -> Result<(), SandboxError> {
        let mut flags = CloneFlags::empty();
        let outer_uid = Uid::current().as_raw();
        let outer_gid = Gid::current().as_raw();

        // User namespace must be created first for rootless operation
        if config.user_ns {
            unshare(CloneFlags::CLONE_NEWUSER).map_err(|e| {
                SandboxError::NamespaceCreation(format!("failed to create user namespace: {e}"))
            })?;

            let pid = std::process::id();
            setup_uid_map_for_pid(pid, config.inner_uid, outer_uid)?;
            setup_gid_map_for_pid(pid, config.inner_gid, outer_gid)?;
        }

        if config.mount_ns {
            flags |= CloneFlags::CLONE_NEWNS;
        }
        if config.pid_ns {
            flags |= CloneFlags::CLONE_NEWPID;
        }
        if config.net_ns {
            flags |= CloneFlags::CLONE_NEWNET;
        }
        if config.uts_ns {
            flags |= CloneFlags::CLONE_NEWUTS;
        }
        if config.ipc_ns {
            flags |= CloneFlags::CLONE_NEWIPC;
        }

        if !flags.is_empty() {
            unshare(flags).map_err(|e| {
                SandboxError::NamespaceCreation(format!("failed to create namespaces: {e}"))
            })?;
        }

        Ok(())
    }

    fn setup_uid_map_for_pid(pid: u32, inner_uid: u32, outer_uid: u32) -> Result<(), SandboxError> {
        setup_id_map_for_pid(IdMapKind::Uid, pid, inner_uid, outer_uid)
    }

    fn setup_gid_map_for_pid(pid: u32, inner_gid: u32, outer_gid: u32) -> Result<(), SandboxError> {
        write_file(&format!("/proc/{pid}/setgroups"), "deny\n")?;
        setup_id_map_for_pid(IdMapKind::Gid, pid, inner_gid, outer_gid)
    }

    fn setup_id_map_for_pid(
        kind: IdMapKind,
        pid: u32,
        inner_id: u32,
        outer_id: u32,
    ) -> Result<(), SandboxError> {
        let mut helper_error = None;
        match try_setup_id_map_with_helper(kind, pid, inner_id, outer_id) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => {
                helper_error = Some(error);
            }
        }

        let direct_map_content = format!("{inner_id} {outer_id} 1\n");
        let direct_path = format!("/proc/{pid}/{}_map", kind.map_name());
        if let Err(direct_error) = write_file(&direct_path, &direct_map_content) {
            if let Some(helper_error) = helper_error {
                return Err(SandboxError::NamespaceCreation(format!(
                    "failed to configure {} mapping: helper error: {}; direct write error: {}",
                    kind.map_name(),
                    helper_error,
                    direct_error
                )));
            }
            return Err(direct_error);
        }

        if let Some(helper_error) = helper_error {
            tracing::debug!(
                map = kind.map_name(),
                %helper_error,
                "ID-map helper failed; fell back to direct single-ID mapping"
            );
        }

        Ok(())
    }

    fn try_setup_id_map_with_helper(
        kind: IdMapKind,
        pid: u32,
        inner_id: u32,
        outer_id: u32,
    ) -> Result<bool, String> {
        let username = User::from_uid(Uid::from_raw(outer_id))
            .ok()
            .flatten()
            .map(|user| user.name);
        let ranges = read_subordinate_ranges(kind.subid_file(), username.as_deref(), outer_id)?;
        if ranges.is_empty() {
            return Ok(false);
        }

        let entries = build_helper_map_entries(inner_id, outer_id, &ranges);
        if entries.len() <= 1 {
            return Ok(false);
        }

        run_idmap_helper(kind.helper_binary(), pid, &entries)
    }

    fn read_subordinate_ranges(
        path: &str,
        username: Option<&str>,
        uid: u32,
    ) -> Result<Vec<SubordinateRange>, String> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("failed to read {path}: {error}")),
        };
        Ok(parse_subordinate_ranges(&content, username, uid))
    }

    fn parse_subordinate_ranges(
        content: &str,
        username: Option<&str>,
        uid: u32,
    ) -> Vec<SubordinateRange> {
        let uid_str = uid.to_string();
        let mut ranges = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let mut fields = line.split(':');
            let Some(owner) = fields.next().map(str::trim) else {
                continue;
            };
            let Some(start_str) = fields.next().map(str::trim) else {
                continue;
            };
            let Some(count_str) = fields.next().map(str::trim) else {
                continue;
            };
            if fields.next().is_some() {
                continue;
            }

            let owner_matches = owner == uid_str || username == Some(owner);
            if !owner_matches {
                continue;
            }

            let Ok(start_u64) = start_str.parse::<u64>() else {
                continue;
            };
            let Ok(count_u64) = count_str.parse::<u64>() else {
                continue;
            };
            if count_u64 == 0 {
                continue;
            }
            let Ok(outer_start) = u32::try_from(start_u64) else {
                continue;
            };
            let Ok(count) = u32::try_from(count_u64) else {
                continue;
            };

            ranges.push(SubordinateRange { outer_start, count });
        }

        ranges
    }

    fn build_helper_map_entries(
        inner_id: u32,
        outer_id: u32,
        ranges: &[SubordinateRange],
    ) -> Vec<IdMapEntry> {
        let mut entries = vec![IdMapEntry {
            inside: inner_id,
            outside: outer_id,
            count: 1,
        }];

        let Some(mut next_inside) = inner_id.checked_add(1) else {
            return entries;
        };

        for range in ranges {
            entries.push(IdMapEntry {
                inside: next_inside,
                outside: range.outer_start,
                count: range.count,
            });

            let Some(next) = next_inside.checked_add(range.count) else {
                break;
            };
            next_inside = next;
        }

        entries
    }

    fn run_idmap_helper(helper: &str, pid: u32, entries: &[IdMapEntry]) -> Result<bool, String> {
        let mut command = Command::new(helper);
        command.arg(pid.to_string());
        for entry in entries {
            command
                .arg(entry.inside.to_string())
                .arg(entry.outside.to_string())
                .arg(entry.count.to_string());
        }

        let output = match command.output() {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("failed to launch {helper}: {error}")),
        };

        if output.status.success() {
            return Ok(true);
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exit status {}", output.status)
        };

        Err(format!("{helper} failed: {detail}"))
    }

    fn write_file(path: &str, content: &str) -> Result<(), SandboxError> {
        let mut file = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| SandboxError::NamespaceCreation(format!("failed to open {path}: {e}")))?;
        file.write_all(content.as_bytes())
            .map_err(|e| SandboxError::NamespaceCreation(format!("failed to write to {path}: {e}")))
    }

    /// Enter existing namespaces by file descriptors.
    pub fn enter_namespaces(ns_fds: &NamespaceFds) -> Result<(), SandboxError> {
        use nix::sched::setns;

        if let Some(fd) = &ns_fds.user_ns {
            setns(fd, CloneFlags::CLONE_NEWUSER).map_err(|e| {
                SandboxError::NamespaceCreation(format!("failed to enter user namespace: {e}"))
            })?;
        }
        if let Some(fd) = &ns_fds.mount_ns {
            setns(fd, CloneFlags::CLONE_NEWNS).map_err(|e| {
                SandboxError::NamespaceCreation(format!("failed to enter mount namespace: {e}"))
            })?;
        }
        if let Some(fd) = &ns_fds.pid_ns {
            setns(fd, CloneFlags::CLONE_NEWPID).map_err(|e| {
                SandboxError::NamespaceCreation(format!("failed to enter PID namespace: {e}"))
            })?;
        }
        if let Some(fd) = &ns_fds.net_ns {
            setns(fd, CloneFlags::CLONE_NEWNET).map_err(|e| {
                SandboxError::NamespaceCreation(format!("failed to enter network namespace: {e}"))
            })?;
        }
        Ok(())
    }

    impl NamespaceFds {
        pub fn from_pid(pid: Pid) -> Result<Self, SandboxError> {
            let ns_dir = format!("/proc/{}/ns", pid.as_raw());
            let open_ns = |name: &str| -> Option<OwnedFd> {
                let path = format!("{ns_dir}/{name}");
                File::open(&path).ok().map(Into::into)
            };
            Ok(Self {
                user_ns: open_ns("user"),
                mount_ns: open_ns("mnt"),
                pid_ns: open_ns("pid"),
                net_ns: open_ns("net"),
            })
        }
    }

    /// Fork and execute a function in a new set of namespaces.
    pub fn fork_with_namespaces<F>(
        config: &NamespaceConfig,
        child_fn: F,
    ) -> Result<Pid, SandboxError>
    where
        F: FnOnce() -> i32,
    {
        let (read_fd, write_fd) = nix::unistd::pipe()
            .map_err(|e| SandboxError::NamespaceCreation(format!("failed to create pipe: {e}")))?;

        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                drop(write_fd);
                let mut buf = [0u8; 1];
                let mut read_file = File::from(read_fd);
                match read_file.read_exact(&mut buf) {
                    Ok(_) if buf[0] == 0 => Ok(child),
                    Ok(_) => {
                        let _ = waitpid(child, None);
                        Err(SandboxError::NamespaceCreation(
                            "child namespace setup failed".to_string(),
                        ))
                    }
                    Err(e) => {
                        let _ = waitpid(child, None);
                        Err(SandboxError::NamespaceCreation(format!(
                            "failed to read from child: {e}"
                        )))
                    }
                }
            }
            Ok(ForkResult::Child) => {
                drop(read_fd);
                let setup_result = create_namespaces(config);
                let status = if setup_result.is_ok() { 0u8 } else { 1u8 };
                let _ = nix::unistd::write(&write_fd, &[status]);
                drop(write_fd);
                if setup_result.is_err() {
                    std::process::exit(1);
                }
                let exit_code = child_fn();
                std::process::exit(exit_code);
            }
            Err(e) => Err(SandboxError::NamespaceCreation(format!("fork failed: {e}"))),
        }
    }

    /// Enter rootless mode by creating user and mount namespaces.
    /// After this call the process has CAP_SYS_ADMIN inside its own
    /// user namespace, enabling overlay/proc/tmpfs mounts without root.
    /// No-op if already running as root. Idempotent (second call is a no-op
    /// because Uid::current() returns 0 after the first successful call).
    pub fn enter_rootless_mode() -> Result<(), SandboxError> {
        if Uid::current().is_root() {
            return Ok(());
        }

        let orig_uid = Uid::current().as_raw();
        let orig_gid = Gid::current().as_raw();
        ORIGINAL_UID.store(orig_uid, std::sync::atomic::Ordering::Relaxed);
        let pid = std::process::id();

        unshare(CloneFlags::CLONE_NEWUSER).map_err(|e| {
            SandboxError::NamespaceCreation(format!(
                "failed to create user namespace: {e}. \
                 Ensure unprivileged user namespaces are enabled \
                 (sysctl kernel.unprivileged_userns_clone=1)"
            ))
        })?;

        setup_uid_map_for_pid(pid, 0, orig_uid)?;
        setup_gid_map_for_pid(pid, 0, orig_gid)?;

        unshare(CloneFlags::CLONE_NEWNS).map_err(|e| {
            SandboxError::NamespaceCreation(format!("failed to create mount namespace: {e}"))
        })?;

        make_mount_private()?;

        tracing::info!("Entered rootless mode (uid {orig_uid} mapped to root in user namespace)");
        Ok(())
    }

    /// Make the mount namespace private.
    pub fn make_mount_private() -> Result<(), SandboxError> {
        use nix::mount::{mount, MsFlags};
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REC | MsFlags::MS_PRIVATE,
            None::<&str>,
        )
        .map_err(|e| SandboxError::NamespaceCreation(format!("failed to make mount private: {e}")))
    }

    /// Pivot root to a new root filesystem.
    pub fn pivot_root(new_root: &Path, put_old: &Path) -> Result<(), SandboxError> {
        use nix::unistd::{chdir, pivot_root as nix_pivot_root};
        chdir(new_root).map_err(|e| {
            SandboxError::NamespaceCreation(format!("failed to chdir to new root: {e}"))
        })?;
        nix_pivot_root(new_root, put_old)
            .map_err(|e| SandboxError::NamespaceCreation(format!("failed to pivot_root: {e}")))?;
        chdir("/")
            .map_err(|e| SandboxError::NamespaceCreation(format!("failed to chdir to /: {e}")))
    }

    /// Set the hostname inside a UTS namespace.
    pub fn set_hostname(hostname: &str) -> Result<(), SandboxError> {
        nix::unistd::sethostname(hostname)
            .map_err(|e| SandboxError::NamespaceCreation(format!("failed to set hostname: {e}")))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_parse_subordinate_ranges_matches_username_and_uid() {
            let content = r#"
                alice:100000:65536
                1000:200000:32768
                bob:300000:65536
                alice:not-a-number:100
                alice:400000:0
                malformed
            "#;

            let ranges = parse_subordinate_ranges(content, Some("alice"), 1000);
            assert_eq!(ranges.len(), 2);
            assert_eq!(ranges[0].outer_start, 100000);
            assert_eq!(ranges[0].count, 65536);
            assert_eq!(ranges[1].outer_start, 200000);
            assert_eq!(ranges[1].count, 32768);
        }

        #[test]
        fn test_build_helper_map_entries_includes_identity_and_subordinate_ranges() {
            let ranges = vec![
                SubordinateRange {
                    outer_start: 100000,
                    count: 65536,
                },
                SubordinateRange {
                    outer_start: 200000,
                    count: 65536,
                },
            ];

            let entries = build_helper_map_entries(0, 1000, &ranges);
            assert_eq!(entries.len(), 3);

            assert_eq!(entries[0].inside, 0);
            assert_eq!(entries[0].outside, 1000);
            assert_eq!(entries[0].count, 1);

            assert_eq!(entries[1].inside, 1);
            assert_eq!(entries[1].outside, 100000);
            assert_eq!(entries[1].count, 65536);

            assert_eq!(entries[2].inside, 65537);
            assert_eq!(entries[2].outside, 200000);
            assert_eq!(entries[2].count, 65536);
        }
    }
}

// Re-export Linux implementations
#[cfg(target_os = "linux")]
pub use linux_impl::*;

// Stub implementations for non-Linux
#[cfg(not(target_os = "linux"))]
pub fn enter_rootless_mode() -> Result<(), SandboxError> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn create_namespaces(_config: &NamespaceConfig) -> Result<(), SandboxError> {
    Err(SandboxError::NamespaceCreation(
        "namespaces are only available on Linux".to_string(),
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn enter_namespaces(_ns_fds: &NamespaceFds) -> Result<(), SandboxError> {
    Err(SandboxError::NamespaceCreation(
        "namespaces are only available on Linux".to_string(),
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn fork_with_namespaces<F>(_config: &NamespaceConfig, _child_fn: F) -> Result<i32, SandboxError>
where
    F: FnOnce() -> i32,
{
    Err(SandboxError::NamespaceCreation(
        "namespaces are only available on Linux".to_string(),
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn make_mount_private() -> Result<(), SandboxError> {
    Err(SandboxError::NamespaceCreation(
        "mount namespaces are only available on Linux".to_string(),
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn pivot_root(
    _new_root: &std::path::Path,
    _put_old: &std::path::Path,
) -> Result<(), SandboxError> {
    Err(SandboxError::NamespaceCreation(
        "pivot_root is only available on Linux".to_string(),
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn set_hostname(_hostname: &str) -> Result<(), SandboxError> {
    Err(SandboxError::NamespaceCreation(
        "sethostname is only available on Linux".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_namespace_config_default() {
        let config = NamespaceConfig::default();
        assert!(config.user_ns);
        assert!(config.mount_ns);
        assert!(config.pid_ns);
        assert!(!config.net_ns);
    }
}

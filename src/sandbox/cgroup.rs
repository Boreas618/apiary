//! cgroups v2 resource limits for sandboxes.
//!
//! This module provides functions to create and manage cgroups for
//! resource limiting in sandboxes. It supports both root and rootless
//! (delegated) cgroup operation.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::SandboxError;
use crate::config::ResourceLimits;

const CGROUP_V2_BASE: &str = "/sys/fs/cgroup";

/// Setup a cgroup for a sandbox.
pub fn setup_cgroup(sandbox_id: &str, limits: &ResourceLimits) -> Result<PathBuf, SandboxError> {
    let cgroup_path = get_cgroup_path(sandbox_id)?;
    fs::create_dir_all(&cgroup_path).map_err(|e| {
        SandboxError::CgroupSetup(format!(
            "failed to create cgroup dir {}: {e}",
            cgroup_path.display()
        ))
    })?;

    // Enable controllers in the parent so limits can be applied to children.
    if let Some(parent) = cgroup_path.parent() {
        let available = fs::read_to_string(parent.join("cgroup.controllers")).unwrap_or_default();
        let to_enable: String = available
            .split_whitespace()
            .map(|c| format!("+{c}"))
            .collect::<Vec<_>>()
            .join(" ");
        if !to_enable.is_empty() {
            let _ = write_cgroup_file(parent, "cgroup.subtree_control", &to_enable);
        }
    }

    apply_limits(&cgroup_path, limits)?;
    Ok(cgroup_path)
}

fn get_cgroup_path(sandbox_id: &str) -> Result<PathBuf, SandboxError> {
    if let Ok(delegated) = find_delegated_cgroup() {
        return Ok(delegated.join("apiary").join(sandbox_id));
    }
    Ok(PathBuf::from(CGROUP_V2_BASE)
        .join("apiary")
        .join(sandbox_id))
}

fn find_delegated_cgroup() -> Result<PathBuf, SandboxError> {
    // Use the real uid (before user namespace), since cgroup paths
    // on the host are named after the original uid, not the mapped one.
    let uid = super::namespace::original_uid();
    let patterns = [
        format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"),
        format!("/sys/fs/cgroup/user.slice/user-{uid}.slice"),
    ];

    for pattern in &patterns {
        let path = PathBuf::from(pattern);
        if path.exists() && is_cgroup_writable(&path) {
            return Ok(path);
        }
    }

    if let Ok(cgroup_path) = read_current_cgroup() {
        let full_path = PathBuf::from(CGROUP_V2_BASE).join(cgroup_path.trim_start_matches('/'));
        if full_path.exists() && is_cgroup_writable(&full_path) {
            return Ok(full_path);
        }
    }

    Err(SandboxError::CgroupSetup(
        "no delegated cgroup found; run with root or setup systemd delegation".to_string(),
    ))
}

fn is_cgroup_writable(path: &Path) -> bool {
    let test_dir = path.join(".apiary-test");
    if fs::create_dir(&test_dir).is_ok() {
        let _ = fs::remove_dir(&test_dir);
        return true;
    }
    false
}

fn read_current_cgroup() -> Result<String, SandboxError> {
    let content = fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| SandboxError::CgroupSetup(format!("failed to read /proc/self/cgroup: {e}")))?;

    for line in content.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            return Ok(path.to_string());
        }
    }

    Err(SandboxError::CgroupSetup(
        "cgroups v2 not found in /proc/self/cgroup".to_string(),
    ))
}

fn apply_limits(cgroup_path: &Path, limits: &ResourceLimits) -> Result<(), SandboxError> {
    let available = fs::read_to_string(cgroup_path.join("cgroup.controllers")).unwrap_or_default();

    if available.contains("memory") {
        write_cgroup_file(cgroup_path, "memory.max", &limits.memory_max)?;
    }
    if available.contains("cpu") {
        write_cgroup_file(cgroup_path, "cpu.max", &limits.cpu_max)?;
    }
    if available.contains("pids") {
        write_cgroup_file(cgroup_path, "pids.max", &limits.pids_max.to_string())?;
    }
    if available.contains("io") {
        if let Some(ref io_max) = limits.io_max {
            let _ = write_cgroup_file(cgroup_path, "io.max", io_max);
        }
    }

    if !available.contains("memory") || !available.contains("pids") {
        tracing::warn!(
            available = %available.trim(),
            "some cgroup controllers not available; resource limits may be incomplete"
        );
    }

    Ok(())
}

fn write_cgroup_file(cgroup_path: &Path, filename: &str, value: &str) -> Result<(), SandboxError> {
    let file_path = cgroup_path.join(filename);
    let mut file = OpenOptions::new()
        .write(true)
        .open(&file_path)
        .map_err(|e| {
            SandboxError::CgroupSetup(format!("failed to open {}: {e}", file_path.display()))
        })?;
    file.write_all(value.as_bytes()).map_err(|e| {
        SandboxError::CgroupSetup(format!("failed to write to {}: {e}", file_path.display()))
    })
}

fn read_cgroup_file(cgroup_path: &Path, filename: &str) -> Result<String, SandboxError> {
    let file_path = cgroup_path.join(filename);
    fs::read_to_string(&file_path).map_err(|e| {
        SandboxError::CgroupSetup(format!("failed to read {}: {e}", file_path.display()))
    })
}

/// Add a process to a cgroup.
pub fn add_process_to_cgroup(cgroup_path: &Path, pid: u32) -> Result<(), SandboxError> {
    write_cgroup_file(cgroup_path, "cgroup.procs", &pid.to_string())
}

/// Reset cgroup statistics.
pub fn reset_cgroup(cgroup_path: &Path) -> Result<(), SandboxError> {
    kill_cgroup_processes(cgroup_path)?;
    let _ = write_cgroup_file(cgroup_path, "memory.reclaim", "0");
    Ok(())
}

/// Kill all processes in a cgroup.
pub fn kill_cgroup_processes(cgroup_path: &Path) -> Result<(), SandboxError> {
    if write_cgroup_file(cgroup_path, "cgroup.kill", "1").is_ok() {
        return Ok(());
    }

    let procs = read_cgroup_file(cgroup_path, "cgroup.procs")?;
    for line in procs.lines() {
        if let Ok(pid) = line.parse::<i32>() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    Ok(())
}

/// Remove a cgroup. Retries briefly for processes to exit after SIGKILL.
pub fn remove_cgroup(cgroup_path: &Path) -> Result<(), SandboxError> {
    let _ = kill_cgroup_processes(cgroup_path);

    for _ in 0..10 {
        match fs::remove_dir(cgroup_path) {
            Ok(()) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                return Err(SandboxError::CgroupSetup(format!(
                    "failed to remove cgroup {}: {e}",
                    cgroup_path.display()
                )));
            }
        }
    }

    fs::remove_dir(cgroup_path).map_err(|e| {
        SandboxError::CgroupSetup(format!(
            "failed to remove cgroup {} after retries: {e}",
            cgroup_path.display()
        ))
    })
}

/// Get cgroup statistics.
#[derive(Debug, Default)]
pub struct CgroupStats {
    pub memory_current: u64,
    pub memory_peak: u64,
    pub pids_current: u64,
    pub cpu_usage_usec: u64,
}

pub fn get_cgroup_stats(cgroup_path: &Path) -> Result<CgroupStats, SandboxError> {
    let mut stats = CgroupStats::default();

    if let Ok(s) = read_cgroup_file(cgroup_path, "memory.current") {
        stats.memory_current = s.trim().parse().unwrap_or(0);
    }
    if let Ok(s) = read_cgroup_file(cgroup_path, "memory.peak") {
        stats.memory_peak = s.trim().parse().unwrap_or(0);
    }
    if let Ok(s) = read_cgroup_file(cgroup_path, "pids.current") {
        stats.pids_current = s.trim().parse().unwrap_or(0);
    }
    if let Ok(s) = read_cgroup_file(cgroup_path, "cpu.stat") {
        for line in s.lines() {
            if let Some(value) = line.strip_prefix("usage_usec ") {
                stats.cpu_usage_usec = value.parse().unwrap_or(0);
                break;
            }
        }
    }
    Ok(stats)
}

/// Check if cgroups v2 is available.
pub fn is_cgroup_v2_available() -> bool {
    Path::new(CGROUP_V2_BASE)
        .join("cgroup.controllers")
        .exists()
}

/// Check if the current user has a delegated cgroup.
pub fn has_delegated_cgroup() -> bool {
    find_delegated_cgroup().is_ok()
}

/// Parse a memory size string to bytes.
pub fn parse_memory_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, multiplier) = if let Some(n) = s.strip_suffix('G') {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('M') {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('K') {
        (n, 1024)
    } else {
        (s, 1)
    };
    num.trim().parse::<u64>().ok().map(|n| n * multiplier)
}

/// Format bytes as a human-readable string.
pub fn format_memory_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_memory_size() {
        assert_eq!(parse_memory_size("1024"), Some(1024));
        assert_eq!(parse_memory_size("1K"), Some(1024));
        assert_eq!(parse_memory_size("1M"), Some(1024 * 1024));
        assert_eq!(parse_memory_size("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_memory_size("2G"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_memory_size("512M"), Some(512 * 1024 * 1024));
    }

    #[test]
    fn test_format_memory_size() {
        assert_eq!(format_memory_size(500), "500B");
        assert_eq!(format_memory_size(1024), "1.0K");
        assert_eq!(format_memory_size(1024 * 1024), "1.0M");
        assert_eq!(format_memory_size(1024 * 1024 * 1024), "1.0G");
    }

    #[test]
    fn test_cgroup_v2_check() {
        let _ = is_cgroup_v2_available();
    }
}

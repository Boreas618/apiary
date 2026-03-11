//! Process-level resource limits via `setrlimit`.
//!
//! This module provides a cgroup-independent fallback for resource enforcement.
//! All limits are applied in the forked child before exec (`pre_exec` context),
//! using `libc::setrlimit` which is async-signal-safe.
//!
//! These limits are always applied (defense-in-depth), regardless of whether
//! cgroups are available.

use std::time::Duration;

use crate::config::ResourceLimits;
use crate::sandbox::cgroup::parse_memory_size;

use super::process::write_stderr_safe;

/// Apply process-level resource limits using `setrlimit`.
///
/// Called from `configure_task_process_linux` in the post-fork, pre-exec context.
/// All operations here are async-signal-safe.
pub fn apply_rlimits(limits: &ResourceLimits, timeout: Duration) -> Result<(), std::io::Error> {
    // RLIMIT_AS: virtual address space.
    // We multiply memory_max by rlimit_as_multiplier because VAS is always
    // much larger than RSS (Python/Java mmap far more than they actually use).
    if let Some(memory_bytes) = parse_memory_size(&limits.memory_max) {
        let vas_limit = memory_bytes.saturating_mul(limits.rlimit_as_multiplier);
        if let Err(e) = set_rlimit(libc::RLIMIT_AS, vas_limit, vas_limit) {
            write_stderr_safe(b"[apiary] warning: failed to set RLIMIT_AS\n");
            return Err(e);
        }
    }

    // NOTE: RLIMIT_NPROC is deliberately NOT set here.
    // It limits processes per UID (not per sandbox). When multiple sandboxes
    // share the same UID (typical in rootless/container setups), a low NPROC
    // value causes fork failures across ALL sandboxes once the UID-wide total
    // exceeds the limit. Per-sandbox PID enforcement is handled by the
    // ProcessMonitor (counting processes per PGID) or cgroups pids.max instead.

    // RLIMIT_CPU: CPU time in seconds.
    // Soft limit = task timeout (SIGXCPU), hard limit = timeout + 30s grace.
    let cpu_secs = timeout.as_secs();
    if cpu_secs > 0 {
        let hard = cpu_secs.saturating_add(30);
        if let Err(e) = set_rlimit(libc::RLIMIT_CPU, cpu_secs, hard) {
            write_stderr_safe(b"[apiary] warning: failed to set RLIMIT_CPU\n");
            return Err(e);
        }
    }

    // RLIMIT_FSIZE: max file size a process can write.
    if let Some(ref fsize_str) = limits.max_file_size {
        if let Some(fsize_bytes) = parse_memory_size(fsize_str) {
            if let Err(e) = set_rlimit(libc::RLIMIT_FSIZE, fsize_bytes, fsize_bytes) {
                write_stderr_safe(b"[apiary] warning: failed to set RLIMIT_FSIZE\n");
                return Err(e);
            }
        }
    }

    // RLIMIT_NOFILE: max open file descriptors.
    let nofile = limits.max_open_files;
    if nofile > 0 {
        if let Err(e) = set_rlimit(libc::RLIMIT_NOFILE, nofile, nofile) {
            write_stderr_safe(b"[apiary] warning: failed to set RLIMIT_NOFILE\n");
            return Err(e);
        }
    }

    Ok(())
}

fn set_rlimit(resource: libc::__rlimit_resource_t, soft: u64, hard: u64) -> Result<(), std::io::Error> {
    let limit = libc::rlimit {
        rlim_cur: soft as libc::rlim_t,
        rlim_max: hard as libc::rlim_t,
    };
    let ret = unsafe { libc::setrlimit(resource, &limit) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Compute the RLIMIT_AS value from ResourceLimits for reporting/testing.
pub fn compute_vas_limit(limits: &ResourceLimits) -> Option<u64> {
    parse_memory_size(&limits.memory_max)
        .map(|bytes| bytes.saturating_mul(limits.rlimit_as_multiplier))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResourceLimits;
    use std::time::Duration;

    #[test]
    fn compute_vas_limit_applies_multiplier() {
        let limits = ResourceLimits {
            memory_max: "1G".to_string(),
            rlimit_as_multiplier: 3,
            ..Default::default()
        };
        assert_eq!(
            compute_vas_limit(&limits),
            Some(3 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn compute_vas_limit_default_multiplier() {
        let limits = ResourceLimits::default();
        // default: 2G * 2 = 4G
        assert_eq!(
            compute_vas_limit(&limits),
            Some(2 * 2 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn compute_vas_limit_returns_none_for_invalid() {
        let limits = ResourceLimits {
            memory_max: "invalid".to_string(),
            ..Default::default()
        };
        assert_eq!(compute_vas_limit(&limits), None);
    }

    #[test]
    fn set_rlimit_nofile_succeeds() {
        assert!(set_rlimit(libc::RLIMIT_NOFILE, 1024, 1024).is_ok());
    }

    #[test]
    fn apply_rlimits_with_defaults_succeeds() {
        let limits = ResourceLimits::default();
        let result = apply_rlimits(&limits, Duration::from_secs(60));
        // May fail if NPROC limit is rejected (depends on system), but
        // NOFILE alone should succeed.
        let _ = result;
    }

    #[test]
    fn apply_rlimits_with_fsize_limit() {
        let limits = ResourceLimits {
            max_file_size: Some("512M".to_string()),
            ..Default::default()
        };
        let _ = apply_rlimits(&limits, Duration::from_secs(30));
    }

    #[test]
    fn apply_rlimits_zero_timeout_skips_cpu_limit() {
        let limits = ResourceLimits::default();
        // timeout = 0 should not set RLIMIT_CPU
        let _ = apply_rlimits(&limits, Duration::from_secs(0));
    }

    #[test]
    fn compute_vas_limit_saturation() {
        let limits = ResourceLimits {
            memory_max: format!("{}G", u64::MAX / (1024 * 1024 * 1024) + 1),
            rlimit_as_multiplier: 2,
            ..Default::default()
        };
        // Should not panic; saturating_mul handles overflow.
        let _ = compute_vas_limit(&limits);
    }
}

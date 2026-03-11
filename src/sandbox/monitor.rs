//! Daemon-level process monitor for RSS-based memory enforcement.
//!
//! When cgroups are unavailable, `RLIMIT_AS` limits virtual address space
//! (which is often far larger than actual memory usage). This monitor
//! provides a more accurate safety net by polling `/proc/<pid>/status`
//! for `VmRSS` and counting processes in the process group.
//!
//! The monitor runs as a background tokio task and communicates with
//! sandbox execution via an async channel.

use std::collections::HashMap;
use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::sandbox::cgroup::parse_memory_size;

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const CHANNEL_CAPACITY: usize = 256;

/// Limits enforced by the process monitor.
#[derive(Debug, Clone)]
pub struct MonitorLimits {
    /// Maximum resident set size in bytes.
    pub memory_max_bytes: u64,
    /// Maximum number of processes in the process group.
    pub pids_max: u64,
}

/// Commands sent from sandboxes to the monitor background task.
pub(crate) enum MonitorCmd {
    Register {
        pid: u32,
        pgid: u32,
        limits: MonitorLimits,
    },
    Unregister {
        pid: u32,
    },
    Shutdown,
}

/// A tracked process entry.
struct TrackedProcess {
    pid: u32,
    pgid: u32,
    limits: MonitorLimits,
}

/// Handle for communicating with the background process monitor.
#[derive(Clone)]
pub struct ProcessMonitor {
    cmd_tx: mpsc::Sender<MonitorCmd>,
}

impl ProcessMonitor {
    /// Spawn a new process monitor background task.
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(CHANNEL_CAPACITY);
        tokio::spawn(monitor_loop(cmd_rx));
        Self { cmd_tx }
    }

    /// Register a process for monitoring.
    pub async fn register(&self, pid: u32, pgid: u32, limits: MonitorLimits) {
        let _ = self
            .cmd_tx
            .send(MonitorCmd::Register { pid, pgid, limits })
            .await;
    }

    /// Unregister a process (e.g. after it exits).
    pub async fn unregister(&self, pid: u32) {
        let _ = self.cmd_tx.send(MonitorCmd::Unregister { pid }).await;
    }

    /// Shut down the monitor background task.
    pub async fn shutdown(&self) {
        let _ = self.cmd_tx.send(MonitorCmd::Shutdown).await;
    }
}

async fn monitor_loop(mut cmd_rx: mpsc::Receiver<MonitorCmd>) {
    let mut tracked: HashMap<u32, TrackedProcess> = HashMap::new();

    loop {
        // Drain all pending commands before polling.
        loop {
            match cmd_rx.try_recv() {
                Ok(MonitorCmd::Register { pid, pgid, limits }) => {
                    tracked.insert(pid, TrackedProcess { pid, pgid, limits });
                }
                Ok(MonitorCmd::Unregister { pid }) => {
                    tracked.remove(&pid);
                }
                Ok(MonitorCmd::Shutdown) => {
                    tracing::debug!("process monitor shutting down");
                    return;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return,
            }
        }

        // Poll each tracked process.
        let pids: Vec<u32> = tracked.keys().copied().collect();
        for pid in pids {
            let entry = match tracked.get(&pid) {
                Some(e) => e,
                None => continue,
            };

            let violation = check_process(entry);

            match violation {
                Some(reason) => {
                    tracing::warn!(
                        pid = entry.pid,
                        pgid = entry.pgid,
                        reason = %reason,
                        "killing process group: resource limit exceeded"
                    );
                    kill_process_group(entry.pgid);
                    tracked.remove(&pid);
                }
                None => {}
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Check if a tracked process exceeds its limits. Returns the violation reason.
fn check_process(entry: &TrackedProcess) -> Option<String> {
    // Check RSS.
    if let Some(rss_bytes) = read_vmrss(entry.pid) {
        if rss_bytes > entry.limits.memory_max_bytes {
            return Some(format!(
                "VmRSS {rss_bytes} exceeds limit {}",
                entry.limits.memory_max_bytes
            ));
        }
    }

    // Check PID count in the process group.
    if let Some(count) = count_pgid_processes(entry.pgid) {
        if count > entry.limits.pids_max {
            return Some(format!(
                "process count {count} exceeds limit {}",
                entry.limits.pids_max
            ));
        }
    }

    None
}

/// Read VmRSS from `/proc/<pid>/status` in bytes.
fn read_vmrss(pid: u32) -> Option<u64> {
    let path = PathBuf::from(format!("/proc/{pid}/status"));
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let rest = rest.trim();
            // Format: "12345 kB"
            let kb_str = rest.strip_suffix("kB").or_else(|| rest.strip_suffix("KB"))?;
            let kb: u64 = kb_str.trim().parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Count processes belonging to a process group by scanning `/proc`.
fn count_pgid_processes(pgid: u32) -> Option<u64> {
    let proc_dir = std::fs::read_dir("/proc").ok()?;
    let mut count = 0u64;
    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if !name_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let stat_path = entry.path().join("stat");
        if let Ok(stat) = std::fs::read_to_string(&stat_path) {
            if let Some(process_pgid) = parse_pgid_from_stat(&stat) {
                if process_pgid == pgid {
                    count += 1;
                }
            }
        }
    }
    Some(count)
}

/// Parse the PGID (field 5, 0-indexed 4) from `/proc/<pid>/stat`.
///
/// Format: `pid (comm) state ppid pgrp ...`
/// The comm field may contain spaces and parentheses, so we find the
/// last `)` first, then parse fields after it.
fn parse_pgid_from_stat(stat: &str) -> Option<u32> {
    let after_comm = stat.rfind(')')?.checked_add(1)?;
    let rest = stat.get(after_comm..)?;
    // rest starts with: " state ppid pgrp ..."
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?;
    pgrp.parse().ok()
}

fn kill_process_group(pgid: u32) {
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pgid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
}

/// Build `MonitorLimits` from `ResourceLimits`.
pub fn limits_from_config(limits: &crate::config::ResourceLimits) -> MonitorLimits {
    let memory_max_bytes = parse_memory_size(&limits.memory_max).unwrap_or(u64::MAX);
    MonitorLimits {
        memory_max_bytes,
        pids_max: limits.pids_max,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pgid_from_stat_normal() {
        let stat = "1234 (bash) S 1000 5678 5678 0 -1 ...";
        assert_eq!(parse_pgid_from_stat(stat), Some(5678));
    }

    #[test]
    fn parse_pgid_from_stat_with_parens_in_comm() {
        let stat = "999 (my (weird) proc) S 100 4242 4242 0 -1 ...";
        assert_eq!(parse_pgid_from_stat(stat), Some(4242));
    }

    #[test]
    fn parse_pgid_from_stat_empty() {
        assert_eq!(parse_pgid_from_stat(""), None);
    }

    #[test]
    fn read_vmrss_of_self() {
        let rss = read_vmrss(std::process::id());
        // The test process itself must have some RSS.
        assert!(rss.is_some());
        assert!(rss.unwrap() > 0);
    }

    #[test]
    fn count_pgid_processes_finds_self() {
        let our_pgid = unsafe { libc::getpgrp() } as u32;
        let count = count_pgid_processes(our_pgid);
        assert!(count.is_some());
        assert!(count.unwrap() >= 1);
    }

    #[test]
    fn limits_from_config_parses_correctly() {
        let config = crate::config::ResourceLimits {
            memory_max: "512M".to_string(),
            pids_max: 128,
            ..Default::default()
        };
        let limits = limits_from_config(&config);
        assert_eq!(limits.memory_max_bytes, 512 * 1024 * 1024);
        assert_eq!(limits.pids_max, 128);
    }

    #[tokio::test]
    async fn monitor_register_unregister_shutdown() {
        let monitor = ProcessMonitor::spawn();
        let limits = MonitorLimits {
            memory_max_bytes: u64::MAX,
            pids_max: u64::MAX,
        };
        monitor.register(99999, 99999, limits).await;
        monitor.unregister(99999).await;
        monitor.shutdown().await;
    }

    #[test]
    fn check_process_no_violation_when_under_limits() {
        let entry = TrackedProcess {
            pid: std::process::id(),
            pgid: unsafe { libc::getpgrp() } as u32,
            limits: MonitorLimits {
                memory_max_bytes: u64::MAX,
                pids_max: u64::MAX,
            },
        };
        assert!(check_process(&entry).is_none());
    }

    #[test]
    fn check_process_detects_memory_violation() {
        let entry = TrackedProcess {
            pid: std::process::id(),
            pgid: unsafe { libc::getpgrp() } as u32,
            limits: MonitorLimits {
                memory_max_bytes: 1, // 1 byte — always exceeded
                pids_max: u64::MAX,
            },
        };
        let violation = check_process(&entry);
        assert!(violation.is_some());
        assert!(violation.unwrap().contains("VmRSS"));
    }

    #[test]
    fn check_process_detects_pid_violation() {
        let entry = TrackedProcess {
            pid: std::process::id(),
            pgid: unsafe { libc::getpgrp() } as u32,
            limits: MonitorLimits {
                memory_max_bytes: u64::MAX,
                pids_max: 0, // 0 processes allowed — always exceeded
            },
        };
        let violation = check_process(&entry);
        assert!(violation.is_some());
        assert!(violation.unwrap().contains("process count"));
    }

    #[test]
    fn read_vmrss_nonexistent_pid() {
        assert!(read_vmrss(u32::MAX).is_none());
    }

    #[test]
    fn count_pgid_processes_nonexistent_pgid() {
        let count = count_pgid_processes(u32::MAX);
        assert!(count.is_some());
        assert_eq!(count.unwrap(), 0);
    }

    #[test]
    fn limits_from_config_max_on_invalid_memory() {
        let config = crate::config::ResourceLimits {
            memory_max: "invalid".to_string(),
            pids_max: 64,
            ..Default::default()
        };
        let limits = limits_from_config(&config);
        assert_eq!(limits.memory_max_bytes, u64::MAX);
        assert_eq!(limits.pids_max, 64);
    }
}

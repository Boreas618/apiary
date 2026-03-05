//! Integration tests for apiary.
//!
//! Note: Many of these tests require specific Linux features:
//! - User namespaces (kernel 3.8+)
//! - OverlayFS in user namespace (kernel 5.11+)
//! - cgroups v2 with delegation
//!
//! Some tests may be skipped if run without proper permissions or on
//! unsupported systems.

use apiary::{PoolConfig, Task, TaskResult};
use std::time::Duration;

/// Check if we can run namespace tests.
#[allow(dead_code)]
fn can_run_namespace_tests() -> bool {
    // Check if we're on Linux
    if !cfg!(target_os = "linux") {
        return false;
    }

    // Check if user namespaces are available
    let status = std::process::Command::new("unshare")
        .args(["--user", "true"])
        .status();

    status.is_ok() && status.unwrap().success()
}

/// Check if we're running in a sandbox (e.g., CI container).
#[allow(dead_code)]
fn is_in_sandbox() -> bool {
    std::env::var("CODEX_SANDBOX").is_ok()
        || std::env::var("CODEX_SANDBOX_NETWORK_DISABLED").is_ok()
}

#[test]
fn test_task_creation() {
    let task = Task::new("echo hello");
    assert_eq!(task.command, vec!["echo", "hello"]);
    assert!(!task.id.is_empty());
}

#[test]
fn test_task_builder() {
    let task = Task::builder()
        .command("python3 -c 'print(1)'")
        .timeout_secs(30)
        .env("PYTHONPATH", "/app")
        .build()
        .unwrap();

    assert_eq!(task.command[0], "python3");
    assert_eq!(task.timeout, Duration::from_secs(30));
}

#[test]
fn test_config_builder() {
    let config = PoolConfig::builder()
        .pool_size(5)
        .base_image("/tmp/rootfs")
        .build();

    assert!(config.is_ok());
    let config = config.unwrap();
    assert_eq!(config.pool_size, 5);
}

#[test]
fn test_config_builder_rejects_zero_pool_size() {
    let result = PoolConfig::builder()
        .pool_size(0)
        .base_image("/tmp/rootfs")
        .build();

    assert!(result.is_err());
}

#[test]
fn test_config_builder_requires_base_image() {
    let result = PoolConfig::builder().pool_size(5).build();

    assert!(result.is_err());
}

#[test]
fn test_config_serialization() {
    let config = PoolConfig::builder()
        .pool_size(10)
        .base_image("/tmp/rootfs")
        .build()
        .unwrap();

    let toml = toml::to_string(&config).unwrap();
    assert!(toml.contains("pool_size = 10"));

    let parsed: PoolConfig = toml::from_str(&toml).unwrap();
    assert_eq!(parsed.pool_size, config.pool_size);
}

#[test]
fn test_task_result() {
    let result = TaskResult {
        task_id: "test-1".to_string(),
        exit_code: 0,
        stdout: b"hello\n".to_vec(),
        stderr: Vec::new(),
        duration: Duration::from_millis(100),
        timed_out: false,
    };

    assert!(result.success());
    assert_eq!(result.stdout_str().unwrap(), "hello\n");
}

// Tests that require Linux namespace support
#[cfg(target_os = "linux")]
mod linux_tests {
    use super::*;
    use apiary::Pool;

    #[test]
    fn test_seccomp_policy() {
        use apiary::sandbox::seccomp::{network_only_policy, strict_policy};

        let policy = network_only_policy();
        assert!(policy.block_network);
        assert!(policy.allow_unix_sockets);

        let strict = strict_policy();
        assert!(strict.block_network);
        assert!(!strict.blocked_syscalls.is_empty());
    }

    #[test]
    fn test_cgroup_helpers() {
        use apiary::sandbox::cgroup::{format_memory_size, parse_memory_size};

        assert_eq!(parse_memory_size("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_memory_size("512M"), Some(512 * 1024 * 1024));

        assert_eq!(format_memory_size(1024 * 1024 * 1024), "1.0G");
        assert_eq!(format_memory_size(512 * 1024 * 1024), "512.0M");
    }

    #[test]
    fn test_namespace_config() {
        use apiary::sandbox::namespace::NamespaceConfig;

        let config = NamespaceConfig::default();
        assert!(config.user_ns);
        assert!(config.mount_ns);
        assert!(config.pid_ns);
        assert!(!config.net_ns); // Shared host network by default
    }

    #[tokio::test]
    async fn test_pool_creation_without_base_image() {
        // This should fail because the base image doesn't exist
        let config = PoolConfig::builder()
            .pool_size(1)
            .base_image("/nonexistent/rootfs")
            .build()
            .unwrap();

        let result = Pool::new(config).await;
        assert!(result.is_err());
    }
}

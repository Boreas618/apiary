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

#[test]
fn test_task_creation() {
    let task = Task::new("echo hello");
    assert_eq!(task.command, vec!["echo", "hello"]);
    assert!(!task.id.is_empty());
    assert_eq!(task.working_dir, None);
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
    assert_eq!(task.working_dir, None);
}

#[test]
fn test_config_builder() {
    let config = PoolConfig::builder()
        .max_sandboxes(20)
        .images(apiary::ImagesConfig {
            sources: vec!["test:latest".to_string()],
            layers_dir: "/tmp/test_layers".into(),
            docker: "docker".to_string(),
            pull_concurrency: 1,
        })
        .build();

    assert!(config.is_ok());
    let config = config.unwrap();
    assert_eq!(config.max_sandboxes, 20);
}

#[test]
fn test_config_builder_requires_images() {
    let result = PoolConfig::builder().build();
    assert!(result.is_err());
}

#[test]
fn test_config_serialization() {
    let config = PoolConfig::builder()
        .max_sandboxes(40)
        .images(apiary::ImagesConfig {
            sources: vec!["test:latest".to_string()],
            layers_dir: "/tmp/test_layers".into(),
            docker: "docker".to_string(),
            pull_concurrency: 1,
        })
        .build()
        .unwrap();

    let toml_str = toml::to_string(&config).unwrap();
    assert!(toml_str.contains("max_sandboxes = 40"));

    let parsed: PoolConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(parsed.max_sandboxes, config.max_sandboxes);
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

mod linux_tests {
    use super::*;
    use apiary::Pool;

    #[test]
    fn test_default_seccomp_policy() {
        use apiary::SeccompPolicy;

        let policy = SeccompPolicy::default();
        assert!(policy.block_network);
        assert!(policy.allow_unix_sockets);
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
    fn test_namespace_rootless_mode_check() {
        use apiary::sandbox::namespace::is_rootless_mode;
        let _ = is_rootless_mode();
    }

    #[tokio::test]
    async fn test_pool_creation_with_bad_image() {
        let config = PoolConfig::builder()
            .max_sandboxes(4)
            .images(apiary::ImagesConfig {
                sources: vec!["nonexistent:image".to_string()],
                layers_dir: "/nonexistent/layers".into(),
                docker: "docker".to_string(),
                pull_concurrency: 1,
            })
            .build()
            .unwrap();

        let result = Pool::new(config).await;
        assert!(result.is_err());
    }
}

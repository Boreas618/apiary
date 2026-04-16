//! Configuration schema definitions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use super::overlay::OverlayDriver;

/// Main configuration for the sandbox pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolConfig {
    /// Maximum number of sandboxes (hard ceiling for concurrent sessions).
    #[serde(default = "default_max_sandboxes")]
    pub max_sandboxes: usize,

    /// Image registry configuration: which Docker images to load and where
    /// to cache extracted layers.
    pub images: ImagesConfig,

    /// Directory to store overlay layers.
    pub overlay_dir: PathBuf,

    /// Overlay driver to use ("auto", "kernel_overlay", or "fuse_overlayfs").
    #[serde(default)]
    pub overlay_driver: OverlayDriver,

    /// Resource limits for each sandbox.
    #[serde(default)]
    pub resource_limits: ResourceLimits,

    /// seccomp policy configuration.
    #[serde(default)]
    pub seccomp_policy: SeccompPolicy,

    /// Default timeout for tasks.
    #[serde(default = "default_timeout", with = "duration_serde")]
    pub default_timeout: Duration,

    /// Default environment variables for all tasks.
    #[serde(default)]
    pub default_env: HashMap<String, String>,

    /// When true, every task gets a read-only bind mount of the daemon's
    /// `/etc/resolv.conf` at `/etc/resolv.conf` inside the sandbox (before any
    /// task-specific `readonly_mounts`). Skipped if the source file is missing
    /// or the task already mounts `/etc/resolv.conf`. Use this so DNS matches
    /// the apiary process environment (e.g. Docker `--dns` or `-v resolv.conf`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mount_host_resolv_conf: bool,

    /// Directory to write per-session execution logs on close. When set,
    /// each session's history is written as `{session_id}.json` in this
    /// directory. Disabled when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_log_dir: Option<PathBuf>,
}

fn default_max_sandboxes() -> usize {
    40
}

fn default_timeout() -> Duration {
    Duration::from_secs(300)
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_sandboxes: default_max_sandboxes(),
            images: ImagesConfig {
                sources: vec!["ubuntu:22.04".to_string()],
                layers_dir: default_layers_dir(),
                docker: default_docker_bin(),
                pull_concurrency: default_pull_concurrency(),
            },
            overlay_dir: PathBuf::from("./overlays"),
            overlay_driver: OverlayDriver::default(),
            resource_limits: ResourceLimits::default(),
            seccomp_policy: SeccompPolicy::default(),
            default_timeout: default_timeout(),
            default_env: HashMap::new(),
            mount_host_resolv_conf: false,
            session_log_dir: None,
        }
    }
}

/// Resource limits applied via cgroups v2 and/or `setrlimit`.
///
/// The cgroup fields (`memory_max`, `cpu_max`, `pids_max`, `io_max`) are
/// written to cgroup control files when cgroups are available. The rlimit
/// fields (`max_file_size`, `max_open_files`, `rlimit_as_multiplier`) are
/// always applied via `setrlimit` in the child process (defense-in-depth).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    /// Maximum memory in bytes (e.g., "2G", "512M").
    /// Used for cgroup `memory.max` and as the base for `RLIMIT_AS`.
    #[serde(default = "default_memory_max")]
    pub memory_max: String,

    /// CPU quota (e.g., "100000 100000" for 100% of 1 CPU).
    #[serde(default = "default_cpu_max")]
    pub cpu_max: String,

    /// Maximum number of PIDs.
    /// Used for cgroup `pids.max` and `RLIMIT_NPROC`.
    #[serde(default = "default_pids_max")]
    pub pids_max: u64,

    /// I/O limits (device major:minor rbps=N wbps=N).
    #[serde(default)]
    pub io_max: Option<String>,

    /// Max file size a process can write (e.g., "1G"). Applied via `RLIMIT_FSIZE`.
    #[serde(default)]
    pub max_file_size: Option<String>,

    /// Max open file descriptors per process. Applied via `RLIMIT_NOFILE`.
    #[serde(default = "default_max_open_files")]
    pub max_open_files: u64,

    /// Multiplier for `memory_max` when deriving `RLIMIT_AS` (default: 2).
    /// Virtual address space is typically much larger than RSS, so the
    /// multiplier avoids false OOM kills from programs like Python/Java
    /// that mmap large regions without actually using them.
    #[serde(default = "default_rlimit_as_multiplier")]
    pub rlimit_as_multiplier: u64,
}

fn default_memory_max() -> String {
    "4G".to_string()
    // "2G".to_string()
}

fn default_cpu_max() -> String {
    "100000 100000".to_string()
}

fn default_pids_max() -> u64 {
    2048
    // 256
}

fn default_max_open_files() -> u64 {
    2048
}

fn default_rlimit_as_multiplier() -> u64 {
    2
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            memory_max: default_memory_max(),
            cpu_max: default_cpu_max(),
            pids_max: default_pids_max(),
            io_max: None,
            max_file_size: None,
            max_open_files: default_max_open_files(),
            rlimit_as_multiplier: default_rlimit_as_multiplier(),
        }
    }
}

/// seccomp policy configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeccompPolicy {
    /// Whether to block network syscalls.
    #[serde(default = "default_block_network")]
    pub block_network: bool,

    /// Allow AF_UNIX sockets even when blocking network.
    #[serde(default = "default_allow_unix_sockets")]
    pub allow_unix_sockets: bool,

    /// Additional syscalls to block (by name).
    #[serde(default)]
    pub blocked_syscalls: Vec<String>,

    /// Additional syscalls to allow (by name).
    #[serde(default)]
    pub allowed_syscalls: Vec<String>,
}

fn default_block_network() -> bool {
    // true
    false
}

fn default_allow_unix_sockets() -> bool {
    true
}

impl Default for SeccompPolicy {
    fn default() -> Self {
        Self {
            block_network: default_block_network(),
            allow_unix_sockets: default_allow_unix_sockets(),
            blocked_syscalls: Vec::new(),
            allowed_syscalls: Vec::new(),
        }
    }
}

/// Image registry configuration.
///
/// Specifies which Docker images to make available and where to cache
/// extracted layers.  The layer cache directory (`layers_dir`) **must** be
/// on a local filesystem that supports xattr and device nodes (not NFS).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagesConfig {
    /// Docker image names or paths to files listing image names (one per
    /// line). Each entry that is an existing file path is read as a list
    /// file; everything else is treated as a Docker image name.
    #[serde(default)]
    pub sources: Vec<String>,

    /// Local directory for content-addressable layer cache.
    /// Must be on a filesystem with xattr + device node support (not NFS).
    #[serde(default = "default_layers_dir")]
    pub layers_dir: PathBuf,

    /// Docker CLI binary path.
    #[serde(default = "default_docker_bin")]
    pub docker: String,

    /// Max concurrent `docker pull` operations.
    #[serde(default = "default_pull_concurrency")]
    pub pull_concurrency: usize,
}

fn default_layers_dir() -> PathBuf {
    PathBuf::from("/tmp/apiary_layers")
}

fn default_docker_bin() -> String {
    "docker".to_string()
}

fn default_pull_concurrency() -> usize {
    8
}

impl ImagesConfig {
    /// Resolve a single source entry: if it is an existing file, read image
    /// names from it; otherwise return the entry itself as an image name.
    fn resolve_entry(entry: &str) -> anyhow::Result<Vec<String>> {
        let path = std::path::Path::new(entry);
        if path.is_file() {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("read image list {}: {e}", path.display()))?;
            Ok(text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(String::from)
                .collect())
        } else {
            Ok(vec![entry.to_string()])
        }
    }

    /// Resolve `sources` into a flat, deduplicated list of image names.
    pub fn all_image_names(&self) -> anyhow::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in &self.sources {
            names.extend(Self::resolve_entry(entry)?);
        }
        names.sort();
        names.dedup();
        Ok(names)
    }

}

impl PoolConfig {
    /// Validate invariants that must hold for the config to be usable.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_sandboxes == 0 {
            anyhow::bail!("max_sandboxes must be at least 1");
        }
        Ok(())
    }
}

/// Builder for PoolConfig.
#[derive(Debug, Default)]
pub struct PoolConfigBuilder {
    max_sandboxes: Option<usize>,
    images: Option<ImagesConfig>,
    overlay_dir: Option<PathBuf>,
    overlay_driver: Option<OverlayDriver>,
    resource_limits: Option<ResourceLimits>,
    seccomp_policy: Option<SeccompPolicy>,
    default_timeout: Option<Duration>,
    default_env: Option<HashMap<String, String>>,
    mount_host_resolv_conf: Option<bool>,
}

impl PoolConfigBuilder {
    pub fn max_sandboxes(mut self, n: usize) -> Self {
        self.max_sandboxes = Some(n);
        self
    }

    pub fn images(mut self, images: ImagesConfig) -> Self {
        self.images = Some(images);
        self
    }

    pub fn overlay_dir<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.overlay_dir = Some(path.into());
        self
    }

    pub fn overlay_driver(mut self, driver: OverlayDriver) -> Self {
        self.overlay_driver = Some(driver);
        self
    }

    pub fn resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.resource_limits = Some(limits);
        self
    }

    pub fn seccomp_policy(mut self, policy: SeccompPolicy) -> Self {
        self.seccomp_policy = Some(policy);
        self
    }

    pub fn default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = Some(timeout);
        self
    }

    pub fn env<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        self.default_env
            .get_or_insert_with(HashMap::new)
            .insert(key.into(), value.into());
        self
    }

    pub fn mount_host_resolv_conf(mut self, enable: bool) -> Self {
        self.mount_host_resolv_conf = Some(enable);
        self
    }

    pub fn build(self) -> anyhow::Result<PoolConfig> {
        let config = PoolConfig {
            max_sandboxes: self.max_sandboxes.unwrap_or_else(default_max_sandboxes),
            images: self
                .images
                .ok_or_else(|| anyhow::anyhow!("images config is required"))?,
            overlay_dir: self
                .overlay_dir
                .unwrap_or_else(super::PoolConfig::default_overlay_dir),
            overlay_driver: self.overlay_driver.unwrap_or_default(),
            resource_limits: self.resource_limits.unwrap_or_default(),
            seccomp_policy: self.seccomp_policy.unwrap_or_default(),
            default_timeout: self.default_timeout.unwrap_or_else(default_timeout),
            default_env: self.default_env.unwrap_or_default(),
            mount_host_resolv_conf: self.mount_host_resolv_conf.unwrap_or(true),
            session_log_dir: None,
        };
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::{PoolConfig, ResourceLimits};
    use std::path::PathBuf;

    fn test_images_toml() -> &'static str {
        r#"
[images]
sources = ["ubuntu:22.04"]
layers_dir = "/tmp/test_layers"
"#
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        let input = format!(
            r#"
overlay_dir = "/tmp/overlays"
unexpected_field = true
{}"#,
            test_images_toml()
        );
        let error =
            toml::from_str::<PoolConfig>(&input).expect_err("unknown config fields must be rejected");
        let msg = error.to_string();
        assert!(msg.contains("unknown field"), "unexpected error: {msg}");
    }

    #[test]
    fn resource_limits_defaults_are_sane() {
        let limits = ResourceLimits::default();
        assert_eq!(limits.memory_max, "4G");
        assert_eq!(limits.pids_max, 2048);
        assert_eq!(limits.max_open_files, 2048);
        assert_eq!(limits.rlimit_as_multiplier, 2);
        assert!(limits.max_file_size.is_none());
        assert!(limits.io_max.is_none());
    }

    #[test]
    fn resource_limits_deserialize_with_new_fields() {
        let limits: ResourceLimits = toml::from_str(
            r#"
memory_max = "4G"
pids_max = 512
max_file_size = "1G"
max_open_files = 2048
rlimit_as_multiplier = 3
"#,
        )
        .expect("should deserialize with rlimit fields");

        assert_eq!(limits.memory_max, "4G");
        assert_eq!(limits.pids_max, 512);
        assert_eq!(limits.max_file_size, Some("1G".to_string()));
        assert_eq!(limits.max_open_files, 2048);
        assert_eq!(limits.rlimit_as_multiplier, 3);
    }

    #[test]
    fn resource_limits_deserialize_omitted_new_fields() {
        let limits: ResourceLimits = toml::from_str(
            r#"
memory_max = "1G"
"#,
        )
        .expect("should deserialize with defaults for rlimit fields");

        assert_eq!(limits.memory_max, "1G");
        assert_eq!(limits.max_open_files, 2048);
        assert_eq!(limits.rlimit_as_multiplier, 2);
        assert!(limits.max_file_size.is_none());
    }

    #[test]
    fn pool_config_with_images_section() {
        let config: PoolConfig = toml::from_str(&format!(
            r#"
overlay_dir = "/tmp/overlays"

[resource_limits]
memory_max = "8G"
max_file_size = "2G"
rlimit_as_multiplier = 4

{}
"#,
            test_images_toml()
        ))
        .expect("pool config should accept images section");

        assert_eq!(config.resource_limits.memory_max, "8G");
        assert_eq!(config.images.sources, vec!["ubuntu:22.04"]);
        assert_eq!(config.images.layers_dir, PathBuf::from("/tmp/test_layers"));
    }

    #[test]
    fn images_config_defaults() {
        let config: PoolConfig = toml::from_str(&format!(
            r#"
overlay_dir = "/tmp/overlays"
{}
"#,
            test_images_toml()
        ))
        .expect("pool config should parse");

        assert_eq!(config.images.docker, "docker");
        assert_eq!(config.images.pull_concurrency, 8);
    }
}

pub fn serialize_duration<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let secs = duration.as_secs();
    if secs >= 3600 && secs % 3600 == 0 {
        serializer.serialize_str(&format!("{}h", secs / 3600))
    } else if secs >= 60 && secs % 60 == 0 {
        serializer.serialize_str(&format!("{}m", secs / 60))
    } else {
        serializer.serialize_str(&format!("{secs}s"))
    }
}

pub fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    parse_duration(&s).map_err(serde::de::Error::custom)
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix('s') {
        num.trim()
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|e| e.to_string())
    } else if let Some(num) = s.strip_suffix('m') {
        num.trim()
            .parse::<u64>()
            .map(|m| Duration::from_secs(m * 60))
            .map_err(|e| e.to_string())
    } else if let Some(num) = s.strip_suffix('h') {
        num.trim()
            .parse::<u64>()
            .map(|h| Duration::from_secs(h * 3600))
            .map_err(|e| e.to_string())
    } else {
        s.parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|e| e.to_string())
    }
}

mod duration_serde {
    pub use super::{deserialize_duration as deserialize, serialize_duration as serialize};
}

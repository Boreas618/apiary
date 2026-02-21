//! Configuration schema definitions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Main configuration for the sandbox pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    /// Number of sandboxes in the pool.
    pub pool_size: usize,

    /// Path to the base rootfs image (lower layer for OverlayFS).
    pub base_image: PathBuf,

    /// Directory to store overlay layers.
    pub overlay_dir: PathBuf,

    /// Resource limits for each sandbox.
    #[serde(default)]
    pub resource_limits: ResourceLimits,

    /// seccomp policy configuration.
    #[serde(default)]
    pub seccomp_policy: SeccompPolicy,

    /// Network policy.
    #[serde(default)]
    pub network_policy: NetworkPolicy,

    /// Default timeout for tasks.
    #[serde(default = "default_timeout", with = "humantime_serde")]
    pub default_timeout: Duration,

    /// Default working directory inside sandbox.
    #[serde(default = "default_workdir")]
    pub default_workdir: PathBuf,

    /// Default environment variables for all tasks.
    #[serde(default)]
    pub default_env: HashMap<String, String>,
}

fn default_timeout() -> Duration {
    Duration::from_secs(300)
}

fn default_workdir() -> PathBuf {
    PathBuf::from("/workspace")
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            pool_size: 10,
            base_image: PathBuf::from("./rootfs"),
            overlay_dir: PathBuf::from("./overlays"),
            resource_limits: ResourceLimits::default(),
            seccomp_policy: SeccompPolicy::default(),
            network_policy: NetworkPolicy::default(),
            default_timeout: default_timeout(),
            default_workdir: default_workdir(),
            default_env: HashMap::new(),
        }
    }
}

/// Resource limits for cgroups v2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Maximum memory in bytes (e.g., "2G", "512M").
    #[serde(default = "default_memory_max")]
    pub memory_max: String,

    /// CPU quota (e.g., "100000 100000" for 100% of 1 CPU).
    #[serde(default = "default_cpu_max")]
    pub cpu_max: String,

    /// Maximum number of PIDs.
    #[serde(default = "default_pids_max")]
    pub pids_max: u64,

    /// I/O limits (device major:minor rbps=N wbps=N).
    #[serde(default)]
    pub io_max: Option<String>,
}

fn default_memory_max() -> String {
    "2G".to_string()
}

fn default_cpu_max() -> String {
    "100000 100000".to_string()
}

fn default_pids_max() -> u64 {
    256
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            memory_max: default_memory_max(),
            cpu_max: default_cpu_max(),
            pids_max: default_pids_max(),
            io_max: None,
        }
    }
}

/// seccomp policy configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    true
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

/// Network policy configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum NetworkPolicy {
    /// No network access (most secure).
    #[default]
    None,

    /// Shared host network with seccomp filtering.
    SharedHost,

    /// Isolated network namespace (more complex setup).
    Isolated,
}

/// Builder for PoolConfig.
#[derive(Debug, Default)]
pub struct PoolConfigBuilder {
    pool_size: Option<usize>,
    base_image: Option<PathBuf>,
    overlay_dir: Option<PathBuf>,
    resource_limits: Option<ResourceLimits>,
    seccomp_policy: Option<SeccompPolicy>,
    network_policy: Option<NetworkPolicy>,
    default_timeout: Option<Duration>,
    default_workdir: Option<PathBuf>,
    default_env: Option<HashMap<String, String>>,
}

impl PoolConfigBuilder {
    pub fn pool_size(mut self, size: usize) -> Self {
        self.pool_size = Some(size);
        self
    }

    pub fn base_image<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.base_image = Some(path.into());
        self
    }

    pub fn overlay_dir<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.overlay_dir = Some(path.into());
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

    pub fn network_policy(mut self, policy: NetworkPolicy) -> Self {
        self.network_policy = Some(policy);
        self
    }

    pub fn default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = Some(timeout);
        self
    }

    pub fn default_workdir<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.default_workdir = Some(path.into());
        self
    }

    pub fn env<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        self.default_env
            .get_or_insert_with(HashMap::new)
            .insert(key.into(), value.into());
        self
    }

    pub fn build(self) -> anyhow::Result<PoolConfig> {
        let base_image = self
            .base_image
            .ok_or_else(|| anyhow::anyhow!("base_image is required"))?;

        Ok(PoolConfig {
            pool_size: self.pool_size.unwrap_or(10),
            base_image,
            overlay_dir: self
                .overlay_dir
                .unwrap_or_else(super::PoolConfig::default_overlay_dir),
            resource_limits: self.resource_limits.unwrap_or_default(),
            seccomp_policy: self.seccomp_policy.unwrap_or_default(),
            network_policy: self.network_policy.unwrap_or_default(),
            default_timeout: self.default_timeout.unwrap_or_else(default_timeout),
            default_workdir: self.default_workdir.unwrap_or_else(default_workdir),
            default_env: self.default_env.unwrap_or_default(),
        })
    }
}

mod humantime_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("{}s", duration.as_secs()))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
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
}

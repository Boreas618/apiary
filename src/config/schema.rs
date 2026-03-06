//! Configuration schema definitions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::sandbox::overlay::OverlayDriver;

/// Main configuration for the sandbox pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    /// Minimum number of sandboxes (created at startup, never scaled below).
    #[serde(default = "default_min_sandboxes")]
    pub min_sandboxes: usize,

    /// Maximum number of sandboxes (hard ceiling for auto-scaling).
    #[serde(default = "default_max_sandboxes")]
    pub max_sandboxes: usize,

    /// Number of sandboxes to create per scale-up event.
    #[serde(default = "default_scale_up_step")]
    pub scale_up_step: usize,

    /// How long an excess sandbox (above min) can be idle before removal.
    #[serde(default = "default_idle_timeout", with = "duration_serde")]
    pub idle_timeout: Duration,

    /// Minimum interval between scaling events to prevent thrashing.
    #[serde(default = "default_cooldown", with = "duration_serde")]
    pub cooldown: Duration,

    /// Path to the base rootfs image (lower layer for OverlayFS).
    pub base_image: PathBuf,

    /// Directory to store overlay layers.
    pub overlay_dir: PathBuf,

    /// Overlay driver to use ("auto", "kernel_overlay", or "fuse_overlayfs").
    #[serde(default)]
    pub overlay_driver: OverlayDriver,

    /// Resource limits for each sandbox.
    #[serde(default)]
    pub resource_limits: ResourceLimits,

    /// Whether seccomp filtering is enabled. Off by default; enable with --seccomp.
    #[serde(default)]
    pub enable_seccomp: bool,

    /// seccomp policy configuration (only applies when enable_seccomp is true).
    #[serde(default)]
    pub seccomp_policy: SeccompPolicy,

    /// Default timeout for tasks.
    #[serde(default = "default_timeout", with = "duration_serde")]
    pub default_timeout: Duration,

    /// Default working directory inside sandbox.
    #[serde(default = "default_workdir")]
    pub default_workdir: PathBuf,

    /// Default environment variables for all tasks.
    #[serde(default)]
    pub default_env: HashMap<String, String>,
}

fn default_min_sandboxes() -> usize {
    10
}

fn default_max_sandboxes() -> usize {
    40
}

fn default_scale_up_step() -> usize {
    2
}

fn default_idle_timeout() -> Duration {
    Duration::from_secs(300)
}

fn default_cooldown() -> Duration {
    Duration::from_secs(30)
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
            min_sandboxes: default_min_sandboxes(),
            max_sandboxes: default_max_sandboxes(),
            scale_up_step: default_scale_up_step(),
            idle_timeout: default_idle_timeout(),
            cooldown: default_cooldown(),
            base_image: PathBuf::from("./rootfs"),
            overlay_dir: PathBuf::from("./overlays"),
            overlay_driver: OverlayDriver::default(),
            resource_limits: ResourceLimits::default(),
            enable_seccomp: false,
            seccomp_policy: SeccompPolicy::default(),
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

impl PoolConfig {
    /// Validate invariants that must hold for the config to be usable.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.min_sandboxes == 0 {
            anyhow::bail!("min_sandboxes must be at least 1");
        }
        if self.max_sandboxes < self.min_sandboxes {
            anyhow::bail!(
                "max_sandboxes ({}) must be >= min_sandboxes ({})",
                self.max_sandboxes,
                self.min_sandboxes
            );
        }
        if self.scale_up_step == 0 {
            anyhow::bail!("scale_up_step must be at least 1");
        }
        Ok(())
    }

    /// Return a new config with seccomp enabled/disabled.
    pub fn with_seccomp_enabled(mut self, enabled: bool) -> Self {
        self.enable_seccomp = enabled;
        self
    }

    /// Return a new config with adjusted pool bounds (validates invariants).
    pub fn with_pool_bounds(mut self, min: usize, max: usize) -> anyhow::Result<Self> {
        self.min_sandboxes = min;
        self.max_sandboxes = max;
        self.validate()?;
        Ok(self)
    }
}

/// Builder for PoolConfig.
#[derive(Debug, Default)]
pub struct PoolConfigBuilder {
    min_sandboxes: Option<usize>,
    max_sandboxes: Option<usize>,
    scale_up_step: Option<usize>,
    idle_timeout: Option<Duration>,
    cooldown: Option<Duration>,
    base_image: Option<PathBuf>,
    overlay_dir: Option<PathBuf>,
    overlay_driver: Option<OverlayDriver>,
    resource_limits: Option<ResourceLimits>,
    enable_seccomp: Option<bool>,
    seccomp_policy: Option<SeccompPolicy>,
    default_timeout: Option<Duration>,
    default_workdir: Option<PathBuf>,
    default_env: Option<HashMap<String, String>>,
}

impl PoolConfigBuilder {
    pub fn min_sandboxes(mut self, n: usize) -> Self {
        self.min_sandboxes = Some(n);
        self
    }

    pub fn max_sandboxes(mut self, n: usize) -> Self {
        self.max_sandboxes = Some(n);
        self
    }

    pub fn scale_up_step(mut self, n: usize) -> Self {
        self.scale_up_step = Some(n);
        self
    }

    pub fn idle_timeout(mut self, d: Duration) -> Self {
        self.idle_timeout = Some(d);
        self
    }

    pub fn cooldown(mut self, d: Duration) -> Self {
        self.cooldown = Some(d);
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

    pub fn overlay_driver(mut self, driver: OverlayDriver) -> Self {
        self.overlay_driver = Some(driver);
        self
    }

    pub fn resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.resource_limits = Some(limits);
        self
    }

    pub fn enable_seccomp(mut self, enabled: bool) -> Self {
        self.enable_seccomp = Some(enabled);
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

        let min_sandboxes = self.min_sandboxes.unwrap_or_else(default_min_sandboxes);
        if min_sandboxes == 0 {
            anyhow::bail!("min_sandboxes must be at least 1");
        }

        let max_sandboxes = self.max_sandboxes.unwrap_or_else(default_max_sandboxes);
        if max_sandboxes < min_sandboxes {
            anyhow::bail!(
                "max_sandboxes ({max_sandboxes}) must be >= min_sandboxes ({min_sandboxes})"
            );
        }

        Ok(PoolConfig {
            min_sandboxes,
            max_sandboxes,
            scale_up_step: self.scale_up_step.unwrap_or_else(default_scale_up_step),
            idle_timeout: self.idle_timeout.unwrap_or_else(default_idle_timeout),
            cooldown: self.cooldown.unwrap_or_else(default_cooldown),
            base_image,
            overlay_dir: self
                .overlay_dir
                .unwrap_or_else(super::PoolConfig::default_overlay_dir),
            overlay_driver: self.overlay_driver.unwrap_or_default(),
            resource_limits: self.resource_limits.unwrap_or_default(),
            enable_seccomp: self.enable_seccomp.unwrap_or(false),
            seccomp_policy: self.seccomp_policy.unwrap_or_default(),
            default_timeout: self.default_timeout.unwrap_or_else(default_timeout),
            default_workdir: self.default_workdir.unwrap_or_else(default_workdir),
            default_env: self.default_env.unwrap_or_default(),
        })
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

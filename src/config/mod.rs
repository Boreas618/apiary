//! Configuration management for the sandbox pool.

mod schema;

pub use schema::{PoolConfig, PoolConfigBuilder, ResourceLimits, SeccompPolicy};
pub use crate::sandbox::overlay::OverlayDriver;

use std::path::Path;

impl PoolConfig {
    /// Load configuration from a TOML file.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: PoolConfig = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Save configuration to a TOML file.
    pub fn save_to_file(&self, path: &Path) -> anyhow::Result<()> {
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Get the default config file path.
    pub fn default_config_path() -> std::path::PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("apiary")
            .join("config.toml")
    }

    /// Get the default overlay directory path.
    pub fn default_overlay_dir() -> std::path::PathBuf {
        if let Ok(dir) = std::env::var("APIARY_OVERLAY_DIR") {
            return std::path::PathBuf::from(dir);
        }
        dirs::data_local_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("apiary")
            .join("overlays")
    }

    /// Create a new builder.
    pub fn builder() -> PoolConfigBuilder {
        PoolConfigBuilder::default()
    }
}

fn dirs_config_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })
}

fn dirs_data_local_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
}

mod dirs {
    pub fn config_dir() -> Option<std::path::PathBuf> {
        super::dirs_config_dir()
    }

    pub fn data_local_dir() -> Option<std::path::PathBuf> {
        super::dirs_data_local_dir()
    }
}

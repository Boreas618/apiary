//! Configuration management for the sandbox pool.

mod overlay;
mod schema;

pub use overlay::OverlayDriver;
pub use schema::{PoolConfig, PoolConfigBuilder, ResourceLimits, SeccompPolicy};

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
        xdg_config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("apiary")
            .join("config.toml")
    }

    /// Get the default overlay directory path.
    pub fn default_overlay_dir() -> std::path::PathBuf {
        if let Ok(dir) = std::env::var("APIARY_OVERLAY_DIR") {
            return std::path::PathBuf::from(dir);
        }
        xdg_data_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("apiary")
            .join("overlays")
    }

    /// Create a new builder.
    pub fn builder() -> PoolConfigBuilder {
        PoolConfigBuilder::default()
    }
}

fn xdg_config_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
}

fn xdg_data_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
}

#[cfg(test)]
mod tests {
    use super::PoolConfig;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        saved: Vec<(String, Option<OsString>)>,
    }

    impl EnvGuard {
        fn set(vars: &[(&str, Option<&str>)]) -> Self {
            let saved = vars
                .iter()
                .map(|(key, _)| ((*key).to_string(), std::env::var_os(key)))
                .collect::<Vec<_>>();

            for (key, value) in vars {
                match value {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }

            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(value) => unsafe { std::env::set_var(&key, value) },
                    None => unsafe { std::env::remove_var(&key) },
                }
            }
        }
    }

    #[test]
    fn default_config_path_prefers_xdg_config_home() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let _env = EnvGuard::set(&[
            ("XDG_CONFIG_HOME", Some("/tmp/apiary-config-home")),
            ("HOME", Some("/tmp/ignored-home")),
        ]);

        assert_eq!(
            PoolConfig::default_config_path(),
            PathBuf::from("/tmp/apiary-config-home/apiary/config.toml")
        );
    }

    #[test]
    fn default_overlay_dir_prefers_explicit_override() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let _env = EnvGuard::set(&[
            ("APIARY_OVERLAY_DIR", Some("/tmp/apiary-overlay-override")),
            ("XDG_DATA_HOME", Some("/tmp/ignored-data-home")),
            ("HOME", Some("/tmp/ignored-home")),
        ]);

        assert_eq!(
            PoolConfig::default_overlay_dir(),
            PathBuf::from("/tmp/apiary-overlay-override")
        );
    }

    #[test]
    fn default_overlay_dir_falls_back_to_xdg_data_home() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let _env = EnvGuard::set(&[
            ("APIARY_OVERLAY_DIR", None),
            ("XDG_DATA_HOME", Some("/tmp/apiary-data-home")),
            ("HOME", Some("/tmp/ignored-home")),
        ]);

        assert_eq!(
            PoolConfig::default_overlay_dir(),
            PathBuf::from("/tmp/apiary-data-home/apiary/overlays")
        );
    }
}

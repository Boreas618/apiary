//! OverlayFS management for sandbox filesystems.
//!
//! This module provides functions to setup and manage OverlayFS mounts
//! for sandbox isolation. Each sandbox has:
//! - A shared read-only lower layer (base image)
//! - A private upper layer for writable files
//! - A work directory for OverlayFS internals
//! - A merged view that the sandbox sees
//!
//! Note: This module is only functional on Linux.

use std::path::{Path, PathBuf};

use super::SandboxError;

/// OverlayFS mount configuration.
#[derive(Debug, Clone)]
pub struct OverlayConfig {
    /// Path to the lower (read-only) layer.
    pub lower: PathBuf,
    /// Path to the upper (writable) layer.
    pub upper: PathBuf,
    /// Path to the work directory.
    pub work: PathBuf,
    /// Path to the merged mount point.
    pub merged: PathBuf,
}

impl OverlayConfig {
    /// Create a new OverlayConfig for a sandbox.
    pub fn new(sandbox_id: &str, base_image: &Path, overlay_base: &Path) -> Self {
        let sandbox_dir = overlay_base.join(sandbox_id);
        Self {
            lower: base_image.to_path_buf(),
            upper: sandbox_dir.join("upper"),
            work: sandbox_dir.join("work"),
            merged: sandbox_dir.join("merged"),
        }
    }

    /// Get the options string for the mount command.
    pub fn mount_options(&self) -> String {
        format!(
            "lowerdir={},upperdir={},workdir={}",
            self.lower.display(),
            self.upper.display(),
            self.work.display()
        )
    }
}

// Linux-specific implementations
#[cfg(target_os = "linux")]
mod linux_impl {
    use super::*;
    use nix::mount::{mount, umount2, MntFlags, MsFlags};

    /// Setup an OverlayFS mount.
    pub fn setup_overlay(
        merged: &Path,
        upper: &Path,
        work: &Path,
        lower: &Path,
    ) -> Result<(), SandboxError> {
        std::fs::create_dir_all(upper)
            .map_err(|e| SandboxError::OverlaySetup(format!("failed to create upper dir: {e}")))?;
        std::fs::create_dir_all(work)
            .map_err(|e| SandboxError::OverlaySetup(format!("failed to create work dir: {e}")))?;
        std::fs::create_dir_all(merged)
            .map_err(|e| SandboxError::OverlaySetup(format!("failed to create merged dir: {e}")))?;

        if !lower.exists() {
            return Err(SandboxError::OverlaySetup(format!(
                "lower layer does not exist: {}",
                lower.display()
            )));
        }

        let options = format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            upper.display(),
            work.display()
        );

        mount(
            Some("overlay"),
            merged,
            Some("overlay"),
            MsFlags::empty(),
            Some(options.as_str()),
        )
        .map_err(|e| SandboxError::OverlaySetup(format!("failed to mount overlay: {e}")))
    }

    /// Unmount an overlay filesystem.
    pub fn unmount_overlay(merged: &Path) -> Result<(), SandboxError> {
        umount2(merged, MntFlags::MNT_DETACH)
            .map_err(|e| SandboxError::OverlaySetup(format!("failed to unmount overlay: {e}")))
    }

    /// Setup essential filesystem mounts inside the overlay.
    pub fn setup_essential_mounts(root: &Path) -> Result<(), SandboxError> {
        // Mount /proc
        let proc_path = root.join("proc");
        std::fs::create_dir_all(&proc_path).ok();
        mount(
            Some("proc"),
            &proc_path,
            Some("proc"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            None::<&str>,
        )
        .map_err(|e| SandboxError::OverlaySetup(format!("failed to mount /proc: {e}")))?;

        // Mount /sys (read-only)
        let sys_path = root.join("sys");
        std::fs::create_dir_all(&sys_path).ok();
        mount(
            Some("sysfs"),
            &sys_path,
            Some("sysfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC | MsFlags::MS_RDONLY,
            None::<&str>,
        )
        .map_err(|e| SandboxError::OverlaySetup(format!("failed to mount /sys: {e}")))?;

        // Mount /dev (minimal)
        let dev_path = root.join("dev");
        std::fs::create_dir_all(&dev_path).ok();
        mount(
            Some("tmpfs"),
            &dev_path,
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_STRICTATIME,
            Some("mode=755,size=65536k"),
        )
        .map_err(|e| SandboxError::OverlaySetup(format!("failed to mount /dev: {e}")))?;

        create_device_nodes(&dev_path)?;

        // Mount /dev/pts
        let pts_path = dev_path.join("pts");
        std::fs::create_dir_all(&pts_path).ok();
        mount(
            Some("devpts"),
            &pts_path,
            Some("devpts"),
            MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
            Some("newinstance,ptmxmode=0666,mode=620"),
        )
        .map_err(|e| SandboxError::OverlaySetup(format!("failed to mount /dev/pts: {e}")))?;

        // Mount /dev/shm
        let shm_path = dev_path.join("shm");
        std::fs::create_dir_all(&shm_path).ok();
        mount(
            Some("tmpfs"),
            &shm_path,
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            Some("mode=1777,size=65536k"),
        )
        .map_err(|e| SandboxError::OverlaySetup(format!("failed to mount /dev/shm: {e}")))?;

        // Mount /tmp
        let tmp_path = root.join("tmp");
        std::fs::create_dir_all(&tmp_path).ok();
        mount(
            Some("tmpfs"),
            &tmp_path,
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_STRICTATIME,
            Some("mode=1777,size=1g"),
        )
        .map_err(|e| SandboxError::OverlaySetup(format!("failed to mount /tmp: {e}")))
    }

    fn create_device_nodes(dev_path: &Path) -> Result<(), SandboxError> {
        use nix::sys::stat::{makedev, mknod, Mode, SFlag};
        use std::os::unix::fs::symlink;

        let devices = [
            ("null", 0o666, 1, 3),
            ("zero", 0o666, 1, 5),
            ("full", 0o666, 1, 7),
            ("random", 0o666, 1, 8),
            ("urandom", 0o666, 1, 9),
            ("tty", 0o666, 5, 0),
        ];

        for (name, mode, major, minor) in devices {
            let path = dev_path.join(name);
            let dev = makedev(major, minor);
            let _ = mknod(&path, SFlag::S_IFCHR, Mode::from_bits_truncate(mode), dev);
        }

        let symlinks = [
            ("stdin", "/proc/self/fd/0"),
            ("stdout", "/proc/self/fd/1"),
            ("stderr", "/proc/self/fd/2"),
            ("fd", "/proc/self/fd"),
        ];

        for (name, target) in symlinks {
            let path = dev_path.join(name);
            let _ = symlink(target, &path);
        }

        let ptmx_path = dev_path.join("ptmx");
        let _ = symlink("pts/ptmx", &ptmx_path);

        Ok(())
    }
}

// Re-export Linux implementations
#[cfg(target_os = "linux")]
pub use linux_impl::*;

// Stub implementations for non-Linux
#[cfg(not(target_os = "linux"))]
pub fn setup_overlay(
    merged: &Path,
    upper: &Path,
    work: &Path,
    lower: &Path,
) -> Result<(), SandboxError> {
    // Create directories anyway for testing
    std::fs::create_dir_all(upper)
        .map_err(|e| SandboxError::OverlaySetup(format!("failed to create upper dir: {e}")))?;
    std::fs::create_dir_all(work)
        .map_err(|e| SandboxError::OverlaySetup(format!("failed to create work dir: {e}")))?;
    std::fs::create_dir_all(merged)
        .map_err(|e| SandboxError::OverlaySetup(format!("failed to create merged dir: {e}")))?;

    if !lower.exists() {
        return Err(SandboxError::OverlaySetup(format!(
            "lower layer does not exist: {}",
            lower.display()
        )));
    }

    // On non-Linux, we just pretend it worked (for development/testing)
    tracing::warn!("OverlayFS is only available on Linux; using stub implementation");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn unmount_overlay(_merged: &Path) -> Result<(), SandboxError> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn setup_essential_mounts(_root: &Path) -> Result<(), SandboxError> {
    tracing::warn!("Essential mounts are only available on Linux");
    Ok(())
}

/// Clear the upper layer of an overlay.
pub fn clear_upper_layer(upper: &Path) -> Result<(), SandboxError> {
    if !upper.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(upper)
        .map_err(|e| SandboxError::OverlaySetup(format!("failed to read upper dir: {e}")))?
    {
        let entry = entry
            .map_err(|e| SandboxError::OverlaySetup(format!("failed to read dir entry: {e}")))?;
        let path = entry.path();

        if path.is_dir() {
            std::fs::remove_dir_all(&path).map_err(|e| {
                SandboxError::OverlaySetup(format!("failed to remove dir {}: {e}", path.display()))
            })?;
        } else {
            std::fs::remove_file(&path).map_err(|e| {
                SandboxError::OverlaySetup(format!("failed to remove file {}: {e}", path.display()))
            })?;
        }
    }

    Ok(())
}

/// Get the disk usage of an overlay upper layer.
pub fn get_upper_layer_size(upper: &Path) -> Result<u64, SandboxError> {
    fn dir_size(path: &Path) -> std::io::Result<u64> {
        let mut size = 0;
        if path.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    size += dir_size(&path)?;
                } else {
                    size += entry.metadata()?.len();
                }
            }
        }
        Ok(size)
    }

    dir_size(upper).map_err(|e| {
        SandboxError::OverlaySetup(format!("failed to calculate upper layer size: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_overlay_config() {
        let base = PathBuf::from("/base");
        let overlay_base = PathBuf::from("/overlays");
        let config = OverlayConfig::new("test-sandbox", &base, &overlay_base);

        assert_eq!(config.lower, base);
        assert_eq!(config.upper, PathBuf::from("/overlays/test-sandbox/upper"));
        assert_eq!(config.work, PathBuf::from("/overlays/test-sandbox/work"));
        assert_eq!(
            config.merged,
            PathBuf::from("/overlays/test-sandbox/merged")
        );
    }

    #[test]
    fn test_mount_options() {
        let config = OverlayConfig {
            lower: PathBuf::from("/lower"),
            upper: PathBuf::from("/upper"),
            work: PathBuf::from("/work"),
            merged: PathBuf::from("/merged"),
        };

        let options = config.mount_options();
        assert!(options.contains("lowerdir=/lower"));
        assert!(options.contains("upperdir=/upper"));
        assert!(options.contains("workdir=/work"));
    }

    #[test]
    fn test_clear_upper_layer() {
        let tmp = TempDir::new().unwrap();
        let upper = tmp.path().join("upper");
        std::fs::create_dir_all(&upper).unwrap();

        std::fs::write(upper.join("file1.txt"), "content1").unwrap();
        std::fs::create_dir_all(upper.join("subdir")).unwrap();
        std::fs::write(upper.join("subdir/file2.txt"), "content2").unwrap();

        clear_upper_layer(&upper).unwrap();

        assert!(upper.exists());
        assert!(std::fs::read_dir(&upper).unwrap().next().is_none());
    }
}

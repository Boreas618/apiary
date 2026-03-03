//! OverlayFS management for sandbox filesystems.
//!
//! This module provides functions to setup and manage OverlayFS mounts
//! for sandbox isolation. Each sandbox has:
//! - A shared read-only lower layer (base image)
//! - A private upper layer for writable files
//! - A work directory for OverlayFS internals
//! - A merged view that the sandbox sees
//!
//! ## Overlay Drivers
//!
//! Three strategies are available for mounting the overlay filesystem:
//!
//! - **`Auto`** (default): Tries kernel overlayfs first, then falls back to
//!   `fuse-overlayfs`. This is the recommended setting for rootless operation.
//! - **`KernelOverlay`**: Uses the in-kernel overlayfs. Requires either real
//!   root or a kernel >= 5.11 with unprivileged user namespaces. On systems
//!   with AppArmor or SELinux the mount may still be denied.
//! - **`FuseOverlayfs`**: Uses the `fuse-overlayfs` userspace binary. Works
//!   reliably in rootless mode on virtually all distributions. Requires
//!   `fuse-overlayfs` to be installed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::SandboxError;

/// Which overlay implementation to use.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverlayDriver {
    /// Try kernel overlayfs first, fall back to fuse-overlayfs.
    #[default]
    Auto,
    /// Force kernel overlayfs (may require privileges or kernel >= 5.11).
    KernelOverlay,
    /// Force fuse-overlayfs (requires the binary to be installed).
    FuseOverlayfs,
}

/// Tracks which overlay implementation is actively in use for a mount.
/// Needed to select the correct unmount strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveOverlay {
    KernelOverlay,
    FuseOverlayfs,
    /// Non-Linux stub (no real mount).
    Stub,
}

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

// ---------------------------------------------------------------------------
// Linux-specific implementations
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod linux_impl {
    use super::*;
    use crate::sandbox::namespace;
    use nix::mount::{mount, umount2, MntFlags, MsFlags};

    /// Setup an OverlayFS mount using the configured driver strategy.
    ///
    /// Returns the [`ActiveOverlay`] variant that was successfully used,
    /// so callers can pass it to [`unmount_overlay`] later.
    pub fn setup_overlay(
        merged: &Path,
        upper: &Path,
        work: &Path,
        lower: &Path,
        driver: &OverlayDriver,
    ) -> Result<ActiveOverlay, SandboxError> {
        create_overlay_dirs(upper, work, merged)?;

        if !lower.exists() {
            return Err(SandboxError::OverlaySetup(format!(
                "lower layer does not exist: {}",
                lower.display()
            )));
        }

        let rootless = namespace::is_rootless_mode();

        match driver {
            OverlayDriver::Auto => {
                match try_kernel_overlay(merged, upper, work, lower, rootless) {
                    Ok(()) => {
                        tracing::info!("Using kernel overlayfs");
                        Ok(ActiveOverlay::KernelOverlay)
                    }
                    Err(kernel_err) => {
                        tracing::warn!(
                            %kernel_err,
                            "Kernel overlay mount failed; trying fuse-overlayfs"
                        );
                        try_fuse_overlayfs(merged, upper, work, lower).map_err(|fuse_err| {
                            SandboxError::OverlaySetup(format!(
                                "All overlay drivers failed.\n\
                                 Kernel overlay: {kernel_err}\n\
                                 fuse-overlayfs: {fuse_err}\n\n\
                                 Possible fixes:\n\
                                 - Install fuse-overlayfs: apt install fuse-overlayfs (Debian/Ubuntu) \
                                   or dnf install fuse-overlayfs (Fedora/RHEL)\n\
                                 - Use a kernel >= 5.11 with unprivileged overlay support\n\
                                 - Check AppArmor/SELinux policies that may block overlay mounts\n\
                                 - Run as root (not recommended)"
                            ))
                        })?;
                        tracing::info!("Using fuse-overlayfs (kernel overlay unavailable)");
                        Ok(ActiveOverlay::FuseOverlayfs)
                    }
                }
            }
            OverlayDriver::KernelOverlay => {
                try_kernel_overlay(merged, upper, work, lower, rootless)?;
                tracing::info!("Using kernel overlayfs (forced)");
                Ok(ActiveOverlay::KernelOverlay)
            }
            OverlayDriver::FuseOverlayfs => {
                try_fuse_overlayfs(merged, upper, work, lower)?;
                tracing::info!("Using fuse-overlayfs (forced)");
                Ok(ActiveOverlay::FuseOverlayfs)
            }
        }
    }

    fn try_kernel_overlay(
        merged: &Path,
        upper: &Path,
        work: &Path,
        lower: &Path,
        rootless: bool,
    ) -> Result<(), SandboxError> {
        let mut options = format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            upper.display(),
            work.display()
        );

        // In user namespaces (rootless), xattrs must be stored in the
        // user.* namespace instead of trusted.*, which requires the
        // userxattr mount option (Linux 5.11+).
        if rootless {
            options.push_str(",userxattr");
        }

        mount(
            Some("overlay"),
            merged,
            Some("overlay"),
            MsFlags::empty(),
            Some(options.as_str()),
        )
        .map_err(|e| SandboxError::OverlaySetup(format!("kernel overlay mount failed: {e}")))
    }

    fn try_fuse_overlayfs(
        merged: &Path,
        upper: &Path,
        work: &Path,
        lower: &Path,
    ) -> Result<(), SandboxError> {
        let options = format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            upper.display(),
            work.display()
        );

        let output = std::process::Command::new("fuse-overlayfs")
            .arg("-o")
            .arg(&options)
            .arg(merged)
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    SandboxError::OverlaySetup(
                        "fuse-overlayfs binary not found. \
                         Install it with: apt install fuse-overlayfs (Debian/Ubuntu) \
                         or dnf install fuse-overlayfs (Fedora/RHEL)"
                            .to_string(),
                    )
                } else {
                    SandboxError::OverlaySetup(format!("failed to execute fuse-overlayfs: {e}"))
                }
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SandboxError::OverlaySetup(format!(
                "fuse-overlayfs exited with {}: {}",
                output.status,
                stderr.trim()
            )));
        }

        Ok(())
    }

    /// Unmount an overlay filesystem.
    pub fn unmount_overlay(merged: &Path, active: &ActiveOverlay) -> Result<(), SandboxError> {
        match active {
            ActiveOverlay::KernelOverlay => {
                umount2(merged, MntFlags::MNT_DETACH).map_err(|e| {
                    SandboxError::OverlaySetup(format!("failed to unmount kernel overlay: {e}"))
                })
            }
            ActiveOverlay::FuseOverlayfs => unmount_fuse_overlay(merged),
            ActiveOverlay::Stub => Ok(()),
        }
    }

    fn unmount_fuse_overlay(merged: &Path) -> Result<(), SandboxError> {
        // Prefer umount2 (works inside our mount namespace for FUSE mounts)
        if umount2(merged, MntFlags::MNT_DETACH).is_ok() {
            return Ok(());
        }

        // Fall back to fusermount3 / fusermount
        for cmd in ["fusermount3", "fusermount"] {
            if let Ok(status) = std::process::Command::new(cmd)
                .arg("-u")
                .arg(merged)
                .status()
            {
                if status.success() {
                    return Ok(());
                }
            }
        }

        Err(SandboxError::OverlaySetup(format!(
            "failed to unmount fuse-overlayfs at {}",
            merged.display()
        )))
    }

    // -----------------------------------------------------------------------
    // Two-phase mount setup
    //
    // Phase 1 (setup_dev_mounts): BEFORE pivot_root, while host /dev/* is
    //   still accessible.  Creates a tmpfs at {new_root}/dev and bind-mounts
    //   host device nodes into it, avoiding mknod (which requires CAP_MKNOD
    //   and fails inside user namespaces).
    //
    // Phase 2 (setup_post_pivot_mounts): AFTER pivot_root.  Mounts /proc,
    //   /sys, /dev/pts, /dev/shm, /tmp.  Non-critical mounts (/sys, /dev/pts)
    //   are best-effort so the sandbox still starts in restricted environments.
    // -----------------------------------------------------------------------

    /// Phase 1: Mount /dev before pivot_root.
    ///
    /// Must be called BEFORE `pivot_root` while host `/dev/*` nodes are still
    /// reachable, so we can bind-mount them into the sandbox instead of using
    /// `mknod` (which requires `CAP_MKNOD`).
    pub fn setup_dev_mounts(new_root: &Path) -> Result<(), SandboxError> {
        let dev_path = new_root.join("dev");
        std::fs::create_dir_all(&dev_path).ok();

        mount(
            Some("tmpfs"),
            &dev_path,
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_STRICTATIME,
            Some("mode=755,size=65536k"),
        )
        .map_err(|e| SandboxError::OverlaySetup(format!("failed to mount tmpfs on /dev: {e}")))?;

        bind_or_create_device_nodes(&dev_path);
        create_dev_symlinks(&dev_path);

        Ok(())
    }

    /// Phase 2: Mount pseudo-filesystems after pivot_root.
    ///
    /// `/dev` is already set up by [`setup_dev_mounts`].
    /// Non-critical mounts are best-effort with per-mount diagnostics.
    pub fn setup_post_pivot_mounts(root: &Path) -> Result<(), SandboxError> {
        // /proc — critical for nearly every program
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

        // /sys — best-effort; some user namespace configs deny sysfs
        let sys_path = root.join("sys");
        std::fs::create_dir_all(&sys_path).ok();
        if let Err(e) = mount(
            Some("sysfs"),
            &sys_path,
            Some("sysfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC | MsFlags::MS_RDONLY,
            None::<&str>,
        ) {
            tracing::warn!("sysfs mount failed ({e}); /sys will be unavailable");
        }

        // /dev/pts — best-effort; devpts may be denied in user namespaces
        let pts_path = root.join("dev/pts");
        std::fs::create_dir_all(&pts_path).ok();
        if let Err(e) = mount(
            Some("devpts"),
            &pts_path,
            Some("devpts"),
            MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
            Some("newinstance,ptmxmode=0666,mode=620"),
        ) {
            tracing::warn!("devpts mount failed ({e}); PTY allocation unavailable");
        }

        // /dev/shm
        let shm_path = root.join("dev/shm");
        std::fs::create_dir_all(&shm_path).ok();
        if let Err(e) = mount(
            Some("tmpfs"),
            &shm_path,
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            Some("mode=1777,size=65536k"),
        ) {
            tracing::warn!("failed to mount /dev/shm ({e})");
        }

        // /tmp
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

    /// Bind-mount host device nodes, falling back to mknod if bind-mount
    /// fails. Silently skips devices that cannot be created by either method.
    fn bind_or_create_device_nodes(dev_path: &Path) {
        let devices: &[(&str, u64, u64)] = &[
            ("null", 1, 3),
            ("zero", 1, 5),
            ("full", 1, 7),
            ("random", 1, 8),
            ("urandom", 1, 9),
            ("tty", 5, 0),
        ];

        for &(name, major, minor) in devices {
            let host_path = Path::new("/dev").join(name);
            let target = dev_path.join(name);

            // Create an empty regular file as the bind-mount target
            if std::fs::File::create(&target).is_err() {
                tracing::warn!("cannot create mount point for /dev/{name}");
                continue;
            }

            // Strategy 1: bind-mount from host (works without CAP_MKNOD)
            if host_path.exists() {
                if mount(
                    Some(&*host_path),
                    &target,
                    None::<&str>,
                    MsFlags::MS_BIND,
                    None::<&str>,
                )
                .is_ok()
                {
                    continue;
                }
            }

            // Strategy 2: mknod (needs CAP_MKNOD — works when running as
            // real root, but not inside a user namespace)
            use nix::sys::stat::{makedev, mknod, Mode, SFlag};
            let _ = std::fs::remove_file(&target);
            let dev = makedev(major, minor);
            if mknod(
                &target,
                SFlag::S_IFCHR,
                Mode::from_bits_truncate(0o666),
                dev,
            )
            .is_err()
            {
                tracing::warn!(
                    "/dev/{name} unavailable (bind-mount and mknod both failed)"
                );
            }
        }
    }

    fn create_dev_symlinks(dev_path: &Path) {
        use std::os::unix::fs::symlink;

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

        let _ = symlink("pts/ptmx", dev_path.join("ptmx"));
    }

    fn create_overlay_dirs(upper: &Path, work: &Path, merged: &Path) -> Result<(), SandboxError> {
        std::fs::create_dir_all(upper)
            .map_err(|e| SandboxError::OverlaySetup(format!("failed to create upper dir: {e}")))?;
        std::fs::create_dir_all(work)
            .map_err(|e| SandboxError::OverlaySetup(format!("failed to create work dir: {e}")))?;
        std::fs::create_dir_all(merged)
            .map_err(|e| SandboxError::OverlaySetup(format!("failed to create merged dir: {e}")))?;
        Ok(())
    }
}

// Re-export Linux implementations
#[cfg(target_os = "linux")]
pub use linux_impl::*;

// ---------------------------------------------------------------------------
// Non-Linux stubs
// ---------------------------------------------------------------------------
#[cfg(not(target_os = "linux"))]
pub fn setup_overlay(
    merged: &Path,
    upper: &Path,
    work: &Path,
    lower: &Path,
    _driver: &OverlayDriver,
) -> Result<ActiveOverlay, SandboxError> {
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

    tracing::warn!("OverlayFS is only available on Linux; using stub implementation");
    Ok(ActiveOverlay::Stub)
}

#[cfg(not(target_os = "linux"))]
pub fn unmount_overlay(_merged: &Path, _active: &ActiveOverlay) -> Result<(), SandboxError> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn setup_dev_mounts(_new_root: &Path) -> Result<(), SandboxError> {
    tracing::warn!("Device mounts are only available on Linux");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn setup_post_pivot_mounts(_root: &Path) -> Result<(), SandboxError> {
    tracing::warn!("Pseudo-filesystem mounts are only available on Linux");
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
                SandboxError::OverlaySetup(format!(
                    "failed to remove file {}: {e}",
                    path.display()
                ))
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

    #[test]
    fn test_overlay_driver_default() {
        let driver = OverlayDriver::default();
        assert_eq!(driver, OverlayDriver::Auto);
    }

    #[test]
    fn test_overlay_driver_serde() {
        let json = serde_json::to_string(&OverlayDriver::FuseOverlayfs).unwrap();
        assert_eq!(json, "\"fuse_overlayfs\"");

        let parsed: OverlayDriver = serde_json::from_str("\"auto\"").unwrap();
        assert_eq!(parsed, OverlayDriver::Auto);

        let parsed: OverlayDriver = serde_json::from_str("\"kernel_overlay\"").unwrap();
        assert_eq!(parsed, OverlayDriver::KernelOverlay);
    }
}

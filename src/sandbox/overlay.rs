//! OverlayFS management for sandbox filesystems.
//!
//! This module provides functions to setup and manage OverlayFS mounts
//! for sandbox isolation. Each sandbox has:
//! - One or more shared read-only lower layers (base image layers)
//! - A private upper layer for writable files
//! - A work directory for OverlayFS internals
//! - A merged view that the sandbox sees
//!
//! ## Multi-lowerdir
//!
//! OverlayFS supports stacking multiple read-only lower directories via
//! `lowerdir=top:...:base`. This module accepts lower dirs in
//! **bottom-to-top order** (base first) to match Docker convention, and
//! reverses them when formatting the mount option.
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

use nix::mount::{mount, umount2, MntFlags, MsFlags};

use super::SandboxError;
use crate::config::OverlayDriver;
use crate::sandbox::namespace;

/// Tracks which overlay implementation is actively in use for a mount.
/// Needed to select the correct unmount strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveOverlay {
    KernelOverlay,
    FuseOverlayfs,
}

/// Setup an OverlayFS mount using the configured driver strategy.
///
/// `lower_dirs` lists layer directories in **bottom-to-top** order (base
/// first, topmost last) — matching Docker convention.  The function
/// reverses the order when formatting the OverlayFS `lowerdir=` mount
/// option, which expects top-to-bottom.
///
/// Returns the [`ActiveOverlay`] variant that was successfully used,
/// so callers can pass it to [`unmount_overlay`] later.
pub fn setup_overlay(
    merged: &Path,
    upper: &Path,
    work: &Path,
    lower_dirs: &[PathBuf],
    driver: &OverlayDriver,
) -> Result<ActiveOverlay, SandboxError> {
    create_overlay_dirs(upper, work, merged)?;

    if lower_dirs.is_empty() {
        return Err(SandboxError::OverlaySetup(
            "no lower directories provided".to_string(),
        ));
    }

    for (i, lower) in lower_dirs.iter().enumerate() {
        if !lower.exists() {
            return Err(SandboxError::OverlaySetup(format!(
                "lower layer {} does not exist: {}",
                i,
                lower.display()
            )));
        }
    }

    // Pre-flight diagnostics on all lower layers and the upper layer.
    for (i, lower) in lower_dirs.iter().enumerate() {
        diagnose_layer_health(&format!("lower[{i}]"), lower);
    }
    diagnose_layer_health("upper", upper);

    // Ensure every lower layer is on a filesystem with xattr support.
    // Layers that lack it are copied to a per-layer cache directory.
    let overlay_base = work
        .parent()
        .unwrap_or(work)
        .parent()
        .unwrap_or(work);

    let mut effective_lowers: Vec<PathBuf> = Vec::with_capacity(lower_dirs.len());
    let mut _tmpfs_guards: Vec<PathBuf> = Vec::new();

    for (i, lower) in lower_dirs.iter().enumerate() {
        if !check_xattr_support(lower) {
            let cache_dir = overlay_base.join(format!(".rootfs-cache-{i}"));
            let cached = ensure_xattr_lower(lower, &cache_dir)?;
            _tmpfs_guards.push(cache_dir);
            effective_lowers.push(cached);
        } else {
            effective_lowers.push(lower.clone());
        }
    }

    // OverlayFS lowerdir: topmost (highest priority) first, base last.
    let lowerdir_str = effective_lowers
        .iter()
        .rev()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(":");

    tracing::debug!(
        num_layers = lower_dirs.len(),
        lowerdir = %lowerdir_str,
        "formatted multi-lowerdir mount option"
    );

    let rootless = namespace::is_rootless_mode();

    match driver {
        OverlayDriver::Auto => {
            match try_kernel_overlay(merged, upper, work, &lowerdir_str, rootless) {
                Ok(()) => {
                    tracing::info!("Using kernel overlayfs");
                    Ok(ActiveOverlay::KernelOverlay)
                }
                Err(kernel_err) => {
                    tracing::debug!(
                        %kernel_err,
                        "Kernel overlay mount failed; trying fuse-overlayfs"
                    );
                    try_fuse_overlayfs(merged, upper, work, &lowerdir_str).map_err(|fuse_err| {
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
                    validate_overlay_mount(merged).map_err(|e| {
                        let _ = unmount_fuse_overlay(merged);
                        SandboxError::OverlaySetup(format!(
                            "fuse-overlayfs mounted but validation failed: {e}"
                        ))
                    })?;
                    tracing::info!("Using fuse-overlayfs (kernel overlay unavailable)");
                    Ok(ActiveOverlay::FuseOverlayfs)
                }
            }
        }
        OverlayDriver::KernelOverlay => {
            try_kernel_overlay(merged, upper, work, &lowerdir_str, rootless)?;
            tracing::info!("Using kernel overlayfs (forced)");
            Ok(ActiveOverlay::KernelOverlay)
        }
        OverlayDriver::FuseOverlayfs => {
            try_fuse_overlayfs(merged, upper, work, &lowerdir_str)?;
            validate_overlay_mount(merged)?;
            tracing::info!("Using fuse-overlayfs (forced)");
            Ok(ActiveOverlay::FuseOverlayfs)
        }
    }
}

fn try_kernel_overlay(
    merged: &Path,
    upper: &Path,
    work: &Path,
    lowerdir_str: &str,
    _rootless: bool,
) -> Result<(), SandboxError> {
    let base_options = format!(
        "lowerdir={lowerdir_str},upperdir={},workdir={}",
        upper.display(),
        work.display()
    );

    // Always try with userxattr first. It stores overlay metadata in user.*
    // instead of trusted.*, which is required when:
    //   - rootless mode (user namespaces cannot access trusted.* xattrs)
    //   - nested overlayfs (Docker overlay2 — trusted.* not supported by overlayfs)
    //   - filesystems that don't support trusted.* xattrs (Lustre, NFSv3,
    //     NFSv4.0/4.1 with limited xattr support, etc.)
    // On kernel 5.11+ (required anyway for rootless overlayfs) this is always safe.
    let options_with_userxattr = format!("{base_options},userxattr");
    let mount_result = mount(
        Some("overlay"),
        merged,
        Some("overlay"),
        MsFlags::empty(),
        Some(options_with_userxattr.as_str()),
    );

    match mount_result {
        Ok(()) => {
            if let Err(e) = validate_overlay_mount(merged) {
                tracing::warn!(%e, "overlay+userxattr mounted but validation failed; unmounting");
                let _ = umount2(merged, MntFlags::MNT_DETACH);
            } else {
                tracing::info!("kernel overlayfs mounted with userxattr");
                return Ok(());
            }
        }
        Err(e) => {
            tracing::info!(
                error = %e,
                "kernel overlayfs mount with userxattr failed; trying without"
            );
        }
    }

    // Fallback: try without userxattr (for older kernels or other edge cases)
    let fallback_result = mount(
        Some("overlay"),
        merged,
        Some("overlay"),
        MsFlags::empty(),
        Some(base_options.as_str()),
    );

    match fallback_result {
        Ok(()) => {
            if let Err(e) = validate_overlay_mount(merged) {
                let _ = umount2(merged, MntFlags::MNT_DETACH);
                return Err(SandboxError::OverlaySetup(format!(
                    "kernel overlay mount failed validation both with and without userxattr. \
                     Last error: {e}\n\
                     The underlying filesystem does not support the xattrs overlayfs needs. \
                     Set overlay_driver = \"fuse_overlayfs\" in the config, or install \
                     fuse-overlayfs (apt install fuse-overlayfs)."
                )));
            }
            tracing::info!("kernel overlayfs mounted without userxattr");
            Ok(())
        }
        Err(e) => Err(SandboxError::OverlaySetup(format!(
            "kernel overlay mount failed: {e}"
        ))),
    }
}

fn validate_overlay_mount(merged: &Path) -> Result<(), SandboxError> {
    // Call metadata() directly on known paths — do NOT use exists() because it
    // silently returns false when stat() returns EOPNOTSUPP, hiding the real error.
    let test_targets = ["bin/sh", "usr/bin/env", "etc/passwd"];
    for relpath in &test_targets {
        let full = merged.join(relpath);
        match std::fs::metadata(&full) {
            Ok(_) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::ENOENT)
                || e.raw_os_error() == Some(libc::ENOTDIR) =>
            {
                continue;
            }
            Err(e) => {
                return Err(SandboxError::OverlaySetup(format!(
                    "overlay mount at {} appears broken — stat({}) failed: {e} (errno {:?}). \
                     The underlying filesystem likely does not support the xattrs overlayfs needs. \
                     Set overlay_driver = \"fuse_overlayfs\" in the config.",
                    merged.display(),
                    full.display(),
                    e.raw_os_error(),
                )));
            }
        }
    }
    Ok(())
}

/// Check if a path's filesystem supports user.* xattrs.
fn check_xattr_support(path: &Path) -> bool {
    let test_path = if path.is_dir() {
        // Try a known file inside the directory first
        let candidates = ["bin/sh", "usr/bin/env", "etc/passwd"];
        candidates
            .iter()
            .map(|r| path.join(r))
            .find(|p| {
                // Use metadata() not exists() — exists() hides EOPNOTSUPP
                std::fs::metadata(p).is_ok()
            })
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    };

    let c_path = match std::ffi::CString::new(test_path.to_string_lossy().as_bytes()) {
        Ok(p) => p,
        Err(_) => return true, // assume OK if path is weird
    };

    let ret = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            b"user.overlay.test\0".as_ptr() as *const libc::c_char,
            std::ptr::null_mut(),
            0,
        )
    };

    if ret < 0 {
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EOPNOTSUPP) => {
                tracing::info!(
                    path = %test_path.display(),
                    "filesystem does not support xattrs"
                );
                return false;
            }
            // ENODATA means "attribute not found" — xattrs ARE supported
            _ => return true,
        }
    }
    true
}

/// Copy the rootfs to a directory with xattr support when the original
/// filesystem doesn't support them (NFSv3, NFSv4.0/4.1 with limited xattr,
/// Lustre, etc.). NFSv4.2 fully supports xattrs but is not yet common.
///
/// Strategy (in order of preference):
///   1. Reuse existing cache (with marker file)
///   2. Copy to `cache_dir` on the overlay volume — uses disk, not RAM
///   3. Mount tmpfs at `cache_dir` and copy — uses RAM, always works
fn ensure_xattr_lower(lower: &Path, cache_dir: &Path) -> Result<std::path::PathBuf, SandboxError> {
    let marker = cache_dir.join(".apiary-rootfs-cached");

    // Reuse existing cache
    if marker.exists() && check_xattr_support(cache_dir) {
        tracing::info!(cache = %cache_dir.display(), "reusing rootfs cache (xattr OK)");
        return Ok(cache_dir.to_path_buf());
    }

    tracing::info!(
        lower = %lower.display(),
        "lower layer on NFS/Lustre without xattr support; creating local rootfs cache"
    );

    std::fs::create_dir_all(cache_dir).map_err(|e| {
        SandboxError::OverlaySetup(format!(
            "failed to create rootfs cache dir {}: {e}",
            cache_dir.display()
        ))
    })?;

    // Strategy 1: try using the directory as-is (Docker volume is often ext4/xfs)
    let need_tmpfs = !check_xattr_support_on_dir(cache_dir);

    if need_tmpfs {
        // Strategy 2: mount tmpfs over the cache dir
        tracing::info!(
            "overlay volume also lacks xattr support; mounting tmpfs at {}",
            cache_dir.display()
        );
        mount(
            Some("tmpfs"),
            cache_dir,
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            Some("size=4g"),
        )
        .map_err(|e| {
            SandboxError::OverlaySetup(format!(
                "failed to mount tmpfs at {} for rootfs cache: {e}. \
                 As a workaround, copy the rootfs to a local filesystem with xattr support \
                 and pass that path with --base-image.",
                cache_dir.display()
            ))
        })?;
        tracing::info!("rootfs cache will use tmpfs (RAM-backed)");
    } else {
        tracing::info!(
            "rootfs cache will use overlay volume at {} (disk-backed)",
            cache_dir.display()
        );
    }

    copy_rootfs(lower, cache_dir)?;

    // Write marker so subsequent sandboxes reuse the cache
    let _ = std::fs::write(&marker, if need_tmpfs { "tmpfs" } else { "disk" });

    let cache_size = std::process::Command::new("du")
        .args(["-sh", &cache_dir.to_string_lossy()])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    tracing::info!(
        cache = %cache_dir.display(),
        size = %cache_size,
        backing = if need_tmpfs { "tmpfs (RAM)" } else { "disk" },
        "rootfs cached successfully"
    );

    Ok(cache_dir.to_path_buf())
}

/// Test xattr support on a directory by writing and removing a test xattr.
fn check_xattr_support_on_dir(dir: &Path) -> bool {
    let test_file = dir.join(".apiary-xattr-test");
    if std::fs::write(&test_file, b"test").is_err() {
        return false;
    }
    let result = check_xattr_support(&test_file);
    let _ = std::fs::remove_file(&test_file);
    result
}

fn copy_rootfs(src: &Path, dst: &Path) -> Result<(), SandboxError> {
    // Use rsync if available (handles partial failures gracefully),
    // fall back to cp -a which may fail on NFS root_squash files.
    let (program, args) = if which_exists("rsync") {
        ("rsync", vec!["-a", "--ignore-errors", "--quiet"])
    } else {
        ("cp", vec!["-a", "--force"])
    };

    let output = std::process::Command::new(program)
        .args(&args)
        .arg(format!("{}/.", src.display()))
        .arg(dst)
        .output()
        .map_err(|e| {
            SandboxError::OverlaySetup(format!("failed to run {program} for rootfs cache: {e}"))
        })?;

    // cp/rsync may return non-zero due to NFS root_squash preventing reads on
    // files like /etc/shadow, /root, lock files etc. These are non-essential
    // for sandbox operation. Accept the copy if key files are present.
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            program,
            exit_code = ?output.status.code(),
            "rootfs copy had errors (likely NFS root_squash on restricted files). \
             Verifying essential files were copied."
        );
        if !stderr.is_empty() {
            for line in stderr.lines().take(5) {
                tracing::warn!("  {line}");
            }
            let total_errors = stderr.lines().count();
            if total_errors > 5 {
                tracing::warn!("  ... and {} more errors", total_errors - 5);
            }
        }
    }

    // Verify essential files exist in the cache
    let essential = ["bin/sh", "usr", "lib"];
    let mut found = 0;
    for name in &essential {
        let p = dst.join(name);
        if std::fs::metadata(&p).is_ok() {
            found += 1;
        }
    }
    if found == 0 {
        return Err(SandboxError::OverlaySetup(format!(
            "rootfs copy to {} failed — no essential files (bin/sh, usr, lib) found. \
             Check permissions on the source rootfs at {}.",
            dst.display(),
            src.display(),
        )));
    }

    if !check_xattr_support(dst) {
        return Err(SandboxError::OverlaySetup(
            "rootfs cache still lacks xattr support after copy (unexpected)".to_string(),
        ));
    }

    Ok(())
}

fn which_exists(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Log the health of an overlay layer's underlying filesystem.
fn diagnose_layer_health(label: &str, path: &Path) {
    // stat the directory itself
    match std::fs::metadata(path) {
        Ok(_) => {}
        Err(e) => {
            tracing::error!(
                layer = label,
                path = %path.display(),
                error = %e,
                raw_errno = ?e.raw_os_error(),
                "overlay layer directory stat FAILED"
            );
            return;
        }
    }

    // stat a known file inside the layer
    let test_files = ["bin/sh", "usr/bin/env", "etc/passwd"];
    for relpath in &test_files {
        let full = path.join(relpath);
        match std::fs::metadata(&full) {
            Ok(meta) => {
                use std::os::unix::fs::MetadataExt;
                tracing::trace!(
                    layer = label,
                    path = %full.display(),
                    mode = format_args!("{:#o}", meta.mode()),
                    size = meta.len(),
                    dev = format_args!("{:#x}", meta.dev()),
                    "overlay {label} layer: direct stat OK"
                );

                // check statfs to identify filesystem type
                let c_path = std::ffi::CString::new(
                    full.to_string_lossy().as_bytes(),
                )
                .ok();
                if let Some(ref c_path) = c_path {
                    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
                    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } == 0 {
                        let fs_name = match buf.f_type {
                            0xEF53 => "ext2/ext3/ext4",
                            0x794c7630 => "overlayfs",
                            0x01021994 => "tmpfs",
                            0x9123683e => "btrfs",
                            0x58465342 => "xfs",
                            0x65735546 => "fuse",
                            0x6969 => "nfs",
                            0x0BD00BD0 => "lustre",
                            _ => "unknown",
                        };
                        tracing::trace!(
                            layer = label,
                            f_type = format_args!("{:#x}", buf.f_type),
                            fs_name,
                            "overlay {label} layer filesystem"
                        );
                    }
                }

                // check xattr support
                let c_full = std::ffi::CString::new(
                    full.to_string_lossy().as_bytes(),
                )
                .ok();
                if let Some(ref c_full) = c_full {
                    let mut xattr_buf = [0u8; 256];
                    let ret = unsafe {
                        libc::getxattr(
                            c_full.as_ptr(),
                            b"user.overlay.opaque\0".as_ptr() as *const libc::c_char,
                            xattr_buf.as_mut_ptr() as *mut libc::c_void,
                            xattr_buf.len(),
                        )
                    };
                    if ret < 0 {
                        let err = std::io::Error::last_os_error();
                        tracing::trace!(
                            layer = label,
                            error = %err,
                            raw_errno = ?err.raw_os_error(),
                            "overlay {label} layer: getxattr(user.overlay.opaque) result \
                             (ENODATA=normal, EOPNOTSUPP=no xattr support)"
                        );
                    } else {
                        tracing::trace!(
                            layer = label,
                            "overlay {label} layer: getxattr(user.overlay.opaque) OK"
                        );
                    }
                }

                return;
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => continue,
            Err(e) => {
                tracing::error!(
                    layer = label,
                    path = %full.display(),
                    error = %e,
                    raw_errno = ?e.raw_os_error(),
                    "overlay {label} layer: direct stat on file FAILED (filesystem broken?)"
                );
                return;
            }
        }
    }
    // upper layers are expected to be empty initially — don't warn for them
    if label == "upper" {
        tracing::trace!(
            layer = label,
            path = %path.display(),
            "overlay {label} layer: empty (expected for fresh upper)"
        );
    } else {
        tracing::warn!(
            layer = label,
            path = %path.display(),
            "overlay {label} layer: no known test files found"
        );
    }
}

fn try_fuse_overlayfs(
    merged: &Path,
    upper: &Path,
    work: &Path,
    lowerdir_str: &str,
) -> Result<(), SandboxError> {
    let options = format!(
        "lowerdir={lowerdir_str},upperdir={},workdir={}",
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
        ActiveOverlay::KernelOverlay => umount2(merged, MntFlags::MNT_DETACH).map_err(|e| {
            SandboxError::OverlaySetup(format!("failed to unmount kernel overlay: {e}"))
        }),
        ActiveOverlay::FuseOverlayfs => unmount_fuse_overlay(merged),
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

// ---------------------------------------------------------------------------
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
// ---------------------------------------------------------------------------

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
            tracing::warn!("/dev/{name} unavailable (bind-mount and mknod both failed)");
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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

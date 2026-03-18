use std::path::Path;
use std::time::Duration;

use crate::config::{ResourceLimits, SeccompPolicy};
use crate::task::MountSpec;

use super::mounts::apply_task_mounts;
#[cfg(target_os = "linux")]
use super::seccomp;
use super::{cgroup, namespace, overlay, rlimits, SandboxError};

/// Configure the child process for sandbox isolation.
///
/// `debug_fd` is a dup'd copy of the parent's stderr, used for debug output
/// that bypasses the captured stderr pipe. Pass -1 to disable debug output.
/// `shell` is the binary that will be exec'd after this function returns,
/// used for pre-flight diagnostic checks.
pub(super) fn configure_task_process_linux(
    root: &Path,
    workdir: &Path,
    cgroup_path: Option<&Path>,
    seccomp_policy: &SeccompPolicy,
    resource_limits: &ResourceLimits,
    timeout: Duration,
    uid: Option<u32>,
    gid: Option<u32>,
    writable_mounts: &[MountSpec],
    readonly_mounts: &[MountSpec],
    debug_fd: i32,
    shell: &str,
) -> std::io::Result<()> {
    // NOTE: This function runs in a forked child before exec().
    // Only async-signal-safe functions may be called. In particular,
    // tracing/log macros, heap allocation, and lock acquisition are
    // forbidden. Use write_stderr_safe() / write_debug_safe() for diagnostics.
    use nix::mount::{mount, umount2, MntFlags, MsFlags};
    use nix::sched::{unshare, CloneFlags};

    write_debug_safe(debug_fd, b"[apiary:pre_exec] === sandbox process configuration started ===\n");

    // Step 1: setpgid — create a new process group
    write_debug_safe(debug_fd, b"[apiary:pre_exec] step 1/11: setpgid(0,0)\n");
    if unsafe { libc::setpgid(0, 0) } != 0 {
        let err = std::io::Error::last_os_error();
        write_debug_safe(debug_fd, b"[apiary:pre_exec] FAILED at step 1: setpgid\n");
        return Err(std::io::Error::other(format!(
            "step 1 setpgid(0,0) failed: {err}"
        )));
    }

    // Step 2: attach to cgroup (best-effort)
    write_debug_safe(debug_fd, b"[apiary:pre_exec] step 2/11: cgroup attach\n");
    if let Some(cgroup_path) = cgroup_path {
        if cgroup::add_process_to_cgroup(cgroup_path, std::process::id()).is_err() {
            write_debug_safe(debug_fd, b"[apiary:pre_exec] warning: failed to attach process to cgroup\n");
        }
    }

    // Step 3: apply rlimits (defense-in-depth: works even without cgroups)
    write_debug_safe(debug_fd, b"[apiary:pre_exec] step 3/11: apply rlimits\n");
    if rlimits::apply_rlimits(resource_limits, timeout).is_err() {
        write_debug_safe(debug_fd, b"[apiary:pre_exec] warning: failed to apply rlimits; continuing\n");
    }

    // Probe: stat a known file inside the overlay BEFORE any namespace changes
    let probe_path_in_root = root.join("bin/sh");
    probe_stat(debug_fd, b"BEFORE unshare", probe_path_in_root.as_path());

    // Step 4: unshare mount/IPC/UTS namespaces
    write_debug_safe(debug_fd, b"[apiary:pre_exec] step 4/11: unshare(NEWNS|NEWIPC|NEWUTS)\n");
    unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_NEWUTS)
        .map_err(|e| {
            write_debug_safe(debug_fd, b"[apiary:pre_exec] FAILED at step 4: unshare\n");
            std::io::Error::other(format!("step 4 unshare(NEWNS|NEWIPC|NEWUTS) failed: {e}"))
        })?;

    // Step 5: make mount namespace private
    write_debug_safe(debug_fd, b"[apiary:pre_exec] step 5/11: make_mount_private\n");
    namespace::make_mount_private().map_err(|e| {
        write_debug_safe(debug_fd, b"[apiary:pre_exec] FAILED at step 5: make_mount_private\n");
        sandbox_error_to_io(e)
    })?;

    // Probe: stat after unshare + make_mount_private
    probe_stat(debug_fd, b"AFTER unshare+private", probe_path_in_root.as_path());

    // Step 6: bind mount the overlay root onto itself
    write_debug_safe(debug_fd, b"[apiary:pre_exec] step 6/11: bind mount root\n");
    mount(
        Some(root),
        root,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| {
        write_debug_safe(debug_fd, b"[apiary:pre_exec] FAILED at step 6: bind mount root\n");
        std::io::Error::other(format!(
            "step 6 bind mount root ({}) failed: {e}",
            root.display()
        ))
    })?;

    // Probe: stat after bind mount
    probe_stat(debug_fd, b"AFTER bind mount", probe_path_in_root.as_path());

    // Step 7: apply task-specific bind mounts
    write_debug_safe(debug_fd, b"[apiary:pre_exec] step 7/11: apply task mounts\n");
    apply_task_mounts(root, writable_mounts, readonly_mounts).map_err(|e| {
        write_debug_safe(debug_fd, b"[apiary:pre_exec] FAILED at step 7: task mounts\n");
        std::io::Error::other(format!("step 7 apply_task_mounts failed: {e}"))
    })?;

    // Step 8: setup /dev (best-effort)
    write_debug_safe(debug_fd, b"[apiary:pre_exec] step 8/11: setup dev mounts\n");
    if overlay::setup_dev_mounts(root).is_err() {
        write_debug_safe(debug_fd, b"[apiary:pre_exec] warning: failed to setup /dev; continuing\n");
    }

    // Step 9: pivot_root into the sandbox filesystem
    write_debug_safe(debug_fd, b"[apiary:pre_exec] step 9/11: pivot_root\n");
    let put_old = root.join(".old_root");
    std::fs::create_dir_all(&put_old).map_err(|e| {
        write_debug_safe(debug_fd, b"[apiary:pre_exec] FAILED at step 9: create put_old dir\n");
        std::io::Error::other(format!(
            "step 9 create put_old dir ({}) failed: {e}",
            put_old.display()
        ))
    })?;
    namespace::pivot_root(root, &put_old).map_err(|e| {
        write_debug_safe(debug_fd, b"[apiary:pre_exec] FAILED at step 9: pivot_root\n");
        sandbox_error_to_io(e)
    })?;

    // Probe: stat AFTER pivot_root but BEFORE unmounting old root
    probe_stat(debug_fd, b"AFTER pivot_root, BEFORE umount", std::path::Path::new("/bin/sh"));

    umount2("/.old_root", MntFlags::MNT_DETACH).map_err(|e| {
        write_debug_safe(debug_fd, b"[apiary:pre_exec] FAILED at step 9: unmount old root\n");
        std::io::Error::other(format!("step 9 unmount /.old_root failed: {e}"))
    })?;
    let _ = std::fs::remove_dir_all("/.old_root");

    // Probe: stat AFTER unmounting old root
    probe_stat(debug_fd, b"AFTER umount old_root", std::path::Path::new("/bin/sh"));

    // Step 10: mount pseudo-filesystems + set working directory
    write_debug_safe(debug_fd, b"[apiary:pre_exec] step 10/11: post-pivot mounts + workdir\n");
    if overlay::setup_post_pivot_mounts(Path::new("/")).is_err() {
        write_debug_safe(debug_fd, b"[apiary:pre_exec] warning: failed to mount pseudo-filesystems; continuing\n");
    }

    let effective_workdir = if workdir.is_absolute() {
        workdir.to_path_buf()
    } else {
        Path::new("/").join(workdir)
    };
    std::fs::create_dir_all(&effective_workdir).map_err(|e| {
        write_debug_safe(debug_fd, b"[apiary:pre_exec] FAILED at step 10: create workdir\n");
        std::io::Error::other(format!(
            "step 10 create workdir ({}) failed: {e}",
            effective_workdir.display()
        ))
    })?;
    std::env::set_current_dir(&effective_workdir).map_err(|e| {
        write_debug_safe(debug_fd, b"[apiary:pre_exec] FAILED at step 10: chdir to workdir\n");
        std::io::Error::other(format!(
            "step 10 chdir to workdir ({}) failed: {e}",
            effective_workdir.display()
        ))
    })?;

    // Step 11: set uid/gid and apply seccomp
    write_debug_safe(debug_fd, b"[apiary:pre_exec] step 11/11: uid/gid + seccomp\n");
    if let Some(gid) = gid {
        if unsafe { libc::setgid(gid) } != 0 {
            let err = std::io::Error::last_os_error();
            write_debug_safe(debug_fd, b"[apiary:pre_exec] FAILED at step 11: setgid\n");
            return Err(std::io::Error::other(format!(
                "step 11 setgid({gid}) failed: {err}"
            )));
        }
    }
    if let Some(uid) = uid {
        if unsafe { libc::setuid(uid) } != 0 {
            let err = std::io::Error::last_os_error();
            write_debug_safe(debug_fd, b"[apiary:pre_exec] FAILED at step 11: setuid\n");
            return Err(std::io::Error::other(format!(
                "step 11 setuid({uid}) failed: {err}"
            )));
        }
    }

    #[cfg(target_os = "linux")]
    {
        if seccomp::set_no_new_privs().is_err() {
            write_debug_safe(
                debug_fd,
                b"[apiary:pre_exec] warning: failed to set no_new_privs; continuing without seccomp\n",
            );
        } else if seccomp::apply_seccomp_filter(seccomp_policy).is_err() {
            write_debug_safe(
                debug_fd,
                b"[apiary:pre_exec] warning: failed to apply seccomp filter; continuing without seccomp\n",
            );
        }
    }

    // Diagnostic: verify the exec target binary before returning to stdlib's execvp.
    // After pivot_root we're in the sandbox root — resolve and inspect the binary.
    write_debug_safe(debug_fd, b"[apiary:pre_exec] exec target diagnostics:\n");
    diagnose_exec_target(debug_fd, shell);

    write_debug_safe(debug_fd, b"[apiary:pre_exec] === sandbox process configuration complete (execvp next) ===\n");
    Ok(())
}

/// Write a message to stderr using async-signal-safe `libc::write`.
/// Safe to call from a `pre_exec` (post-fork, pre-exec) context.
pub(super) fn write_stderr_safe(msg: &[u8]) {
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            msg.as_ptr() as *const libc::c_void,
            msg.len(),
        );
    }
}

/// Write a debug message to a specific fd (typically the dup'd parent stderr).
/// No-op if fd < 0. Safe to call from a `pre_exec` context.
pub(super) fn write_debug_safe(fd: i32, msg: &[u8]) {
    if fd < 0 {
        return;
    }
    unsafe {
        libc::write(fd, msg.as_ptr() as *const libc::c_void, msg.len());
    }
}

/// Probe: try stat() on a path and log the result to debug_fd.
/// Used to pinpoint exactly when overlayfs operations start failing.
fn probe_stat(debug_fd: i32, label: &[u8], path: &std::path::Path) {
    if debug_fd < 0 {
        return;
    }
    let msg_prefix = format!(
        "[apiary:pre_exec] probe stat({}) @ ",
        path.display()
    );
    write_debug_safe(debug_fd, msg_prefix.as_bytes());
    write_debug_safe(debug_fd, label);

    match std::fs::metadata(path) {
        Ok(meta) => {
            use std::os::unix::fs::MetadataExt;
            let detail = format!(
                ": OK (mode={:#o}, size={}, ino={})\n",
                meta.mode(),
                meta.len(),
                meta.ino(),
            );
            write_debug_safe(debug_fd, detail.as_bytes());
        }
        Err(e) => {
            let detail = format!(": FAILED ({})\n", e);
            write_debug_safe(debug_fd, detail.as_bytes());
        }
    }
}

/// Pre-flight check: resolve the shell binary in PATH and log filesystem info.
/// All output goes to debug_fd (the parent's original stderr). Uses only
/// async-signal-safe-ish operations (the format! calls are technically not
/// signal-safe, but the existing code already relies on this being fine).
fn diagnose_exec_target(debug_fd: i32, shell: &str) {
    if debug_fd < 0 {
        return;
    }

    let resolved = if shell.contains('/') {
        // Absolute or relative path — use as-is
        Some(std::path::PathBuf::from(shell))
    } else {
        // Search PATH
        let path_var = std::env::var("PATH").unwrap_or_default();
        path_var.split(':').find_map(|dir| {
            let candidate = std::path::Path::new(dir).join(shell);
            if candidate.exists() {
                Some(candidate)
            } else {
                None
            }
        })
    };

    match resolved {
        None => {
            let msg = format!("[apiary:pre_exec]   shell={shell} NOT FOUND in PATH\n");
            write_debug_safe(debug_fd, msg.as_bytes());
        }
        Some(ref path) => {
            let msg = format!(
                "[apiary:pre_exec]   shell={shell} resolved to {}\n",
                path.display()
            );
            write_debug_safe(debug_fd, msg.as_bytes());

            // Check metadata
            match std::fs::metadata(path) {
                Ok(meta) => {
                    use std::os::unix::fs::MetadataExt;
                    let msg = format!(
                        "[apiary:pre_exec]   mode={:#o} size={} uid={} gid={} dev={:#x}\n",
                        meta.mode(),
                        meta.len(),
                        meta.uid(),
                        meta.gid(),
                        meta.dev(),
                    );
                    write_debug_safe(debug_fd, msg.as_bytes());

                    // Check if executable bit is set
                    if meta.mode() & 0o111 == 0 {
                        write_debug_safe(
                            debug_fd,
                            b"[apiary:pre_exec]   WARNING: no execute permission bits set!\n",
                        );
                    }
                }
                Err(e) => {
                    let msg = format!(
                        "[apiary:pre_exec]   metadata({}) failed: {e}\n",
                        path.display()
                    );
                    write_debug_safe(debug_fd, msg.as_bytes());
                }
            }

            // Check symlink target (bash is often a symlink)
            match std::fs::read_link(path) {
                Ok(target) => {
                    let msg = format!(
                        "[apiary:pre_exec]   symlink -> {}\n",
                        target.display()
                    );
                    write_debug_safe(debug_fd, msg.as_bytes());
                }
                Err(_) => {
                    write_debug_safe(debug_fd, b"[apiary:pre_exec]   (not a symlink)\n");
                }
            }

            // Check filesystem type via statfs
            let mut statfs_buf: libc::statfs = unsafe { std::mem::zeroed() };
            let c_path = std::ffi::CString::new(path.to_string_lossy().as_bytes()).ok();
            if let Some(ref c_path) = c_path {
                let ret = unsafe { libc::statfs(c_path.as_ptr(), &mut statfs_buf) };
                if ret == 0 {
                    let fs_type = statfs_buf.f_type;
                    let fs_name = match fs_type {
                        0x61756673 => "aufs",
                        0xEF53 => "ext2/ext3/ext4",
                        0x794c7630 => "overlayfs",
                        0x01021994 => "tmpfs",
                        0x9123683e => "btrfs",
                        0x58465342 => "xfs",
                        0x65735546 => "fuse",
                        _ => "unknown",
                    };
                    let msg = format!(
                        "[apiary:pre_exec]   filesystem: type={:#x} ({})\n",
                        fs_type, fs_name,
                    );
                    write_debug_safe(debug_fd, msg.as_bytes());
                } else {
                    write_debug_safe(debug_fd, b"[apiary:pre_exec]   statfs failed\n");
                }
            }

            // Try access(X_OK) — this is what execvp checks
            if let Some(ref c_path) = c_path {
                let ret = unsafe { libc::access(c_path.as_ptr(), libc::X_OK) };
                if ret == 0 {
                    write_debug_safe(debug_fd, b"[apiary:pre_exec]   access(X_OK) = OK\n");
                } else {
                    let err = std::io::Error::last_os_error();
                    let msg = format!(
                        "[apiary:pre_exec]   access(X_OK) FAILED: {err}\n"
                    );
                    write_debug_safe(debug_fd, msg.as_bytes());
                }
            }
        }
    }
}

fn sandbox_error_to_io(error: SandboxError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

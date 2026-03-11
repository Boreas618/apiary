use std::path::Path;
use std::time::Duration;

use crate::config::{ResourceLimits, SeccompPolicy};
use crate::task::MountSpec;

use super::mounts::apply_task_mounts;
#[cfg(target_os = "linux")]
use super::seccomp;
use super::{cgroup, namespace, overlay, rlimits, SandboxError};

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
) -> std::io::Result<()> {
    // NOTE: This function runs in a forked child before exec().
    // Only async-signal-safe functions may be called. In particular,
    // tracing/log macros, heap allocation, and lock acquisition are
    // forbidden. Use write_stderr_safe() for diagnostics.
    use nix::mount::{mount, umount2, MntFlags, MsFlags};
    use nix::sched::{unshare, CloneFlags};

    if unsafe { libc::setpgid(0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    if let Some(cgroup_path) = cgroup_path {
        if cgroup::add_process_to_cgroup(cgroup_path, std::process::id()).is_err() {
            write_stderr_safe(b"[apiary] warning: failed to attach process to cgroup\n");
        }
    }

    // Apply rlimits unconditionally (defense-in-depth: works even without cgroups).
    if rlimits::apply_rlimits(resource_limits, timeout).is_err() {
        write_stderr_safe(b"[apiary] warning: failed to apply rlimits; continuing\n");
    }

    unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_NEWUTS)
        .map_err(|e| std::io::Error::other(format!("failed to unshare task namespaces: {e}")))?;
    namespace::make_mount_private().map_err(sandbox_error_to_io)?;

    mount(
        Some(root),
        root,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| std::io::Error::other(format!("failed to bind mount new root: {e}")))?;

    apply_task_mounts(root, writable_mounts, readonly_mounts)?;

    if overlay::setup_dev_mounts(root).is_err() {
        write_stderr_safe(b"[apiary] warning: failed to setup /dev; continuing\n");
    }

    let put_old = root.join(".old_root");
    std::fs::create_dir_all(&put_old)?;
    namespace::pivot_root(root, &put_old).map_err(sandbox_error_to_io)?;
    umount2("/.old_root", MntFlags::MNT_DETACH)
        .map_err(|e| std::io::Error::other(format!("failed to unmount old root: {e}")))?;
    let _ = std::fs::remove_dir_all("/.old_root");

    if overlay::setup_post_pivot_mounts(Path::new("/")).is_err() {
        write_stderr_safe(b"[apiary] warning: failed to mount pseudo-filesystems; continuing\n");
    }

    let effective_workdir = if workdir.is_absolute() {
        workdir.to_path_buf()
    } else {
        Path::new("/").join(workdir)
    };
    std::fs::create_dir_all(&effective_workdir)?;
    std::env::set_current_dir(&effective_workdir)?;

    if let Some(gid) = gid {
        if unsafe { libc::setgid(gid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    if let Some(uid) = uid {
        if unsafe { libc::setuid(uid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    #[cfg(target_os = "linux")]
    {
        if seccomp::set_no_new_privs().is_err() {
            write_stderr_safe(
                b"[apiary] warning: failed to set no_new_privs; continuing without seccomp\n",
            );
        } else if seccomp::apply_seccomp_filter(seccomp_policy).is_err() {
            write_stderr_safe(
                b"[apiary] warning: failed to apply seccomp filter; continuing without seccomp\n",
            );
        }
    }

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

fn sandbox_error_to_io(error: SandboxError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

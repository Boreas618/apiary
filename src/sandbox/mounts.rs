use std::path::{Component, Path, PathBuf};

use nix::mount::{mount, MsFlags};

use crate::task::MountSpec;

pub(super) fn overlay_base_dir(root_path: &Path) -> Option<&Path> {
    let overlay_base = root_path.parent()?;
    if overlay_base == Path::new("/") {
        return None;
    }
    Some(overlay_base)
}

pub(super) fn apply_task_mounts(
    root: &Path,
    writable_mounts: &[MountSpec],
    readonly_mounts: &[MountSpec],
) -> std::io::Result<()> {
    for spec in writable_mounts {
        bind_mount_spec(root, spec, false)?;
    }
    for spec in readonly_mounts {
        bind_mount_spec(root, spec, true)?;
    }
    Ok(())
}

fn bind_mount_spec(root: &Path, spec: &MountSpec, readonly: bool) -> std::io::Result<()> {
    if !spec.source.exists() {
        return Err(std::io::Error::other(format!(
            "mount source does not exist: {}",
            spec.source.display()
        )));
    }

    let target = mount_target_path(root, &spec.dest)?;
    prepare_mount_target(&spec.source, &target)?;

    mount(
        Some(&spec.source),
        &target,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| {
        std::io::Error::other(format!(
            "failed to bind mount {} -> {}: {e}",
            spec.source.display(),
            spec.dest.display()
        ))
    })?;

    if readonly {
        mount(
            None::<&str>,
            &target,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(|e| {
            std::io::Error::other(format!(
                "failed to remount {} as read-only: {e}",
                spec.dest.display()
            ))
        })?;
    }

    Ok(())
}

fn mount_target_path(root: &Path, dest: &Path) -> std::io::Result<PathBuf> {
    if !dest.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "mount destination must be an absolute path inside the sandbox: {}",
                dest.display()
            ),
        ));
    }

    let relative = dest
        .strip_prefix("/")
        .expect("absolute paths have a root prefix");
    if relative.as_os_str().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "mount destination must not be the sandbox root",
        ));
    }
    if relative
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "mount destination must not contain parent traversal: {}",
                dest.display()
            ),
        ));
    }

    Ok(root.join(relative))
}

fn prepare_mount_target(source: &Path, target: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        std::fs::create_dir_all(target)?;
        return Ok(());
    }

    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("mount target has no parent directory: {}", target.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;

    if !target.exists() {
        std::fs::File::create(target)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{mount_target_path, overlay_base_dir};
    use std::path::{Path, PathBuf};

    #[test]
    fn overlay_base_dir_returns_parent_for_sandbox_root() {
        assert_eq!(
            overlay_base_dir(Path::new("/tmp/apiary/sandbox-1/merged")),
            Some(Path::new("/tmp/apiary/sandbox-1"))
        );
    }

    #[test]
    fn overlay_base_dir_rejects_filesystem_root() {
        assert_eq!(overlay_base_dir(Path::new("/")), None);
    }

    #[test]
    fn mount_target_path_requires_absolute_destinations() {
        let error = mount_target_path(Path::new("/sandbox"), Path::new("workspace"))
            .expect_err("relative mount destinations should be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn mount_target_path_rejects_sandbox_root_destination() {
        let error = mount_target_path(Path::new("/sandbox"), Path::new("/"))
            .expect_err("mounting over the sandbox root should be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn mount_target_path_rejects_parent_traversal() {
        let error = mount_target_path(Path::new("/sandbox"), Path::new("/workspace/../etc"))
            .expect_err("parent traversal should be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn mount_target_path_joins_absolute_destination_under_root() {
        assert_eq!(
            mount_target_path(Path::new("/sandbox"), Path::new("/workspace/project"))
                .expect("absolute sandbox mount destination should resolve"),
            PathBuf::from("/sandbox/workspace/project")
        );
    }
}

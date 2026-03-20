"""Docker image layer extraction with content-addressable caching.

Preserves Docker's layer structure by using ``docker save`` (not ``docker
export``), extracting each layer exactly once into a shared cache keyed by
the layer's SHA-256 diff ID.  Docker whiteout files (``.wh.*``) are
converted to OverlayFS format during extraction so the layer directories
can be passed directly as multiple ``lowerdir`` entries to OverlayFS.

Uses the Docker CLI (``docker inspect`` / ``docker save``) and stdlib
``tarfile`` — no ``docker`` Python SDK required.
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import shutil
import stat
import subprocess
import tarfile
import threading

logger = logging.getLogger(__name__)


class RootfsManager:
    """Manages a content-addressable cache of Docker image layers.

    Layers are stored once in ``{cache_dir}/.layers/{diff_id_hex}/`` and
    shared across all images that contain them.  The public entry point
    :meth:`ensure_layers` returns an ordered list of layer directories
    (base first, topmost last) suitable for OverlayFS multi-lowerdir.
    """

    def __init__(self, cache_dir: str = "/tmp/apiary_rootfs"):
        self._cache_dir = cache_dir
        self._layers_dir = os.path.join(cache_dir, ".layers")
        os.makedirs(self._layers_dir, exist_ok=True)

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def ensure_layers(self, image: str) -> list[str]:
        """Return an ordered list of layer directory paths for *image*.

        Layers are listed base-first (bottom of the stack) to topmost
        (highest priority), matching Docker convention.  The Rust side
        reverses the order when formatting the OverlayFS ``lowerdir=``
        mount option.

        Each layer is extracted at most once; subsequent images that share
        the same layer reuse the cached directory.
        """
        diff_ids = self._get_diff_ids(image)
        if not diff_ids:
            raise RuntimeError(
                f"docker inspect returned no layers for {image!r}"
            )

        missing = [d for d in diff_ids if not self._layer_cached(d)]
        if missing:
            logger.info(
                "%d/%d layers missing for %s — running docker save",
                len(missing),
                len(diff_ids),
                image,
            )
            self._extract_layers_from_save(image, diff_ids)
        else:
            logger.info(
                "All %d layers cached for %s", len(diff_ids), image,
            )

        paths = [self._layer_path(d) for d in diff_ids]
        return paths

    # ------------------------------------------------------------------
    # Layer cache helpers
    # ------------------------------------------------------------------

    def _layer_path(self, diff_id: str) -> str:
        hex_id = diff_id.removeprefix("sha256:")
        return os.path.join(self._layers_dir, hex_id)

    def _layer_cached(self, diff_id: str) -> bool:
        p = self._layer_path(diff_id)
        return os.path.isdir(p) and bool(os.listdir(p))

    # ------------------------------------------------------------------
    # Docker CLI helpers
    # ------------------------------------------------------------------

    def _get_diff_ids(self, image: str) -> list[str]:
        """Return the ordered list of layer diff IDs via ``docker inspect``."""
        result = subprocess.run(
            [
                "docker", "inspect",
                "--format", "{{json .RootFS.Layers}}",
                image,
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        return json.loads(result.stdout.strip())

    def _extract_layers_from_save(
        self, image: str, diff_ids: list[str],
    ) -> None:
        """``docker save`` the image and extract uncached layers.

        The archive produced by ``docker save`` contains a ``manifest.json``
        listing layer tarballs in application order (base first).  Each
        layer tarball is streamed to a temp file while computing its
        SHA-256 (which equals the diff ID), then extracted with Docker
        whiteout conversion.
        """
        tid = threading.get_ident()
        archive_path = os.path.join(self._cache_dir, f".tmp-save-{tid}.tar")
        try:
            logger.info("Running docker save for %s ...", image)
            subprocess.run(
                ["docker", "save", "-o", archive_path, image],
                check=True,
                capture_output=True,
            )

            with tarfile.open(archive_path) as archive:
                manifest_bytes = archive.extractfile("manifest.json")
                if manifest_bytes is None:
                    raise RuntimeError(
                        "docker save archive missing manifest.json"
                    )
                manifest = json.loads(manifest_bytes.read())
                layer_tar_paths = manifest[0]["Layers"]

                if len(layer_tar_paths) != len(diff_ids):
                    raise RuntimeError(
                        f"Layer count mismatch: manifest has "
                        f"{len(layer_tar_paths)} layers but docker inspect "
                        f"reported {len(diff_ids)} diff IDs"
                    )

                for idx, layer_tar_path in enumerate(layer_tar_paths):
                    expected_diff_id = diff_ids[idx]

                    if self._layer_cached(expected_diff_id):
                        logger.debug(
                            "Layer %d/%d already cached: %s",
                            idx + 1, len(layer_tar_paths),
                            expected_diff_id[:20],
                        )
                        continue

                    self._extract_single_layer(
                        archive, layer_tar_path,
                        expected_diff_id, idx, len(layer_tar_paths),
                    )

        except subprocess.CalledProcessError as e:
            raise RuntimeError(
                f"docker save failed for {image!r}: "
                f"{e.stderr.decode(errors='replace')}"
            ) from e
        finally:
            if os.path.exists(archive_path):
                os.unlink(archive_path)

    def _extract_single_layer(
        self,
        archive: tarfile.TarFile,
        layer_tar_path: str,
        expected_diff_id: str,
        idx: int,
        total: int,
    ) -> None:
        """Extract one layer tarball from the docker-save archive."""
        tid = threading.get_ident()
        tmp_layer = os.path.join(self._layers_dir, f".tmp-layer-{tid}.tar")
        try:
            layer_fileobj = archive.extractfile(layer_tar_path)
            if layer_fileobj is None:
                raise RuntimeError(
                    f"Cannot read {layer_tar_path} from archive"
                )

            h = hashlib.sha256()
            with open(tmp_layer, "wb") as f:
                while True:
                    chunk = layer_fileobj.read(65536)
                    if not chunk:
                        break
                    h.update(chunk)
                    f.write(chunk)

            actual_diff_id = "sha256:" + h.hexdigest()
            if actual_diff_id != expected_diff_id:
                raise RuntimeError(
                    f"Layer {idx} hash mismatch: expected "
                    f"{expected_diff_id}, got {actual_diff_id}"
                )

            layer_dir = self._layer_path(expected_diff_id)
            os.makedirs(layer_dir, exist_ok=True)

            with tarfile.open(tmp_layer) as layer_tar:
                _extract_with_whiteout_conversion(layer_tar, layer_dir)

            logger.info(
                "Extracted layer %d/%d: %s",
                idx + 1, total, expected_diff_id[:20],
            )

        except Exception:
            layer_dir = self._layer_path(expected_diff_id)
            if os.path.isdir(layer_dir):
                shutil.rmtree(layer_dir, ignore_errors=True)
            raise
        finally:
            if os.path.exists(tmp_layer):
                os.unlink(tmp_layer)


# ----------------------------------------------------------------------
# Docker whiteout -> OverlayFS conversion
# ----------------------------------------------------------------------

def _extract_with_whiteout_conversion(
    layer_tar: tarfile.TarFile,
    dest_dir: str,
) -> None:
    """Extract a layer tarball, converting Docker whiteouts to OverlayFS.

    Docker whiteout format:
      ``.wh.{filename}``   — file deletion (overlayfs: char device 0,0)
      ``.wh..wh..opq``     — opaque directory (overlayfs: xattr)

    OverlayFS equivalents (userxattr mode, used by Apiary for rootless):
      char device (major=0, minor=0) named ``{filename}``
      ``user.overlay.opaque`` xattr set to ``"y"`` on the directory
    """
    for member in layer_tar:
        name = member.name
        basename = os.path.basename(name)
        dirname = os.path.dirname(name)

        if basename == ".wh..wh..opq":
            parent_dir = os.path.join(dest_dir, dirname) if dirname else dest_dir
            os.makedirs(parent_dir, exist_ok=True)
            _set_opaque_xattr(parent_dir)

        elif basename.startswith(".wh."):
            real_name = basename[4:]
            parent = os.path.join(dest_dir, dirname) if dirname else dest_dir
            whiteout_path = os.path.join(parent, real_name)
            os.makedirs(parent, exist_ok=True)
            _create_whiteout_device(whiteout_path)

        else:
            layer_tar.extract(member, dest_dir, numeric_owner=True)


def _set_opaque_xattr(dir_path: str) -> None:
    """Mark a directory as opaque for OverlayFS (userxattr mode)."""
    try:
        os.setxattr(dir_path, b"user.overlay.opaque", b"y")
    except OSError as e:
        logger.warning(
            "Failed to set user.overlay.opaque on %s: %s "
            "(opaque directory may not work correctly)",
            dir_path, e,
        )


def _create_whiteout_device(path: str) -> None:
    """Create an OverlayFS whiteout marker (char device major=0, minor=0).

    Tries direct ``mknod`` first (works as root), then falls back to
    ``unshare -r`` which creates a user namespace where the calling user
    is mapped to uid 0, granting ``CAP_MKNOD``.
    """
    if os.path.exists(path) or os.path.islink(path):
        os.unlink(path)

    dev = os.makedev(0, 0)
    try:
        os.mknod(path, stat.S_IFCHR | 0o666, dev)
        return
    except PermissionError:
        pass

    result = subprocess.run(
        ["unshare", "-r", "sh", "-c", f'mknod "$1" c 0 0', "_", path],
        capture_output=True,
    )
    if result.returncode != 0:
        logger.warning(
            "Cannot create whiteout device at %s (mknod and unshare both "
            "failed: %s). Whiteout will be skipped.",
            path,
            result.stderr.decode(errors="replace").strip(),
        )

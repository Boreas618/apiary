"""Docker image to rootfs directory export with disk caching.

Uses the Docker CLI (``docker create`` / ``docker export``) and stdlib
``tarfile`` — no ``docker`` Python SDK required.
"""

import logging
import os
import shutil
import subprocess
import tarfile
import uuid

logger = logging.getLogger(__name__)


class RootfsManager:
    """Manages a cache of rootfs directories extracted from Docker images.

    Each Docker image is exported exactly once; subsequent calls for the same
    image return the cached directory instantly.
    """

    def __init__(self, cache_dir: str = "/tmp/apiary_rootfs"):
        self._cache_dir = cache_dir
        os.makedirs(self._cache_dir, exist_ok=True)

    def cache_path(self, image: str) -> str:
        """Return the deterministic cache directory path for *image*."""
        key = image.replace("/", "__").replace(":", "__")
        return os.path.join(self._cache_dir, key)

    def ensure(self, image: str) -> str:
        """Return a rootfs directory for *image*, exporting from Docker if not
        already cached.

        The export flow:

        1. ``docker create`` a temporary (non-started) container from *image*
        2. ``docker export`` its filesystem into a tarball
        3. Extract the tarball into the cache directory
        4. Clean up the container and tarball

        Returns the absolute path to the rootfs directory.
        """
        rootfs_dir = self.cache_path(image)

        if os.path.isdir(rootfs_dir) and os.listdir(rootfs_dir):
            logger.info("Rootfs cache hit: %s", rootfs_dir)
            return rootfs_dir

        logger.info("Exporting Docker image %s → %s ...", image, rootfs_dir)
        os.makedirs(rootfs_dir, exist_ok=True)

        container_name = f"apiary-rootfs-{uuid.uuid4().hex[:12]}"
        tar_path = rootfs_dir + ".tar"

        try:
            subprocess.run(
                ["docker", "create", "--name", container_name, image],
                check=True,
                capture_output=True,
                text=True,
            )

            with open(tar_path, "wb") as f:
                subprocess.run(
                    ["docker", "export", container_name],
                    check=True,
                    stdout=f,
                    stderr=subprocess.PIPE,
                )

            with tarfile.open(tar_path) as tar:
                tar.extractall(rootfs_dir)

            logger.info("Rootfs exported: %s", rootfs_dir)
            return rootfs_dir

        except Exception as e:
            if os.path.isdir(rootfs_dir):
                shutil.rmtree(rootfs_dir, ignore_errors=True)
            raise RuntimeError(
                f"Failed to export Docker image {image!r} to rootfs: {e}"
            ) from e

        finally:
            subprocess.run(
                ["docker", "rm", "-f", container_name],
                capture_output=True,
            )
            if os.path.exists(tar_path):
                os.unlink(tar_path)

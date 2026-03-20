"""High-level SWE-bench session lifecycle on top of Apiary.

Combines :class:`ApiaryClient` (HTTP) and :class:`RootfsManager` (rootfs
export) into a single entry point:

    with SWEBenchSession(image="swebench/sweb.eval.x86_64.django_1776_django-12345:latest") as s:
        result = s.execute("python -c 'import django; print(django.__version__)'")
        print(result.stdout)
"""

import logging
import shlex

from apiary_swebench.client import ApiaryClient, TaskResult
from apiary_swebench.rootfs import RootfsManager

logger = logging.getLogger(__name__)

_DEFAULT_INTERPRETER = ["bash", "-c"]


class SWEBenchSession:
    """Manages rootfs export + Apiary session for a single SWE-bench instance.

    Parameters
    ----------
    apiary_url:
        Base URL of a running Apiary daemon.
    apiary_token:
        Optional Bearer token for Apiary API authentication.
    image:
        Docker image name (e.g. ``swebench/sweb.eval.x86_64.django_...``).
        Automatically exported to a rootfs directory on first use.
    rootfs_cache_dir:
        Directory used to cache exported rootfs trees.
    working_dir:
        Default working directory inside the sandbox.
    env:
        Environment variables injected into every command.
    timeout:
        Default command timeout in **seconds**.
    interpreter:
        Shell interpreter prefix.  Defaults to ``["bash", "-c"]``.
    """

    def __init__(
        self,
        apiary_url: str = "http://127.0.0.1:8080",
        apiary_token: str | None = None,
        image: str = "",
        rootfs_cache_dir: str = "/tmp/apiary_rootfs",
        working_dir: str = "/testbed",
        env: dict[str, str] | None = None,
        timeout: int = 60,
        interpreter: list[str] | None = None,
    ):
        self._env = env or {}
        self._timeout = timeout
        self._interpreter = interpreter or list(_DEFAULT_INTERPRETER)
        self._working_dir = working_dir

        rootfs_mgr = RootfsManager(cache_dir=rootfs_cache_dir)
        layer_paths = rootfs_mgr.ensure_layers(image)

        self._client = ApiaryClient(url=apiary_url, token=apiary_token)
        self._session_id: str = self._client.create_session(
            working_dir=working_dir,
            base_image=layer_paths,
        )
        logger.info(
            "SWEBenchSession created: session=%s image=%s layers=%d",
            self._session_id,
            image,
            len(layer_paths),
        )

    # ------------------------------------------------------------------
    # Properties
    # ------------------------------------------------------------------

    @property
    def session_id(self) -> str:
        """Apiary session identifier."""
        return self._session_id

    # ------------------------------------------------------------------
    # Command execution
    # ------------------------------------------------------------------

    def execute(
        self,
        command: str,
        timeout: int | None = None,
        working_dir: str | None = None,
    ) -> TaskResult:
        """Run *command* in the sandbox.

        The command is wrapped with the configured interpreter (default
        ``bash -c '...'``) and environment variables before being sent to
        the Apiary daemon.
        """
        wrapped = " ".join(self._interpreter) + " " + shlex.quote(command)
        timeout_ms = (timeout or self._timeout) * 1000
        return self._client.execute(
            session_id=self._session_id,
            command=wrapped,
            timeout_ms=timeout_ms,
            working_dir=working_dir,
            env=self._env or None,
        )

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def close(self) -> None:
        """Close the Apiary session (destroys the sandbox) and the HTTP
        connection."""
        if self._session_id:
            try:
                self._client.close_session(self._session_id)
            except Exception:
                logger.warning(
                    "Failed to close Apiary session %s", self._session_id, exc_info=True,
                )
            self._session_id = ""
        self._client.close()

    # ------------------------------------------------------------------
    # Context manager
    # ------------------------------------------------------------------

    def __enter__(self) -> "SWEBenchSession":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    def __del__(self) -> None:
        if getattr(self, "_session_id", ""):
            self.close()

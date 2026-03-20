"""HTTP client for the Apiary sandbox pool REST API."""

import json
import logging
import time
from dataclasses import dataclass

import requests

LOGGER = logging.getLogger(__name__)


@dataclass
class TaskResult:
    """Result of executing a command in an Apiary session."""

    task_id: str
    session_id: str
    exit_code: int
    timed_out: bool
    duration_ms: int
    stdout: str
    stderr: str


class ApiaryClient:
    """Synchronous client for the Apiary sandbox pool HTTP API.

    Wraps the ``/api/v1/`` endpoints: session lifecycle, task execution,
    health checks, and pool status.
    """

    def __init__(self, url: str = "http://127.0.0.1:8080", token: str | None = None):
        self.url = url.rstrip("/")
        self._http = requests.Session()
        if token:
            self._http.headers["Authorization"] = f"Bearer {token}"

    @staticmethod
    def _raise_for_status(
        resp: requests.Response,
        *,
        context: str = "",
        payload: dict | None = None,
    ) -> None:
        if resp.ok:
            return
        try:
            body = resp.text
        except Exception:
            body = "<unreadable>"
        LOGGER.error(
            "HTTP %d %s\n"
            "  Context:  %s\n"
            "  Request:  %s %s\n"
            "  Payload:  %s\n"
            "  Response: %s\n"
            "  Headers:  %s",
            resp.status_code,
            resp.reason,
            context or "n/a",
            resp.request.method,
            resp.request.url,
            json.dumps(payload, default=str)[:2000] if payload else "n/a",
            body[:4000],
            dict(resp.headers),
        )
        resp.raise_for_status()

    # ------------------------------------------------------------------
    # Health / status
    # ------------------------------------------------------------------

    def health_check(self, retries: int = 30, interval: float = 1.0) -> bool:
        """Block until the Apiary daemon is healthy, or return *False* after
        *retries* attempts spaced *interval* seconds apart."""
        for _ in range(retries):
            try:
                resp = self._http.get(f"{self.url}/healthz", timeout=5)
                if resp.status_code == 200:
                    return True
            except requests.ConnectionError:
                pass
            time.sleep(interval)
        return False

    def status(self) -> dict:
        """Return the current pool status dict."""
        resp = self._http.get(f"{self.url}/api/v1/status", timeout=10)
        self._raise_for_status(resp, context="pool status")
        return resp.json()

    # ------------------------------------------------------------------
    # Session lifecycle
    # ------------------------------------------------------------------

    def create_session(
        self,
        working_dir: str | None = None,
        base_image: list[str] | None = None,
    ) -> str:
        """Create a persistent session.  Returns the ``session_id``.

        Parameters
        ----------
        base_image:
            Ordered list of layer directory paths (base first, topmost
            last) to use as the OverlayFS lower dirs for this session.
        """
        payload: dict = {}
        if working_dir:
            payload["working_dir"] = working_dir
        if base_image:
            payload["base_image"] = base_image
        resp = self._http.post(
            f"{self.url}/api/v1/sessions",
            json=payload or None,
            timeout=120,
        )
        self._raise_for_status(resp, context="create_session", payload=payload)
        return resp.json()["session_id"]

    def close_session(self, session_id: str) -> None:
        """Close a session and release its sandbox."""
        resp = self._http.delete(
            f"{self.url}/api/v1/sessions/{session_id}",
            timeout=60,
        )
        self._raise_for_status(resp, context=f"close_session {session_id}")

    # ------------------------------------------------------------------
    # Task execution
    # ------------------------------------------------------------------

    def execute(
        self,
        session_id: str,
        command: str,
        timeout_ms: int | None = None,
        working_dir: str | None = None,
        env: dict[str, str] | None = None,
    ) -> TaskResult:
        """Execute *command* inside the session's sandbox."""
        payload: dict = {
            "session_id": session_id,
            "command": command,
        }
        if timeout_ms is not None:
            payload["timeout_ms"] = timeout_ms
        if working_dir is not None:
            payload["working_dir"] = working_dir
        if env is not None:
            payload["env"] = env

        http_timeout = (timeout_ms / 1000 + 60) if timeout_ms else 3600
        resp = self._http.post(
            f"{self.url}/api/v1/tasks",
            json=payload,
            timeout=http_timeout,
        )
        self._raise_for_status(
            resp,
            context=f"execute task session={session_id}",
            payload=payload,
        )
        data = resp.json()
        return TaskResult(
            task_id=data["task_id"],
            session_id=data["session_id"],
            exit_code=data["exit_code"],
            timed_out=data["timed_out"],
            duration_ms=data["duration_ms"],
            stdout=data["stdout"],
            stderr=data["stderr"],
        )

    # ------------------------------------------------------------------
    # Cleanup
    # ------------------------------------------------------------------

    def close(self) -> None:
        """Close the underlying HTTP session."""
        self._http.close()

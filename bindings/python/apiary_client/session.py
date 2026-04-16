"""Apiary sandbox client and HTTP transport.

:class:`AsyncApiary` manages a single sandbox session (async).
:class:`Apiary` is its synchronous wrapper.
:class:`ApiaryMux` multiplexes many clients over a shared HTTP
connection to the Apiary daemon.

All three share a common :class:`_Transport` for HTTP and exec calls.
"""

from __future__ import annotations

import asyncio
import json
import logging
import shlex
import time
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

import httpx

logger = logging.getLogger(__name__)

_DEFAULT_INTERPRETER = ["bash", "-c"]


# ======================================================================
# Data types
# ======================================================================


@dataclass
class TaskResult:
    """Result of executing a command in an Apiary session."""

    task_id: str
    exit_code: int
    timed_out: bool
    duration_ms: int
    stdout: str
    stderr: str


# ======================================================================
# Shared helpers
# ======================================================================


def _raise_for_status(
    resp: httpx.Response,
    *,
    context: str = "",
    payload: dict | None = None,
) -> None:
    """Log diagnostics and raise on non-2xx responses."""
    if resp.is_success:
        return
    try:
        body = resp.text
    except Exception:
        body = "<unreadable>"
    logger.error(
        "HTTP %d\n"
        "  Context:  %s\n"
        "  Request:  %s %s\n"
        "  Payload:  %s\n"
        "  Response: %s",
        resp.status_code,
        context or "n/a",
        resp.request.method,
        resp.request.url,
        json.dumps(payload, default=str)[:2000] if payload else "n/a",
        body[:4000],
    )
    resp.raise_for_status()


def _is_session_lost(exc: BaseException) -> bool:
    """True when *exc* looks like an HTTP 404 (session gone) from the daemon."""
    response = getattr(exc, "response", None)
    if response is None:
        return False
    return getattr(response, "status_code", None) == 404


def _parse_task_result(data: dict) -> TaskResult:
    return TaskResult(
        task_id=data["task_id"],
        exit_code=data["exit_code"],
        timed_out=data["timed_out"],
        duration_ms=data["duration_ms"],
        stdout=data["stdout"],
        stderr=data["stderr"],
    )


def _read_file_cmd(path: str, offset: int = 1, limit: int | None = None) -> str:
    """Build an ``awk`` command that prints lines with 1-indexed numbers."""
    safe = shlex.quote(path)
    if limit is not None:
        end = offset + limit - 1
        awk = f'NR>={offset}&&NR<={end}{{printf "%6d|%s\\n",NR,$0}}NR>{end}{{exit}}'
    else:
        awk = f'NR>={offset}{{printf "%6d|%s\\n",NR,$0}}'
    return f"awk '{awk}' {safe}"


def _list_dir_cmd(path: str, depth: int = 1) -> str:
    """Build a ``find`` command that lists entries as ``<type> <path>``."""
    safe = shlex.quote(path)
    return (
        f'find {safe} -maxdepth {depth} -not -path {safe}'
        f' -printf "%y %p\\n" | sort'
    )


def _grep_files_cmd(
    pattern: str,
    path: str = ".",
    include: str | None = None,
    limit: int = 100,
) -> str:
    """Build a ``grep -rn`` command."""
    parts = ["grep -rn --color=never"]
    if include:
        parts.append(f"--include={shlex.quote(include)}")
    parts.extend(["--", shlex.quote(pattern), shlex.quote(path)])
    cmd = " ".join(parts)
    if limit > 0:
        cmd += f" | head -n {limit}"
    return cmd


def _apply_patch_cmd(patch: str) -> str:
    """Build a ``patch -p1`` command that receives the diff on stdin."""
    return f"printf %s {shlex.quote(patch)} | patch -p1 --no-backup-if-mismatch"


# ======================================================================
# Shared HTTP transport
# ======================================================================


class _Transport:
    """Low-level HTTP transport for the Apiary daemon REST API.

    Encapsulates HTTP client management, session CRUD, and raw command
    execution.  Shared between :class:`AsyncApiary` and :class:`ApiaryMux`.
    """

    def __init__(
        self,
        url: str,
        token: str | None = None,
        **client_kwargs: Any,
    ):
        self._url = url.rstrip("/")
        self._token = token
        self._client_kwargs = client_kwargs
        self._http: httpx.AsyncClient | None = None

    async def http(self) -> httpx.AsyncClient:
        """Return (and lazily create) the ``httpx`` async client."""
        if self._http is None or self._http.is_closed:
            headers: dict[str, str] = {"Content-Type": "application/json"}
            if self._token:
                headers["Authorization"] = f"Bearer {self._token}"
            self._http = httpx.AsyncClient(
                base_url=self._url,
                headers=headers,
                timeout=httpx.Timeout(timeout=300.0),
                **self._client_kwargs,
            )
        return self._http

    # -- session CRUD --------------------------------------------------

    async def create_session(
        self,
        *,
        image: str,
        working_dir: str,
    ) -> str:
        http = await self.http()
        payload: dict[str, Any] = {"image": image, "working_dir": working_dir}
        resp = await http.post(
            "/api/v1/sessions", json=payload, timeout=120,
        )
        _raise_for_status(resp, context="create_session", payload=payload)
        return resp.json()["session_id"]

    async def delete_session(self, session_id: str) -> None:
        http = await self.http()
        resp = await http.delete(
            f"/api/v1/sessions/{session_id}", timeout=60,
        )
        _raise_for_status(resp, context=f"delete session {session_id}")

    # -- command execution ---------------------------------------------

    async def exec(
        self,
        session_id: str,
        command: str,
        *,
        timeout_ms: int | None = None,
        working_dir: str | None = None,
        env: dict[str, str] | None = None,
    ) -> TaskResult:
        """Send a raw exec request to the daemon."""
        http = await self.http()
        payload: dict[str, Any] = {"command": command}
        if timeout_ms is not None:
            payload["timeout_ms"] = timeout_ms
        if working_dir is not None:
            payload["working_dir"] = working_dir
        if env is not None:
            payload["env"] = env

        http_timeout = (timeout_ms / 1000 + 60) if timeout_ms else 3600
        resp = await http.post(
            f"/api/v1/sessions/{session_id}/exec",
            json=payload,
            timeout=http_timeout,
        )
        _raise_for_status(
            resp, context=f"execute session={session_id}", payload=payload,
        )
        return _parse_task_result(resp.json())

    # -- health / status -----------------------------------------------

    async def health(self) -> bool:
        http = await self.http()
        resp = await http.get("/healthz", timeout=5)
        return resp.status_code == 200

    async def status(self) -> dict[str, Any]:
        http = await self.http()
        resp = await http.get("/api/v1/status", timeout=10)
        _raise_for_status(resp, context="pool status")
        return resp.json()

    # -- lifecycle -----------------------------------------------------

    async def close(self) -> None:
        if self._http and not self._http.is_closed:
            await self._http.aclose()
            self._http = None


# ======================================================================
# Async single-session
# ======================================================================


class AsyncApiary:
    """Manages a single Apiary sandbox session.

    Talks to the Apiary daemon REST API via :class:`_Transport`.

    * Lazy session creation -- the Apiary session is created on first
      use or explicitly in ``async with``.
    * Transparent auto-recovery -- if the daemon returns 404 (session
      lost), a new session is created and the operation is retried once.
    * File-operation helpers implemented as ``execute()`` wrappers.

    Parameters
    ----------
    apiary_url:
        Base URL of a running Apiary daemon.
    apiary_token:
        Optional Bearer token for Apiary API authentication.
    image:
        Docker image name (required).
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
        *,
        image: str,
        apiary_url: str = "http://127.0.0.1:8080",
        apiary_token: str | None = None,
        working_dir: str = "/workspace",
        env: dict[str, str] | None = None,
        timeout: int = 60,
        interpreter: list[str] | None = None,
    ):
        self._transport = _Transport(apiary_url, apiary_token)
        self._env = env or {}
        self._timeout = timeout
        self._interpreter = interpreter or list(_DEFAULT_INTERPRETER)
        self._working_dir = working_dir
        self._image = image

        self._session_id: str | None = None
        self._lock = asyncio.Lock()

    @property
    def session_id(self) -> str | None:
        """Apiary session identifier (``None`` until the session is created)."""
        return self._session_id

    # ------------------------------------------------------------------
    # Health / status
    # ------------------------------------------------------------------

    async def health_check(self, retries: int = 30, interval: float = 1.0) -> bool:
        """Block until the Apiary daemon is healthy, or return *False*
        after *retries* attempts."""
        for _ in range(retries):
            try:
                if await self._transport.health():
                    return True
            except Exception:
                pass
            await asyncio.sleep(interval)
        return False

    async def status(self) -> dict[str, Any]:
        """Return the current pool status dict."""
        return await self._transport.status()

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    async def _ensure_session(self) -> str:
        async with self._lock:
            if self._session_id is not None:
                return self._session_id
            self._session_id = await self._transport.create_session(
                image=self._image,
                working_dir=self._working_dir,
            )
            logger.info(
                "Session created: session=%s image=%s",
                self._session_id,
                self._image,
            )
            return self._session_id

    async def _invalidate_session(self) -> None:
        async with self._lock:
            self._session_id = None

    async def _with_retry(self, fn: Callable[[str], Any]) -> Any:
        """Call ``fn(session_id)``; on 404 recreate the session and retry."""
        session_id = await self._ensure_session()
        try:
            return await fn(session_id)
        except Exception as exc:
            if not _is_session_lost(exc):
                raise
            logger.warning("Lost session %s; recreating", session_id)
            await self._invalidate_session()
            session_id = await self._ensure_session()
            return await fn(session_id)

    # ------------------------------------------------------------------
    # Command execution
    # ------------------------------------------------------------------

    async def execute(
        self,
        command: str,
        timeout: int | None = None,
        working_dir: str | None = None,
    ) -> TaskResult:
        """Run *command* wrapped with the configured interpreter."""
        wrapped = " ".join(self._interpreter) + " " + shlex.quote(command)
        timeout_ms = (timeout or self._timeout) * 1000
        return await self._with_retry(
            lambda sid: self._transport.exec(
                sid, wrapped,
                timeout_ms=timeout_ms,
                working_dir=working_dir,
                env=self._env or None,
            ),
        )

    # ------------------------------------------------------------------
    # File operations
    # ------------------------------------------------------------------

    async def read_file(
        self, path: str, offset: int = 1, limit: int | None = None,
    ) -> TaskResult:
        """Read file content with 1-indexed line numbers via ``awk``."""
        return await self.execute(_read_file_cmd(path, offset, limit))

    async def list_dir(self, path: str, depth: int = 1) -> TaskResult:
        """List directory entries via ``find``."""
        return await self.execute(_list_dir_cmd(path, depth))

    async def grep_files(
        self,
        pattern: str,
        path: str = ".",
        include: str | None = None,
        limit: int = 100,
    ) -> TaskResult:
        """Search files for *pattern* via ``grep -rn``."""
        return await self.execute(_grep_files_cmd(pattern, path, include, limit))

    async def apply_patch(self, patch: str) -> TaskResult:
        """Apply a unified diff via ``patch -p1``."""
        return await self.execute(_apply_patch_cmd(patch))

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def close(self) -> None:
        """Destroy the Apiary session and close the HTTP connection."""
        if self._session_id is not None:
            try:
                await self._transport.delete_session(self._session_id)
            except Exception:
                logger.warning(
                    "Failed to close session %s",
                    self._session_id,
                    exc_info=True,
                )
            self._session_id = None
        await self._transport.close()

    async def __aenter__(self) -> AsyncApiary:
        await self._ensure_session()
        return self

    async def __aexit__(self, *exc) -> None:
        await self.close()


# ======================================================================
# Synchronous wrapper
# ======================================================================


class Apiary:
    """Synchronous facade over :class:`AsyncApiary`.

    The sandbox session is created eagerly in ``__init__``.  Everything
    else -- method names, parameters, return types -- is identical.
    """

    def __init__(
        self,
        *,
        image: str,
        apiary_url: str = "http://127.0.0.1:8080",
        apiary_token: str | None = None,
        working_dir: str = "/workspace",
        env: dict[str, str] | None = None,
        timeout: int = 60,
        interpreter: list[str] | None = None,
    ):
        self._loop = asyncio.new_event_loop()
        self._async = AsyncApiary(
            image=image,
            apiary_url=apiary_url,
            apiary_token=apiary_token,
            working_dir=working_dir,
            env=env,
            timeout=timeout,
            interpreter=interpreter,
        )
        self._loop.run_until_complete(self._async._ensure_session())

    def _run(self, coro):  # noqa: ANN001
        return self._loop.run_until_complete(coro)

    @property
    def session_id(self) -> str | None:
        return self._async.session_id

    def health_check(self, retries: int = 30, interval: float = 1.0) -> bool:
        return self._run(self._async.health_check(retries, interval))

    def status(self) -> dict[str, Any]:
        return self._run(self._async.status())

    def execute(
        self, command: str, timeout: int | None = None, working_dir: str | None = None,
    ) -> TaskResult:
        return self._run(self._async.execute(command, timeout, working_dir))

    def read_file(
        self, path: str, offset: int = 1, limit: int | None = None,
    ) -> TaskResult:
        return self._run(self._async.read_file(path, offset, limit))

    def list_dir(self, path: str, depth: int = 1) -> TaskResult:
        return self._run(self._async.list_dir(path, depth))

    def grep_files(
        self,
        pattern: str,
        path: str = ".",
        include: str | None = None,
        limit: int = 100,
    ) -> TaskResult:
        return self._run(self._async.grep_files(pattern, path, include, limit))

    def apply_patch(self, patch: str) -> TaskResult:
        return self._run(self._async.apply_patch(patch))

    def close(self) -> None:
        self._run(self._async.close())
        self._loop.close()

    def __enter__(self) -> Apiary:
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    def __del__(self) -> None:
        try:
            if getattr(self, "_loop", None) and not self._loop.is_closed():
                self._run(self._async.close())
                self._loop.close()
        except Exception:
            pass


# ======================================================================
# Multi-client async multiplexer
# ======================================================================

_DEFAULT_REAPER_INTERVAL = 60.0


class ApiaryMux:
    """Multiplexes many async Apiary sessions keyed by client identifier.

    Talks to the Apiary daemon REST API via a shared :class:`_Transport`.

    Intended for servers (e.g. MCP) that multiplex many clients over a
    shared connection to the Apiary daemon.

    Features
    --------
    * Lazy per-client session creation.
    * Per-client locking to prevent duplicate session creation.
    * Reference counting via :meth:`attach` / :meth:`detach`.
    * Background reaper that destroys sessions idle longer than
      *idle_timeout*.
    * Automatic session recovery on HTTP 404 (session lost).
    """

    def __init__(
        self,
        *,
        image: str,
        apiary_url: str = "http://127.0.0.1:8080",
        apiary_token: str | None = None,
        working_dir: str = "/workspace",
        idle_timeout: float = 1800.0,
        reaper_interval: float = _DEFAULT_REAPER_INTERVAL,
        on_client_destroy: Callable[[str], Any] | None = None,
        max_connections: int = 500,
        max_keepalive_connections: int = 200,
    ):
        self._transport = _Transport(
            apiary_url, apiary_token,
            limits=httpx.Limits(
                max_connections=max_connections,
                max_keepalive_connections=max_keepalive_connections,
                keepalive_expiry=30.0,
            ),
        )
        self._image = image
        self._working_dir = working_dir
        self._idle_timeout = idle_timeout
        self._reaper_interval = reaper_interval
        self._on_client_destroy = on_client_destroy

        self._sessions: dict[str, str] = {}
        self._images: dict[str, str] = {}
        self._locks: dict[str, asyncio.Lock] = {}
        self._refcounts: dict[str, int] = {}
        self._detached_at: dict[str, float] = {}
        self._reaper_task: asyncio.Task | None = None  # type: ignore[type-arg]

    @property
    def working_dir(self) -> str:
        return self._working_dir

    @property
    def active_sessions(self) -> int:
        return len(self._sessions)

    # ------------------------------------------------------------------
    # Session lifecycle
    # ------------------------------------------------------------------

    def _lock_for(self, cid: str) -> asyncio.Lock:
        if cid not in self._locks:
            self._locks[cid] = asyncio.Lock()
        return self._locks[cid]

    async def ensure_session(self, cid: str, *, image: str = "") -> str:
        """Return (and lazily create) the Apiary session for *cid*.

        *image* is only used when creating a new session.  On subsequent
        calls (including automatic 404 recovery) the originally recorded
        image for *cid* is reused.  Falls back to the mux-level default
        image when not specified per-client.
        """
        lock = self._lock_for(cid)
        async with lock:
            if cid in self._sessions:
                return self._sessions[cid]
            if image:
                self._images[cid] = image
            effective_image = self._images.get(cid) or self._image
            session_id = await self._transport.create_session(
                image=effective_image,
                working_dir=self._working_dir,
            )
            self._sessions[cid] = session_id
            logger.info("Created session %s for client %s", session_id, cid)
            return session_id

    async def _invalidate_session(self, cid: str) -> None:
        lock = self._lock_for(cid)
        async with lock:
            self._sessions.pop(cid, None)

    async def destroy_client(self, cid: str) -> None:
        """Tear down *cid*'s session and clean up all bookkeeping."""
        session_id = self._sessions.pop(cid, None)
        self._images.pop(cid, None)
        self._locks.pop(cid, None)
        self._refcounts.pop(cid, None)
        self._detached_at.pop(cid, None)
        if self._on_client_destroy is not None:
            self._on_client_destroy(cid)
        if session_id:
            try:
                await self._transport.delete_session(session_id)
            except Exception:
                logger.warning(
                    "Failed to destroy session %s for client %s",
                    session_id, cid, exc_info=True,
                )
            logger.info("Destroyed session %s for client %s", session_id, cid)

    # ------------------------------------------------------------------
    # Reference counting
    # ------------------------------------------------------------------

    def attach(self, cid: str) -> None:
        """Increment the reference count for *cid*."""
        self._refcounts[cid] = self._refcounts.get(cid, 0) + 1
        self._detached_at.pop(cid, None)

    def detach(self, cid: str) -> None:
        """Decrement the reference count; start the idle clock at zero."""
        count = self._refcounts.get(cid, 1) - 1
        if count <= 0:
            self._refcounts.pop(cid, None)
            self._detached_at[cid] = time.monotonic()
        else:
            self._refcounts[cid] = count

    # ------------------------------------------------------------------
    # Idle reaper
    # ------------------------------------------------------------------

    def start_reaper(self) -> None:
        """Launch the background reaper task (idempotent)."""
        if self._reaper_task is None:
            self._reaper_task = asyncio.create_task(self._reap_loop())

    async def _reap_loop(self) -> None:
        while True:
            await asyncio.sleep(self._reaper_interval)
            now = time.monotonic()
            for cid in list(self._detached_at):
                if cid in self._refcounts:
                    self._detached_at.pop(cid, None)
                    continue
                if cid not in self._sessions:
                    self._detached_at.pop(cid, None)
                    continue
                if now - self._detached_at[cid] >= self._idle_timeout:
                    logger.info("Reaping idle client %s", cid)
                    await self.destroy_client(cid)

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    async def _with_retry(self, cid: str, fn: Callable[[str], Any]) -> Any:
        """Call ``fn(session_id)``; on 404 recreate the session and retry."""
        session_id = await self._ensure_existing_session(cid)
        try:
            return await fn(session_id)
        except Exception as exc:
            if not _is_session_lost(exc):
                raise
            logger.warning(
                "Lost session %s for client %s; recreating", session_id, cid,
            )
            await self._invalidate_session(cid)
            session_id = await self._ensure_existing_session(cid)
            return await fn(session_id)

    async def _ensure_existing_session(self, cid: str) -> str:
        """Re-create a session for *cid* using its previously recorded image."""
        return await self.ensure_session(cid)

    # ------------------------------------------------------------------
    # Command execution
    # ------------------------------------------------------------------

    async def shell(
        self,
        cid: str,
        script: str,
        *,
        timeout_ms: int | None = None,
        working_dir: str | None = None,
        env: dict[str, str] | None = None,
    ) -> TaskResult:
        """Wrap *script* with ``bash -c`` and send it as a raw command."""
        cmd = "bash -c " + shlex.quote(script)
        return await self.execute(
            cid, cmd, timeout_ms=timeout_ms, working_dir=working_dir, env=env,
        )

    async def execute(
        self,
        cid: str,
        command: str,
        *,
        timeout_ms: int | None = None,
        working_dir: str | None = None,
        env: dict[str, str] | None = None,
    ) -> TaskResult:
        """Execute a raw *command* in the session for *cid*.

        No shell interpreter wrapping is applied.
        """
        return await self._with_retry(
            cid,
            lambda sid: self._transport.exec(
                sid, command,
                timeout_ms=timeout_ms,
                working_dir=working_dir,
                env=env,
            ),
        )

    # ------------------------------------------------------------------
    # File operations
    # ------------------------------------------------------------------

    async def read_file(
        self, cid: str, path: str, offset: int = 1, limit: int | None = None,
    ) -> TaskResult:
        """Read file content with 1-indexed line numbers via ``awk``."""
        return await self.shell(cid, _read_file_cmd(path, offset, limit))

    async def list_dir(self, cid: str, path: str, depth: int = 1) -> TaskResult:
        """List directory entries via ``find``."""
        return await self.shell(cid, _list_dir_cmd(path, depth))

    async def grep_files(
        self,
        cid: str,
        pattern: str,
        path: str = ".",
        include: str | None = None,
        limit: int = 100,
    ) -> TaskResult:
        """Search files for *pattern* via ``grep -rn``."""
        return await self.shell(cid, _grep_files_cmd(pattern, path, include, limit))

    async def apply_patch(self, cid: str, patch: str) -> TaskResult:
        """Apply a unified diff via ``patch -p1``."""
        return await self.shell(cid, _apply_patch_cmd(patch))

    # ------------------------------------------------------------------
    # Shutdown
    # ------------------------------------------------------------------

    async def shutdown(self) -> None:
        """Cancel the reaper, destroy all sessions, close the HTTP client."""
        if self._reaper_task is not None:
            self._reaper_task.cancel()
            try:
                await self._reaper_task
            except asyncio.CancelledError:
                pass
        for cid in list(self._sessions):
            await self.destroy_client(cid)
        await self._transport.close()

"""MCP server backed by Apiary sandboxes.

Provides shell execution and file operations inside isolated Linux sandboxes.
Each client is identified by a ``client_id`` and gets its own persistent
sandbox.  Sessions are managed internally and never exposed to callers.

Client identity & session lifecycle:
    * SSE — pass ``client_id`` as a query parameter on the SSE endpoint
      (``/sse?client_id=<id>``).  If omitted a random ephemeral ID is used.
      Sandboxes survive reconnections: as long as the same ``client_id`` is
      reused, the sandbox (and all filesystem state inside it) is preserved.
      Sandboxes with no active connection are reaped after an idle timeout
      (default 30 min, configurable via ``--idle-timeout``).
    * stdio — a single sandbox for the sole client (implicit ``client_id``).

If a sandbox session is lost server-side (Apiary returns 404), it is
transparently recreated, though accumulated filesystem state will be gone.

Environment variables:
    APIARY_URL           Apiary daemon URL (default: http://127.0.0.1:8080)
    APIARY_API_TOKEN     Bearer token for Apiary daemon authentication
    APIARY_WORKING_DIR   Default working directory inside sandbox (default: /workspace)
    MCP_AUTH_TOKEN       If set, clients must present this Bearer token on the
                         SSE endpoint to connect
"""

import argparse
import asyncio
import base64
import contextvars
import hmac
import logging
import os
import shlex
import time
import uuid
from typing import Optional

import httpx
import uvicorn
from mcp.server import Server
from mcp.server.fastmcp import FastMCP
from mcp.server.sse import SseServerTransport
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse, Response
from starlette.routing import Mount, Route

logging.basicConfig(level=logging.INFO)
LOGGER = logging.getLogger(__name__)

mcp = FastMCP("apiary_sandbox")

# Carries the current client identity into tool handlers.
# In SSE mode each connection sets it to the client_id from the query string;
# in stdio mode the default is used since there is only one client.
_client_id: contextvars.ContextVar[str] = contextvars.ContextVar(
    "_client_id", default="stdio"
)


# ---------------------------------------------------------------------------
# Session manager — maps client IDs to Apiary sandbox sessions
# ---------------------------------------------------------------------------

_REAPER_INTERVAL = 60  # seconds between idle-reaper sweeps


class SessionManager:
    """Manages per-client Apiary sandbox sessions.

    Each ``client_id`` maps to exactly one Apiary sandbox session.  Multiple
    SSE connections can share the same ``client_id`` (and therefore the same
    sandbox).  Sandboxes outlive individual connections: they are only
    destroyed after no connection has been active for *idle_timeout* seconds,
    or on server shutdown.
    """

    def __init__(
        self,
        base_url: str,
        token: Optional[str] = None,
        working_dir: str = "/workspace",
        idle_timeout: float = 1800.0,
    ):
        self._base_url = base_url.rstrip("/")
        self._token = token
        self._working_dir = working_dir
        self._idle_timeout = idle_timeout

        self._sessions: dict[str, str] = {}  # client_id -> apiary session_id
        self._locks: dict[str, asyncio.Lock] = {}
        self._refcounts: dict[str, int] = {}  # active SSE connections
        self._detached_at: dict[str, float] = {}  # monotonic timestamp

        self._client: Optional[httpx.AsyncClient] = None
        self._reaper_task: Optional[asyncio.Task] = None

    # -- HTTP client --

    async def _get_client(self) -> httpx.AsyncClient:
        if self._client is None or self._client.is_closed:
            headers: dict[str, str] = {"Content-Type": "application/json"}
            if self._token:
                headers["Authorization"] = f"Bearer {self._token}"
            self._client = httpx.AsyncClient(
                base_url=self._base_url,
                headers=headers,
                timeout=httpx.Timeout(timeout=300.0),
            )
        return self._client

    # -- Per-client lock --

    def _lock_for(self, cid: str) -> asyncio.Lock:
        if cid not in self._locks:
            self._locks[cid] = asyncio.Lock()
        return self._locks[cid]

    # -- Apiary session helpers --

    async def _create_apiary_session(self) -> str:
        client = await self._get_client()
        resp = await client.post(
            "/api/v1/sessions",
            json={"working_dir": self._working_dir},
        )
        resp.raise_for_status()
        return resp.json()["session_id"]

    async def _destroy_apiary_session(self, session_id: str) -> None:
        try:
            client = await self._get_client()
            await client.delete(f"/api/v1/sessions/{session_id}")
        except Exception:
            LOGGER.warning(
                "Failed to destroy apiary session %s",
                session_id,
                exc_info=True,
            )

    # -- Session lifecycle --

    async def _ensure_session(self, cid: str) -> str:
        lock = self._lock_for(cid)
        async with lock:
            if cid in self._sessions:
                return self._sessions[cid]
            session_id = await self._create_apiary_session()
            self._sessions[cid] = session_id
            LOGGER.info(
                "Session %s created for client %s (%d active)",
                session_id,
                cid,
                len(self._sessions),
            )
            return session_id

    async def _destroy_client(self, cid: str) -> None:
        session_id = self._sessions.pop(cid, None)
        self._locks.pop(cid, None)
        self._refcounts.pop(cid, None)
        self._detached_at.pop(cid, None)
        if session_id:
            await self._destroy_apiary_session(session_id)
            LOGGER.info(
                "Session %s destroyed for client %s (%d remaining)",
                session_id,
                cid,
                len(self._sessions),
            )

    # -- Connection ref-counting --

    def attach(self, cid: str) -> None:
        """Register a new SSE connection for *cid*."""
        self._refcounts[cid] = self._refcounts.get(cid, 0) + 1
        self._detached_at.pop(cid, None)

    def detach(self, cid: str) -> None:
        """Un-register an SSE connection.  Starts the idle timer when the
        last connection for *cid* closes."""
        count = self._refcounts.get(cid, 1) - 1
        if count <= 0:
            self._refcounts.pop(cid, None)
            self._detached_at[cid] = time.monotonic()
        else:
            self._refcounts[cid] = count

    # -- Idle reaper --

    def start_reaper(self) -> None:
        if self._reaper_task is None:
            self._reaper_task = asyncio.create_task(self._reap_loop())

    async def _reap_loop(self) -> None:
        while True:
            await asyncio.sleep(_REAPER_INTERVAL)
            now = time.monotonic()
            for cid in list(self._detached_at):
                if cid in self._refcounts:
                    self._detached_at.pop(cid, None)
                    continue
                if cid not in self._sessions:
                    self._detached_at.pop(cid, None)
                    continue
                if now - self._detached_at[cid] >= self._idle_timeout:
                    LOGGER.info("Reaping idle client %s", cid)
                    await self._destroy_client(cid)

    # -- Command execution --

    async def execute(
        self,
        command: str,
        *,
        timeout_ms: Optional[int] = None,
        working_dir: Optional[str] = None,
        env: Optional[dict[str, str]] = None,
    ) -> dict:
        """Run *command* (interpreted by ``bash -c``) in the caller's sandbox.

        The caller is identified via the ``_client_id`` context variable that
        the transport layer sets for each connection.
        """
        cid = _client_id.get()
        wrapped = f"bash -c {shlex.quote(command)}"
        session_id = await self._ensure_session(cid)
        client = await self._get_client()

        payload: dict = {"command": wrapped, "session_id": session_id}
        if timeout_ms is not None:
            payload["timeout_ms"] = timeout_ms
        if working_dir is not None:
            payload["working_dir"] = working_dir
        if env:
            payload["env"] = env

        resp = await client.post("/api/v1/tasks", json=payload)

        if resp.status_code == 404:
            LOGGER.warning(
                "Session %s lost for client %s, recreating…",
                session_id,
                cid,
            )
            lock = self._lock_for(cid)
            async with lock:
                self._sessions.pop(cid, None)
            session_id = await self._ensure_session(cid)
            payload["session_id"] = session_id
            resp = await client.post("/api/v1/tasks", json=payload)

        resp.raise_for_status()
        return resp.json()

    # -- Shutdown --

    async def shutdown(self) -> None:
        if self._reaper_task:
            self._reaper_task.cancel()
            try:
                await self._reaper_task
            except asyncio.CancelledError:
                pass
        for cid in list(self._sessions):
            await self._destroy_client(cid)
        if self._client and not self._client.is_closed:
            await self._client.aclose()

    @property
    def active_sessions(self) -> int:
        return len(self._sessions)


_session = SessionManager(
    os.getenv("APIARY_URL", "http://127.0.0.1:8080"),
    os.getenv("APIARY_API_TOKEN"),
    os.getenv("APIARY_WORKING_DIR", "/workspace"),
)

_mcp_auth_token: Optional[str] = os.getenv("MCP_AUTH_TOKEN")


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _fmt(result: dict) -> str:
    """Format a task-execution result for MCP output."""
    parts: list[str] = []
    if result.get("stdout"):
        parts.append(result["stdout"])
    if result.get("stderr"):
        parts.append(f"[stderr]\n{result['stderr']}")
    if result.get("timed_out"):
        parts.append("[timed out]")
    exit_code = result.get("exit_code", -1)
    if exit_code != 0:
        parts.append(f"[exit code: {exit_code}]")
    return "\n".join(parts) if parts else "(no output)"


def _q(value: str) -> str:
    """Shell-quote a value for safe interpolation inside ``bash -c``."""
    return shlex.quote(value)


# ---------------------------------------------------------------------------
# Tool: shell execution
# ---------------------------------------------------------------------------


@mcp.tool()
async def shell_exec(
    command: str,
    timeout_ms: int = 30000,
    working_dir: str | None = None,
) -> str:
    """Execute a shell command inside an isolated Linux sandbox.

    The sandbox keeps filesystem state between calls — installed packages,
    created files, and environment changes all persist as long as the same
    client_id is used.

    Args:
        command: Shell command (interpreted by bash; pipes, redirects, etc. work).
        timeout_ms: Maximum execution time in milliseconds (default 30 000).
        working_dir: Override the working directory for this command.
    """
    result = await _session.execute(
        command, timeout_ms=timeout_ms, working_dir=working_dir
    )
    return _fmt(result)


# ---------------------------------------------------------------------------
# Tools: file operations
# ---------------------------------------------------------------------------


@mcp.tool()
async def read_file(path: str, byte_limit: int = 1_000_000) -> str:
    """Read a file from the sandbox.

    Args:
        path: Absolute or workspace-relative path.
        byte_limit: Maximum bytes to read (default 1 MB) to prevent huge output.
    """
    result = await _session.execute(
        f"head -c {int(byte_limit)} -- {_q(path)}"
    )
    if result.get("exit_code") != 0:
        return f"Error reading file: {result.get('stderr', '').strip()}"
    return result.get("stdout", "")


@mcp.tool()
async def write_file(path: str, content: str, append: bool = False) -> str:
    """Write text to a file in the sandbox. Creates parent directories as needed.

    Content is base64-transported to safely handle arbitrary characters.

    Args:
        path: Absolute or workspace-relative file path.
        content: Text content to write.
        append: If true, append to the file instead of overwriting.
    """
    encoded = base64.b64encode(content.encode()).decode()
    redir = ">>" if append else ">"
    cmd = (
        f"mkdir -p -- \"$(dirname {_q(path)})\" && "
        f"printf '%s' {_q(encoded)} | base64 -d {redir} {_q(path)}"
    )
    result = await _session.execute(cmd)
    if result.get("exit_code") != 0:
        return f"Error writing file: {result.get('stderr', '').strip()}"
    return f"Wrote {len(content)} bytes to {path}"


@mcp.tool()
async def list_directory(path: str = ".", show_hidden: bool = False) -> str:
    """List directory contents in the sandbox.

    Args:
        path: Directory path (default: working directory).
        show_hidden: Include hidden (dot) files.
    """
    flags = "-lhA" if show_hidden else "-lh"
    result = await _session.execute(f"ls {flags} -- {_q(path)}")
    if result.get("exit_code") != 0:
        return f"Error listing directory: {result.get('stderr', '').strip()}"
    return result.get("stdout", "")


@mcp.tool()
async def create_directory(path: str) -> str:
    """Create a directory (and any missing parents) in the sandbox.

    Args:
        path: Directory path to create.
    """
    result = await _session.execute(f"mkdir -p -- {_q(path)}")
    if result.get("exit_code") != 0:
        return f"Error creating directory: {result.get('stderr', '').strip()}"
    return f"Created directory: {path}"


@mcp.tool()
async def remove(path: str, recursive: bool = False) -> str:
    """Remove a file or directory from the sandbox.

    Args:
        path: Path to remove.
        recursive: Remove directories and all their contents.
    """
    flags = "-rf" if recursive else "-f"
    result = await _session.execute(f"rm {flags} -- {_q(path)}")
    if result.get("exit_code") != 0:
        return f"Error removing: {result.get('stderr', '').strip()}"
    return f"Removed: {path}"


@mcp.tool()
async def move_file(source: str, destination: str) -> str:
    """Move or rename a file/directory in the sandbox.

    Args:
        source: Current path.
        destination: New path.
    """
    result = await _session.execute(
        f"mv -- {_q(source)} {_q(destination)}"
    )
    if result.get("exit_code") != 0:
        return f"Error moving: {result.get('stderr', '').strip()}"
    return f"Moved {source} -> {destination}"


@mcp.tool()
async def copy_file(source: str, destination: str) -> str:
    """Copy a file or directory in the sandbox.

    Args:
        source: Source path.
        destination: Destination path.
    """
    result = await _session.execute(
        f"cp -r -- {_q(source)} {_q(destination)}"
    )
    if result.get("exit_code") != 0:
        return f"Error copying: {result.get('stderr', '').strip()}"
    return f"Copied {source} -> {destination}"


@mcp.tool()
async def file_info(path: str) -> str:
    """Get metadata (type, size, permissions, timestamps) for a path in the sandbox.

    Args:
        path: Path to inspect.
    """
    cmd = f"stat -- {_q(path)} 2>&1; file -- {_q(path)} 2>&1"
    result = await _session.execute(cmd)
    if result.get("exit_code") != 0:
        return f"Error getting info: {result.get('stderr', '').strip()}"
    return result.get("stdout", "")


# ---------------------------------------------------------------------------
# SSE transport
# ---------------------------------------------------------------------------


def create_starlette_app(
    mcp_server: Server, *, debug: bool = False
) -> Starlette:
    sse = SseServerTransport("/messages/")

    async def handle_sse(request: Request):
        if _mcp_auth_token:
            provided = request.headers.get("authorization", "")
            expected = f"Bearer {_mcp_auth_token}"
            if not hmac.compare_digest(provided.encode(), expected.encode()):
                return JSONResponse(
                    {"error": "unauthorized"}, status_code=401
                )

        cid = request.query_params.get("client_id") or uuid.uuid4().hex[:12]
        _client_id.set(cid)
        _session.attach(cid)
        LOGGER.info(
            "Client %s connected (refcount=%d)",
            cid,
            _session._refcounts.get(cid, 0),
        )

        async with sse.connect_sse(
            request.scope,
            request.receive,
            request._send,
        ) as (read_stream, write_stream):
            try:
                await mcp_server.run(
                    read_stream,
                    write_stream,
                    mcp_server.create_initialization_options(),
                )
            finally:
                _session.detach(cid)
                LOGGER.info(
                    "Client %s disconnected (refcount=%d)",
                    cid,
                    _session._refcounts.get(cid, 0),
                )

        return Response()

    async def handle_health(_: Request):
        return JSONResponse(
            {
                "status": "ok",
                "sessions": _session.active_sessions,
            }
        )

    async def on_startup():
        _session.start_reaper()

    async def on_shutdown():
        await _session.shutdown()

    return Starlette(
        debug=debug,
        routes=[
            Route("/health", endpoint=handle_health, methods=["GET"]),
            Route("/sse", endpoint=handle_sse, methods=["GET"]),
            Mount("/messages/", app=sse.handle_post_message),
        ],
        on_startup=[on_startup],
        on_shutdown=[on_shutdown],
    )


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="MCP server for shell & file operations in Apiary sandboxes"
    )
    parser.add_argument("--host", default="0.0.0.0", help="SSE bind host")
    parser.add_argument(
        "--port", type=int, default=8082, help="SSE bind port"
    )
    parser.add_argument(
        "--apiary-url",
        default=os.getenv("APIARY_URL", "http://127.0.0.1:8080"),
        help="Apiary daemon URL",
    )
    parser.add_argument(
        "--apiary-token",
        default=os.getenv("APIARY_API_TOKEN"),
        help="Apiary daemon bearer token",
    )
    parser.add_argument(
        "--mcp-token",
        default=os.getenv("MCP_AUTH_TOKEN"),
        help="Require this bearer token on the MCP SSE endpoint",
    )
    parser.add_argument(
        "--working-dir",
        default=os.getenv("APIARY_WORKING_DIR", "/workspace"),
        help="Default sandbox working directory",
    )
    parser.add_argument(
        "--idle-timeout",
        type=float,
        default=300.0,
        help="Seconds before an unconnected sandbox is reaped (default 300)",
    )
    parser.add_argument(
        "--transport",
        choices=["sse", "stdio"],
        default="sse",
        help="MCP transport (default: sse)",
    )
    args = parser.parse_args()

    _session = SessionManager(
        args.apiary_url,
        args.apiary_token,
        args.working_dir,
        idle_timeout=args.idle_timeout,
    )
    _mcp_auth_token = args.mcp_token

    if args.transport == "stdio":
        mcp.run(transport="stdio")
    else:
        starlette_app = create_starlette_app(mcp._mcp_server, debug=True)
        uvicorn.run(starlette_app, host=args.host, port=args.port)

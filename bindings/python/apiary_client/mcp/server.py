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
      (default 5 min, configurable via ``--idle-timeout``).
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
import base64
import contextvars
import hmac
import logging
import os
import shlex
import uuid
from typing import Optional

import uvicorn
from mcp.server import Server
from mcp.server.fastmcp import FastMCP
from mcp.server.sse import SseServerTransport
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse, Response
from starlette.routing import Mount, Route

from ..session import ApiaryMux, TaskResult

logging.basicConfig(level=logging.INFO)
LOGGER = logging.getLogger(__name__)

mcp = FastMCP("apiary_sandbox")

_client_id: contextvars.ContextVar[str] = contextvars.ContextVar(
    "_client_id", default="stdio"
)

_session: ApiaryMux
_mcp_auth_token: Optional[str] = os.getenv("MCP_AUTH_TOKEN")


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _cid() -> str:
    return _client_id.get()


def _fmt(result: TaskResult) -> str:
    """Format a task-execution result for MCP output."""
    parts: list[str] = []
    if result.stdout:
        parts.append(result.stdout)
    if result.stderr:
        parts.append(f"[stderr]\n{result.stderr}")
    if result.timed_out:
        parts.append("[timed out]")
    if result.exit_code != 0:
        parts.append(f"[exit code: {result.exit_code}]")
    return "\n".join(parts) if parts else "(no output)"


def _q(value: str) -> str:
    return shlex.quote(value)


# ---------------------------------------------------------------------------
# Tools
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
        command: Shell command (interpreted by /bin/sh; pipes, redirects, etc. work).
        timeout_ms: Maximum execution time in milliseconds (default 30 000).
        working_dir: Override the working directory for this command.
    """
    result = await _session.shell(
        _cid(), command, timeout_ms=timeout_ms, working_dir=working_dir,
    )
    return _fmt(result)


@mcp.tool()
async def read_file(path: str, byte_limit: int = 1_000_000) -> str:
    """Read a file from the sandbox.

    Args:
        path: Absolute or workspace-relative path.
        byte_limit: Maximum bytes to read (default 1 MB) to prevent huge output.
    """
    result = await _session.shell(
        _cid(), f"head -c {int(byte_limit)} -- {_q(path)}",
    )
    if result.exit_code != 0:
        return f"Error reading file: {result.stderr.strip()}"
    return result.stdout


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
    result = await _session.shell(_cid(), cmd)
    if result.exit_code != 0:
        return f"Error writing file: {result.stderr.strip()}"
    return f"Wrote {len(content)} bytes to {path}"


@mcp.tool()
async def list_directory(path: str = ".", show_hidden: bool = False) -> str:
    """List directory contents in the sandbox.

    Args:
        path: Directory path (default: working directory).
        show_hidden: Include hidden (dot) files.
    """
    flags = "-lhA" if show_hidden else "-lh"
    result = await _session.shell(_cid(), f"ls {flags} -- {_q(path)}")
    if result.exit_code != 0:
        return f"Error listing directory: {result.stderr.strip()}"
    return result.stdout


@mcp.tool()
async def create_directory(path: str) -> str:
    """Create a directory (and any missing parents) in the sandbox.

    Args:
        path: Directory path to create.
    """
    result = await _session.shell(_cid(), f"mkdir -p -- {_q(path)}")
    if result.exit_code != 0:
        return f"Error creating directory: {result.stderr.strip()}"
    return f"Created directory: {path}"


@mcp.tool()
async def remove(path: str, recursive: bool = False) -> str:
    """Remove a file or directory from the sandbox.

    Args:
        path: Path to remove.
        recursive: Remove directories and all their contents.
    """
    flags = "-rf" if recursive else "-f"
    result = await _session.shell(_cid(), f"rm {flags} -- {_q(path)}")
    if result.exit_code != 0:
        return f"Error removing: {result.stderr.strip()}"
    return f"Removed: {path}"


@mcp.tool()
async def move_file(source: str, destination: str) -> str:
    """Move or rename a file/directory in the sandbox.

    Args:
        source: Current path.
        destination: New path.
    """
    result = await _session.shell(
        _cid(), f"mv -- {_q(source)} {_q(destination)}",
    )
    if result.exit_code != 0:
        return f"Error moving: {result.stderr.strip()}"
    return f"Moved {source} -> {destination}"


@mcp.tool()
async def copy_file(source: str, destination: str) -> str:
    """Copy a file or directory in the sandbox.

    Args:
        source: Source path.
        destination: Destination path.
    """
    result = await _session.shell(
        _cid(), f"cp -r -- {_q(source)} {_q(destination)}",
    )
    if result.exit_code != 0:
        return f"Error copying: {result.stderr.strip()}"
    return f"Copied {source} -> {destination}"


@mcp.tool()
async def file_info(path: str) -> str:
    """Get metadata (type, size, permissions, timestamps) for a path in the sandbox.

    Args:
        path: Path to inspect.
    """
    cmd = f"stat -- {_q(path)} 2>&1; file -- {_q(path)} 2>&1"
    result = await _session.shell(_cid(), cmd)
    if result.exit_code != 0:
        return f"Error getting info: {result.stderr.strip()}"
    return result.stdout


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
        LOGGER.info("Client %s connected", cid)

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
                LOGGER.info("Client %s disconnected", cid)

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


def _install_uvloop() -> bool:
    try:
        import uvloop  # type: ignore[import-untyped]

        uvloop.install()
        LOGGER.info("Using uvloop event loop")
        return True
    except ImportError:
        LOGGER.info("uvloop not available, using default asyncio event loop")
        return False


def main() -> None:
    """CLI entry point for ``apiary-mcp``."""
    global _session, _mcp_auth_token

    _install_uvloop()

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
        "--image",
        default=os.getenv("APIARY_IMAGE", ""),
        required=not bool(os.getenv("APIARY_IMAGE")),
        help="Docker image name for sandbox sessions (required; also APIARY_IMAGE env)",
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
    parser.add_argument(
        "--backlog",
        type=int,
        default=2048,
        help="TCP listen backlog (default 2048)",
    )
    parser.add_argument(
        "--limit-concurrency",
        type=int,
        default=500,
        help="Max concurrent connections (default 500)",
    )
    args = parser.parse_args()

    _session = ApiaryMux(
        image=args.image,
        apiary_url=args.apiary_url,
        apiary_token=args.apiary_token,
        working_dir=args.working_dir,
        idle_timeout=args.idle_timeout,
    )
    _mcp_auth_token = args.mcp_token

    if args.transport == "stdio":
        mcp.run(transport="stdio")
    else:
        starlette_app = create_starlette_app(mcp._mcp_server, debug=True)
        uvicorn.run(
            starlette_app,
            host=args.host,
            port=args.port,
            backlog=args.backlog,
            limit_concurrency=args.limit_concurrency,
            timeout_keep_alive=30,
            log_level="info",
        )


if __name__ == "__main__":
    main()

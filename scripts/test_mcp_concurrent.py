#!/usr/bin/env python3
"""Concurrent agent stress test for Apiary + MCP.

Tests the sandbox pool and MCP server under load with many concurrent agents.
Each agent creates its own session, performs file and shell operations, and
verifies sandbox isolation from other agents.

Two test modes:
    apiary  — Direct REST API calls to the Apiary daemon.  Simple, reliable,
              best for measuring raw sandbox throughput.
    mcp     — Full MCP protocol via SSE transport.  Tests the entire stack
              including session management, JSON-RPC, and SSE streaming.

Usage:
    # Apiary direct mode (default)
    python test_mcp_concurrent.py --mode apiary --url http://127.0.0.1:8080

    # MCP SSE mode
    python test_mcp_concurrent.py --mode mcp --url http://127.0.0.1:8082

    # Custom concurrency levels and operations
    python test_mcp_concurrent.py --levels 1,5,10,20,50,100 --ops 20

    # Save results to JSON
    python test_mcp_concurrent.py --output results.json
"""

import argparse
import asyncio
import json
import statistics
import sys
import time
import uuid
from dataclasses import asdict, dataclass, field
from typing import Any, Optional

import httpx


# ---------------------------------------------------------------------------
# Data classes for results
# ---------------------------------------------------------------------------

@dataclass
class OpResult:
    agent_id: str
    operation: str
    latency_ms: float
    success: bool
    error: Optional[str] = None
    payload: Optional[str] = None


@dataclass
class AgentResult:
    agent_id: str
    total_time_ms: float
    session_create_ms: float = 0.0
    operations: list[OpResult] = field(default_factory=list)

    @property
    def success_count(self) -> int:
        return sum(1 for op in self.operations if op.success)

    @property
    def error_count(self) -> int:
        return sum(1 for op in self.operations if not op.success)


@dataclass
class LevelReport:
    concurrency: int
    total_time_ms: float
    agents: list[AgentResult] = field(default_factory=list)

    @property
    def all_latencies(self) -> list[float]:
        return [op.latency_ms for a in self.agents for op in a.operations if op.success]

    @property
    def total_ops(self) -> int:
        return sum(len(a.operations) for a in self.agents)

    @property
    def total_errors(self) -> int:
        return sum(a.error_count for a in self.agents)

    @property
    def throughput(self) -> float:
        if self.total_time_ms <= 0:
            return 0.0
        return self.total_ops / (self.total_time_ms / 1000.0)

    def percentile(self, p: float) -> float:
        lats = sorted(self.all_latencies)
        if not lats:
            return 0.0
        idx = min(int(len(lats) * p / 100.0), len(lats) - 1)
        return lats[idx]

    def summary(self) -> dict:
        lats = self.all_latencies
        if not lats:
            return {
                "concurrency": self.concurrency,
                "total_ops": self.total_ops,
                "total_errors": self.total_errors,
                "error_rate_pct": 100.0 if self.total_ops > 0 else 0,
                "throughput_ops_sec": 0,
                "wall_time_sec": round(self.total_time_ms / 1000, 2),
                "error": "no successful operations",
            }
        session_lats = [a.session_create_ms for a in self.agents if a.session_create_ms > 0]
        return {
            "concurrency": self.concurrency,
            "total_ops": self.total_ops,
            "total_errors": self.total_errors,
            "error_rate_pct": round(self.total_errors / max(self.total_ops, 1) * 100, 2),
            "throughput_ops_sec": round(self.throughput, 1),
            "latency_ms": {
                "mean": round(statistics.mean(lats), 1),
                "median": round(statistics.median(lats), 1),
                "p90": round(self.percentile(90), 1),
                "p95": round(self.percentile(95), 1),
                "p99": round(self.percentile(99), 1),
                "max": round(max(lats), 1),
                "min": round(min(lats), 1),
                "stdev": round(statistics.stdev(lats), 1) if len(lats) > 1 else 0,
            },
            "session_create_ms": {
                "mean": round(statistics.mean(session_lats), 1) if session_lats else 0,
                "max": round(max(session_lats), 1) if session_lats else 0,
            },
            "wall_time_sec": round(self.total_time_ms / 1000, 2),
        }


# ---------------------------------------------------------------------------
# Agent workload definitions
# ---------------------------------------------------------------------------

def build_apiary_workload(uid: str) -> list[tuple[str, str]]:
    """Operations for the Apiary direct API mode.  Each is (name, command)."""
    return [
        (f"write_marker", f"bash -c 'echo agent_{uid} > /workspace/marker_{uid}.txt'"),
        (f"read_marker", f"cat /workspace/marker_{uid}.txt"),
        ("compute", "bash -c 'echo $((42 * 137))'"),
        ("list_dir", "ls -la /workspace/"),
        ("env_check", "bash -c 'echo pid=$$ uid=$(id -u) hostname=$(hostname)'"),
        (f"write_data", f"bash -c 'for i in $(seq 1 10); do echo line_$i >> /workspace/data_{uid}.txt; done'"),
        (f"read_data", f"wc -l /workspace/data_{uid}.txt"),
        ("nested_shell", "bash -c 'bash -c \"echo nested_ok\"'"),
        (f"mkdir", f"mkdir -p /workspace/subdir_{uid}/a/b/c"),
        (f"tree", f"ls -R /workspace/subdir_{uid}/"),
        ("proc_info", "head -5 /proc/self/status"),
        (f"verify_isolation", f"bash -c 'content=$(cat /workspace/marker_{uid}.txt 2>/dev/null); [ \"$content\" = \"agent_{uid}\" ] && echo ISOLATION_OK || echo ISOLATION_FAIL'"),
    ]


def build_mcp_workload(uid: str) -> list[tuple[str, dict]]:
    """Operations for the MCP mode.  Each is (tool_name, arguments)."""
    return [
        ("shell_exec", {"command": f"echo agent_{uid} > /workspace/marker_{uid}.txt"}),
        ("read_file", {"path": f"/workspace/marker_{uid}.txt"}),
        ("shell_exec", {"command": "echo $((42 * 137))"}),
        ("list_directory", {"path": "/workspace"}),
        ("shell_exec", {"command": "echo pid=$$ uid=$(id -u)"}),
        ("write_file", {"path": f"/workspace/testfile_{uid}.txt", "content": f"hello from agent {uid}\n"}),
        ("read_file", {"path": f"/workspace/testfile_{uid}.txt"}),
        ("create_directory", {"path": f"/workspace/subdir_{uid}/nested"}),
        ("file_info", {"path": f"/workspace/testfile_{uid}.txt"}),
        ("shell_exec", {"command": f"cat /workspace/marker_{uid}.txt | grep -q 'agent_{uid}' && echo ISOLATION_OK || echo ISOLATION_FAIL"}),
        ("shell_exec", {"command": "bash -c 'for i in $(seq 1 5); do echo line_$i; done'"}),
        ("shell_exec", {"command": "uname -a"}),
    ]


# ---------------------------------------------------------------------------
# Apiary direct agent
# ---------------------------------------------------------------------------

class ApiaryAgent:
    """Agent that communicates directly with the Apiary REST API."""

    def __init__(self, agent_id: str, base_url: str, client: httpx.AsyncClient):
        self.agent_id = agent_id
        self.base_url = base_url.rstrip("/")
        self.client = client
        self.session_id: Optional[str] = None

    async def create_session(self) -> float:
        t0 = time.monotonic()
        resp = await self.client.post(
            f"{self.base_url}/api/v1/sessions",
            json={"working_dir": "/workspace"},
        )
        if resp.status_code >= 400:
            body = resp.text[:500]
            raise httpx.HTTPStatusError(
                f"HTTP {resp.status_code} for {resp.url}: {body}",
                request=resp.request,
                response=resp,
            )
        self.session_id = resp.json()["session_id"]
        return (time.monotonic() - t0) * 1000

    async def execute(self, command: str, timeout_ms: int = 30_000) -> dict:
        resp = await self.client.post(
            f"{self.base_url}/api/v1/tasks",
            json={
                "command": command,
                "session_id": self.session_id,
                "timeout_ms": timeout_ms,
            },
        )
        resp.raise_for_status()
        return resp.json()

    async def close_session(self):
        if self.session_id:
            try:
                await self.client.delete(
                    f"{self.base_url}/api/v1/sessions/{self.session_id}"
                )
            except Exception:
                pass

    async def run_workload(self, ops_per_agent: int) -> AgentResult:
        t_start = time.monotonic()
        result = AgentResult(agent_id=self.agent_id, total_time_ms=0)

        try:
            result.session_create_ms = await self.create_session()
        except Exception as e:
            result.total_time_ms = (time.monotonic() - t_start) * 1000
            result.operations.append(
                OpResult(self.agent_id, "create_session", 0, False, str(e))
            )
            return result

        uid = self.agent_id.replace("-", "")[:8]
        workload = build_apiary_workload(uid)

        for i in range(ops_per_agent):
            name, command = workload[i % len(workload)]
            t0 = time.monotonic()
            try:
                data = await self.execute(command)
                latency = (time.monotonic() - t0) * 1000
                exit_code = data.get("exit_code", -1)
                success = exit_code == 0
                error = data.get("stderr", "").strip() if not success else None
                result.operations.append(OpResult(
                    self.agent_id, name, latency, success, error,
                    payload=data.get("stdout", "").strip()[:200],
                ))
            except Exception as e:
                latency = (time.monotonic() - t0) * 1000
                result.operations.append(
                    OpResult(self.agent_id, name, latency, False, str(e))
                )

        await self.close_session()
        result.total_time_ms = (time.monotonic() - t_start) * 1000
        return result


# ---------------------------------------------------------------------------
# MCP SSE agent
# ---------------------------------------------------------------------------

class MCPAgent:
    """Agent that communicates via the MCP SSE protocol.

    Implements a lightweight MCP client using httpx for SSE streaming and
    JSON-RPC message passing — enough to exercise all MCP tools without
    pulling in the full MCP client SDK.
    """

    def __init__(self, agent_id: str, mcp_url: str, client: httpx.AsyncClient):
        self.agent_id = agent_id
        self.mcp_url = mcp_url.rstrip("/")
        self.client = client
        self._msg_endpoint: Optional[str] = None
        self._sse_response: Any = None
        self._request_id = 0
        self._pending: dict[int, asyncio.Future] = {}
        self._reader_task: Optional[asyncio.Task] = None
        self._initialized = False
        self._debug_first_chunk: Optional[str] = None

    async def _read_sse_events(self, response: httpx.Response):
        """Parse the SSE event stream and dispatch JSON-RPC responses."""
        try:
            buffer = ""
            async for chunk in response.aiter_text():
                if self._debug_first_chunk is None:
                    self._debug_first_chunk = chunk[:300]
                buffer += chunk.replace("\r\n", "\n").replace("\r", "\n")
                while "\n\n" in buffer:
                    event_text, buffer = buffer.split("\n\n", 1)
                    event_data = None
                    event_type = "message"
                    for line in event_text.split("\n"):
                        line = line.strip()
                        if line.startswith("event:"):
                            event_type = line[len("event:"):].strip()
                        elif line.startswith("data:"):
                            event_data = line[len("data:"):].strip()

                    if event_type == "endpoint" and event_data:
                        self._msg_endpoint = event_data
                    elif event_type == "message" and event_data:
                        try:
                            msg = json.loads(event_data)
                            req_id = msg.get("id")
                            if req_id is not None and req_id in self._pending:
                                self._pending[req_id].set_result(msg)
                        except (json.JSONDecodeError, asyncio.InvalidStateError):
                            pass
        except (httpx.ReadError, httpx.RemoteProtocolError, asyncio.CancelledError):
            pass
        except Exception:
            pass
        finally:
            err = ConnectionError("SSE stream closed")
            for future in self._pending.values():
                if not future.done():
                    future.set_exception(err)

    async def connect(self) -> float:
        """Open the SSE connection and complete the MCP handshake."""
        t0 = time.monotonic()
        url = f"{self.mcp_url}/sse?client_id={self.agent_id}"
        req = self.client.build_request("GET", url)
        self._sse_response = await self.client.send(req, stream=True)
        self._reader_task = asyncio.create_task(
            self._read_sse_events(self._sse_response)
        )

        # Wait for the server to send the message endpoint
        for _ in range(100):
            if self._msg_endpoint is not None:
                break
            await asyncio.sleep(0.05)
        if self._msg_endpoint is None:
            raise RuntimeError(
                f"SSE endpoint event not received within 5s "
                f"(first_data={self._debug_first_chunk!r})"
            )

        # MCP initialize handshake
        resp = await self._send_rpc("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": f"test-agent-{self.agent_id}",
                "version": "1.0.0",
            },
        })
        self._initialized = True

        # Acknowledge initialization
        await self._send_notification("notifications/initialized", {})
        return (time.monotonic() - t0) * 1000

    def _resolve_endpoint(self) -> str:
        ep = self._msg_endpoint or ""
        if ep.startswith("http"):
            return ep
        return f"{self.mcp_url}{ep}"

    async def _send_rpc(
        self, method: str, params: dict, timeout: float = 60.0
    ) -> dict:
        self._request_id += 1
        req_id = self._request_id

        loop = asyncio.get_event_loop()
        future: asyncio.Future = loop.create_future()
        self._pending[req_id] = future

        msg = {"jsonrpc": "2.0", "id": req_id, "method": method, "params": params}
        await self.client.post(
            self._resolve_endpoint(),
            content=json.dumps(msg),
            headers={"Content-Type": "application/json"},
        )

        try:
            return await asyncio.wait_for(future, timeout=timeout)
        finally:
            self._pending.pop(req_id, None)

    async def _send_notification(self, method: str, params: dict):
        msg = {"jsonrpc": "2.0", "method": method, "params": params}
        await self.client.post(
            self._resolve_endpoint(),
            content=json.dumps(msg),
            headers={"Content-Type": "application/json"},
        )

    async def call_tool(self, name: str, arguments: dict) -> dict:
        return await self._send_rpc("tools/call", {
            "name": name,
            "arguments": arguments,
        })

    async def disconnect(self):
        if self._reader_task:
            self._reader_task.cancel()
            try:
                await self._reader_task
            except (asyncio.CancelledError, Exception):
                pass
        if self._sse_response:
            await self._sse_response.aclose()

    @staticmethod
    def _extract_text(resp: dict) -> str:
        """Pull plain text from a tools/call response."""
        result = resp.get("result", {})
        if isinstance(result, str):
            return result.strip()
        content = result.get("content", []) if isinstance(result, dict) else []
        parts = []
        for item in content:
            if isinstance(item, dict) and item.get("type") == "text":
                parts.append(item.get("text", ""))
        return "\n".join(parts).strip()

    async def run_workload(self, ops_per_agent: int) -> AgentResult:
        t_start = time.monotonic()
        result = AgentResult(agent_id=self.agent_id, total_time_ms=0)

        try:
            result.session_create_ms = await self.connect()
        except Exception as e:
            result.total_time_ms = (time.monotonic() - t_start) * 1000
            result.operations.append(
                OpResult(self.agent_id, "connect", 0, False, str(e))
            )
            return result

        uid = self.agent_id.replace("-", "")[:8]
        workload = build_mcp_workload(uid)

        for i in range(ops_per_agent):
            tool_name, args = workload[i % len(workload)]
            t0 = time.monotonic()
            try:
                resp = await self.call_tool(tool_name, args)
                latency = (time.monotonic() - t0) * 1000
                if "error" in resp:
                    result.operations.append(OpResult(
                        self.agent_id, tool_name, latency, False,
                        str(resp["error"]),
                    ))
                else:
                    text = self._extract_text(resp)
                    result.operations.append(OpResult(
                        self.agent_id, tool_name, latency, True,
                        payload=text[:200],
                    ))
            except Exception as e:
                latency = (time.monotonic() - t0) * 1000
                result.operations.append(
                    OpResult(self.agent_id, tool_name, latency, False, str(e))
                )

        await self.disconnect()
        result.total_time_ms = (time.monotonic() - t_start) * 1000
        return result


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

async def check_health(url: str, path: str = "/healthz") -> tuple[bool, str]:
    """Returns (ok, detail) where detail describes the outcome."""
    try:
        async with httpx.AsyncClient(timeout=5.0) as c:
            r = await c.get(f"{url.rstrip('/')}{path}")
            if r.status_code == 200:
                return True, "OK"
            return False, f"HTTP {r.status_code}"
    except httpx.ConnectError:
        return False, "connection refused"
    except httpx.TimeoutException:
        return False, "timeout"
    except Exception as e:
        return False, str(e)


async def get_pool_status(url: str) -> Optional[dict]:
    try:
        async with httpx.AsyncClient(timeout=5.0) as c:
            r = await c.get(f"{url.rstrip('/')}/api/v1/status")
            if r.status_code == 200:
                return r.json()
    except Exception:
        pass
    return None


# ---------------------------------------------------------------------------
# Concurrency level runner
# ---------------------------------------------------------------------------

async def run_level(
    mode: str,
    url: str,
    concurrency: int,
    ops_per_agent: int,
) -> LevelReport:
    limits = httpx.Limits(
        max_connections=concurrency * 3 + 50,
        max_keepalive_connections=concurrency * 2 + 20,
    )
    timeout = httpx.Timeout(
        connect=30.0,
        read=120.0,
        write=30.0,
        pool=60.0,
    )

    t_start = time.monotonic()
    async with httpx.AsyncClient(timeout=timeout, limits=limits) as client:
        agents = []
        for _ in range(concurrency):
            aid = f"agent-{uuid.uuid4().hex[:8]}"
            if mode == "apiary":
                agents.append(ApiaryAgent(aid, url, client))
            else:
                agents.append(MCPAgent(aid, url, client))

        tasks = [a.run_workload(ops_per_agent) for a in agents]
        raw_results = await asyncio.gather(*tasks, return_exceptions=True)

    report = LevelReport(
        concurrency=concurrency,
        total_time_ms=(time.monotonic() - t_start) * 1000,
    )
    for r in raw_results:
        if isinstance(r, BaseException):
            report.agents.append(AgentResult(
                agent_id="unknown", total_time_ms=0,
                operations=[OpResult("unknown", "agent_error", 0, False, str(r))],
            ))
        else:
            report.agents.append(r)
    return report


# ---------------------------------------------------------------------------
# Isolation verification
# ---------------------------------------------------------------------------

async def run_isolation_test(mode: str, url: str, count: int = 5) -> dict:
    """Verify that sandboxes do not share filesystem state."""
    print("\n--- Isolation Test ---")
    limits = httpx.Limits(max_connections=count * 3 + 10, max_keepalive_connections=count + 5)
    timeout = httpx.Timeout(connect=15.0, read=60.0, write=15.0, pool=30.0)

    results: dict[str, dict] = {}

    async with httpx.AsyncClient(timeout=timeout, limits=limits) as client:
        agents: list = []
        for i in range(count):
            aid = f"iso-{i}"
            if mode == "apiary":
                agents.append(ApiaryAgent(aid, url, client))
            else:
                agents.append(MCPAgent(aid, url, client))

        # Phase 1: each agent creates a session and writes a unique marker
        for a in agents:
            try:
                if mode == "apiary":
                    await a.create_session()
                    await a.execute(f"bash -c 'echo SECRET_{a.agent_id} > /workspace/secret.txt'")
                else:
                    await a.connect()
                    await a.call_tool("shell_exec", {
                        "command": f"echo SECRET_{a.agent_id} > /workspace/secret.txt",
                    })
            except Exception as e:
                results[a.agent_id] = {"expected": f"SECRET_{a.agent_id}", "got": "", "error": str(e), "isolated": False}

        # Phase 2: each agent reads back its own marker
        for a in agents:
            if a.agent_id in results:
                continue
            expected = f"SECRET_{a.agent_id}"
            try:
                if mode == "apiary":
                    data = await a.execute("cat /workspace/secret.txt")
                    content = data.get("stdout", "").strip()
                else:
                    resp = await a.call_tool("read_file", {"path": "/workspace/secret.txt"})
                    content = MCPAgent._extract_text(resp)
            except Exception as e:
                content = f"ERROR: {e}"

            ok = content == expected
            results[a.agent_id] = {"expected": expected, "got": content, "isolated": ok}
            status = "PASS" if ok else "FAIL"
            print(f"  Agent {a.agent_id}: {status}  (expected={expected!r}, got={content!r})")

        # Cleanup
        for a in agents:
            try:
                if mode == "apiary":
                    await a.close_session()
                else:
                    await a.disconnect()
            except Exception:
                pass

    all_pass = all(r.get("isolated", False) for r in results.values())
    print(f"  Isolation: {'ALL PASSED' if all_pass else 'FAILED'}")
    if not all_pass:
        for aid, detail in results.items():
            if not detail.get("isolated"):
                err = detail.get("error", detail.get("got", "?"))
                print(f"    {aid}: {err[:200]}")
    return {"passed": all_pass, "details": results}


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def print_bar(width: int, label: str):
    bar = "=" * width
    print(f"\n{bar}")
    print(label)
    print(bar)


async def async_main():
    parser = argparse.ArgumentParser(
        description="Concurrent agent stress test for Apiary + MCP",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument(
        "--mode", choices=["apiary", "mcp"], default="apiary",
        help="Test mode (default: apiary)",
    )
    parser.add_argument(
        "--url", default=None,
        help="Service URL (default: http://127.0.0.1:8080 for apiary, :8082 for mcp)",
    )
    parser.add_argument(
        "--levels", default="1,5,10,20,50",
        help="Comma-separated concurrency levels (default: 1,5,10,20,50)",
    )
    parser.add_argument(
        "--ops", type=int, default=12,
        help="Operations per agent at each level (default: 12)",
    )
    parser.add_argument(
        "--skip-isolation", action="store_true",
        help="Skip the sandbox isolation verification test",
    )
    parser.add_argument(
        "--output", default=None,
        help="Path to write JSON results",
    )
    parser.add_argument(
        "--apiary-url", default="http://127.0.0.1:8080",
        help="Apiary daemon URL for pool status queries (used in both modes)",
    )
    args = parser.parse_args()

    if args.url is None:
        args.url = ("http://127.0.0.1:8080" if args.mode == "apiary"
                     else "http://127.0.0.1:8082")

    levels = [int(x.strip()) for x in args.levels.split(",") if x.strip()]

    print_bar(70, "Apiary / MCP Concurrent Agent Stress Test")
    print(f"  Mode:           {args.mode}")
    print(f"  URL:            {args.url}")
    print(f"  Apiary URL:     {args.apiary_url}")
    print(f"  Levels:         {levels}")
    print(f"  Ops per agent:  {args.ops}")

    # Health checks
    print("\n--- Health Check ---")
    apiary_ok, apiary_detail = await check_health(args.apiary_url)
    print(f"  Apiary daemon ({args.apiary_url}): {apiary_detail}")
    if not apiary_ok:
        print("  WARNING: cannot reach Apiary daemon. Tests may fail.")

    pool = await get_pool_status(args.apiary_url)
    if pool:
        print(f"  Pool: {json.dumps(pool)}")

    # Isolation test
    isolation_result: Optional[dict] = None
    if not args.skip_isolation:
        try:
            isolation_result = await run_isolation_test(args.mode, args.url)
        except Exception as e:
            print(f"  Isolation test error: {e}")
            isolation_result = {"passed": False, "error": str(e)}

    # Concurrency sweep
    all_level_summaries: list[dict] = []
    for level in levels:
        print_bar(50, f"Concurrency Level: {level}")

        pool_before = await get_pool_status(args.apiary_url)
        report = await run_level(args.mode, args.url, level, args.ops)
        pool_after = await get_pool_status(args.apiary_url)

        summary = report.summary()
        lat = summary.get("latency_ms", {})
        sess = summary.get("session_create_ms", {})

        print(f"  Total ops:       {summary.get('total_ops', 'N/A')}")
        print(f"  Errors:          {summary.get('total_errors', 0)} ({summary.get('error_rate_pct', 0)}%)")
        print(f"  Throughput:      {summary.get('throughput_ops_sec', 0)} ops/sec")
        if isinstance(lat, dict):
            print(f"  Latency (ms):    mean={lat.get('mean','?')}  med={lat.get('median','?')}  "
                  f"p90={lat.get('p90','?')}  p95={lat.get('p95','?')}  p99={lat.get('p99','?')}  max={lat.get('max','?')}")
        if isinstance(sess, dict) and sess.get("mean", 0) > 0:
            print(f"  Session create:  mean={sess.get('mean','?')} ms  max={sess.get('max','?')} ms")
        print(f"  Wall time:       {summary.get('wall_time_sec', '?')} sec")
        if pool_after:
            print(f"  Pool status:     {json.dumps(pool_after)}")

        # Check for isolation failures in results
        iso_failures = 0
        for agent in report.agents:
            for op in agent.operations:
                if op.success and op.payload and "ISOLATION_FAIL" in op.payload:
                    iso_failures += 1
        if iso_failures:
            print(f"  *** ISOLATION FAILURES DETECTED: {iso_failures} ***")

        all_level_summaries.append({
            "summary": summary,
            "pool_before": pool_before,
            "pool_after": pool_after,
            "isolation_failures": iso_failures,
        })

    # Final summary
    print_bar(70, "FINAL SUMMARY")

    final_results = {
        "mode": args.mode,
        "url": args.url,
        "ops_per_agent": args.ops,
        "isolation_test": isolation_result,
        "levels": all_level_summaries,
    }

    # Table view
    header = f"{'Conc':>6} {'Ops':>6} {'Errors':>7} {'Err%':>6} {'Tput':>10} {'Mean':>8} {'P90':>8} {'P99':>8} {'Max':>8} {'Wall':>8}"
    print(header)
    print("-" * len(header))
    for entry in all_level_summaries:
        s = entry["summary"]
        lat = s.get("latency_ms", {})
        print(
            f"{s.get('concurrency','?'):>6} "
            f"{s.get('total_ops','?'):>6} "
            f"{s.get('total_errors','?'):>7} "
            f"{s.get('error_rate_pct','?'):>5}% "
            f"{s.get('throughput_ops_sec','?'):>9} "
            f"{lat.get('mean','?') if isinstance(lat,dict) else '?':>8} "
            f"{lat.get('p90','?') if isinstance(lat,dict) else '?':>8} "
            f"{lat.get('p99','?') if isinstance(lat,dict) else '?':>8} "
            f"{lat.get('max','?') if isinstance(lat,dict) else '?':>8} "
            f"{s.get('wall_time_sec','?'):>7}s"
        )
    print()

    if args.output:
        with open(args.output, "w") as f:
            json.dump(final_results, f, indent=2, default=str)
        print(f"Results written to {args.output}")

    # Exit with failure if isolation broke
    if isolation_result and not isolation_result.get("passed", True):
        print("\nFAILED: sandbox isolation test did not pass")
        sys.exit(1)

    total_iso_failures = sum(e.get("isolation_failures", 0) for e in all_level_summaries)
    if total_iso_failures:
        print(f"\nWARNING: {total_iso_failures} isolation failures detected during load test")

    print("\nDone.")


def main():
    asyncio.run(async_main())


if __name__ == "__main__":
    main()

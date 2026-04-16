"""Python client for the Apiary sandbox pool.

``AsyncApiary`` / ``Apiary``
    Single-session clients with interpreter-wrapped execution,
    auto-recovery, and file-operation helpers.

``ApiaryMux``
    Multi-client async session pool with reference counting and idle
    reaper.

Quick start (sync)::

    from apiary_client import Apiary

    with Apiary(image="ubuntu:22.04") as s:
        result = s.execute("echo hello")
        print(result.stdout)

Quick start (async)::

    from apiary_client import AsyncApiary

    async with AsyncApiary(image="ubuntu:22.04") as s:
        result = await s.execute("echo hello")
        print(result.stdout)
"""

from apiary_client.session import Apiary, AsyncApiary, ApiaryMux, TaskResult

__all__ = [
    "Apiary",
    "AsyncApiary",
    "TaskResult",
    "ApiaryMux",
]

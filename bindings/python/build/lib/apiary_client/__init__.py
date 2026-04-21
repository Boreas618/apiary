"""Python client for the Apiary sandbox pool.

Two layers:

``Apiary`` / ``AsyncApiary``  (canonical client)
    Single entry point for image-set management, pool-wide admin, image
    job polling, and per-session emission. Most users only need this.

``ApiarySession`` / ``AsyncApiarySession`` / ``ApiarySessionMux``
    Per-session clients. Returned by ``apiary.session(image=...)``;
    can also be constructed directly for ad-hoc use.

Quick start (async, batch driver)::

    import asyncio
    from apiary_client import AsyncApiary

    async def main():
        async with AsyncApiary(
            apiary_url="http://127.0.0.1:8080",
            images=["ubuntu:22.04"],
        ) as apiary:
            async with apiary.session(image="ubuntu:22.04") as s:
                result = await s.execute("echo hello")
                print(result.stdout)

    asyncio.run(main())

Quick start (sync)::

    from apiary_client import Apiary

    with Apiary(apiary_url="http://127.0.0.1:8080", images=["ubuntu:22.04"]) as apiary:
        # apiary.session(...) returns an AsyncApiarySession; for a fully
        # sync per-session client wrap with ApiarySession directly.
        ...
"""

from apiary_client.apiary import Apiary, AsyncApiary
from apiary_client.session import (
    ApiarySession,
    ApiarySessionMux,
    AsyncApiarySession,
    ImageJobNotFound,
    ImageJobStatus,
    ImageProgress,
    RegisterResponse,
    TaskResult,
)

__all__ = [
    # Canonical client
    "Apiary",
    "AsyncApiary",
    # Per-session (returned by Apiary.session(...))
    "ApiarySession",
    "AsyncApiarySession",
    "ApiarySessionMux",
    # Result + image-job dataclasses
    "TaskResult",
    "RegisterResponse",
    "ImageJobStatus",
    "ImageProgress",
    # Exceptions
    "ImageJobNotFound",
]

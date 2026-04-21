"""Canonical Apiary client.

:class:`AsyncApiary` (and its sync facade :class:`Apiary`) is the
primary entry point of the Python bindings. A single ``Apiary``
instance ties together:

* a curated *image set* it loads (and optionally unloads) on
  ``__enter__`` / ``__exit__`` — `load`, `add`, `remove`;
* *pool-wide admin* operations against the daemon — `all_images`,
  `delete_image`, `status`, `health_check`;
* *low-level job submission/polling* for ad-hoc loads outside the
  curated set — `submit_load`, `job_status`, `wait_for_job`;
* a *session factory* — `session(image=...)` returning an
  :class:`apiary_client.session.AsyncApiarySession` bound to one of the
  loaded images.

Two usage shapes::

    # Batch driver (typical):
    async with AsyncApiary(
        apiary_url="http://127.0.0.1:8080",
        images=swebench_resolve("lite", "test"),
        on_progress=tqdm_progress,
    ) as apiary:
        for instance in instances:
            async with apiary.session(image=instance.image) as s:
                await s.execute(instance.command)

    # Pure admin (no image set):
    async with AsyncApiary(apiary_url="...") as apiary:
        print(await apiary.all_images())
        await apiary.delete_image("stale:tag")

Method-naming discipline distinguishes namespace-scoped vs pool-wide
operations:

* ``images`` / ``loaded`` (this Apiary's set) vs ``all_images``
  (pool-wide).
* ``remove`` (drop from set + pool) vs ``delete_image`` (pool-only;
  leaves ``self.images`` alone).

``unload_on_exit`` only affects ``self.loaded`` — images registered
through other paths are never touched.
"""

from __future__ import annotations

import asyncio
import logging
from collections.abc import Callable
from typing import Any

from apiary_client.session import (
    AsyncApiarySession,
    ImageJobNotFound,
    ImageJobStatus,
    RegisterResponse,
    _Transport,
)

logger = logging.getLogger(__name__)


# ======================================================================
# Async canonical client
# ======================================================================


class AsyncApiary:
    """Canonical async client for an Apiary daemon.

    Parameters
    ----------
    apiary_url:
        Base URL of a running Apiary daemon.
    apiary_token:
        Optional Bearer token for API authentication.
    images:
        Curated image set this Apiary manages. ``None`` or ``[]``
        skips the load step on ``__aenter__`` and turns the instance
        into a pure admin client.
    unload_on_exit:
        When ``True``, ``__aexit__`` issues a ``DELETE`` for every
        image in ``self.loaded``. Defaults to ``False`` so the layer
        cache stays warm across batches.
    load_timeout:
        Per-job timeout (seconds) for the auto-load on
        ``__aenter__`` / ``add()``. ``None`` means wait forever.
    on_progress:
        Optional callback invoked with each non-terminal
        :class:`ImageJobStatus` snapshot during polling. Useful for
        driving a tqdm-style progress bar.
    default_working_dir / default_env / default_timeout:
        Forwarded to :class:`AsyncApiarySession` on every call to
        :meth:`session`.
    """

    def __init__(
        self,
        *,
        apiary_url: str = "http://127.0.0.1:8080",
        apiary_token: str | None = None,
        images: list[str] | None = None,
        unload_on_exit: bool = False,
        load_timeout: float | None = None,
        on_progress: Callable[[ImageJobStatus], Any] | None = None,
        default_working_dir: str = "/workspace",
        default_env: dict[str, str] | None = None,
        default_timeout: int = 60,
    ):
        self._url = apiary_url
        self._token = apiary_token
        self._transport = _Transport(apiary_url, apiary_token)

        # De-duplicate while preserving caller-supplied order.
        if images:
            seen: set[str] = set()
            ordered = []
            for img in images:
                if img not in seen:
                    seen.add(img)
                    ordered.append(img)
            self._images: list[str] = ordered
        else:
            self._images = []

        self._unload_on_exit = unload_on_exit
        self._load_timeout = load_timeout
        self._on_progress = on_progress
        self._default_working_dir = default_working_dir
        self._default_env = dict(default_env) if default_env else None
        self._default_timeout = default_timeout

        # Populated by load() / add().
        self._loaded: list[str] = []
        self._failed: list[dict] = []

    # ------------------------------------------------------------------
    # This Apiary's image set (scope: self.images)
    # ------------------------------------------------------------------

    @property
    def images(self) -> list[str]:
        """Images this Apiary is configured to manage (the declared set)."""
        return list(self._images)

    @property
    def loaded(self) -> list[str]:
        """Subset of ``self.images`` that loaded successfully."""
        return list(self._loaded)

    @property
    def failed(self) -> list[dict]:
        """Per-image failure records, e.g. ``[{"name": ..., "reason": ...}]``."""
        return list(self._failed)

    async def load(self) -> ImageJobStatus | None:
        """Load every image in ``self.images``. No-op if the set is empty.

        Idempotent: already-registered images are reported as
        ``alreadypresent`` and skip the load pipeline. Updates
        ``self.loaded`` and ``self.failed`` in place.
        """

        if not self._images:
            self._loaded = []
            self._failed = []
            return None

        status = await self._submit_and_wait(self._images)
        self._absorb(status)
        return status

    async def add(self, images: list[str]) -> ImageJobStatus | None:
        """Add ``images`` to this Apiary's set and load them.

        Already-tracked images are kept; new ones are appended in order.
        """

        new_images: list[str] = []
        seen = set(self._images)
        for img in images:
            if img not in seen:
                self._images.append(img)
                new_images.append(img)
                seen.add(img)
        if not new_images:
            return None
        status = await self._submit_and_wait(new_images)
        self._absorb(status)
        return status

    async def remove(self, images: list[str]) -> None:
        """Remove ``images`` from this Apiary's set AND the pool registry.

        Use :meth:`delete_image` if you want to drop something from the
        pool but keep it in this Apiary's tracked set.
        """

        targets = list(images)
        await asyncio.gather(
            *(self._safe_delete_image(name) for name in targets),
            return_exceptions=False,
        )
        self._images = [img for img in self._images if img not in set(targets)]
        self._loaded = [img for img in self._loaded if img not in set(targets)]
        self._failed = [
            entry for entry in self._failed if entry.get("name") not in set(targets)
        ]

    # ------------------------------------------------------------------
    # Pool-wide admin (scope: entire daemon)
    # ------------------------------------------------------------------

    async def all_images(self) -> list[str]:
        """Every image registered with the daemon (not just this Apiary's set)."""
        return await self._transport.list_images()

    async def delete_image(self, image: str) -> None:
        """Delete ``image`` from the pool, even if it isn't in this Apiary's set."""
        await self._transport.unregister_image(image)

    async def delete_images(self, images: list[str]) -> None:
        """Issue parallel ``delete_image`` calls. Errors are aggregated."""
        results = await asyncio.gather(
            *(self._safe_delete_image(name) for name in images),
            return_exceptions=True,
        )
        errors = [r for r in results if isinstance(r, BaseException)]
        if errors:
            # Surface the first error; the rest are logged inside _safe_delete_image.
            raise errors[0]

    async def status(self) -> dict[str, Any]:
        """Pool status snapshot from ``GET /api/v1/status``."""
        return await self._transport.status()

    async def health_check(self, retries: int = 30, interval: float = 1.0) -> bool:
        """Block until ``/healthz`` returns 200 or *retries* attempts elapse."""
        for _ in range(retries):
            try:
                if await self._transport.health():
                    return True
            except Exception:
                pass
            await asyncio.sleep(interval)
        return False

    # ------------------------------------------------------------------
    # Low-level job submission/polling (escape hatch)
    # ------------------------------------------------------------------

    async def submit_load(
        self,
        images: list[str],
        *,
        wait: bool = True,
        poll: float = 2.0,
        timeout: float | None = None,
        on_progress: Callable[[ImageJobStatus], Any] | None = None,
    ) -> ImageJobStatus | RegisterResponse:
        """POST ``/api/v1/images`` without touching ``self.images``.

        Useful for ad-hoc loads outside this Apiary's tracked set. With
        ``wait=True`` (default) polls until the job is terminal and
        returns the :class:`ImageJobStatus`; with ``wait=False`` returns
        the raw :class:`RegisterResponse` so the caller can poll later.
        """

        ack = await self._transport.register_images(images)
        if not wait:
            return ack
        return await self.wait_for_job(
            ack.job_id,
            poll=poll,
            timeout=timeout,
            on_progress=on_progress or self._on_progress,
        )

    async def job_status(self, job_id: str) -> ImageJobStatus:
        """Single snapshot of an image-load job."""
        return await self._transport.image_job(job_id)

    async def wait_for_job(
        self,
        job_id: str,
        *,
        poll: float = 2.0,
        timeout: float | None = None,
        on_progress: Callable[[ImageJobStatus], Any] | None = None,
    ) -> ImageJobStatus:
        """Block until the named job reaches a terminal state."""
        return await self._transport.wait_for_image_job(
            job_id,
            poll=poll,
            timeout=timeout,
            on_progress=on_progress,
        )

    # ------------------------------------------------------------------
    # Session factory
    # ------------------------------------------------------------------

    def session(
        self,
        *,
        image: str,
        working_dir: str | None = None,
        env: dict[str, str] | None = None,
        timeout: int | None = None,
    ) -> AsyncApiarySession:
        """Construct an :class:`AsyncApiarySession` bound to ``image``.

        Raises ``ValueError`` if ``image`` is not in ``self.loaded`` —
        this catches typos and partial-load failures up front. To use an
        image outside this Apiary's set, call :meth:`submit_load` first
        or construct an :class:`AsyncApiarySession` directly.
        """

        if image not in self._loaded:
            if image in self._images:
                raise ValueError(
                    f"image {image!r} is in this Apiary's set but failed to load; "
                    f"check `failed` for the reason"
                )
            raise ValueError(
                f"image {image!r} is not in this Apiary's loaded set "
                f"({len(self._loaded)} loaded). Call submit_load() to register "
                f"it first, or construct AsyncApiarySession directly."
            )

        merged_env: dict[str, str] | None
        if env is None:
            merged_env = dict(self._default_env) if self._default_env else None
        else:
            merged_env = dict(self._default_env or {})
            merged_env.update(env)

        return AsyncApiarySession(
            image=image,
            apiary_url=self._url,
            apiary_token=self._token,
            working_dir=working_dir or self._default_working_dir,
            env=merged_env,
            timeout=timeout if timeout is not None else self._default_timeout,
        )

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def close(self) -> None:
        """Close the shared HTTP transport."""
        await self._transport.close()

    async def __aenter__(self) -> AsyncApiary:
        await self.load()
        return self

    async def __aexit__(self, *exc: Any) -> None:
        if self._unload_on_exit and self._loaded:
            await self.delete_images(list(self._loaded))
        await self.close()

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    async def _submit_and_wait(self, images: list[str]) -> ImageJobStatus:
        ack = await self._transport.register_images(images)
        return await self._transport.wait_for_image_job(
            ack.job_id,
            poll=2.0,
            timeout=self._load_timeout,
            on_progress=self._on_progress,
        )

    def _absorb(self, status: ImageJobStatus) -> None:
        """Project ``status`` onto ``self.loaded`` / ``self.failed``."""
        loaded_set = set(self._loaded)
        failed_index = {entry.get("name"): entry for entry in self._failed}

        for name, progress in status.per_image.items():
            if name not in self._images:
                # Was loaded via submit_load() outside the tracked set.
                continue
            if progress.is_success:
                if name not in loaded_set:
                    self._loaded.append(name)
                    loaded_set.add(name)
                failed_index.pop(name, None)
            elif progress.state == "failed":
                if name in loaded_set:
                    self._loaded.remove(name)
                    loaded_set.discard(name)
                failed_index[name] = {
                    "name": name,
                    "reason": progress.error or "unknown",
                }
        self._failed = list(failed_index.values())

    async def _safe_delete_image(self, image: str) -> None:
        try:
            await self._transport.unregister_image(image)
        except Exception:
            logger.warning(
                "delete_image failed for %s; continuing", image, exc_info=True,
            )
            raise


# ======================================================================
# Synchronous facade
# ======================================================================


class Apiary:
    """Synchronous facade over :class:`AsyncApiary`.

    Mirrors the async surface; runs everything inside a private event
    loop. Use this when you want the canonical client without writing
    ``async``/``await`` everywhere — typical for scripts and notebooks.
    """

    def __init__(
        self,
        *,
        apiary_url: str = "http://127.0.0.1:8080",
        apiary_token: str | None = None,
        images: list[str] | None = None,
        unload_on_exit: bool = False,
        load_timeout: float | None = None,
        on_progress: Callable[[ImageJobStatus], Any] | None = None,
        default_working_dir: str = "/workspace",
        default_env: dict[str, str] | None = None,
        default_timeout: int = 60,
    ):
        self._loop = asyncio.new_event_loop()
        self._async = AsyncApiary(
            apiary_url=apiary_url,
            apiary_token=apiary_token,
            images=images,
            unload_on_exit=unload_on_exit,
            load_timeout=load_timeout,
            on_progress=on_progress,
            default_working_dir=default_working_dir,
            default_env=default_env,
            default_timeout=default_timeout,
        )

    def _run(self, coro):  # noqa: ANN001
        return self._loop.run_until_complete(coro)

    # ---- Image set ---------------------------------------------------

    @property
    def images(self) -> list[str]:
        return self._async.images

    @property
    def loaded(self) -> list[str]:
        return self._async.loaded

    @property
    def failed(self) -> list[dict]:
        return self._async.failed

    def load(self) -> ImageJobStatus | None:
        return self._run(self._async.load())

    def add(self, images: list[str]) -> ImageJobStatus | None:
        return self._run(self._async.add(images))

    def remove(self, images: list[str]) -> None:
        return self._run(self._async.remove(images))

    # ---- Pool-wide admin --------------------------------------------

    def all_images(self) -> list[str]:
        return self._run(self._async.all_images())

    def delete_image(self, image: str) -> None:
        return self._run(self._async.delete_image(image))

    def delete_images(self, images: list[str]) -> None:
        return self._run(self._async.delete_images(images))

    def status(self) -> dict[str, Any]:
        return self._run(self._async.status())

    def health_check(self, retries: int = 30, interval: float = 1.0) -> bool:
        return self._run(self._async.health_check(retries, interval))

    # ---- Low-level job control --------------------------------------

    def submit_load(
        self,
        images: list[str],
        *,
        wait: bool = True,
        poll: float = 2.0,
        timeout: float | None = None,
        on_progress: Callable[[ImageJobStatus], Any] | None = None,
    ) -> ImageJobStatus | RegisterResponse:
        return self._run(
            self._async.submit_load(
                images,
                wait=wait,
                poll=poll,
                timeout=timeout,
                on_progress=on_progress,
            )
        )

    def job_status(self, job_id: str) -> ImageJobStatus:
        return self._run(self._async.job_status(job_id))

    def wait_for_job(
        self,
        job_id: str,
        *,
        poll: float = 2.0,
        timeout: float | None = None,
        on_progress: Callable[[ImageJobStatus], Any] | None = None,
    ) -> ImageJobStatus:
        return self._run(
            self._async.wait_for_job(
                job_id, poll=poll, timeout=timeout, on_progress=on_progress
            )
        )

    # ---- Session factory --------------------------------------------

    def session(
        self,
        *,
        image: str,
        working_dir: str | None = None,
        env: dict[str, str] | None = None,
        timeout: int | None = None,
    ) -> AsyncApiarySession:
        """Construct an :class:`AsyncApiarySession`.

        The returned object is async even from the sync facade — wrap
        with :class:`apiary_client.session.ApiarySession` if you need a
        sync per-session client too.
        """
        return self._async.session(
            image=image,
            working_dir=working_dir,
            env=env,
            timeout=timeout,
        )

    # ---- Lifecycle ---------------------------------------------------

    def close(self) -> None:
        self._run(self._async.close())
        if not self._loop.is_closed():
            self._loop.close()

    def __enter__(self) -> Apiary:
        self._run(self._async.load())
        return self

    def __exit__(self, *exc: Any) -> None:
        try:
            if self._async._unload_on_exit and self._async._loaded:
                self._run(
                    self._async.delete_images(list(self._async._loaded))
                )
        finally:
            self.close()

    def __del__(self) -> None:
        try:
            if getattr(self, "_loop", None) and not self._loop.is_closed():
                self._run(self._async.close())
                self._loop.close()
        except Exception:
            pass


__all__ = [
    "Apiary",
    "AsyncApiary",
    "ImageJobNotFound",
]

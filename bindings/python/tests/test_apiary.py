"""Unit tests for the canonical ``AsyncApiary`` client.

The tests substitute a fake ``_Transport`` (no HTTP) so they can run
without a live daemon. Coverage focuses on the surface that the plan
calls out: image-set tracking, partial-load tolerance, the
namespace-vs-pool method split, and ``unload_on_exit`` semantics.
"""

from __future__ import annotations

import asyncio
import unittest
from typing import Any

from apiary_client import (
    AsyncApiary,
    AsyncApiarySession,
    ImageJobStatus,
    ImageProgress,
    RegisterResponse,
)


def run(coro):
    loop = asyncio.new_event_loop()
    try:
        return loop.run_until_complete(coro)
    finally:
        loop.close()


class FakeTransport:
    """In-memory stand-in for ``_Transport`` used by ``AsyncApiary`` tests.

    Tracks register/unregister calls, fakes job status as terminal-on-poll,
    and surfaces a ``registry`` set the test harness can pre-seed.
    """

    def __init__(self) -> None:
        self.registry: set[str] = set()
        self.failures: dict[str, str] = {}  # image -> reason; otherwise success
        self.register_calls: list[list[str]] = []
        self.delete_calls: list[str] = []
        self.list_calls = 0
        self.status_calls = 0
        self.health_calls = 0
        self.closed = False
        self._next_job_id = 0
        self._jobs: dict[str, ImageJobStatus] = {}

    async def register_images(self, images: list[str]) -> RegisterResponse:
        self.register_calls.append(list(images))
        already_present = [img for img in images if img in self.registry]
        queued = [img for img in images if img not in self.registry]
        # Synchronously "load" everything, applying optional failures.
        for img in queued:
            if img in self.failures:
                continue
            self.registry.add(img)
        self._next_job_id += 1
        job_id = f"job-{self._next_job_id}"
        self._jobs[job_id] = self._build_terminal_status(job_id, queued, already_present)
        return RegisterResponse(
            job_id=job_id,
            queued=queued,
            already_present=already_present,
        )

    def _build_terminal_status(
        self,
        job_id: str,
        queued: list[str],
        already_present: list[str],
    ) -> ImageJobStatus:
        per_image: dict[str, ImageProgress] = {}
        failed_images: list[dict] = []
        any_success = False
        any_failure = False
        for img in already_present:
            per_image[img] = ImageProgress(state="alreadypresent")
            any_success = True
        for img in queued:
            if img in self.failures:
                reason = self.failures[img]
                per_image[img] = ImageProgress(state="failed", error=reason)
                failed_images.append({"name": img, "reason": reason})
                any_failure = True
            else:
                per_image[img] = ImageProgress(state="done")
                any_success = True
        if any_success:
            state = "done"
        elif any_failure:
            state = "failed"
        else:
            state = "done"
        return ImageJobStatus(
            job_id=job_id,
            state=state,
            started_at="2026-01-01T00:00:00Z",
            updated_at="2026-01-01T00:00:01Z",
            per_image=per_image,
            failed_images=failed_images,
        )

    async def image_job(self, job_id: str) -> ImageJobStatus:
        return self._jobs[job_id]

    async def wait_for_image_job(
        self,
        job_id: str,
        *,
        poll: float = 2.0,
        timeout: float | None = None,
        on_progress: Any = None,
    ) -> ImageJobStatus:
        # Job is already terminal in this fake, so wait is a no-op.
        return self._jobs[job_id]

    async def list_images(self) -> list[str]:
        self.list_calls += 1
        return sorted(self.registry)

    async def unregister_image(self, name: str) -> None:
        self.delete_calls.append(name)
        if name not in self.registry:
            raise RuntimeError(f"image not registered: {name}")
        self.registry.discard(name)

    async def status(self) -> dict[str, Any]:
        self.status_calls += 1
        return {"total": 0, "registered_images": len(self.registry)}

    async def health(self) -> bool:
        self.health_calls += 1
        return True

    async def close(self) -> None:
        self.closed = True


def make_async_apiary(
    *,
    images: list[str] | None = None,
    unload_on_exit: bool = False,
    failures: dict[str, str] | None = None,
    pre_registered: list[str] | None = None,
) -> tuple[AsyncApiary, FakeTransport]:
    apiary = AsyncApiary(
        apiary_url="http://test.invalid",
        images=images,
        unload_on_exit=unload_on_exit,
    )
    fake = FakeTransport()
    if pre_registered:
        for img in pre_registered:
            fake.registry.add(img)
    if failures:
        fake.failures.update(failures)
    apiary._transport = fake  # type: ignore[attr-defined]
    return apiary, fake


# ---------------------------------------------------------------------------
# Construction & deduplication
# ---------------------------------------------------------------------------


class ConstructionTests(unittest.TestCase):
    def test_dedupes_images_preserving_first_occurrence_order(self):
        apiary = AsyncApiary(
            apiary_url="http://x",
            images=["b", "a", "b", "c", "a"],
        )
        self.assertEqual(apiary.images, ["b", "a", "c"])

    def test_none_images_means_pure_admin(self):
        apiary = AsyncApiary(apiary_url="http://x")
        self.assertEqual(apiary.images, [])
        self.assertEqual(apiary.loaded, [])
        self.assertEqual(apiary.failed, [])

    def test_empty_list_images_also_means_pure_admin(self):
        apiary = AsyncApiary(apiary_url="http://x", images=[])
        self.assertEqual(apiary.images, [])


# ---------------------------------------------------------------------------
# load() / add() / remove()
# ---------------------------------------------------------------------------


class LoadTests(unittest.TestCase):
    def test_load_registers_all_and_populates_loaded(self):
        apiary, fake = make_async_apiary(images=["a", "b"])
        status = run(apiary.load())
        assert status is not None
        self.assertEqual(status.state, "done")
        self.assertEqual(set(apiary.loaded), {"a", "b"})
        self.assertEqual(apiary.failed, [])
        self.assertEqual(fake.register_calls, [["a", "b"]])

    def test_load_no_op_when_image_set_empty(self):
        apiary, fake = make_async_apiary(images=None)
        status = run(apiary.load())
        self.assertIsNone(status)
        self.assertEqual(apiary.loaded, [])
        self.assertEqual(fake.register_calls, [])

    def test_partial_failure_partitions_loaded_and_failed(self):
        apiary, fake = make_async_apiary(
            images=["good", "bad"],
            failures={"bad": "boom"},
        )
        status = run(apiary.load())
        assert status is not None
        # Job state is "done" because at least one image succeeded.
        self.assertEqual(status.state, "done")
        self.assertEqual(apiary.loaded, ["good"])
        self.assertEqual(len(apiary.failed), 1)
        self.assertEqual(apiary.failed[0]["name"], "bad")
        self.assertEqual(apiary.failed[0]["reason"], "boom")

    def test_already_present_short_circuit(self):
        apiary, fake = make_async_apiary(
            images=["seeded"],
            pre_registered=["seeded"],
        )
        status = run(apiary.load())
        assert status is not None
        self.assertEqual(apiary.loaded, ["seeded"])
        # The fake reports it as already_present.
        self.assertEqual(
            status.per_image["seeded"].state,
            "alreadypresent",
        )

    def test_add_extends_image_set_and_loads_only_new(self):
        apiary, fake = make_async_apiary(images=["a"])
        run(apiary.load())
        self.assertEqual(fake.register_calls, [["a"]])

        status = run(apiary.add(["b", "a"]))  # "a" already tracked → ignored
        self.assertEqual(apiary.images, ["a", "b"])
        self.assertEqual(set(apiary.loaded), {"a", "b"})
        # Second register only for the new "b".
        self.assertEqual(fake.register_calls, [["a"], ["b"]])
        assert status is not None

    def test_add_no_op_when_all_images_already_tracked(self):
        apiary, fake = make_async_apiary(images=["a"])
        status = run(apiary.add(["a"]))
        self.assertIsNone(status)
        # No registration call beyond what was needed.
        self.assertEqual(fake.register_calls, [])


class RemoveTests(unittest.TestCase):
    def test_remove_drops_from_namespace_and_pool(self):
        apiary, fake = make_async_apiary(images=["a", "b"])
        run(apiary.load())
        run(apiary.remove(["b"]))

        self.assertEqual(apiary.images, ["a"])
        self.assertEqual(apiary.loaded, ["a"])
        self.assertEqual(fake.delete_calls, ["b"])
        self.assertNotIn("b", fake.registry)


# ---------------------------------------------------------------------------
# Pool-wide vs namespace scope
# ---------------------------------------------------------------------------


class ScopingTests(unittest.TestCase):
    def test_all_images_returns_full_pool_inventory(self):
        apiary, fake = make_async_apiary(
            images=["mine"],
            pre_registered=["someone-else"],
        )
        run(apiary.load())
        # The pool has both, but apiary.images only has "mine".
        all_imgs = run(apiary.all_images())
        self.assertEqual(set(all_imgs), {"mine", "someone-else"})
        self.assertEqual(apiary.images, ["mine"])

    def test_delete_image_does_not_touch_self_images(self):
        apiary, fake = make_async_apiary(images=["a"])
        run(apiary.load())
        # delete_image() targets the pool; self.images stays.
        run(apiary.delete_image("a"))
        self.assertEqual(apiary.images, ["a"], "namespace tracking should not change")
        self.assertEqual(apiary.loaded, ["a"], "loaded reflects last load(), not pool")
        self.assertNotIn("a", fake.registry, "pool no longer has 'a'")
        self.assertEqual(fake.delete_calls, ["a"])


# ---------------------------------------------------------------------------
# session() factory
# ---------------------------------------------------------------------------


class SessionFactoryTests(unittest.TestCase):
    def test_session_returns_async_apiary_session(self):
        apiary, _ = make_async_apiary(images=["ubuntu:22.04"])
        run(apiary.load())
        sess = apiary.session(image="ubuntu:22.04")
        self.assertIsInstance(sess, AsyncApiarySession)
        # Defaults flow through.
        self.assertEqual(sess._image, "ubuntu:22.04")
        self.assertEqual(sess._working_dir, "/workspace")

    def test_session_raises_when_image_not_in_loaded(self):
        apiary, _ = make_async_apiary(images=["a"])
        run(apiary.load())
        with self.assertRaises(ValueError) as ctx:
            apiary.session(image="b")
        self.assertIn("not in this Apiary's loaded set", str(ctx.exception))

    def test_session_raises_with_specific_message_when_image_failed(self):
        apiary, _ = make_async_apiary(
            images=["a", "b"], failures={"b": "broken"},
        )
        run(apiary.load())
        with self.assertRaises(ValueError) as ctx:
            apiary.session(image="b")
        # Message should distinguish "in set but failed" from "not in set".
        msg = str(ctx.exception)
        self.assertIn("failed to load", msg)


# ---------------------------------------------------------------------------
# Context manager + unload_on_exit
# ---------------------------------------------------------------------------


class ContextManagerTests(unittest.TestCase):
    def test_aexit_does_not_unload_by_default(self):
        async def go():
            apiary, fake = make_async_apiary(images=["a", "b"])
            async with apiary as ns:
                self.assertEqual(set(ns.loaded), {"a", "b"})
            # Default unload_on_exit=False.
            self.assertEqual(fake.delete_calls, [])
            self.assertTrue(fake.closed)
            self.assertIn("a", fake.registry)
            self.assertIn("b", fake.registry)
        run(go())

    def test_aexit_unloads_only_self_loaded(self):
        async def go():
            apiary, fake = make_async_apiary(
                images=["mine-a", "mine-b"],
                pre_registered=["external"],
                unload_on_exit=True,
            )
            async with apiary:
                pass
            # Only the apiary's own images get deleted.
            self.assertEqual(set(fake.delete_calls), {"mine-a", "mine-b"})
            self.assertNotIn("mine-a", fake.registry)
            self.assertNotIn("mine-b", fake.registry)
            # External image untouched.
            self.assertIn("external", fake.registry)
        run(go())

    def test_aenter_no_op_for_admin_only_apiary(self):
        async def go():
            apiary, fake = make_async_apiary(images=None)
            async with apiary as ns:
                self.assertEqual(ns.loaded, [])
                # No registration was attempted.
                self.assertEqual(fake.register_calls, [])
        run(go())


# ---------------------------------------------------------------------------
# submit_load() escape hatch
# ---------------------------------------------------------------------------


class SubmitLoadTests(unittest.TestCase):
    def test_submit_load_does_not_touch_self_images(self):
        apiary, fake = make_async_apiary(images=["a"])
        run(apiary.load())
        # Submit an ad-hoc image outside the apiary's tracked set.
        result = run(apiary.submit_load(["b"]))
        # Result is the terminal status (we passed wait=True default).
        self.assertIsInstance(result, ImageJobStatus)
        # Self.images unchanged; apiary.loaded unchanged.
        self.assertEqual(apiary.images, ["a"])
        self.assertEqual(apiary.loaded, ["a"])
        # But the pool now has "b".
        self.assertIn("b", fake.registry)

    def test_submit_load_no_wait_returns_register_response(self):
        apiary, fake = make_async_apiary()
        result = run(apiary.submit_load(["x"], wait=False))
        self.assertIsInstance(result, RegisterResponse)
        self.assertEqual(result.queued, ["x"])


if __name__ == "__main__":
    unittest.main()

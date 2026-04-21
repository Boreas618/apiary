"""Unit tests for the SWE-bench resolver helper."""

from __future__ import annotations

import unittest

from apiary_client.swebench.load import (
    DATASET_ALIASES,
    get_docker_image,
    resolve,
)


class DatasetAliasTests(unittest.TestCase):
    def test_known_aliases(self):
        for key in ("lite", "full", "verified", "multimodal", "multilingual"):
            self.assertIn(key, DATASET_ALIASES, f"{key!r} should be an alias")
        # Spot check: lite points at the conventional repo id.
        self.assertEqual(DATASET_ALIASES["lite"], "princeton-nlp/SWE-bench_Lite")


class GetDockerImageTests(unittest.TestCase):
    def test_explicit_image_name_wins(self):
        instance = {
            "image_name": "  ghcr.io/foo/bar:tag  ",
            "instance_id": "ignored",
        }
        self.assertEqual(get_docker_image(instance), "ghcr.io/foo/bar:tag")

    def test_docker_image_field_used_when_image_name_missing(self):
        instance = {
            "docker_image": "registry.io/baz:1.2",
            "instance_id": "ignored",
        }
        self.assertEqual(get_docker_image(instance), "registry.io/baz:1.2")

    def test_falls_back_to_instance_id_pattern(self):
        instance = {"instance_id": "django__django-11099"}
        # __ is replaced with _1776_ to dodge namespace conflicts.
        self.assertEqual(
            get_docker_image(instance),
            "docker.io/swebench/sweb.eval.x86_64.django_1776_django-11099:latest",
        )

    def test_lowercases_derived_image_name(self):
        instance = {"instance_id": "Foo__Bar-Baz"}
        self.assertEqual(
            get_docker_image(instance),
            "docker.io/swebench/sweb.eval.x86_64.foo_1776_bar-baz:latest",
        )

    def test_missing_instance_id_raises(self):
        with self.assertRaises(ValueError):
            get_docker_image({})


class ResolveBatchingTests(unittest.TestCase):
    """Test the batching logic by stubbing :func:`load_instances`."""

    def setUp(self):
        # Inject a fake load path so the test doesn't hit HuggingFace.
        from apiary_client.swebench import load as swload
        self._original_load_instances = swload.load_instances
        # Build 7 instances that yield 7 distinct images.
        instances = [{"instance_id": f"proj__case-{i:02d}"} for i in range(7)]
        swload.load_instances = lambda dataset, split: instances

    def tearDown(self):
        from apiary_client.swebench import load as swload
        swload.load_instances = self._original_load_instances

    def test_full_resolve_returns_all_unique_sorted(self):
        images = resolve("dummy", "test")
        self.assertEqual(len(images), 7)
        self.assertEqual(images, sorted(images), "should be sorted")

    def test_batching_returns_expected_slice(self):
        # batch_size=3, batch_id=1 → indices 3..6 of a 7-element list.
        images = resolve("dummy", "test", batch_size=3, batch_id=1)
        self.assertEqual(len(images), 3)
        # Verify slicing is consistent with full resolve.
        full = resolve("dummy", "test")
        self.assertEqual(images, full[3:6])

    def test_batching_last_batch_is_partial(self):
        # batch_size=3, batch_id=2 → indices 6..7 (only 1 image left).
        images = resolve("dummy", "test", batch_size=3, batch_id=2)
        self.assertEqual(len(images), 1)
        full = resolve("dummy", "test")
        self.assertEqual(images, full[6:7])

    def test_batch_id_out_of_range_raises_value_error(self):
        with self.assertRaises(ValueError):
            resolve("dummy", "test", batch_size=3, batch_id=99)


if __name__ == "__main__":
    unittest.main()

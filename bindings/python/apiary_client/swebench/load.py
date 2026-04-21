#!/usr/bin/env python3
"""Resolve a SWE-bench dataset to Docker image names and load them into Apiary.

Two layers:

* :func:`resolve` is a pure function — given a dataset id (or alias, or
  local JSON/JSONL path) it returns a deduplicated, sorted list of
  Docker image names. No HTTP traffic to Apiary; useful in tests and
  for offline pipelines.

* The ``apiary-load-swebench`` CLI builds an :class:`AsyncApiary` from
  ``resolve(...)`` and submits/polls the load job, printing per-image
  progress along the way.

Examples::

    # Default: load SWE-bench Lite test split into a daemon.
    apiary-load-swebench --apiary-url http://127.0.0.1:8080

    # Verified, only the first 50 images.
    apiary-load-swebench --dataset verified --batch-size 50 --batch-id 0

    # Submit and exit without polling.
    apiary-load-swebench --no-wait

    # Pure resolve, no daemon contact.
    apiary-load-swebench --print-only

    # Local instance file.
    apiary-load-swebench --dataset ./instances.jsonl
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import sys
from pathlib import Path

from apiary_client.apiary import AsyncApiary
from apiary_client.session import ImageJobStatus, RegisterResponse

logger = logging.getLogger(__name__)


DATASET_ALIASES: dict[str, str] = {
    "full": "princeton-nlp/SWE-bench",
    "verified": "princeton-nlp/SWE-bench_Verified",
    "lite": "princeton-nlp/SWE-bench_Lite",
    "multimodal": "princeton-nlp/SWE-bench_Multimodal",
    "multilingual": "swe-bench/SWE-Bench_Multilingual",
}


# ---------------------------------------------------------------------------
# Pure resolution (no apiary I/O)
# ---------------------------------------------------------------------------


def get_docker_image(instance: dict) -> str:
    """Derive the Docker image name from a SWE-bench instance.

    Priority: explicit ``image_name`` > ``docker_image`` > derived from
    ``instance_id`` using the canonical ``swebench/sweb.eval.x86_64.*``
    pattern.
    """

    if name := instance.get("image_name"):
        if name.strip():
            return name.strip()
    if name := instance.get("docker_image"):
        if name.strip():
            return name.strip()
    iid = instance.get("instance_id", "")
    if not iid:
        raise ValueError(f"instance missing instance_id: {instance}")
    id_compat = iid.replace("__", "_1776_")
    return f"docker.io/swebench/sweb.eval.x86_64.{id_compat}:latest".lower()


def load_instances_local(path: Path) -> list[dict]:
    """Load instances from a local JSON or JSONL file."""

    text = path.read_text().strip()
    if path.suffix == ".jsonl":
        return [json.loads(line) for line in text.splitlines() if line.strip()]
    if text.startswith("["):
        return json.loads(text)
    obj = json.loads(text)
    if isinstance(obj, dict):
        return list(obj.values())
    return obj


def load_instances_hf(dataset: str, split: str) -> list[dict]:
    """Load instances from HuggingFace using the ``datasets`` library."""

    try:
        from datasets import load_dataset
    except ImportError:
        print(
            "ERROR: `datasets` package not installed. Install with:\n"
            "  pip install apiary-client[swebench]\n"
            "Or pass a local JSON/JSONL file path to --dataset instead.",
            file=sys.stderr,
        )
        sys.exit(1)

    repo_id = DATASET_ALIASES.get(dataset, dataset)
    logger.info("Loading dataset %s split=%s from HuggingFace...", repo_id, split)
    ds = load_dataset(repo_id, split=split)
    return [dict(row) for row in ds]


def load_instances(dataset: str, split: str) -> list[dict]:
    path = Path(dataset)
    if path.exists():
        return load_instances_local(path)
    return load_instances_hf(dataset, split)


def resolve(
    dataset: str = "lite",
    split: str = "test",
    *,
    batch_size: int = 0,
    batch_id: int = 0,
) -> list[str]:
    """Resolve a SWE-bench dataset to a sorted, deduplicated image list.

    ``dataset`` is an alias from :data:`DATASET_ALIASES`, a HuggingFace
    repo id, or a local JSON/JSONL file path. ``batch_size`` and
    ``batch_id`` slice the resolved list for parallel deployment
    (``batch_size=0`` returns the full list).
    """

    instances = load_instances(dataset, split)
    if not instances:
        raise RuntimeError(
            f"no instances loaded from dataset={dataset!r} split={split!r}"
        )

    images = sorted({get_docker_image(inst) for inst in instances})
    total_unique = len(images)
    logger.info(
        "%d instances → %d unique images (dataset=%s split=%s)",
        len(instances),
        total_unique,
        dataset,
        split,
    )

    if batch_size <= 0:
        return images

    start = batch_id * batch_size
    if start >= len(images):
        num_batches = (len(images) + batch_size - 1) // batch_size
        raise ValueError(
            f"batch slice empty: batch_id={batch_id} batch_size={batch_size} "
            f"but only {len(images)} unique images (use batch_id in 0..{num_batches})"
        )
    end = min(start + batch_size, len(images))
    sliced = images[start:end]
    logger.info(
        "batch mode: batch_id=%d batch_size=%d → images [%d..%d) of %d unique",
        batch_id,
        batch_size,
        start,
        end,
        total_unique,
    )
    return sliced


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def _print_progress(status: ImageJobStatus) -> None:
    """Console-friendly progress printer for the CLI poll loop."""

    counts = {"queued": 0, "pulling": 0, "extracting": 0, "done": 0,
              "alreadypresent": 0, "failed": 0}
    for prog in status.per_image.values():
        counts[prog.state] = counts.get(prog.state, 0) + 1
    total = sum(counts.values())
    finished = counts["done"] + counts["alreadypresent"] + counts["failed"]
    print(
        f"[apiary-load-swebench] job={status.job_id[:8]} "
        f"state={status.state} progress={finished}/{total} "
        f"(done={counts['done']}, present={counts['alreadypresent']}, "
        f"pulling={counts['pulling']}, extracting={counts['extracting']}, "
        f"queued={counts['queued']}, failed={counts['failed']})",
        flush=True,
    )


def _build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Resolve a SWE-bench dataset and submit the unique image set to "
            "an Apiary daemon for runtime registration."
        ),
    )
    parser.add_argument(
        "--apiary-url",
        default=os.getenv("APIARY_URL", "http://127.0.0.1:8080"),
        help="Apiary daemon URL (default: $APIARY_URL or http://127.0.0.1:8080)",
    )
    parser.add_argument(
        "--apiary-token",
        default=os.getenv("APIARY_API_TOKEN"),
        help="Apiary daemon Bearer token (default: $APIARY_API_TOKEN)",
    )
    parser.add_argument(
        "--dataset",
        default="lite",
        help=(
            "Dataset alias (lite, full, verified, multimodal, multilingual), "
            "HuggingFace id, or path to JSON/JSONL"
        ),
    )
    parser.add_argument("--split", default="test", help="HuggingFace split")
    parser.add_argument(
        "--lite-dev",
        action="store_true",
        help="Shorthand for --dataset lite --split dev",
    )
    parser.add_argument("--batch-size", type=int, default=0, help="Images per batch (0 = all)")
    parser.add_argument("--batch-id", type=int, default=0, help="Which batch (0-based)")
    parser.add_argument(
        "--no-wait",
        action="store_true",
        help="Submit the load job and exit without polling for completion",
    )
    parser.add_argument(
        "--poll-interval",
        type=float,
        default=2.0,
        help="Seconds between job-status polls when waiting (default: 2.0)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=None,
        help="Maximum seconds to wait for the load job (default: no timeout)",
    )
    parser.add_argument(
        "--print-only",
        action="store_true",
        help="Resolve and print image names to stdout; do not contact Apiary",
    )
    return parser


def main() -> None:
    parser = _build_arg_parser()
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
    )

    if args.lite_dev:
        args.dataset = "lite"
        args.split = "dev"

    try:
        images = resolve(
            args.dataset,
            args.split,
            batch_size=args.batch_size,
            batch_id=args.batch_id,
        )
    except (RuntimeError, ValueError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        sys.exit(1)

    if not images:
        print("ERROR: no images to load", file=sys.stderr)
        sys.exit(1)

    if args.print_only:
        for img in images:
            print(img)
        return

    print(
        f"[apiary-load-swebench] submitting {len(images)} images to {args.apiary_url}"
    )
    asyncio.run(_run_load(args, images))


async def _run_load(args: argparse.Namespace, images: list[str]) -> None:
    apiary = AsyncApiary(
        apiary_url=args.apiary_url,
        apiary_token=args.apiary_token,
    )
    try:
        if args.no_wait:
            ack: RegisterResponse = await apiary.submit_load(images, wait=False)
            print(
                f"[apiary-load-swebench] submitted job_id={ack.job_id} "
                f"queued={len(ack.queued)} already_present={len(ack.already_present)}"
            )
            return

        status = await apiary.submit_load(
            images,
            wait=True,
            poll=args.poll_interval,
            timeout=args.timeout,
            on_progress=_print_progress,
        )
        if not isinstance(status, ImageJobStatus):
            # Defensive: submit_load returns either the ack (no-wait) or
            # the terminal status (wait). We requested wait=True so we
            # must have an ImageJobStatus here.
            print(
                f"ERROR: unexpected non-status return from submit_load: {type(status)!r}",
                file=sys.stderr,
            )
            sys.exit(1)
        succeeded = status.succeeded
        failed = status.failed
        print(
            f"[apiary-load-swebench] terminal state={status.state} "
            f"succeeded={len(succeeded)} failed={len(failed)}"
        )
        for entry in status.failed_images:
            print(
                f"  FAIL {entry.get('name')}: {entry.get('reason')}",
                file=sys.stderr,
            )
        if status.state == "failed":
            sys.exit(2)
    finally:
        await apiary.close()


if __name__ == "__main__":
    main()

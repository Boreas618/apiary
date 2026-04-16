#!/usr/bin/env python3
"""Resolve unique Docker image names from a SWE-bench dataset.

Writes an image list file (one name per line) consumable by Apiary's
``[images] source_list`` config field.

Usage examples::

    # SWE-bench Lite test split (default)
    python -m apiary_client.swebench.resolve_images --write-list images.txt

    # SWE-bench Verified
    python -m apiary_client.swebench.resolve_images --dataset verified --write-list images.txt

    # Local JSON/JSONL file
    python -m apiary_client.swebench.resolve_images --dataset ./instances.jsonl --write-list images.txt

    # Just print to stdout
    python -m apiary_client.swebench.resolve_images --list-only

    # With batching (for parallel runs)
    python -m apiary_client.swebench.resolve_images --batch-size 50 --batch-id 0 --write-list batch0.txt
"""

from __future__ import annotations

import argparse
import json
import logging
import sys
from pathlib import Path

logger = logging.getLogger(__name__)

DATASET_ALIASES = {
    "full": "princeton-nlp/SWE-bench",
    "verified": "princeton-nlp/SWE-bench_Verified",
    "lite": "princeton-nlp/SWE-bench_Lite",
    "multimodal": "princeton-nlp/SWE-bench_Multimodal",
    "multilingual": "swe-bench/SWE-Bench_Multilingual",
}


def get_docker_image(instance: dict) -> str:
    """Derive the Docker image name from a SWE-bench instance."""
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


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Resolve SWE-bench dataset to Docker image list for Apiary",
    )
    parser.add_argument(
        "--dataset",
        default="lite",
        help="Dataset alias (lite, full, verified, ...), HuggingFace id, or path to JSON/JSONL",
    )
    parser.add_argument("--split", default="test", help="HuggingFace split")
    parser.add_argument(
        "--lite-dev",
        action="store_true",
        help="Shorthand for --dataset lite --split dev",
    )
    parser.add_argument(
        "--write-list",
        type=Path,
        help="Write image list file (one name per line)",
    )
    parser.add_argument(
        "--list-only",
        action="store_true",
        help="Print unique image names to stdout and exit",
    )
    parser.add_argument("--batch-size", type=int, default=0, help="Images per batch (0 = all)")
    parser.add_argument("--batch-id", type=int, default=0, help="Which batch (0-based)")

    args = parser.parse_args()

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
    )

    if args.lite_dev:
        args.dataset = "lite"
        args.split = "dev"

    instances = load_instances(args.dataset, args.split)
    if not instances:
        print("ERROR: no instances loaded", file=sys.stderr)
        sys.exit(1)

    images = sorted(set(get_docker_image(inst) for inst in instances))
    total_unique = len(images)
    logger.info(
        "%d instances → %d unique images (dataset=%s split=%s)",
        len(instances),
        total_unique,
        args.dataset,
        args.split,
    )

    if args.batch_size > 0:
        start = args.batch_id * args.batch_size
        if start >= len(images):
            num_batches = (len(images) + args.batch_size - 1) // args.batch_size
            print(
                f"ERROR: batch slice empty: batch_id={args.batch_id} "
                f"batch_size={args.batch_size} but only {len(images)} unique images "
                f"(use batch_id in 0..{num_batches})",
                file=sys.stderr,
            )
            sys.exit(1)
        end = min(start + args.batch_size, len(images))
        images = images[start:end]
        logger.info(
            "batch mode: batch_id=%d batch_size=%d → images [%d..%d) of %d unique",
            args.batch_id,
            args.batch_size,
            start,
            end,
            total_unique,
        )

    if args.write_list:
        args.write_list.write_text("\n".join(images) + "\n")
        logger.info("Wrote %d image names to %s", len(images), args.write_list)

    if args.list_only or not args.write_list:
        for img in images:
            print(img)


if __name__ == "__main__":
    main()

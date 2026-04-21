"""SWE-bench dataset utilities for Apiary sandboxes.

Resolve a SWE-bench dataset to image names and load them into a running
Apiary daemon::

    apiary-load-swebench --apiary-url http://127.0.0.1:8080 --dataset lite

Pure resolution (no daemon contact)::

    from apiary_client.swebench.load import resolve
    images = resolve("lite", "test")

Requires the ``swebench`` extra::

    pip install apiary-client[swebench]
"""

from apiary_client.swebench.load import (
    DATASET_ALIASES,
    get_docker_image,
    load_instances,
    resolve,
)

__all__ = ["DATASET_ALIASES", "get_docker_image", "load_instances", "resolve"]

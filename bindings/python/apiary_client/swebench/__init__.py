"""SWE-bench dataset utilities for Apiary sandboxes.

    apiary-resolve-images --dataset lite --write-list images.txt

Requires the ``swebench`` extra::

    pip install apiary-client[swebench]
"""

from apiary_client.swebench.resolve_images import (
    DATASET_ALIASES,
    get_docker_image,
    load_instances,
)

__all__ = ["DATASET_ALIASES", "get_docker_image", "load_instances"]

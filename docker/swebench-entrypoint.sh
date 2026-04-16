#!/usr/bin/env bash
set -eu

# SWE-bench deployment entrypoint.
#
# Resolves a SWE-bench dataset to Docker image names, initialises the Apiary
# sandbox pool, and starts the daemon.  All behaviour is configurable via
# environment variables — see defaults below.
#
# Called as CMD by Dockerfile.swebench; the shared entrypoint.sh (cgroup
# setup) runs first as ENTRYPOINT and then `exec`s into this script.

# ── Defaults ──────────────────────────────────────────────────────────────────

: "${SWEBENCH_DATASET:=lite}"
: "${SWEBENCH_SPLIT:=test}"
: "${SWEBENCH_BATCH_SIZE:=0}"
: "${SWEBENCH_BATCH_ID:=0}"
: "${APIARY_MAX_SANDBOXES:=40}"
: "${APIARY_BIND:=0.0.0.0:8080}"
: "${APIARY_IMAGE_LIST:=}"
: "${APIARY_API_TOKEN:=}"
: "${APIARY_LAYERS_DIR:=/var/lib/apiary/layers}"

IMAGE_LIST="/tmp/apiary-images.txt"

# ── Step 1: Resolve image list ────────────────────────────────────────────────

if [ -n "$APIARY_IMAGE_LIST" ]; then
    if [ ! -f "$APIARY_IMAGE_LIST" ]; then
        echo "[swebench] ERROR: APIARY_IMAGE_LIST=$APIARY_IMAGE_LIST does not exist" >&2
        exit 1
    fi
    IMAGE_LIST="$APIARY_IMAGE_LIST"
    count=$(grep -c '[^[:space:]]' "$IMAGE_LIST" || true)
    echo "[swebench] Using provided image list: $IMAGE_LIST ($count images)"
else
    resolve_args=(
        --dataset "$SWEBENCH_DATASET"
        --split   "$SWEBENCH_SPLIT"
        --write-list "$IMAGE_LIST"
    )
    if [ "$SWEBENCH_BATCH_SIZE" -gt 0 ]; then
        resolve_args+=(--batch-size "$SWEBENCH_BATCH_SIZE" --batch-id "$SWEBENCH_BATCH_ID")
    fi
    echo "[swebench] Resolving images: dataset=$SWEBENCH_DATASET split=$SWEBENCH_SPLIT" \
         "batch_size=$SWEBENCH_BATCH_SIZE batch_id=$SWEBENCH_BATCH_ID"
    apiary-resolve-images "${resolve_args[@]}"
    count=$(grep -c '[^[:space:]]' "$IMAGE_LIST" || true)
    echo "[swebench] Resolved $count unique images"
fi

# ── Step 2: Initialise the sandbox pool ───────────────────────────────────────

echo "[swebench] Initialising pool: max_sandboxes=$APIARY_MAX_SANDBOXES layers_dir=$APIARY_LAYERS_DIR"
apiary init \
    --image "$IMAGE_LIST" \
    --layers-dir "$APIARY_LAYERS_DIR" \
    --max-sandboxes "$APIARY_MAX_SANDBOXES"

# ── Step 3: Start the daemon ─────────────────────────────────────────────────

daemon_args=(--bind "$APIARY_BIND")
if [ -n "$APIARY_API_TOKEN" ]; then
    daemon_args+=(--api-token "$APIARY_API_TOKEN")
fi

echo "[swebench] Starting daemon on $APIARY_BIND"
exec apiary daemon "${daemon_args[@]}"

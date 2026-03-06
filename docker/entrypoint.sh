#!/usr/bin/env bash
set -eu

echo "[apiary] Setting up cgroups v2..."

# --- Mount cgroup2 filesystem if not already present ---
if ! mountpoint -q /sys/fs/cgroup 2>/dev/null || \
   [ ! -f /sys/fs/cgroup/cgroup.controllers ]; then
    mount -t cgroup2 none /sys/fs/cgroup
    echo "[apiary] Mounted cgroup2 filesystem"
fi

# --- Discover available controllers ---
controllers=$(cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null || true)
if [ -z "$controllers" ]; then
    echo "[apiary] WARNING: no cgroup controllers available from host"
    exec "$@"
fi

subtree_ctl=""
for c in cpu io memory pids; do
    if echo "$controllers" | grep -qw "$c"; then
        subtree_ctl="$subtree_ctl +$c"
    fi
done

if [ -z "$subtree_ctl" ]; then
    echo "[apiary] WARNING: none of the required controllers (cpu io memory pids) available"
    exec "$@"
fi

# --- Move PID 1 out of root cgroup ---
# This script IS PID 1, so it is the ONLY process in the root cgroup.
# Moving a single PID is deterministic — no retry loop, no race condition.
mkdir -p /sys/fs/cgroup/init
echo $$ > /sys/fs/cgroup/init/cgroup.procs

# --- Enable controllers on the now-empty root ---
echo "$subtree_ctl" > /sys/fs/cgroup/cgroup.subtree_control
echo "[apiary] Enabled controllers:$subtree_ctl"

# --- Create apiary subtree and propagate controllers ---
mkdir -p /sys/fs/cgroup/apiary
echo "$subtree_ctl" > /sys/fs/cgroup/apiary/cgroup.subtree_control 2>/dev/null || true

enabled=$(cat /sys/fs/cgroup/apiary/cgroup.controllers 2>/dev/null || true)
echo "[apiary] cgroup ready at /sys/fs/cgroup/apiary (controllers: $enabled)"

exec "$@"

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

required="cpu io memory pids"
available_list=""
for c in $required; do
    if echo "$controllers" | grep -qw "$c"; then
        available_list="$available_list $c"
    fi
done

if [ -z "$available_list" ]; then
    echo "[apiary] WARNING: none of the required controllers ($required) available"
    exec "$@"
fi

# --- Move PID 1 to a leaf cgroup ---
# cgroup v2's "no internal process" rule: a cgroup with subtree_control
# must not have processes. The "root is exempt" exception only applies to
# the real host root cgroup, NOT to a cgroup namespace root (which is what
# Docker's "cgroup: private" gives us). So we must move out of the
# namespace root before enabling domain controllers like memory and io.
mkdir -p /sys/fs/cgroup/daemon
echo $$ > /sys/fs/cgroup/daemon/cgroup.procs

# --- Enable controllers on the now-empty namespace root ---
# Write one controller at a time: a batch write with any unavailable
# controller causes the kernel to reject the entire line (EINVAL).
enabled_root=""
for c in $available_list; do
    if echo "+$c" > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null; then
        enabled_root="$enabled_root $c"
    else
        echo "[apiary] WARNING: failed to enable controller '$c' on root cgroup"
    fi
done
echo "[apiary] Enabled controllers on root:$enabled_root"

# --- Create apiary subtree for sandbox cgroups ---
mkdir -p /sys/fs/cgroup/apiary
for c in $enabled_root; do
    echo "+$c" > /sys/fs/cgroup/apiary/cgroup.subtree_control 2>/dev/null || true
done

enabled=$(cat /sys/fs/cgroup/apiary/cgroup.controllers 2>/dev/null || true)
echo "[apiary] cgroup ready at /sys/fs/cgroup/apiary (controllers: ${enabled:-none})"

# Tell Apiary where to create sandbox cgroups. Without this, Apiary would
# read /proc/self/cgroup (which returns /daemon after the move above) and
# create sandbox cgroups under /daemon/apiary/ — missing the subtree_control
# we just set up on /apiary/.
export APIARY_CGROUP_BASE=/sys/fs/cgroup/apiary

missing=""
for c in $required; do
    if ! echo "$enabled" | grep -qw "$c"; then
        missing="$missing $c"
    fi
done
if [ -n "$missing" ]; then
    echo "[apiary] WARNING: controllers not available from host:$missing"
    echo "[apiary]   resource limits for these will not be enforced"
fi

exec "$@"

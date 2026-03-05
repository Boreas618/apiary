#!/usr/bin/env bash
set -eu

APIARY_USER="${APIARY_USER:-apiary}"
APIARY_GROUP="$(id -gn "$APIARY_USER" 2>/dev/null || echo "$APIARY_USER")"

echo "[apiary] Setting up cgroups v2 delegation..."

# --- Phase 1: Mount cgroup2 filesystem ---
if ! mountpoint -q /sys/fs/cgroup 2>/dev/null || \
   [ ! -f /sys/fs/cgroup/cgroup.controllers ]; then
    mount -t cgroup2 none /sys/fs/cgroup
    echo "[apiary] Mounted cgroup2 filesystem"
fi

# --- Phase 2: Discover available controllers ---
controllers=$(cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null || true)
if [ -z "$controllers" ]; then
    echo "[apiary] WARNING: no cgroup controllers available from host"
    exec gosu "$APIARY_USER" "$@"
fi

subtree_ctl=""
for c in cpu io memory pids; do
    if echo "$controllers" | grep -qw "$c"; then
        subtree_ctl="$subtree_ctl +$c"
    fi
done

if [ -z "$subtree_ctl" ]; then
    echo "[apiary] WARNING: none of the required controllers (cpu io memory pids) available"
    exec gosu "$APIARY_USER" "$@"
fi

# --- Phase 3: Move PID 1 out of root cgroup ---
#
# This is the key advantage of running as a Docker entrypoint: this script
# IS PID 1, so it is the ONLY process in the root cgroup.  Moving a single
# PID is deterministic — no retry loop, no race condition.
mkdir -p /sys/fs/cgroup/init
echo $$ > /sys/fs/cgroup/init/cgroup.procs

# --- Phase 4: Enable controllers on the now-empty root ---
echo "$subtree_ctl" > /sys/fs/cgroup/cgroup.subtree_control
echo "[apiary] Enabled controllers:$subtree_ctl"

# --- Phase 5: Create delegated subtree and hand ownership to unprivileged user ---
mkdir -p /sys/fs/cgroup/apiary
chown -R "$APIARY_USER":"$APIARY_GROUP" /sys/fs/cgroup/apiary

# cgroup v2 process migration requires write access to cgroup.procs in the
# common ancestor of source and destination.  Sandbox processes start in
# /init and move to /apiary/sandbox-xxx; their common ancestor is root.
chown "$APIARY_USER":"$APIARY_GROUP" /sys/fs/cgroup/cgroup.procs
chown "$APIARY_USER":"$APIARY_GROUP" /sys/fs/cgroup/init/cgroup.procs 2>/dev/null || true

# --- Phase 6: Propagate controllers into the apiary subtree ---
echo "$subtree_ctl" > /sys/fs/cgroup/apiary/cgroup.subtree_control 2>/dev/null || true

enabled=$(cat /sys/fs/cgroup/apiary/cgroup.controllers 2>/dev/null || true)
echo "[apiary] cgroup delegation ready at /sys/fs/cgroup/apiary (owner: $APIARY_USER, controllers: $enabled)"

# --- Phase 7: Match container user UID/GID to the workspace mount owner ---
if [ -d /workspace ]; then
    HOST_UID=$(stat -c '%u' /workspace)
    HOST_GID=$(stat -c '%g' /workspace)
    CUR_UID=$(id -u "$APIARY_USER")
    CUR_GID=$(id -g "$APIARY_USER")

    if [ "$HOST_GID" != "$CUR_GID" ]; then
        groupmod -o -g "$HOST_GID" "$APIARY_GROUP" 2>/dev/null || true
    fi
    if [ "$HOST_UID" != "$CUR_UID" ]; then
        usermod -o -u "$HOST_UID" "$APIARY_USER" 2>/dev/null || true
    fi
fi

# --- Drop privileges and exec into the requested command ---
exec gosu "$APIARY_USER" "$@"

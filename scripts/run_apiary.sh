#!/usr/bin/env bash
set -euo pipefail

APIARY_DIR=/lustre/fs1/portfolios/coreai/projects/coreai_comparch_trtllm/users/rysun/apiary
APIARY_BIN=${APIARY_BIN:-$APIARY_DIR/target/release/apiary}
ROOTFS_DIR=${ROOTFS_DIR:-$APIARY_DIR/rootfs}
LOG_DIR=${LOG_DIR:-$APIARY_DIR/logs}
MCP_DIR=${MCP_DIR:-$APIARY_DIR/mcp}

APIARY_BIND=${APIARY_BIND:-127.0.0.1:38080}
MCP_BIND=${MCP_BIND:-0.0.0.0:38082}

MIN_SANDBOXES=${MIN_SANDBOXES:-10}
MAX_SANDBOXES=${MAX_SANDBOXES:-100}

# How long to wait for each service (seconds)
HEALTH_TIMEOUT=${HEALTH_TIMEOUT:-120}

CARGO_HOME=${CARGO_HOME:-/lustre/fs1/portfolios/coreai/projects/coreai_comparch_trtllm/users/rysun/.cargo}
export PATH="$CARGO_HOME/bin:$PATH"

# ---------------------------------------------------------------------------
# Overlay directory setup
#
# Kernel overlayfs cannot be stacked on top of another overlayfs (EINVAL).
# Inside an enroot/pyxis container the root filesystem IS overlayfs, so any
# path on it (including /root/.local/share/...) will fail.
#
# We mount a dedicated tmpfs for the overlay upper/work directories. tmpfs
# supports d_type, xattrs, and is never itself overlayfs, so kernel overlay
# mounts succeed.  The data is ephemeral, but sandboxes are too.
# ---------------------------------------------------------------------------
OVERLAY_DIR=${OVERLAY_DIR:-/tmp/apiary-overlays}
mkdir -p "$OVERLAY_DIR"

if ! mountpoint -q "$OVERLAY_DIR" 2>/dev/null; then
    echo "[apiary-run] Mounting tmpfs at $OVERLAY_DIR for overlay upper/work dirs..."
    if mount -t tmpfs -o size=8g tmpfs "$OVERLAY_DIR" 2>/dev/null; then
        echo "  tmpfs mounted at $OVERLAY_DIR (8 GiB)"
    else
        echo "  WARNING: tmpfs mount failed; falling back to $OVERLAY_DIR on existing FS"
        echo "  (kernel overlayfs may fail if the backing FS is itself overlayfs)"
    fi
else
    echo "[apiary-run] $OVERLAY_DIR is already a mountpoint, reusing it."
fi

export APIARY_OVERLAY_DIR="$OVERLAY_DIR"

echo "========================================"
echo "[apiary-run] Launching Apiary stack"
echo "========================================"
echo "  APIARY_BIN:     $APIARY_BIN"
echo "  ROOTFS_DIR:     $ROOTFS_DIR"
echo "  OVERLAY_DIR:    $OVERLAY_DIR"
echo "  APIARY_BIND:    $APIARY_BIND"
echo "  MCP_BIND:       $MCP_BIND"
echo "  MIN_SANDBOXES:  $MIN_SANDBOXES"
echo "  MAX_SANDBOXES:  $MAX_SANDBOXES"
echo ""

mkdir -p "$LOG_DIR"

# ---------------------------------------------------------------------------
# Preflight checks
# ---------------------------------------------------------------------------
echo "[apiary-run] Preflight checks..."

if [ ! -x "$APIARY_BIN" ]; then
    echo "ERROR: apiary binary not found at $APIARY_BIN"
    echo "  Run build_apiary.sh first."
    exit 1
fi

if [ ! -d "$ROOTFS_DIR/bin" ]; then
    echo "ERROR: rootfs not found at $ROOTFS_DIR"
    echo "  Run create_rootfs.sh first."
    exit 1
fi

# Install fuse-overlayfs as a fallback overlay driver (in case kernel
# overlayfs still fails even on tmpfs, e.g. due to AppArmor/SELinux).
echo "[apiary-run] Ensuring fuse-overlayfs is available..."
if ! command -v fuse-overlayfs >/dev/null 2>&1; then
    if command -v apt-get >/dev/null 2>&1; then
        apt-get update -qq && apt-get install -y -qq fuse-overlayfs 2>/dev/null && \
            echo "  fuse-overlayfs installed via apt" || \
            echo "  WARNING: failed to install fuse-overlayfs (kernel overlay must succeed)"
    elif command -v dnf >/dev/null 2>&1; then
        dnf install -y -q fuse-overlayfs 2>/dev/null && \
            echo "  fuse-overlayfs installed via dnf" || \
            echo "  WARNING: failed to install fuse-overlayfs (kernel overlay must succeed)"
    else
        echo "  WARNING: fuse-overlayfs not found and no package manager available"
    fi
else
    echo "  fuse-overlayfs: OK ($(command -v fuse-overlayfs))"
fi

# Verify sandbox prerequisites (non-fatal warnings)
echo "[apiary-run] Checking sandbox prerequisites..."

if command -v unshare >/dev/null 2>&1; then
    echo "  unshare: OK ($(command -v unshare))"
else
    echo "  WARNING: unshare not found"
fi

if command -v newuidmap >/dev/null 2>&1 && command -v newgidmap >/dev/null 2>&1; then
    echo "  uidmap helpers: OK"
else
    echo "  WARNING: newuidmap/newgidmap not found (install uidmap)"
fi

USER_NAME=$(id -un)
if grep -q "^${USER_NAME}:" /etc/subuid 2>/dev/null; then
    echo "  /etc/subuid: OK ($USER_NAME)"
else
    echo "  WARNING: /etc/subuid has no entry for $USER_NAME"
    echo "${USER_NAME}:100000:65536" >> /etc/subuid 2>/dev/null || true
fi
if grep -q "^${USER_NAME}:" /etc/subgid 2>/dev/null; then
    echo "  /etc/subgid: OK ($USER_NAME)"
else
    echo "  WARNING: /etc/subgid has no entry for $USER_NAME"
    echo "${USER_NAME}:100000:65536" >> /etc/subgid 2>/dev/null || true
fi

if unshare --user --map-root-user true >/dev/null 2>&1; then
    echo "  user namespace: OK"
else
    echo "  WARNING: user namespace check failed"
fi

# ---------------------------------------------------------------------------
# Setup cgroups v2 (adapted from docker/entrypoint.sh)
# ---------------------------------------------------------------------------
echo ""
echo "[apiary-run] Setting up cgroups v2..."

setup_cgroups() {
    # pyxis/enroot mounts /sys/fs/cgroup read-only by default.
    # Remount rw so sandboxes can create sub-cgroups for resource limits.
    if mountpoint -q /sys/fs/cgroup 2>/dev/null && \
       ! touch /sys/fs/cgroup/.rw-test 2>/dev/null; then
        echo "  /sys/fs/cgroup is read-only, attempting remount..."
        if mount -o remount,rw /sys/fs/cgroup 2>/dev/null; then
            echo "  remounted /sys/fs/cgroup as rw"
        else
            echo "  WARNING: remount failed; trying fresh cgroup2 mount..."
            umount /sys/fs/cgroup 2>/dev/null || true
            mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null || true
        fi
    elif ! mountpoint -q /sys/fs/cgroup 2>/dev/null; then
        mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null || true
    else
        rm -f /sys/fs/cgroup/.rw-test 2>/dev/null || true
    fi

    local controllers
    controllers=$(cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null || true)
    if [ -z "$controllers" ]; then
        echo "  WARNING: no cgroup controllers available"
        return
    fi
    echo "  available controllers: $controllers"

    local subtree_ctl=""
    for c in cpu io memory pids; do
        if echo "$controllers" | grep -qw "$c"; then
            subtree_ctl="$subtree_ctl +$c"
        fi
    done

    if [ -z "$subtree_ctl" ]; then
        echo "  WARNING: no required controllers (cpu/io/memory/pids) available"
        return
    fi

    # Determine the cgroup base: use the current process's cgroup if possible
    # (Slurm jobs are placed in a subtree; writing to root often fails).
    local cgroup_base="/sys/fs/cgroup"
    local my_cgroup
    my_cgroup=$(grep -oP '0::\K.*' /proc/self/cgroup 2>/dev/null || true)
    if [ -n "$my_cgroup" ] && [ "$my_cgroup" != "/" ]; then
        local candidate="${cgroup_base}${my_cgroup}"
        if [ -d "$candidate" ] && [ -w "$candidate" ]; then
            cgroup_base="$candidate"
            echo "  using job cgroup: $cgroup_base"
        fi
    fi

    # Move current process out of the leaf cgroup if needed
    if [ -w "${cgroup_base}/cgroup.procs" ]; then
        mkdir -p "${cgroup_base}/init" 2>/dev/null || true
        echo $$ > "${cgroup_base}/init/cgroup.procs" 2>/dev/null || true
    fi

    echo "$subtree_ctl" > "${cgroup_base}/cgroup.subtree_control" 2>/dev/null || true

    mkdir -p "${cgroup_base}/apiary" 2>/dev/null || true
    echo "$subtree_ctl" > "${cgroup_base}/apiary/cgroup.subtree_control" 2>/dev/null || true

    local enabled
    enabled=$(cat "${cgroup_base}/apiary/cgroup.controllers" 2>/dev/null || true)
    echo "  cgroup ready at ${cgroup_base}/apiary (controllers: $enabled)"
}

setup_cgroups

# ---------------------------------------------------------------------------
# Cleanup stale state
# ---------------------------------------------------------------------------
echo ""
echo "[apiary-run] Cleaning up stale state..."
"$APIARY_BIN" clean --force 2>/dev/null || true

# ---------------------------------------------------------------------------
# Initialize sandbox pool
# ---------------------------------------------------------------------------
echo ""
echo "[apiary-run] Initializing sandbox pool..."
"$APIARY_BIN" init \
    --base-image "$ROOTFS_DIR" \
    --min-sandboxes "$MIN_SANDBOXES" \
    --max-sandboxes "$MAX_SANDBOXES" \
    2>&1 | tee "$LOG_DIR/apiary_init.log"

# ---------------------------------------------------------------------------
# Start apiary daemon (background)
# ---------------------------------------------------------------------------
echo ""
echo "[apiary-run] Starting apiary daemon on $APIARY_BIND..."

APIARY_HOST=$(echo "$APIARY_BIND" | cut -d: -f1)
APIARY_PORT=$(echo "$APIARY_BIND" | cut -d: -f2)

export RUST_BACKTRACE=1
export RUST_LOG=${RUST_LOG:-info}
APIARY_URL="http://${APIARY_BIND}"

_kill_port() {
    local port=$1
    fuser -k "${port}/tcp" 2>/dev/null || true
    local pids
    pids=$(ss -tlnp "sport = :${port}" 2>/dev/null | grep -oP 'pid=\K[0-9]+' || true)
    for p in $pids; do
        kill "$p" 2>/dev/null || true
    done
}

MAX_BIND_RETRIES=5
for _attempt in $(seq 1 $MAX_BIND_RETRIES); do
    _kill_port "$APIARY_PORT"
    sleep 1

    "$APIARY_BIN" daemon --bind "$APIARY_BIND" \
        > "$LOG_DIR/apiary_daemon.log" 2>&1 &
    APIARY_PID=$!
    echo "  PID: $APIARY_PID (attempt $_attempt/$MAX_BIND_RETRIES)"

    # Wait for apiary health
    echo "[apiary-run] Waiting for apiary daemon to be ready..."
    elapsed=0
    while ! curl -fsS --connect-timeout 2 "${APIARY_URL}/healthz" >/dev/null 2>&1; do
        if ! kill -0 "$APIARY_PID" 2>/dev/null; then
            echo "  WARNING: daemon process (PID $APIARY_PID) exited"
            if [ "$_attempt" -lt "$MAX_BIND_RETRIES" ]; then
                echo "  Retrying..."
                tail -5 "$LOG_DIR/apiary_daemon.log" 2>/dev/null || true
            fi
            break
        fi
        elapsed=$((elapsed + 2))
        if [ $elapsed -ge $HEALTH_TIMEOUT ]; then
            echo "ERROR: apiary daemon failed to start after ${HEALTH_TIMEOUT}s"
            echo "--- daemon log (tail) ---"
            tail -50 "$LOG_DIR/apiary_daemon.log"
            kill $APIARY_PID 2>/dev/null || true
            exit 1
        fi
        echo "  waiting... (${elapsed}s / ${HEALTH_TIMEOUT}s)"
        sleep 2
    done

    if kill -0 "$APIARY_PID" 2>/dev/null; then
        break
    fi
done

if ! kill -0 "$APIARY_PID" 2>/dev/null; then
    echo "ERROR: apiary daemon failed to start after $MAX_BIND_RETRIES attempts"
    echo "--- daemon log ---"
    cat "$LOG_DIR/apiary_daemon.log"
    exit 1
fi

echo "[apiary-run] Apiary daemon is ready!"
curl -sS "${APIARY_URL}/api/v1/status" | python3 -m json.tool 2>/dev/null || true

# ---------------------------------------------------------------------------
# Start MCP server (background)
# ---------------------------------------------------------------------------
echo ""
echo "[apiary-run] Starting MCP server on $MCP_BIND..."

MCP_HOST=$(echo "$MCP_BIND" | cut -d: -f1)
MCP_PORT=$(echo "$MCP_BIND" | cut -d: -f2)

_kill_port "$MCP_PORT"
sleep 1

if command -v uv >/dev/null 2>&1; then
    cd "$MCP_DIR"
    uv run python apiary_mcp.py \
        --host "$MCP_HOST" \
        --port "$MCP_PORT" \
        --apiary-url "$APIARY_URL" \
        --idle-timeout 600 \
        > "$LOG_DIR/mcp_server.log" 2>&1 &
    MCP_PID=$!
else
    python3 "$MCP_DIR/apiary_mcp.py" \
        --host "$MCP_HOST" \
        --port "$MCP_PORT" \
        --apiary-url "$APIARY_URL" \
        --idle-timeout 600 \
        > "$LOG_DIR/mcp_server.log" 2>&1 &
    MCP_PID=$!
fi
echo "  PID: $MCP_PID"

# Wait for MCP health
echo "[apiary-run] Waiting for MCP server to be ready..."
MCP_URL="http://${MCP_BIND}"
elapsed=0
while true; do
    if curl -fsS --connect-timeout 2 --max-time 5 "${MCP_URL}/health" >/dev/null 2>&1; then
        break
    fi
    if ! kill -0 "$MCP_PID" 2>/dev/null; then
        echo "ERROR: MCP server process (PID $MCP_PID) exited prematurely"
        echo "--- MCP log ---"
        cat "$LOG_DIR/mcp_server.log"
        kill $APIARY_PID 2>/dev/null || true
        exit 1
    fi
    elapsed=$((elapsed + 2))
    if [ $elapsed -ge $HEALTH_TIMEOUT ]; then
        echo "ERROR: MCP server failed to start after ${HEALTH_TIMEOUT}s"
        echo "--- MCP log (tail) ---"
        tail -50 "$LOG_DIR/mcp_server.log"
        kill $MCP_PID 2>/dev/null || true
        kill $APIARY_PID 2>/dev/null || true
        exit 1
    fi
    echo "  waiting... (${elapsed}s / ${HEALTH_TIMEOUT}s)"
    sleep 2
done

echo "[apiary-run] MCP server is ready!"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "========================================"
echo "[apiary-run] Apiary stack is running"
echo "========================================"
echo "  Apiary daemon:  $APIARY_URL  (PID $APIARY_PID)"
echo "  MCP server:     $MCP_URL  (PID $MCP_PID)"
echo "  Logs:           $LOG_DIR/"
echo ""
echo "  Pool status:"
curl -sS "${APIARY_URL}/api/v1/status" 2>/dev/null | python3 -m json.tool 2>/dev/null || true
echo ""
echo "  To run the concurrent test:"
echo "    python3 $APIARY_DIR/scripts/test_mcp_concurrent.py --mode apiary --url $APIARY_URL"
echo "    python3 $APIARY_DIR/scripts/test_mcp_concurrent.py --mode mcp --url $MCP_URL"
echo ""

# ---------------------------------------------------------------------------
# If called with --test, run the concurrent test automatically
# ---------------------------------------------------------------------------
if [[ "${1:-}" == "--test" ]]; then
    shift
    echo "[apiary-run] Running concurrent agent test..."
    python3 "$APIARY_DIR/scripts/test_mcp_concurrent.py" \
        --mode apiary \
        --url "$APIARY_URL" \
        --apiary-url "$APIARY_URL" \
        --output "$LOG_DIR/test_results_apiary.json" \
        "$@" \
        2>&1 | tee "$LOG_DIR/test_apiary.log"

    echo ""
    echo "[apiary-run] Running MCP concurrent test..."
    python3 "$APIARY_DIR/scripts/test_mcp_concurrent.py" \
        --mode mcp \
        --url "$MCP_URL" \
        --apiary-url "$APIARY_URL" \
        --output "$LOG_DIR/test_results_mcp.json" \
        "$@" \
        2>&1 | tee "$LOG_DIR/test_mcp.log"

    echo ""
    echo "[apiary-run] All tests complete. Results in $LOG_DIR/"
fi

# ---------------------------------------------------------------------------
# Keep running (for interactive use or long-running jobs)
# ---------------------------------------------------------------------------
if [[ "${1:-}" == "--foreground" ]] || [[ "${APIARY_FOREGROUND:-}" == "1" ]]; then
    echo "[apiary-run] Running in foreground. Press Ctrl+C to stop."
    trap 'echo "[apiary-run] Shutting down..."; kill $MCP_PID $APIARY_PID 2>/dev/null; wait' EXIT INT TERM
    wait $APIARY_PID $MCP_PID
fi

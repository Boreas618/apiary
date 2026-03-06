#!/usr/bin/env bash
set -u

echo "[apiary] Verifying sandbox prerequisites..."

# --- Namespace tools ---
if command -v unshare >/dev/null 2>&1; then
    echo "[apiary] unshare: $(command -v unshare)"
else
    echo "[apiary] WARNING: unshare not found in PATH"
fi

# --- uidmap helpers ---
if command -v newuidmap >/dev/null 2>&1 && command -v newgidmap >/dev/null 2>&1; then
    echo "[apiary] uidmap helpers: OK"
else
    echo "[apiary] WARNING: newuidmap/newgidmap not found (install uidmap package)"
fi

# --- Subordinate ID ranges ---
user_name="$(id -un)"
if grep -q "^${user_name}:" /etc/subuid 2>/dev/null; then
    echo "[apiary] /etc/subuid entry: OK (${user_name})"
else
    echo "[apiary] WARNING: /etc/subuid has no entry for ${user_name}"
fi
if grep -q "^${user_name}:" /etc/subgid 2>/dev/null; then
    echo "[apiary] /etc/subgid entry: OK (${user_name})"
else
    echo "[apiary] WARNING: /etc/subgid has no entry for ${user_name}"
fi

# --- Seccomp status ---
if awk '/Seccomp|NoNewPrivs/ {print}' /proc/self/status >/tmp/apiary-seccomp-status 2>/dev/null; then
    sed 's/^/[apiary] /' /tmp/apiary-seccomp-status
    rm -f /tmp/apiary-seccomp-status
fi

# --- User namespace ---
if unshare --user --map-root-user true >/dev/null 2>&1; then
    echo "[apiary] user namespace: OK"
else
    echo "[apiary] WARNING: user namespace check failed"
fi

# --- cgroup ---
if [ -d /sys/fs/cgroup/apiary ]; then
    controllers=$(cat /sys/fs/cgroup/apiary/cgroup.controllers 2>/dev/null || true)
    echo "[apiary] cgroup: OK (controllers: $controllers)"
else
    echo "[apiary] WARNING: cgroup subtree not set up at /sys/fs/cgroup/apiary"
fi

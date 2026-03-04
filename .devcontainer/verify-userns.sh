#!/usr/bin/env bash
set -u

echo "[apiary] Verifying namespace prerequisites..."

if command -v unshare >/dev/null 2>&1; then
    echo "[apiary] unshare: $(command -v unshare)"
else
    echo "[apiary] WARNING: unshare not found in PATH."
fi

if command -v newuidmap >/dev/null 2>&1 && command -v newgidmap >/dev/null 2>&1; then
    echo "[apiary] uidmap helpers: OK"
else
    echo "[apiary] WARNING: newuidmap/newgidmap not found (install uidmap package)."
fi

user_name="$(id -un)"
if grep -q "^${user_name}:" /etc/subuid 2>/dev/null; then
    echo "[apiary] /etc/subuid entry: OK (${user_name})"
else
    echo "[apiary] WARNING: /etc/subuid has no entry for ${user_name}."
fi

if grep -q "^${user_name}:" /etc/subgid 2>/dev/null; then
    echo "[apiary] /etc/subgid entry: OK (${user_name})"
else
    echo "[apiary] WARNING: /etc/subgid has no entry for ${user_name}."
fi

if awk '/Seccomp|NoNewPrivs/ {print}' /proc/self/status >/tmp/apiary-seccomp-status 2>/dev/null; then
    sed 's/^/[apiary] /' /tmp/apiary-seccomp-status
    rm -f /tmp/apiary-seccomp-status
fi

if unshare --user --map-root-user true >/dev/null 2>&1; then
    echo "[apiary] user namespace check: OK"
else
    echo "[apiary] WARNING: user namespace check failed (unshare blocked)."
    echo "[apiary]          Ensure run args include --security-opt seccomp=unconfined."
fi

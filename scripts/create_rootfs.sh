#!/usr/bin/env bash
set -euo pipefail

APIARY_DIR=/lustre/fs1/portfolios/coreai/projects/coreai_comparch_trtllm/users/rysun/apiary
ROOTFS_DIR=${ROOTFS_DIR:-$APIARY_DIR/rootfs}
ARCH=$(dpkg --print-architecture 2>/dev/null || echo "arm64")
DISTRO=${DISTRO:-jammy}
MIRROR=${MIRROR:-http://ports.ubuntu.com/ubuntu-ports}

echo "========================================"
echo "[apiary-rootfs] Creating sandbox rootfs"
echo "========================================"
echo "  Target:  $ROOTFS_DIR"
echo "  Arch:    $ARCH"
echo "  Distro:  $DISTRO"
echo ""

if [ -d "$ROOTFS_DIR/bin" ] && [ -d "$ROOTFS_DIR/usr" ]; then
    echo "[apiary-rootfs] Rootfs already exists at $ROOTFS_DIR, skipping creation."
    echo "  To force recreation, run: rm -rf $ROOTFS_DIR"
    exit 0
fi

mkdir -p "$ROOTFS_DIR"

# ---------------------------------------------------------------------------
# Method 1: debootstrap (preferred — creates a clean minimal Ubuntu rootfs)
# ---------------------------------------------------------------------------
try_debootstrap() {
    if ! command -v debootstrap >/dev/null 2>&1; then
        echo "[apiary-rootfs] debootstrap not found, trying fallback..."
        return 1
    fi

    echo "[apiary-rootfs] Creating rootfs via debootstrap ($DISTRO, $ARCH)..."
    debootstrap --variant=minbase --arch="$ARCH" "$DISTRO" "$ROOTFS_DIR" "$MIRROR" 2>&1

    mkdir -p "$ROOTFS_DIR"/{workspace,tmp,proc,sys,dev,run}
    chmod 1777 "$ROOTFS_DIR/tmp"
    echo "[apiary-rootfs] debootstrap rootfs created successfully."
    return 0
}

# ---------------------------------------------------------------------------
# Method 2: Extract minimal rootfs from running system
# ---------------------------------------------------------------------------
try_system_extract() {
    echo "[apiary-rootfs] Creating rootfs from running system (fallback)..."

    for d in bin sbin lib usr etc; do
        if [ -d "/$d" ]; then
            echo "  Copying /$d ..."
            cp -a "/$d" "$ROOTFS_DIR/" 2>/dev/null || true
        fi
    done

    # lib64 is a symlink on some systems, handle both cases
    if [ -L /lib64 ]; then
        cp -a /lib64 "$ROOTFS_DIR/" 2>/dev/null || true
    elif [ -d /lib64 ]; then
        cp -a /lib64 "$ROOTFS_DIR/" 2>/dev/null || true
    fi

    mkdir -p "$ROOTFS_DIR"/{proc,sys,dev,tmp,run,workspace,root,home,var/{tmp,log,run}}
    chmod 1777 "$ROOTFS_DIR/tmp" "$ROOTFS_DIR/var/tmp"

    # Minimal /etc fixups for sandbox use
    echo "root:x:0:0:root:/root:/bin/bash" > "$ROOTFS_DIR/etc/passwd"
    echo "root:x:0:" > "$ROOTFS_DIR/etc/group"
    echo "sandbox" > "$ROOTFS_DIR/etc/hostname"
    echo "nameserver 8.8.8.8" > "$ROOTFS_DIR/etc/resolv.conf"

    echo "[apiary-rootfs] System-extract rootfs created."
    return 0
}

# ---------------------------------------------------------------------------
# Method 3: Busybox minimal rootfs (last resort)
# ---------------------------------------------------------------------------
try_busybox() {
    echo "[apiary-rootfs] Creating minimal busybox rootfs (last resort)..."

    apt-get install -y --no-install-recommends busybox-static 2>/dev/null || true

    BUSYBOX=$(command -v busybox 2>/dev/null || echo "")
    if [ -z "$BUSYBOX" ]; then
        echo "[apiary-rootfs] ERROR: busybox not found"
        return 1
    fi

    mkdir -p "$ROOTFS_DIR"/{bin,sbin,usr/{bin,sbin},etc,tmp,proc,sys,dev,run,workspace,root}
    cp "$BUSYBOX" "$ROOTFS_DIR/bin/busybox"

    # Create symlinks for common applets
    for cmd in sh bash ash cat ls cp mv rm mkdir rmdir head tail echo printf \
               test tr wc cut sort uniq grep sed awk tee touch chmod chown \
               ln readlink stat sleep true false pwd env id whoami hostname \
               uname date df du find xargs tar gzip gunzip vi less more; do
        ln -sf busybox "$ROOTFS_DIR/bin/$cmd" 2>/dev/null || true
    done

    echo "root:x:0:0:root:/root:/bin/sh" > "$ROOTFS_DIR/etc/passwd"
    echo "root:x:0:" > "$ROOTFS_DIR/etc/group"
    echo "sandbox" > "$ROOTFS_DIR/etc/hostname"
    chmod 1777 "$ROOTFS_DIR/tmp"

    echo "[apiary-rootfs] Busybox rootfs created."
    return 0
}

# ---------------------------------------------------------------------------
# Try each method in order
# ---------------------------------------------------------------------------
if try_debootstrap; then
    :
elif try_system_extract; then
    :
elif try_busybox; then
    :
else
    echo "[apiary-rootfs] ERROR: all rootfs creation methods failed"
    exit 1
fi

echo ""
echo "[apiary-rootfs] Rootfs ready at $ROOTFS_DIR"
echo "  Size: $(du -sh "$ROOTFS_DIR" 2>/dev/null | cut -f1)"
echo "  Contents:"
ls "$ROOTFS_DIR"/

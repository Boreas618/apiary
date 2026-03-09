#!/usr/bin/env bash
set -euo pipefail

APIARY_DIR=/lustre/fs1/portfolios/coreai/projects/coreai_comparch_trtllm/users/rysun/apiary
CARGO_HOME=${CARGO_HOME:-/lustre/fs1/portfolios/coreai/projects/coreai_comparch_trtllm/users/rysun/.cargo}
RUSTUP_HOME=${RUSTUP_HOME:-/lustre/fs1/portfolios/coreai/projects/coreai_comparch_trtllm/users/rysun/.rustup}

export CARGO_HOME RUSTUP_HOME

echo "========================================"
echo "[apiary-build] Building Apiary"
echo "========================================"
echo "  APIARY_DIR:  $APIARY_DIR"
echo "  CARGO_HOME:  $CARGO_HOME"
echo "  RUSTUP_HOME: $RUSTUP_HOME"
echo "  arch:        $(uname -m)"
echo "  kernel:      $(uname -r)"
echo ""

# ---------------------------------------------------------------------------
# 1. Install system packages (uidmap, fuse-overlayfs, debootstrap)
# ---------------------------------------------------------------------------
echo "[apiary-build] Installing system packages..."
apt-get update -qq
apt-get install -y --no-install-recommends \
    uidmap \
    fuse-overlayfs \
    util-linux \
    iproute2 \
    procps \
    debootstrap \
    curl \
    ca-certificates \
    pkg-config \
    2>/dev/null || echo "[apiary-build] WARNING: some packages could not be installed (non-fatal)"

# ---------------------------------------------------------------------------
# 2. Install Rust via rustup (persisted to lustre)
# ---------------------------------------------------------------------------
if [ -x "$CARGO_HOME/bin/cargo" ]; then
    echo "[apiary-build] Rust already installed at $CARGO_HOME"
    export PATH="$CARGO_HOME/bin:$PATH"
    rustc --version
    cargo --version
else
    echo "[apiary-build] Installing Rust via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --default-toolchain stable
    export PATH="$CARGO_HOME/bin:$PATH"
    echo "[apiary-build] Rust installed:"
    rustc --version
    cargo --version
fi

# ---------------------------------------------------------------------------
# 3. Build apiary (release mode, LTO enabled via Cargo.toml)
# ---------------------------------------------------------------------------
echo ""
echo "[apiary-build] Building apiary binary..."
cd "$APIARY_DIR"

NPROC=$(nproc 2>/dev/null || echo 64)
JOBS=$((NPROC > 128 ? 128 : NPROC))

cargo build --release -j "$JOBS" 2>&1

BINARY="$APIARY_DIR/target/release/apiary"
if [ ! -x "$BINARY" ]; then
    echo "[apiary-build] ERROR: binary not found at $BINARY"
    exit 1
fi

echo "[apiary-build] Binary built: $BINARY"
ls -lh "$BINARY"
file "$BINARY"

# ---------------------------------------------------------------------------
# 4. Install MCP Python dependencies
# ---------------------------------------------------------------------------
echo ""
echo "[apiary-build] Installing MCP Python dependencies..."

if command -v uv >/dev/null 2>&1; then
    echo "[apiary-build] Using uv for Python package management"
    cd "$APIARY_DIR/mcp"
    uv sync 2>&1
elif command -v pip3 >/dev/null 2>&1; then
    echo "[apiary-build] Using pip3 for Python package management"
    pip3 install --quiet httpx mcp starlette uvicorn 2>&1
else
    echo "[apiary-build] WARNING: neither uv nor pip3 found; MCP deps not installed"
fi

# ---------------------------------------------------------------------------
# 5. Configure subordinate ID ranges
# ---------------------------------------------------------------------------
echo ""
echo "[apiary-build] Configuring subordinate ID ranges..."
USER_NAME=$(id -un)
if ! grep -q "^${USER_NAME}:" /etc/subuid 2>/dev/null; then
    echo "${USER_NAME}:100000:65536" >> /etc/subuid 2>/dev/null || true
    echo "[apiary-build] Added $USER_NAME to /etc/subuid"
fi
if ! grep -q "^${USER_NAME}:" /etc/subgid 2>/dev/null; then
    echo "${USER_NAME}:100000:65536" >> /etc/subgid 2>/dev/null || true
    echo "[apiary-build] Added $USER_NAME to /etc/subgid"
fi

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------
echo ""
echo "========================================"
echo "[apiary-build] Build complete"
echo "========================================"
echo "  Binary:  $BINARY"
echo "  MCP dir: $APIARY_DIR/mcp/"
echo ""
echo "Next steps:"
echo "  1. Create rootfs:  bash $APIARY_DIR/scripts/create_rootfs.sh"
echo "  2. Run apiary:     bash $APIARY_DIR/scripts/run_apiary.sh"

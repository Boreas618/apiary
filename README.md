# Apiary

A lightweight sandbox pool for running AI agent tasks on Linux with namespace isolation.

## Features

- **Namespace Isolation**: User, Mount, PID, IPC, and UTS namespace isolation for each sandbox
- **Docker Image Support**: Use any Docker image as sandbox base; layers are extracted and cached locally
- **OverlayFS**: Shared read-only base layers with per-sandbox writable layers (saves 95%+ disk space)
- **seccomp**: Syscall filtering for security (configurable network blocking)
- **cgroups v2**: Resource limits (CPU, memory, PIDs, I/O) with rlimit fallback
- **Rootless**: Runs without root privileges using user namespaces (Linux 5.11+)
- **On-demand Sandboxes**: Dedicated sandbox per session, created on-demand up to `max_sandboxes`
- **Persistent Sessions**: Pin a sandbox to a session and keep filesystem state across commands
- **Daemon API**: HTTP REST API with optional Bearer token authentication
- **Python Client**: `apiary-client` package with sync/async clients and MCP server

## Requirements

- Linux kernel 5.11+ (for rootless OverlayFS)
- cgroups v2 with delegation (for rootless resource limits)
- `uidmap` package (`newuidmap`/`newgidmap`) for subordinate ID mappings
- Docker CLI (for pulling and inspecting images)
- Rust 1.70+ (build only)

### Rootless namespace prerequisites

Apiary needs `CLONE_NEWUSER` support. If your distribution disables it:

```bash
sudo sysctl -w kernel.unprivileged_userns_clone=1
```

Configure subordinate ID ranges:

```bash
# /etc/subuid
your-user:100000:65536

# /etc/subgid
your-user:100000:65536
```

## Installation

```bash
git clone https://github.com/Boreas618/apiary.git
cd apiary
cargo build --release
```

## Docker

All Dockerfiles live in `docker/`:

| File | Purpose |
|------|---------|
| `docker/Dockerfile` | Production image (multi-stage build, minimal runtime) |
| `docker/Dockerfile.dev` | Development container (full Rust toolchain, source bind-mounted) |
| `docker/Dockerfile.swebench` | SWE-bench deployment (prod + Python image-resolution tooling) |

### Development

```bash
# Build and enter the dev container
docker compose up -d
docker compose exec apiary bash

# Inside the container: build and run
cargo build --release
apiary init --image ubuntu:22.04
apiary daemon --bind 0.0.0.0:8080
```

The dev container is preconfigured with all sandbox prerequisites:

- `uidmap`, `fuse-overlayfs`, subordinate ID ranges
- Full Docker engine (docker-in-docker via socket mount)
- cgroups v2 delegation (set up by `entrypoint.sh`)
- `SYS_ADMIN` capability, `seccomp=unconfined`
- Run `verify-sandbox.sh` inside the container for a health check

### Production

```bash
# Build the production image
docker build -f docker/Dockerfile -t apiary .

# Run (requires host Docker socket for image layer extraction)
docker run --rm \
  --cap-add SYS_ADMIN --cap-add MKNOD \
  --device /dev/fuse \
  --security-opt seccomp=unconfined \
  --security-opt apparmor=unconfined \
  --cgroup-parent apiary.slice \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -p 8080:8080 \
  apiary
```

### SWE-bench Deployment

One-command deployment for SWE-bench evaluation:

```bash
# Default: SWE-bench Lite, test split, all images
docker compose -f docker-compose.swebench.yml up -d

# SWE-bench Verified, only first 50 images
SWEBENCH_DATASET=verified SWEBENCH_BATCH_SIZE=50 \
  docker compose -f docker-compose.swebench.yml up -d

# Custom image list (skip SWE-bench resolution entirely)
APIARY_IMAGE_LIST=/data/my-images.txt \
  docker compose -f docker-compose.swebench.yml up -d
```

Environment variables for parameterization:

| Variable | Default | Purpose |
|---|---|---|
| `SWEBENCH_DATASET` | `lite` | Dataset alias (`lite`, `full`, `verified`, `multimodal`, `multilingual`), HuggingFace id, or local JSON/JSONL path |
| `SWEBENCH_SPLIT` | `test` | HuggingFace split |
| `SWEBENCH_BATCH_SIZE` | `0` (all) | Subset size for parallel deployment |
| `SWEBENCH_BATCH_ID` | `0` | Which batch (0-based) |
| `APIARY_MAX_SANDBOXES` | `40` | Pool concurrency cap |
| `APIARY_API_TOKEN` | (empty) | Bearer token for API authentication |
| `APIARY_IMAGE_LIST` | (empty) | Bypass resolution -- use a pre-made list file directly |

## Quick Start

### 1. Initialize the Pool

```bash
# Single image
apiary init --image ubuntu:22.04

# Multiple images
apiary init --image ubuntu:22.04 --image python:3.12-slim

# From an image list file (one name per line, # comments allowed)
apiary init --image images.txt --max-sandboxes 20

# Custom directories
apiary init --image ubuntu:22.04 \
  --layers-dir /data/apiary/layers \
  --overlay-dir /data/apiary/overlays
```

This pulls missing Docker images, extracts their layers into a content-addressable cache, and writes a config file to `~/.config/apiary/config.toml`.

### 2. Start the Daemon

```bash
apiary daemon --bind 127.0.0.1:8080

# With API authentication
apiary daemon --bind 0.0.0.0:8080 --api-token my-secret-token
```

### 3. Use the API

```bash
# Create a session
SESSION_ID=$(curl -sS -X POST http://127.0.0.1:8080/api/v1/sessions \
  -H "Content-Type: application/json" \
  -d '{"image":"ubuntu:22.04","working_dir":"/workspace"}' | jq -r '.session_id')

# Execute a command
curl -sS -X POST "http://127.0.0.1:8080/api/v1/sessions/${SESSION_ID}/exec" \
  -H "Content-Type: application/json" \
  -d '{"command":"echo hello from apiary"}'

# Close session
curl -sS -X DELETE "http://127.0.0.1:8080/api/v1/sessions/${SESSION_ID}"
```

## CLI Reference

```
apiary [OPTIONS] <COMMAND>

Commands:
  init     Initialize the sandbox pool (pull images, extract layers, write config)
  daemon   Start the HTTP API server
  status   Show pool configuration
  clean    Remove sandbox data and config

Options:
  -c, --config <PATH>   Config file path (default: ~/.config/apiary/config.toml)
  -v, --verbose         Increase log verbosity (-v = debug, -vv = trace)
```

### `apiary init`

```
Options:
  --image <NAME|FILE>   Docker image name or path to image-list file (repeatable, required)
  --layers-dir <DIR>    Layer cache directory [default: /tmp/apiary_layers]
  --max-sandboxes <N>   Hard ceiling for concurrent sessions [default: 40]
  --overlay-dir <DIR>   Directory for per-sandbox overlay layers
```

### `apiary daemon`

```
Options:
  --bind <ADDR>         Listen address [default: 127.0.0.1:8080]
  --api-token <TOKEN>   Bearer token for API auth (also reads APIARY_API_TOKEN env)
```

## Daemon HTTP API

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/healthz` | Liveness probe |
| `GET` | `/api/v1/status` | Pool status and counters |
| `POST` | `/api/v1/sessions` | Create a session (reserves a sandbox) |
| `DELETE` | `/api/v1/sessions/{session_id}` | Close session, cleanup sandbox |
| `POST` | `/api/v1/sessions/{session_id}/exec` | Execute a command in the session |

### Create Session

```json
{
  "image": "ubuntu:22.04",
  "working_dir": "/workspace"
}
```

Both `image` and `working_dir` are required.

### Execute Command

```json
{
  "command": "bash -lc 'echo hello'",
  "timeout_ms": 30000,
  "working_dir": "/workspace/subdir",
  "env": { "MY_VAR": "hello" }
}
```

- `command` is required; all other fields are optional.
- `timeout_ms` defaults to the pool's `default_timeout` (300s).
- `working_dir` overrides the session default. Relative paths resolve against the session `working_dir`.
- `env` is merged with the pool's `default_env` (task values win on conflict).

### Session Persistence

Filesystem changes persist within a session:

```bash
# Write a file
curl -sS -X POST ".../exec" -d '{"command":"echo data > /workspace/file.txt"}'

# Read it back in the same session
curl -sS -X POST ".../exec" -d '{"command":"cat /workspace/file.txt"}'
# → "data"
```

Closing and recreating a session starts fresh.

## Configuration

Generated by `apiary init` at `~/.config/apiary/config.toml`:

```toml
max_sandboxes = 40
overlay_dir = "/home/user/.local/share/apiary/overlays"
default_timeout = "300s"
mount_host_resolv_conf = true

[images]
sources = ["ubuntu:22.04"]
layers_dir = "/tmp/apiary_layers"
docker = "docker"
pull_concurrency = 8

[resource_limits]
memory_max = "4G"
cpu_max = "100000 100000"
pids_max = 2048
max_open_files = 2048
# max_file_size = "1G"
# io_max = "8:0 rbps=52428800 wbps=41943040"

[seccomp_policy]
block_network = false
allow_unix_sockets = true
# blocked_syscalls = ["ptrace"]
# allowed_syscalls = []
```

## Library API

```rust
use apiary::{ImagesConfig, Pool, PoolConfig, SessionOptions, Task};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = PoolConfig::builder()
        .max_sandboxes(16)
        .images(ImagesConfig {
            sources: vec!["ubuntu:22.04".into()],
            layers_dir: "/tmp/apiary_layers".into(),
            docker: "docker".into(),
            pull_concurrency: 8,
        })
        .build()?;

    let pool = Pool::new(config).await?;

    let session_id = pool
        .create_session(SessionOptions::new("ubuntu:22.04", "/workspace"))
        .await?;

    let task = Task::new("echo hello")
        .timeout(std::time::Duration::from_secs(30));
    let result = pool.execute_in_session(&session_id, task).await?;

    println!("exit_code={} stdout={}", result.exit_code, result.stdout_lossy());

    pool.close_session(&session_id).await?;
    pool.shutdown().await;
    Ok(())
}
```

## Python Client

Install from the `bindings/python/` directory:

```bash
pip install ./bindings/python            # base client
pip install ./bindings/python[swebench]  # + SWE-bench image resolution
pip install ./bindings/python[mcp]       # + MCP server
```

### Usage

```python
from apiary_client import Apiary, AsyncApiary

# Synchronous
client = Apiary("http://127.0.0.1:8080")
session = client.create_session(image="ubuntu:22.04", working_dir="/workspace")
result = client.exec(session, "echo hello")
print(result["stdout"])
client.close_session(session)

# Async
async with AsyncApiary("http://127.0.0.1:8080") as client:
    session = await client.create_session(image="ubuntu:22.04", working_dir="/workspace")
    result = await client.exec(session, "echo hello")
    await client.close_session(session)
```

### SWE-bench Image Resolution

```bash
# Generate image list from SWE-bench Lite (test split)
apiary-resolve-images --write-list images.txt

# SWE-bench Verified
apiary-resolve-images --dataset verified --write-list images.txt

# Batched for parallel deployment
apiary-resolve-images --dataset full --batch-size 50 --batch-id 0 --write-list batch0.txt

# Feed into apiary
apiary init --image images.txt
```

### MCP Server

```bash
apiary-mcp --apiary-url http://127.0.0.1:8080
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      Daemon (HTTP API)                       │
│  ┌─────────────────────────────────────────────────────┐    │
│  │ axum router: /healthz, /api/v1/{status,sessions,..} │    │
│  └─────────────────────────────────────────────────────┘    │
└───────────────────────────┬─────────────────────────────────┘
                            │
┌───────────────────────────▼─────────────────────────────────┐
│                        Pool Manager                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────┐  │
│  │ Session Map  │  │ Sandbox Map  │  │ Image Registry   │  │
│  └──────────────┘  └──────────────┘  └──────────────────┘  │
└───────────────────────────┬─────────────────────────────────┘
                            │ on-demand creation
          ┌─────────────────┼─────────────────┐
          ▼                 ▼                 ▼
┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐
│  Sandbox #0     │ │  Sandbox #1     │ │  Sandbox #N     │
├─────────────────┤ ├─────────────────┤ ├─────────────────┤
│ User NS         │ │ User NS         │ │ User NS         │
│ Mount NS (ovl)  │ │ Mount NS (ovl)  │ │ Mount NS (ovl)  │
│ PID NS          │ │ PID NS          │ │ PID NS          │
│ seccomp filter  │ │ seccomp filter  │ │ seccomp filter  │
│ cgroup / rlimit │ │ cgroup / rlimit │ │ cgroup / rlimit │
└─────────────────┘ └─────────────────┘ └─────────────────┘
```

## Security

Each sandbox provides multiple layers of isolation:

1. **User Namespace**: Maps root inside sandbox to unprivileged user outside
2. **Mount Namespace**: Isolated filesystem view via OverlayFS (shared read-only base layers + per-sandbox writable upper)
3. **PID Namespace**: Isolated process tree
4. **IPC Namespace**: Isolated System V IPC and POSIX message queues
5. **UTS Namespace**: Isolated hostname
6. **seccomp**: Configurable syscall filtering (network blocking, custom allow/block lists)
7. **cgroups v2**: CPU, memory, PIDs, and I/O limits (with rlimit fallback when cgroups are unavailable)

## License

MIT

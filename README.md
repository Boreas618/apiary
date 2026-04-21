# Apiary

A lightweight sandbox pool for running AI agent tasks on Linux with namespace isolation.

## Features

- **Image-agnostic at startup**: The daemon starts with an empty registry. Clients register Docker images at runtime via HTTP; layers are pulled, extracted, and cached on demand.
- **Namespace Isolation**: User, Mount, PID, IPC, and UTS namespace isolation for each sandbox.
- **Docker Image Support**: Use any Docker image as sandbox base; layers are extracted and cached locally in a content-addressable store.
- **OverlayFS**: Shared read-only base layers with per-sandbox writable layers (saves 95%+ disk space).
- **seccomp**: Syscall filtering for security (configurable network blocking).
- **cgroups v2**: Resource limits (CPU, memory, PIDs, I/O) with rlimit fallback.
- **Rootless**: Runs without root privileges using user namespaces (Linux 5.11+).
- **On-demand Sandboxes**: Dedicated sandbox per session, created on-demand up to `max_sandboxes`.
- **Persistent Sessions**: Pin a sandbox to a session and keep filesystem state across commands.
- **Daemon API**: HTTP REST API with optional Bearer token authentication, including async image-load jobs.
- **Python Client**: `apiary-client` package with `Apiary` (canonical client) + `ApiarySession` (per-session) + MCP server.

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

### Production deployment

```bash
docker compose up -d
```

The container starts an apiary daemon with an empty image registry on port 8080. Clients register images at runtime via the HTTP API (see "Loading images at runtime" below).

Environment variables (override on the `docker compose` command line or in `.env`):

| Variable | Default | Purpose |
|---|---|---|
| `APIARY_PORT` | `8080` | Host port to publish |
| `APIARY_BIND` | `0.0.0.0:8080` | Bind address inside the container |
| `APIARY_API_TOKEN` | (empty) | Bearer token for API auth (empty disables auth) |
| `APIARY_MAX_SANDBOXES` | `40` | Pool concurrency cap |
| `APIARY_LAYERS_DIR` | `/var/lib/apiary/layers` | Layer cache (mounted as a named volume) |
| `APIARY_OVERLAY_DIR` | `/var/lib/apiary/overlays` | Overlay scratch (mounted as a named volume) |

### Development

```bash
# Build and enter the dev container (source is bind-mounted)
docker compose -f docker-compose.dev.yml up -d
docker compose -f docker-compose.dev.yml exec apiary bash

# Inside the container: build and run
cargo build --release
apiary init
apiary daemon --bind 0.0.0.0:8080
```

The dev container is preconfigured with all sandbox prerequisites:

- `uidmap`, `fuse-overlayfs`, subordinate ID ranges
- Full Docker engine (docker-in-docker via socket mount)
- cgroups v2 delegation (set up by `entrypoint.sh`)
- `SYS_ADMIN` capability, `seccomp=unconfined`
- Run `verify-sandbox.sh` inside the container for a health check

## Loading images at runtime

The daemon never pre-loads any image. Clients populate the registry over HTTP and then create sessions against the loaded images.

### Quick example with `curl`

```bash
# 1. Submit an image-load job (returns a job_id)
JOB_ID=$(curl -sS -X POST http://127.0.0.1:8080/api/v1/images \
  -H "Content-Type: application/json" \
  -d '{"images":["ubuntu:22.04","python:3.12-slim"]}' | jq -r '.job_id')

# 2. Poll until the job is terminal
while true; do
  STATE=$(curl -sS "http://127.0.0.1:8080/api/v1/images/jobs/${JOB_ID}" | jq -r '.state')
  echo "state=$STATE"
  [[ "$STATE" == "running" ]] || break
  sleep 2
done

# 3. Create a session against one of the loaded images
SESSION_ID=$(curl -sS -X POST http://127.0.0.1:8080/api/v1/sessions \
  -H "Content-Type: application/json" \
  -d '{"image":"ubuntu:22.04","working_dir":"/workspace"}' | jq -r '.session_id')

# 4. Execute a command
curl -sS -X POST "http://127.0.0.1:8080/api/v1/sessions/${SESSION_ID}/exec" \
  -H "Content-Type: application/json" \
  -d '{"command":"echo hello from apiary"}'

# 5. Close session
curl -sS -X DELETE "http://127.0.0.1:8080/api/v1/sessions/${SESSION_ID}"
```

### Quick example with the Python client

```python
import asyncio
from apiary_client import AsyncApiary

async def main():
    async with AsyncApiary(
        apiary_url="http://127.0.0.1:8080",
        images=["ubuntu:22.04", "python:3.12-slim"],
    ) as apiary:
        async with apiary.session(image="ubuntu:22.04") as s:
            result = await s.execute("echo hello from apiary")
            print(result.stdout)

asyncio.run(main())
```

See "Python Client" below for the full surface.

## CLI Reference

```
apiary [OPTIONS] <COMMAND>

Commands:
  init     Write a config file and prepare directories (no images required)
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
  --layers-dir <DIR>    Layer cache directory [default: /tmp/apiary_layers]
  --max-sandboxes <N>   Hard ceiling for concurrent sessions [default: 40]
  --overlay-dir <DIR>   Directory for per-sandbox overlay layers
```

`init` no longer takes an `--image` flag. The pool starts empty; images are added at runtime via the HTTP API.

### `apiary daemon`

```
Options:
  --bind <ADDR>         Listen address [default: 127.0.0.1:8080]
  --api-token <TOKEN>   Bearer token for API auth (also reads APIARY_API_TOKEN env)
```

## Daemon HTTP API

| Method | Path | Description |
|--------|------|-------------|
| `GET`    | `/healthz` | Liveness probe |
| `GET`    | `/api/v1/status` | Pool status, counters, registered image count |
| `POST`   | `/api/v1/sessions` | Create a session (404 if image not registered) |
| `DELETE` | `/api/v1/sessions/{session_id}` | Close session, cleanup sandbox |
| `POST`   | `/api/v1/sessions/{session_id}/exec` | Execute a command in the session |
| `GET`    | `/api/v1/images` | List registered image names |
| `POST`   | `/api/v1/images` | Submit an async image-load job (returns 202 + job_id) |
| `DELETE` | `/api/v1/images/{name}` | Drop an image from the registry |
| `GET`    | `/api/v1/images/jobs/{job_id}` | Poll image-load job status |

### Register Images (async)

```json
POST /api/v1/images
{
  "images": ["ubuntu:22.04", "docker.io/library/python:3.12-slim"]
}

→ 202
{
  "job_id": "9e4f...",
  "queued": ["docker.io/library/python:3.12-slim"],
  "already_present": ["ubuntu:22.04"]
}
```

### Poll Image Job

```json
GET /api/v1/images/jobs/9e4f...
{
  "job_id": "9e4f...",
  "state": "running",          // "running" | "done" | "failed"
  "started_at": "...",
  "updated_at": "...",
  "per_image": {
    "ubuntu:22.04":               { "state": "alreadypresent" },
    "docker.io/.../python:3.12":  { "state": "extracting", "layers_done": 7, "layers_total": 12 }
  },
  "failed_images": []
}
```

A job ends in `done` if at least one image succeeded, `failed` only when every image failed. Per-image failures appear in both `per_image[name].state == "failed"` and the top-level `failed_images` list.

### Create Session

```json
{
  "image": "ubuntu:22.04",
  "working_dir": "/workspace"
}
```

Both `image` and `working_dir` are required. Returns **404** if the image isn't registered yet — load it first via `POST /api/v1/images`.

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

[image_cache]
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

The `[image_cache]` section configures the layer cache; it does **not** enumerate which images the pool serves. Images are registered at runtime.

## Library API

```rust
use apiary::{LayerCacheConfig, Pool, PoolConfig, SessionOptions, Task};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = PoolConfig::builder()
        .max_sandboxes(16)
        .image_cache(LayerCacheConfig {
            layers_dir: "/tmp/apiary_layers".into(),
            docker: "docker".into(),
            pull_concurrency: 8,
        })
        .build()?;

    let pool = Pool::new(config).await?;

    // Register an image at runtime via the loader.
    pool.image_loader()
        .load_one("ubuntu:22.04", |_stage| {})
        .await;

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
pip install ./bindings/python[swebench]  # + SWE-bench image resolution helpers
pip install ./bindings/python[mcp]       # + MCP server
```

### Class layout

| Class | Role |
|---|---|
| `Apiary` / `AsyncApiary` | **Canonical client.** Image-set management + pool admin + session factory. |
| `ApiarySession` / `AsyncApiarySession` | Per-session client. Returned by `apiary.session(image=...)`; can also be constructed directly. |
| `ApiarySessionMux` | Multi-client session multiplexer (used by the MCP server). |

### Usage

```python
from apiary_client import AsyncApiary

# Batch driver: load image set, run jobs against it, exit.
async with AsyncApiary(
    apiary_url="http://127.0.0.1:8080",
    apiary_token=token,             # optional Bearer
    images=["ubuntu:22.04", "python:3.12-slim"],
) as apiary:
    async with apiary.session(image="ubuntu:22.04") as s:   # AsyncApiarySession
        result = await s.execute("echo hello")
        print(result.stdout)

# Pure admin: no image set, just inspect/modify the pool.
async with AsyncApiary(apiary_url="http://127.0.0.1:8080") as apiary:
    print(await apiary.all_images())
    await apiary.delete_image("stale:tag")
```

### SWE-bench Image Resolution

```bash
# Resolve a SWE-bench dataset and load the images into a running daemon
apiary-load-swebench --apiary-url http://127.0.0.1:8080 --dataset lite

# Pure resolution, no daemon needed
python -m apiary_client.swebench.load --dataset verified --print-only
```

### MCP Server

```bash
apiary-mcp --apiary-url http://127.0.0.1:8080 --image ubuntu:22.04
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      Daemon (HTTP API)                       │
│  axum router: /healthz, /api/v1/{status, sessions, images}   │
└───────────────────────────┬─────────────────────────────────┘
                            │
┌───────────────────────────▼─────────────────────────────────┐
│                        Pool Manager                          │
│  ┌────────┐ ┌────────┐ ┌─────────┐ ┌────────┐ ┌──────────┐ │
│  │Sessions│ │Sandbox │ │Image    │ │Image   │ │Image     │ │
│  │Map     │ │Map     │ │Registry │ │Loader  │ │Jobs      │ │
│  │        │ │        │ │(mut)    │ │(dedupe)│ │(tracker) │ │
│  └────────┘ └────────┘ └─────────┘ └────────┘ └──────────┘ │
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

1. **User Namespace**: Maps root inside sandbox to unprivileged user outside.
2. **Mount Namespace**: Isolated filesystem view via OverlayFS (shared read-only base layers + per-sandbox writable upper).
3. **PID Namespace**: Isolated process tree.
4. **IPC Namespace**: Isolated System V IPC and POSIX message queues.
5. **UTS Namespace**: Isolated hostname.
6. **seccomp**: Configurable syscall filtering (network blocking, custom allow/block lists).
7. **cgroups v2**: CPU, memory, PIDs, and I/O limits (with rlimit fallback when cgroups are unavailable).

## License

MIT

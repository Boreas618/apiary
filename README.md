# Sandbox Pool

A lightweight sandbox pool for running AI agent tasks on Linux with namespace isolation.

## Features

- **Namespace Isolation**: User, Mount, IPC, and UTS namespace isolation for each sandbox
- **OverlayFS**: Shared read-only base with per-sandbox writable layers (saves 95%+ disk space)
- **seccomp**: Network syscall filtering for security
- **cgroups v2**: Resource limits (CPU, memory, PIDs, I/O)
- **Rootless**: Can run without root privileges (Linux 5.11+)
- **Pool Management**: Pre-created sandbox pool for fast task execution
- **Batch Execution**: Run multiple tasks in parallel
- **Daemon API**: HTTP REST API for task execution
- **Persistent Sessions**: Pin a sandbox to a session and keep filesystem state across commands

## Requirements

- Linux kernel 5.11+ (for rootless OverlayFS)
- cgroups v2 with delegation (for rootless resource limits)
- `uidmap` package (`newuidmap`/`newgidmap`) for helper-based subordinate ID mappings
- Rust 1.70+

### Rootless namespace prerequisites

`apiary` always needs `CLONE_NEWUSER` support for non-root execution. If your distribution disables it, enable:

```bash
sudo sysctl -w kernel.unprivileged_userns_clone=1
```

For helper-based expanded ID maps, configure subordinate ranges and ensure setuid helpers exist:

```bash
# /etc/subuid
your-user:100000:65536

# /etc/subgid
your-user:100000:65536
```

### Recommended Distributions

- Ubuntu 22.04+
- Fedora 36+
- Debian 12+

## Installation

```bash
# Clone the repository
git clone https://github.com/Boreas618/apiary.git
cd apiary

# Build
cargo build --release

# Install (optional)
cargo install --path .
```

## Docker (Recommended)

This repository includes a `Dockerfile` and `docker-compose.yml` at the project root.

```bash
# Build and start the development container
docker compose run --rm apiary bash

# Or build and run manually
docker compose build
docker compose run --rm apiary cargo build
```

The Docker setup is preconfigured for sandbox features:

- Installs `uidmap` (`newuidmap`/`newgidmap`) and `fuse-overlayfs`
- Sets subordinate ID ranges in `/etc/subuid` and `/etc/subgid` for root
- Starts the container with security options needed for `unshare` (`SYS_ADMIN`, `seccomp=unconfined`)
- Entrypoint automatically sets up cgroups v2 delegation (controller enablement and subtree creation)
- The container runs as root; `apiary` then enters its own user namespace for rootless sandbox operation
- Run `verify-sandbox.sh` inside the container for a quick health check

## Quick Start

### 1. Create a Base Rootfs

```bash
# Using Docker (easiest)
mkdir -p rootfs
CID=$(sudo docker create ubuntu:jammy) \
  && sudo docker export "$CID" | tar -xf - --exclude='dev/*' -C rootfs \
  && sudo docker rm "$CID" > /dev/null

# Or using debootstrap (Ubuntu/Debian, requires native Linux filesystem)
sudo debootstrap --variant=minbase jammy ./rootfs
```

### 2. Initialize the Pool

```bash
apiary init --base-image ./rootfs --min-sandboxes 10 --max-sandboxes 40
```

### 3. Run Tasks

```bash
# Single task
apiary run --command "echo hello world"

# Batch tasks from JSON file
apiary batch --tasks tasks.json --parallelism 5
```

## Usage

### CLI

```bash
# Initialize pool
apiary init --base-image ./rootfs --min-sandboxes 10 --max-sandboxes 40

# Run daemon (for API access)
apiary daemon --bind 127.0.0.1:8080

# Execute single command
apiary run --command "python script.py" --timeout 60

# Execute batch tasks
apiary batch --tasks ./tasks.json --parallelism 10

# Show status
apiary status

# Cleanup
apiary clean --force
```

`apiary run` and `apiary batch` now run in session-only mode internally:
they create session(s), execute command(s), then close session(s) to release sandboxes.

### Daemon HTTP API

When running `apiary daemon`, the server exposes:

- `GET /healthz` - liveness probe
- `GET /api/v1/status` - pool status and counters (includes `reserved` session sandboxes)
- `POST /api/v1/sessions` - create a persistent session (reserves one sandbox; accepts optional `working_dir`)
- `DELETE /api/v1/sessions/:session_id` - close session, reset sandbox, and release it
- `POST /api/v1/tasks` - execute a task in a session and return JSON result (`session_id` required)

Working directory resolution order is:

1. Task `working_dir`, when provided
2. Session `working_dir`, when provided at session creation time
3. Config `default_workdir`

If a task `working_dir` is relative, it is resolved against the session `working_dir`.

Example JSON task request (`session_id` is required, `working_dir` is an optional task-level override):

```json
{
  "command": "bash -lc 'echo start && sleep 1 && echo done'",
  "timeout_secs": 30,
  "working_dir": "/workspace",
  "session_id": "required-session-id",
  "env": {
    "MY_VAR": "hello"
  }
}
```

Execute and wait for final JSON result:

```bash
# Create or reuse a session first
SESSION_ID=$(curl -sS -X POST "http://127.0.0.1:8080/api/v1/sessions" | jq -r '.session_id')

curl -sS \
  -X POST "http://127.0.0.1:8080/api/v1/tasks" \
  -H "Content-Type: application/json" \
  -d "{\"command\":\"echo hello from api\",\"session_id\":\"${SESSION_ID}\"}"
```

Create and use a persistent session (filesystem changes survive between commands):

```bash
# 1) Create session with a session-level working directory
SESSION_ID=$(curl -sS \
  -X POST "http://127.0.0.1:8080/api/v1/sessions" \
  -H "Content-Type: application/json" \
  -d '{"working_dir":"/workspace"}' | jq -r '.session_id')

# 2) First command writes a file
curl -sS \
  -X POST "http://127.0.0.1:8080/api/v1/tasks" \
  -H "Content-Type: application/json" \
  -d "{\"command\":\"bash -lc 'echo hello > /workspace/marker.txt'\",\"session_id\":\"${SESSION_ID}\"}"

# 3) Second command in same session can still read it
curl -sS \
  -X POST "http://127.0.0.1:8080/api/v1/tasks" \
  -H "Content-Type: application/json" \
  -d "{\"command\":\"cat /workspace/marker.txt\",\"session_id\":\"${SESSION_ID}\"}"

# 4) Close session and release sandbox back to pool
curl -sS -X DELETE "http://127.0.0.1:8080/api/v1/sessions/${SESSION_ID}"
```

### Library API

The library is session-only: create a session before execution, and close it when done.
Task `working_dir` overrides the session `working_dir`; otherwise the session default is used.

```rust
use apiary::{Pool, PoolConfig, SessionOptions, Task};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create configuration
    let config = PoolConfig::builder()
        .min_sandboxes(10)
        .max_sandboxes(10)
        .base_image("./rootfs")
        .build()?;

    // Initialize pool
    let pool = Pool::new(config).await?;

    // Create and execute a task
    let task = Task::new("echo hello")
        .timeout(Duration::from_secs(30))
        .env("MY_VAR", "value");

    let session_id = pool
        .create_session(SessionOptions::default().working_dir("/workspace"))
        .await?;
    let result = pool.execute_in_session(&session_id, task).await?;
    println!("Exit code: {}", result.exit_code);
    println!("Output: {}", result.stdout_lossy());

    pool.close_session(&session_id).await?;

    pool.shutdown().await;
    Ok(())
}
```

## Configuration

Configuration is stored in `~/.config/apiary/config.toml`:

```toml
min_sandboxes = 10
max_sandboxes = 40
scale_up_step = 2
idle_timeout = "300s"
cooldown = "30s"
base_image = "./rootfs"
overlay_dir = "~/.local/share/apiary/overlays"
default_timeout = "300s"
default_workdir = "/workspace"
enable_seccomp = false

[resource_limits]
memory_max = "2G"
cpu_max = "100000 100000"
pids_max = 256

[seccomp_policy]
block_network = true
allow_unix_sockets = true
```

## Tasks JSON Format

The batch tasks file uses the `Task` struct directly. Note that `timeout` is in
**milliseconds** (unlike the HTTP API which accepts `timeout_secs` or `timeout_ms`).

```json
[
  {
    "id": "task-1",
    "command": ["python3", "-c", "print('hello')"],
    "env": {"PYTHONPATH": "/app"},
    "working_dir": "/workspace",
    "timeout": 60000
  },
  {
    "id": "task-2", 
    "command": ["bash", "-c", "echo $MY_VAR"],
    "env": {"MY_VAR": "world"},
    "timeout": 30000
  }
]
```

If `working_dir` is omitted, the task inherits the session `working_dir`. Relative
task paths are resolved against the session `working_dir`.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      Sandbox Pool                           │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │ Pool Manager │  │ Task Queue   │  │ Config       │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
└─────────────────────────────────────────────────────────────┘
                              │
          ┌───────────────────┼───────────────────┐
          ▼                   ▼                   ▼
┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐
│  Sandbox #1     │  │  Sandbox #2     │  │  Sandbox #N     │
├─────────────────┤  ├─────────────────┤  ├─────────────────┤
│ User NS         │  │ User NS         │  │ User NS         │
│ Mount NS        │  │ Mount NS        │  │ Mount NS        │
│ PID NS          │  │ PID NS          │  │ PID NS          │
│ OverlayFS       │  │ OverlayFS       │  │ OverlayFS       │
│ seccomp         │  │ seccomp         │  │ seccomp         │
│ cgroup          │  │ cgroup          │  │ cgroup          │
└─────────────────┘  └─────────────────┘  └─────────────────┘
```

## Security

The sandbox provides multiple layers of isolation:

1. **User Namespace**: Maps root inside sandbox to unprivileged user outside
2. **Mount Namespace**: Isolated filesystem view with OverlayFS
3. **IPC Namespace**: Isolated System V IPC and POSIX message queues
4. **UTS Namespace**: Isolated hostname
5. **seccomp**: Blocks network syscalls and other dangerous operations (when enabled)
6. **cgroups**: Limits CPU, memory, and other resources

### Protected Paths

- `/proc`, `/sys`: Mounted with appropriate restrictions
- Network: Blocked by default via seccomp syscall filtering (note: this is not network namespace isolation; enable seccomp with `--seccomp`)

## License

MIT
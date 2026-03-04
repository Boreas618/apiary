# Sandbox Pool

A lightweight sandbox pool for running AI agent tasks on Linux with namespace isolation.

## Features

- **Namespace Isolation**: User, Mount, and PID namespace isolation for each sandbox
- **OverlayFS**: Shared read-only base with per-sandbox writable layers (saves 95%+ disk space)
- **seccomp**: Network syscall filtering for security
- **cgroups v2**: Resource limits (CPU, memory, PIDs, I/O)
- **Rootless**: Can run without root privileges (Linux 5.11+)
- **Pool Management**: Pre-created sandbox pool for fast task execution
- **Batch Execution**: Run multiple tasks in parallel
- **Daemon API**: HTTP REST with optional SSE task streaming

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
git clone https://github.com/your-username/apiary
cd apiary

# Build
cargo build --release

# Install (optional)
cargo install --path .
```

## Dev Container (One-Click)

This repository includes a ready-to-use devcontainer in `.devcontainer/`.

1. Open the project in Cursor or VS Code.
2. Run **Dev Containers: Reopen in Container**.
3. Wait for the initial image build and `postCreateCommand` to finish.

The devcontainer is preconfigured for this project's sandbox features:

- Installs `uidmap` (`newuidmap`/`newgidmap`) and `fuse-overlayfs`
- Sets subordinate ID ranges in `/etc/subuid` and `/etc/subgid`
- Starts the container with relaxed security options needed for `unshare`
- Runs `.devcontainer/verify-userns.sh` on startup to print a quick health check

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
apiary init --base-image ./rootfs --pool-size 10
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
apiary init --base-image ./rootfs --pool-size 10

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

### Daemon HTTP API

When running `apiary daemon`, the server exposes:

- `GET /healthz` - liveness probe
- `GET /api/v1/status` - pool status and counters
- `POST /api/v1/tasks` - execute a task and return JSON result
- `POST /api/v1/tasks?stream=true` - execute a task and stream output via SSE

Example JSON task request:

```json
{
  "command": "bash -lc 'echo start && sleep 1 && echo done'",
  "timeout_secs": 30,
  "working_dir": "/workspace",
  "env": {
    "MY_VAR": "hello"
  }
}
```

Execute and wait for final JSON result:

```bash
curl -sS \
  -X POST "http://127.0.0.1:8080/api/v1/tasks" \
  -H "Content-Type: application/json" \
  -d '{"command":"echo hello from api"}'
```

Execute with SSE streaming:

```bash
curl -N \
  -X POST "http://127.0.0.1:8080/api/v1/tasks?stream=true" \
  -H "Content-Type: application/json" \
  -d '{"command":"bash -lc '\''echo out; echo err 1>&2; sleep 1; echo done'\''"}'
```

### Library API

```rust
use apiary::{Pool, PoolConfig, Task};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create configuration
    let config = PoolConfig::builder()
        .pool_size(10)
        .base_image("./rootfs")
        .build()?;

    // Initialize pool
    let pool = Pool::new(config).await?;

    // Create and execute a task
    let task = Task::new("echo hello")
        .timeout(Duration::from_secs(30))
        .env("MY_VAR", "value");

    let result = pool.execute(task).await?;
    println!("Exit code: {}", result.exit_code);
    println!("Output: {}", result.stdout_lossy());

    // Execute batch
    let tasks = vec![
        Task::new("echo task1"),
        Task::new("echo task2"),
        Task::new("echo task3"),
    ];
    let results = pool.execute_batch(tasks).await;

    pool.shutdown().await;
    Ok(())
}
```

## Configuration

Configuration is stored in `~/.config/apiary/config.toml`:

```toml
pool_size = 10
base_image = "./rootfs"
overlay_dir = "~/.local/share/apiary/overlays"
default_timeout = "300s"
default_workdir = "/workspace"

[resource_limits]
memory_max = "2G"
cpu_max = "100000 100000"
pids_max = 256

[seccomp_policy]
block_network = true
allow_unix_sockets = true
```

## Tasks JSON Format

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
3. **PID Namespace**: Isolated process tree (PIDs start from 1)
4. **seccomp**: Blocks network syscalls and other dangerous operations
5. **cgroups**: Limits CPU, memory, and other resources

### Protected Paths

- `/proc`, `/sys`: Mounted with appropriate restrictions
- Network: Blocked by default via seccomp

## Performance

- Sandbox creation: ~1-5ms (reused from pool)
- Task overhead: <10ms
- Memory per sandbox: ~10MB base + overlay writes
- Disk per sandbox: Only stores diff from base image

## License

MIT
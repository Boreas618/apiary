//! # Sandbox Pool
//!
//! A lightweight sandbox pool for running AI agent tasks on Linux with namespace isolation.
//!
//! ## Features
//!
//! - **Namespace Isolation**: User, Mount, and PID namespace isolation for each sandbox
//! - **OverlayFS**: Shared read-only base with per-sandbox writable layers
//! - **seccomp**: Network syscall filtering for security
//! - **cgroups v2**: Resource limits (CPU, memory, PIDs, I/O)
//! - **Rootless**: Can run without root privileges (Linux 5.11+)
//! - **Pool Management**: Pre-created sandbox pool for fast task execution
//!
//! ## Example
//!
//! ```rust,no_run
//! use apiary::{Pool, PoolConfig, SessionOptions, Task};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = PoolConfig::builder()
//!         .min_sandboxes(4)
//!         .max_sandboxes(16)
//!         .base_image("./rootfs")
//!         .build()?;
//!
//!     let pool = Pool::new(config).await?;
//!
//!     let task = Task::new("echo hello")
//!         .timeout(std::time::Duration::from_secs(30));
//!
//!     let session_id = pool
//!         .create_session_with_options(SessionOptions::default().working_dir("/workspace"))
//!         .await?;
//!     let result = pool.execute_in_session(&session_id, task).await?;
//!     println!("Exit code: {}", result.exit_code);
//!     pool.close_session(&session_id).await?;
//!
//!     Ok(())
//! }
//! ```

pub mod config;
pub mod pool;
pub mod sandbox;
pub mod task;

pub use config::{PoolConfig, PoolConfigBuilder, ResourceLimits, SeccompPolicy};
pub use pool::{Pool, PoolError, SessionOptions};
pub use sandbox::overlay::OverlayDriver;
pub use sandbox::{Sandbox, SandboxError, SandboxState};
pub use task::{MountSpec, Task, TaskResult};
